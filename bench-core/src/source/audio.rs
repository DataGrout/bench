//! Audio input — the real path. A sound card is a 48 kHz, 16–24-bit ADC that
//! every machine has, and a phone playing a tone into it is a complete test
//! rig. Behind the `audio` feature so a fresh clone builds without system
//! audio libraries.
//!
//! # Threading
//!
//! `cpal` delivers samples on its own callback thread and its stream handle is
//! not `Send` on every platform. So the stream lives on a dedicated thread that
//! parks for the source's lifetime, the callback pushes into a shared staging
//! buffer, and [`Source::fill`] drains it — the push-to-pull adapter the trait
//! docs describe. Dropping the source unparks that thread, which drops the
//! stream.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::Source;

/// Most samples the staging buffer holds before the oldest are dropped —
/// one second at 192 kHz. The ring behind it is lossy by design too.
const STAGING_CAP: usize = 192_000;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio input device named {0:?}")]
    NoSuchDevice(String),
    #[error("no default audio input device")]
    NoDefaultDevice,
    #[error("could not read the device's input configuration: {0}")]
    Config(String),
    #[error("could not open the input stream: {0}")]
    Stream(String),
    #[error("unsupported sample format {0}")]
    Format(String),
}

/// A capture device, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
}

/// One input device, one channel, streaming into the ring.
pub struct AudioSource {
    device: String,
    sample_rate: f64,
    channels: u16,
    /// Which interleaved channel to take. Read by the callback.
    channel: Arc<AtomicUsize>,
    /// Linear gain as `f32` bits; applied in the callback.
    gain_bits: Arc<AtomicU64>,
    staging: Arc<Mutex<VecDeque<f32>>>,
    /// Samples the staging buffer threw away because nobody drained it.
    dropped: Arc<AtomicU64>,
    /// Peak absolute sample seen since the last read, as `f32` bits.
    peak_bits: Arc<AtomicU64>,
    /// Sending (or dropping) this ends the stream thread.
    _stop: Sender<()>,
}

impl AudioSource {
    /// Input devices the host reports, default first.
    pub fn devices() -> Vec<DeviceInfo> {
        let host = cpal::default_host();
        let default = host.default_input_device().and_then(|d| d.name().ok());
        let mut out: Vec<DeviceInfo> = host
            .input_devices()
            .map(|it| {
                it.filter_map(|d| d.name().ok())
                    .map(|name| DeviceInfo {
                        is_default: default.as_deref() == Some(name.as_str()),
                        name,
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|d| !d.is_default);
        out
    }

    /// Open a device by name, or the default input when `None`, at its own
    /// default sample rate.
    pub fn open(device_name: Option<&str>) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => host
                .input_devices()
                .map_err(|e| AudioError::Config(e.to_string()))?
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .ok_or_else(|| AudioError::NoSuchDevice(name.to_string()))?,
            None => host
                .default_input_device()
                .ok_or(AudioError::NoDefaultDevice)?,
        };
        let name = device.name().unwrap_or_else(|_| "audio input".to_string());
        let supported = device
            .default_input_config()
            .map_err(|e| AudioError::Config(e.to_string()))?;
        let sample_rate = supported.sample_rate().0 as f64;
        let channels = supported.channels();
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let channel = Arc::new(AtomicUsize::new(0));
        let gain_bits = Arc::new(AtomicU64::new(1.0f32.to_bits() as u64));
        let staging = Arc::new(Mutex::new(VecDeque::with_capacity(STAGING_CAP)));
        let dropped = Arc::new(AtomicU64::new(0));
        let peak_bits = Arc::new(AtomicU64::new(0));

        // The stream is built and kept on its own thread; see the module docs.
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), AudioError>>();
        {
            let channel = Arc::clone(&channel);
            let gain_bits = Arc::clone(&gain_bits);
            let staging = Arc::clone(&staging);
            let dropped = Arc::clone(&dropped);
            let peak_bits = Arc::clone(&peak_bits);
            std::thread::Builder::new()
                .name("bench-audio-input".into())
                .spawn(move || {
                    let sink = Sink {
                        channels: channels as usize,
                        channel,
                        gain_bits,
                        staging,
                        dropped,
                        peak_bits,
                    };
                    let err_fn = |e| tracing::warn!("audio input stream error: {e}");
                    let built = match format {
                        cpal::SampleFormat::F32 => device.build_input_stream(
                            &config,
                            move |data: &[f32], _| sink.push(data.iter().copied()),
                            err_fn,
                            None,
                        ),
                        cpal::SampleFormat::I16 => device.build_input_stream(
                            &config,
                            move |data: &[i16], _| {
                                sink.push(data.iter().map(|s| *s as f32 / i16::MAX as f32))
                            },
                            err_fn,
                            None,
                        ),
                        cpal::SampleFormat::U16 => device.build_input_stream(
                            &config,
                            move |data: &[u16], _| {
                                sink.push(data.iter().map(|s| (*s as f32 - 32_768.0) / 32_768.0))
                            },
                            err_fn,
                            None,
                        ),
                        other => {
                            let _ = ready_tx.send(Err(AudioError::Format(format!("{other:?}"))));
                            return;
                        }
                    };
                    let stream = match built {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = ready_tx.send(Err(AudioError::Stream(e.to_string())));
                            return;
                        }
                    };
                    if let Err(e) = stream.play() {
                        let _ = ready_tx.send(Err(AudioError::Stream(e.to_string())));
                        return;
                    }
                    let _ = ready_tx.send(Ok(()));
                    // Block until the source is dropped; the stream drops with us.
                    let _ = stop_rx.recv();
                })
                .map_err(|e| AudioError::Stream(e.to_string()))?;
        }
        ready_rx
            .recv()
            .map_err(|_| AudioError::Stream("stream thread exited early".into()))??;

        Ok(Self {
            device: name,
            sample_rate,
            channels,
            channel,
            gain_bits,
            staging,
            dropped,
            peak_bits,
            _stop: stop_tx,
        })
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// Interleaved channels the device delivers.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The channel being captured (0-based).
    pub fn channel(&self) -> usize {
        self.channel.load(Ordering::Relaxed)
    }

    pub fn set_channel(&self, channel: usize) {
        let ch = channel.min(self.channels.saturating_sub(1) as usize);
        self.channel.store(ch, Ordering::Relaxed);
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed) as u32)
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain_bits
            .store(gain.max(0.0).to_bits() as u64, Ordering::Relaxed);
    }

    /// Samples lost to a full staging buffer since opening.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Peak absolute level since the last call, then reset — an input meter.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed) as u32)
    }
}

/// The callback's side of the staging buffer.
struct Sink {
    channels: usize,
    channel: Arc<AtomicUsize>,
    gain_bits: Arc<AtomicU64>,
    staging: Arc<Mutex<VecDeque<f32>>>,
    dropped: Arc<AtomicU64>,
    peak_bits: Arc<AtomicU64>,
}

impl Sink {
    fn push(&self, interleaved: impl Iterator<Item = f32>) {
        let ch = self.channel.load(Ordering::Relaxed).min(self.channels - 1);
        let gain = f32::from_bits(self.gain_bits.load(Ordering::Relaxed) as u32);
        let mut peak = f32::from_bits(self.peak_bits.load(Ordering::Relaxed) as u32);
        let mut staging = self.staging.lock().expect("audio staging poisoned");
        let mut dropped = 0u64;
        for s in interleaved
            .enumerate()
            .filter(|(i, _)| i % self.channels == ch)
            .map(|(_, s)| s * gain)
        {
            peak = peak.max(s.abs());
            if staging.len() >= STAGING_CAP {
                staging.pop_front();
                dropped += 1;
            }
            staging.push_back(s);
        }
        drop(staging);
        self.peak_bits
            .store(peak.to_bits() as u64, Ordering::Relaxed);
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
    }
}

impl Source for AudioSource {
    fn name(&self) -> String {
        format!("audio: {}", self.device)
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn fill(&mut self, out: &mut Vec<f32>, max: usize) -> usize {
        out.clear();
        let mut staging = self.staging.lock().expect("audio staging poisoned");
        let n = max.min(staging.len());
        out.extend(staging.drain(..n));
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> Sink {
        Sink {
            channels: 2,
            channel: Arc::new(AtomicUsize::new(1)),
            gain_bits: Arc::new(AtomicU64::new(2.0f32.to_bits() as u64)),
            staging: Arc::new(Mutex::new(VecDeque::new())),
            dropped: Arc::new(AtomicU64::new(0)),
            peak_bits: Arc::new(AtomicU64::new(0)),
        }
    }

    #[test]
    fn the_sink_picks_one_interleaved_channel_and_applies_gain() {
        let s = sink();
        // L R L R
        s.push([0.1, 0.5, 0.2, -0.25].into_iter());
        let got: Vec<f32> = s.staging.lock().unwrap().iter().copied().collect();
        assert_eq!(got, vec![1.0, -0.5]);
        assert_eq!(
            f32::from_bits(s.peak_bits.load(Ordering::Relaxed) as u32),
            1.0
        );
    }

    #[test]
    fn a_full_staging_buffer_drops_the_oldest_and_counts_it() {
        let s = sink();
        s.push(std::iter::repeat_n(0.0, STAGING_CAP * 2 + 4));
        assert_eq!(s.staging.lock().unwrap().len(), STAGING_CAP);
        assert_eq!(s.dropped.load(Ordering::Relaxed), 2);
    }
}
