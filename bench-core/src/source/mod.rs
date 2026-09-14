//! Signal sources. Everything that can fill the [`SampleRing`].
//!
//! Implementation priority:
//!
//! 1. `audio` — the real path. Feature-gated so a clone builds without
//!    system audio libraries.
//! 2. [`synth`] — known-truth generator. Needed regardless: you cannot tell a
//!    broken FFT from a weird signal without a source you trust.
//! 3. `replay` — WAV/CSV. Just another producer.
//! 4. serial/USB — the trait is here; drivers land when a device does.

#[cfg(feature = "audio")]
pub mod audio;
pub mod replay;
pub mod synth;

use crate::ring::SampleRing;

/// A pull-based signal source.
///
/// Pull rather than push so the caller controls cadence and no source can
/// outrun the ring. Real-time sources that are inherently push (an audio
/// callback) adapt by writing into their own staging buffer and draining it
/// here.
pub trait Source: Send {
    /// Human-readable name for the UI.
    fn name(&self) -> String;

    /// Nominal sample rate in Hz.
    fn sample_rate(&self) -> f64;

    /// Produce up to `max` samples into `out`, returning how many were written.
    ///
    /// Returning `0` means "nothing available right now", never "finished" —
    /// use [`is_exhausted`](Source::is_exhausted) for that, so a paused replay
    /// is distinguishable from a finished one.
    fn fill(&mut self, out: &mut Vec<f32>, max: usize) -> usize;

    /// True once the source will never produce again (end of file, device
    /// disconnected). Live sources always return `false`.
    fn is_exhausted(&self) -> bool {
        false
    }
}

/// Drive a source into a ring. Returns the number of samples written.
pub fn pump(source: &mut dyn Source, ring: &SampleRing, max: usize) -> usize {
    let mut scratch = Vec::with_capacity(max);
    let n = source.fill(&mut scratch, max);
    if n > 0 {
        ring.write(&scratch[..n]);
    }
    n
}
