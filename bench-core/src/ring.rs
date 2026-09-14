//! The sample ring — the boundary between "local and fast" and everything else.
//!
//! A source thread writes; the UI thread reads whole frames. Capacity is a
//! power of two so index wrapping is a mask rather than a modulo, which matters
//! at 48 kHz in a callback that must not allocate or block.
//!
//! This buffer is intentionally lossy: if the reader falls behind, the writer
//! overwrites. A scope shows *now*, not a backlog, and blocking an audio
//! callback to preserve history would produce glitches in exchange for samples
//! nobody will look at.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A fixed-capacity ring of `f32` samples.
///
/// Uses a mutex around the storage rather than true lock-free SPSC. That is a
/// deliberate v1 simplification: the critical section is a `copy_from_slice`
/// with no allocation, and it keeps the code readable for an example app.
/// Swap in `rtrb` or similar if a real audio callback ever reports underruns.
pub struct SampleRing {
    buf: Mutex<Vec<f32>>,
    mask: usize,
    /// Total samples ever written. Also the read cursor's frame of reference,
    /// so a reader can tell how far it has fallen behind.
    written: AtomicU64,
}

impl SampleRing {
    /// Create a ring holding at least `capacity` samples, rounded up to the
    /// next power of two.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(2).next_power_of_two();
        Self {
            buf: Mutex::new(vec![0.0; cap]),
            mask: cap - 1,
            written: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Total samples written since creation.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// Append samples, overwriting the oldest if full.
    pub fn write(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let mut buf = self.buf.lock().expect("sample ring poisoned");
        let start = self.written.load(Ordering::Acquire) as usize;

        // Only the last `capacity` samples can survive, so a burst larger than
        // the ring is trimmed before copying rather than wrapping repeatedly.
        let samples = if samples.len() > self.capacity() {
            &samples[samples.len() - self.capacity()..]
        } else {
            samples
        };

        for (i, s) in samples.iter().enumerate() {
            let idx = (start + i) & self.mask;
            buf[idx] = *s;
        }
        self.written
            .fetch_add(samples.len() as u64, Ordering::Release);
    }

    /// Copy the most recent `n` samples in chronological order.
    ///
    /// Returns fewer than `n` only when fewer have ever been written. Never
    /// returns more than [`capacity`](Self::capacity).
    pub fn latest(&self, n: usize) -> Vec<f32> {
        self.window(0, n)
    }

    /// Copy `n` samples ending `back` samples before the newest one, in
    /// chronological order — the gate of a scope, positioned in history.
    ///
    /// `back = 0` is [`latest`](Self::latest). The window is clamped to what
    /// the ring still holds: asking further back than the capacity returns the
    /// oldest surviving samples, and asking before anything was written
    /// returns what exists.
    pub fn window(&self, back: usize, n: usize) -> Vec<f32> {
        let written = self.written.load(Ordering::Acquire) as usize;
        let available = written.min(self.capacity());
        let back = back.min(available);
        let n = n.min(available - back);
        if n == 0 {
            return Vec::new();
        }
        let buf = self.buf.lock().expect("sample ring poisoned");
        let start = written - back - n;
        (0..n).map(|i| buf[(start + i) & self.mask]).collect()
    }

    /// How many samples can currently be read back.
    pub fn available(&self) -> usize {
        (self.written.load(Ordering::Acquire) as usize).min(self.capacity())
    }

    /// Overwrite the newest `samples.len()` samples in place without advancing
    /// the clock.
    ///
    /// For sources that are a pure function of time — the generator — this is
    /// how a knob turned while paused, or while looking at history, shows up
    /// immediately: the history is re-rendered under the new settings rather
    /// than waiting for new samples to arrive. Slices longer than what the ring
    /// holds are trimmed to their tail.
    pub fn rewrite_tail(&self, samples: &[f32]) {
        let available = self.available();
        let n = samples.len().min(available);
        if n == 0 {
            return;
        }
        let samples = &samples[samples.len() - n..];
        let written = self.written.load(Ordering::Acquire) as usize;
        let start = written - n;
        let mut buf = self.buf.lock().expect("sample ring poisoned");
        for (i, s) in samples.iter().enumerate() {
            buf[(start + i) & self.mask] = *s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_capacity_up_to_a_power_of_two() {
        assert_eq!(SampleRing::new(1000).capacity(), 1024);
    }

    #[test]
    fn reads_back_in_chronological_order() {
        let ring = SampleRing::new(8);
        ring.write(&[1.0, 2.0, 3.0]);
        assert_eq!(ring.latest(3), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn overwrites_oldest_when_full() {
        let ring = SampleRing::new(4);
        ring.write(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        // 1.0 is gone; the newest four remain in order.
        assert_eq!(ring.latest(4), vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn a_burst_larger_than_the_ring_keeps_only_the_tail() {
        let ring = SampleRing::new(4);
        ring.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        assert_eq!(ring.latest(4), vec![6.0, 7.0, 8.0, 9.0]);
    }

    #[test]
    fn asking_for_more_than_written_returns_what_exists() {
        let ring = SampleRing::new(16);
        ring.write(&[1.0, 2.0]);
        assert_eq!(ring.latest(10), vec![1.0, 2.0]);
    }

    #[test]
    fn empty_ring_reads_empty() {
        assert!(SampleRing::new(8).latest(4).is_empty());
    }

    #[test]
    fn rewriting_the_tail_replaces_history_without_advancing_the_clock() {
        let ring = SampleRing::new(8);
        ring.write(&[1.0, 2.0, 3.0, 4.0]);
        ring.rewrite_tail(&[9.0, 8.0]);
        assert_eq!(ring.latest(4), vec![1.0, 2.0, 9.0, 8.0]);
        assert_eq!(ring.written(), 4);
        // Longer than what exists: only the tail lands, oldest first.
        ring.rewrite_tail(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        assert_eq!(ring.latest(4), vec![0.3, 0.4, 0.5, 0.6]);
    }

    #[test]
    fn a_window_reads_history_behind_the_newest_sample() {
        let ring = SampleRing::new(8);
        ring.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(ring.window(0, 2), vec![5.0, 6.0]);
        assert_eq!(ring.window(2, 2), vec![3.0, 4.0]);
        // Further back than exists: the oldest surviving samples, not garbage.
        assert_eq!(ring.window(5, 4), vec![1.0]);
        assert!(ring.window(6, 4).is_empty());
        assert_eq!(ring.available(), 6);
    }
}
