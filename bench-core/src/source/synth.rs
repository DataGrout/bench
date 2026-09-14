//! The function generator — a source whose output is known exactly.
//!
//! This exists to validate the chain, not to be interesting. When a spectrum
//! looks wrong you need a signal whose spectrum you can state from first
//! principles; every other source leaves you guessing whether the bug is in the
//! DSP or the world.

use std::f64::consts::TAU;

use serde::{Deserialize, Serialize};

use super::Source;

/// Waveform shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wave {
    Sine,
    Square,
    Saw,
    Triangle,
    /// Uniform white noise. Amplitude is peak, not RMS.
    Noise,
}

/// One additive component of the generated signal.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tone {
    pub wave: Wave,
    pub freq_hz: f64,
    pub amplitude: f64,
    /// Muted components stay in the list but contribute nothing — the way to
    /// hear what one part of a stacked signal is doing.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

impl Wave {
    pub const ALL: [Wave; 5] = [
        Wave::Sine,
        Wave::Square,
        Wave::Saw,
        Wave::Triangle,
        Wave::Noise,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Wave::Sine => "sine",
            Wave::Square => "square",
            Wave::Saw => "saw",
            Wave::Triangle => "triangle",
            Wave::Noise => "noise",
        }
    }
}

impl Tone {
    pub fn new(wave: Wave, freq_hz: f64, amplitude: f64) -> Self {
        Self {
            wave,
            freq_hz,
            amplitude,
            enabled: true,
        }
    }

    pub fn sine(freq_hz: f64, amplitude: f64) -> Self {
        Self::new(Wave::Sine, freq_hz, amplitude)
    }
}

/// Named starting points for the generator.
///
/// Each is a signal whose spectrum you can state in advance, which is what a
/// preset is for on a bench: you dial one in, run the chain, and know what the
/// answer should have been. Frequencies assume audio rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// 440 Hz with a 15% third harmonic — the default; THD has something to measure.
    Harmonic440,
    /// Two close tones at equal amplitude; the beat envelope is the point.
    Beat,
    /// A 1 kHz square: odd harmonics falling off as 1/n.
    Square1k,
    /// A 300 Hz sawtooth: every harmonic, falling off as 1/n.
    Saw300,
    /// A 2 kHz triangle: odd harmonics falling off as 1/n².
    Triangle2k,
    /// A quiet tone under heavy noise, for filters to earn their keep.
    BuriedTone,
    /// White noise only: a flat spectrum, no fundamental.
    NoiseOnly,
    /// Signal Lab's 8 Hz + 40 Hz default, for comparing results with it.
    SignalLabTwoTone,
}

impl Preset {
    pub const ALL: [Preset; 8] = [
        Preset::Harmonic440,
        Preset::Beat,
        Preset::Square1k,
        Preset::Saw300,
        Preset::Triangle2k,
        Preset::BuriedTone,
        Preset::NoiseOnly,
        Preset::SignalLabTwoTone,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Harmonic440 => "440 Hz + 3rd harmonic",
            Preset::Beat => "beat: 440 + 452 Hz",
            Preset::Square1k => "square 1 kHz",
            Preset::Saw300 => "saw 300 Hz",
            Preset::Triangle2k => "triangle 2 kHz",
            Preset::BuriedTone => "tone buried in noise",
            Preset::NoiseOnly => "white noise",
            Preset::SignalLabTwoTone => "Signal Lab two-tone (8 + 40 Hz)",
        }
    }

    /// The components and noise floor this preset dials in.
    pub fn parts(self) -> (Vec<Tone>, f64) {
        match self {
            Preset::Harmonic440 => (vec![Tone::sine(440.0, 1.0), Tone::sine(1320.0, 0.15)], 0.01),
            // 12 Hz apart: one full beat every 83 ms, which is the default
            // scope window, so the envelope reads as a shape and not a drift.
            Preset::Beat => (vec![Tone::sine(440.0, 0.5), Tone::sine(452.0, 0.5)], 0.0),
            Preset::Square1k => (vec![Tone::new(Wave::Square, 1000.0, 0.8)], 0.0),
            Preset::Saw300 => (vec![Tone::new(Wave::Saw, 300.0, 0.8)], 0.0),
            Preset::Triangle2k => (vec![Tone::new(Wave::Triangle, 2000.0, 0.9)], 0.0),
            Preset::BuriedTone => (vec![Tone::sine(1000.0, 0.2)], 0.6),
            Preset::NoiseOnly => (Vec::new(), 0.8),
            Preset::SignalLabTwoTone => (vec![Tone::sine(8.0, 1.0), Tone::sine(40.0, 0.4)], 0.2),
        }
    }
}

/// Sums any number of [`Tone`]s at a fixed sample rate.
///
/// The generator is a **pure function of the sample index**: phase is carried
/// in samples-elapsed rather than a wrapped accumulator, and the noise floor
/// is a hash of the index rather than a stream. Two things follow. Components
/// stay exactly phase-coherent over long runs, and any stretch of history can
/// be re-rendered under new settings — which is how a knob turned while the
/// scope is paused shows up on screen at once (see
/// [`render`](Self::render)).
pub struct SynthSource {
    pub tones: Vec<Tone>,
    pub noise: f64,
    sample_rate: f64,
    elapsed: u64,
    /// Noise seed. Deterministic so captures are reproducible, which matters
    /// when a spec verdict is meant to be re-checkable.
    seed: u64,
}

impl SynthSource {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            tones: vec![Tone::sine(440.0, 1.0)],
            noise: 0.0,
            sample_rate,
            elapsed: 0,
            seed: 0x2545_F491_4F6C_DD1D,
        }
    }

    /// Samples produced so far — the generator's clock.
    pub fn elapsed(&self) -> u64 {
        self.elapsed
    }

    /// Render samples `[start, start + out.len())` of the signal as it is now.
    ///
    /// Does not touch the clock. Used to re-render the ring's history when a
    /// setting changes, so the change is visible without waiting for new
    /// samples.
    pub fn render(&self, start: u64, out: &mut [f32]) {
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.sample_index(start + i as u64);
        }
    }

    /// The signal at one sample index: the components plus the noise floor.
    fn sample_index(&self, index: u64) -> f32 {
        let t = index as f64 / self.sample_rate;
        let noise = if self.noise > 0.0 {
            self.noise * noise_at(self.seed, index)
        } else {
            0.0
        };
        (self.sample_at(t, index) + noise) as f32
    }

    /// An audio-rate default: a 440 Hz fundamental with a mild third harmonic
    /// and a low noise floor.
    ///
    /// Sized for a 48 kHz sample rate and the scope's ~85 ms window, which
    /// shows roughly 37 cycles — readable rather than a blur. The 1320 Hz
    /// harmonic at 15% also gives THD something real to measure, so the meter
    /// reports a number instead of a dash.
    pub fn audio(sample_rate: f64) -> Self {
        let mut s = Self::new(sample_rate);
        s.tones = vec![Tone::sine(440.0, 1.0), Tone::sine(1320.0, 0.15)];
        s.noise = 0.01;
        s
    }

    /// Signal Lab's default, for comparing results between the two.
    ///
    /// **These frequencies assume a low sample rate** (Signal Lab generates at
    /// 256 Hz). At 48 kHz an 8 Hz sine puts less than one cycle in the scope
    /// window, which reads as a slow drifting envelope rather than a tone —
    /// use [`audio`](Self::audio) for anything at audio rates.
    pub fn two_tone(sample_rate: f64) -> Self {
        let mut s = Self::new(sample_rate);
        s.tones = vec![Tone::sine(8.0, 1.0), Tone::sine(40.0, 0.4)];
        s.noise = 0.2;
        s
    }

    pub fn with_noise(mut self, noise: f64) -> Self {
        self.noise = noise;
        self
    }

    /// Dial in a preset, keeping the time base so the trace does not restart.
    pub fn apply(&mut self, preset: Preset) {
        let (tones, noise) = preset.parts();
        self.tones = tones;
        self.noise = noise;
    }

    fn sample_at(&self, t: f64, index: u64) -> f64 {
        self.tones
            .iter()
            .filter(|tone| tone.enabled)
            .enumerate()
            .map(|(k, tone)| {
                let phase = (tone.freq_hz * t).fract();
                // Each noise component gets its own seed so stacked noise
                // components are independent rather than identical.
                let seed = self.seed ^ ((k as u64 + 1) << 32);
                tone.amplitude * shape(tone.wave, phase, seed, index)
            })
            .sum()
    }
}

/// White noise in `[-1, 1)` as a pure function of the sample index.
///
/// SplitMix64's finaliser over `seed ^ index`: a hash, not a stream, so any
/// sample can be regenerated on its own and the same settings always produce
/// the same capture.
fn noise_at(seed: u64, index: u64) -> f64 {
    let mut z = (seed ^ index).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 52) as f64 * 2.0 - 1.0
}

fn shape(wave: Wave, phase: f64, seed: u64, index: u64) -> f64 {
    match wave {
        Wave::Sine => (TAU * phase).sin(),
        Wave::Square => {
            if phase < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        Wave::Saw => 2.0 * phase - 1.0,
        Wave::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
        Wave::Noise => noise_at(seed, index),
    }
}

impl Source for SynthSource {
    fn name(&self) -> String {
        format!(
            "synth ({} tone{})",
            self.tones.len(),
            if self.tones.len() == 1 { "" } else { "s" }
        )
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn fill(&mut self, out: &mut Vec<f32>, max: usize) -> usize {
        out.clear();
        out.resize(max, 0.0);
        self.render(self.elapsed, out);
        self.elapsed += max as u64;
        max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_sine_stays_within_its_amplitude() {
        let mut s = SynthSource::new(1000.0);
        s.tones = vec![Tone::sine(10.0, 1.0)];
        let mut out = Vec::new();
        s.fill(&mut out, 1000);

        assert_eq!(out.len(), 1000);
        assert!(
            out.iter().all(|v| v.abs() <= 1.0001),
            "sine exceeded unit amplitude"
        );
    }

    #[test]
    fn a_full_period_of_a_sine_averages_to_zero() {
        let mut s = SynthSource::new(1000.0);
        s.tones = vec![Tone::sine(1.0, 1.0)];
        let mut out = Vec::new();
        s.fill(&mut out, 1000); // exactly one period

        let mean: f64 = out.iter().map(|v| *v as f64).sum::<f64>() / out.len() as f64;
        assert!(mean.abs() < 1e-9, "DC offset {mean} on a pure sine");
    }

    #[test]
    fn phase_is_continuous_across_fills() {
        let mut a = SynthSource::new(1000.0);
        a.tones = vec![Tone::sine(10.0, 1.0)];
        let mut first = Vec::new();
        let mut second = Vec::new();
        a.fill(&mut first, 500);
        a.fill(&mut second, 500);

        let mut whole = SynthSource::new(1000.0);
        whole.tones = vec![Tone::sine(10.0, 1.0)];
        let mut all = Vec::new();
        whole.fill(&mut all, 1000);

        // Splitting a fill must not restart the waveform.
        assert_eq!(second[0], all[500]);
    }

    #[test]
    fn the_audio_default_fills_the_scope_window_with_cycles() {
        let s = SynthSource::audio(48_000.0);
        // 4096 samples at 48 kHz is ~85 ms; 440 Hz gives ~37 cycles. Fewer
        // than a couple of cycles reads as a drifting envelope, not a tone.
        let window_secs = 4096.0 / 48_000.0;
        let cycles = s.tones[0].freq_hz * window_secs;
        assert!(cycles > 10.0, "only {cycles} cycles in the scope window");
    }

    #[test]
    fn the_audio_default_has_a_measurable_harmonic() {
        let s = SynthSource::audio(48_000.0);
        // Without a harmonic, THD is undefined and the meter shows a dash.
        assert_eq!(s.tones.len(), 2);
        assert!(s.tones[1].amplitude > 0.0);
        assert!(s.tones[1].freq_hz > s.tones[0].freq_hz);
    }

    #[test]
    fn every_preset_stays_below_nyquist_at_audio_rate_and_keeps_the_clock() {
        for preset in Preset::ALL {
            let mut s = SynthSource::audio(48_000.0);
            let mut out = Vec::new();
            s.fill(&mut out, 100);
            s.apply(preset);
            assert!(
                s.tones.iter().all(|t| t.freq_hz < 24_000.0),
                "{preset:?} has a component above Nyquist"
            );
            assert_eq!(s.elapsed, 100, "{preset:?} restarted the time base");
        }
    }

    #[test]
    fn every_component_contributes_and_a_muted_one_does_not() {
        let mut one = SynthSource::new(1000.0);
        one.tones = vec![Tone::sine(10.0, 1.0)];
        let mut two = SynthSource::new(1000.0);
        two.tones = vec![Tone::sine(10.0, 1.0), Tone::sine(30.0, 0.5)];
        let mut muted = SynthSource::new(1000.0);
        muted.tones = two.tones.clone();
        muted.tones[1].enabled = false;

        let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
        one.fill(&mut a, 200);
        two.fill(&mut b, 200);
        muted.fill(&mut c, 200);

        assert_ne!(a, b, "the second component changed nothing");
        assert_eq!(a, c, "a muted component must contribute nothing");
    }

    #[test]
    fn rendering_history_matches_what_fill_produced() {
        // The generator is a pure function of the index, so re-rendering a
        // stretch gives back exactly the samples that were streamed.
        let mut s = SynthSource::audio(48_000.0).with_noise(0.3);
        let mut streamed = Vec::new();
        s.fill(&mut streamed, 500);
        let mut again = vec![0.0; 200];
        s.render(300, &mut again);
        assert_eq!(&streamed[300..], &again[..]);
        assert_eq!(s.elapsed(), 500, "render must not advance the clock");
    }

    #[test]
    fn noise_is_deterministic_for_a_fixed_seed() {
        let mut a = SynthSource::new(100.0).with_noise(0.5);
        let mut b = SynthSource::new(100.0).with_noise(0.5);
        let (mut x, mut y) = (Vec::new(), Vec::new());
        a.fill(&mut x, 64);
        b.fill(&mut y, 64);
        assert_eq!(x, y, "captures must be reproducible");
    }
}
