//! Edge trigger — where a sweep starts.
//!
//! A scope holds a periodic waveform still by starting every sweep at the
//! same kind of edge. Two rules make that work, and the second is the one
//! that is easy to get wrong:
//!
//! 1. **An edge is a rising crossing of the signal's mean, with hysteresis**
//!    (a tenth of the range): the trace must dip below the band and then rise
//!    above it. The mean rather than the midpoint of the extremes, because a
//!    single spike moves the midpoint and an asymmetric wave crosses its mean
//!    once per cycle where it may cross the midpoint several times.
//! 2. **A sweep, once started, runs to its end before the trigger re-arms.**
//!    Choosing "the newest edge in the buffer" every frame instead is a trap:
//!    on a signal with dense crossings — noise, or a tone under a noise floor
//!    — there is always a fresh edge just before the newest complete sweep, so
//!    the chosen edge tracks live and the display rolls exactly as if the
//!    trigger were off. Holding the current sweep and arming only for edges
//!    after it ends gives a periodic signal a still trace and a noisy one a
//!    trace that steps sweep by sweep, which is what a real scope does.

/// Rising crossings of the buffer's mean with hysteresis, as indices into
/// `buf`. Empty when the signal is flat.
pub fn rising_edges(buf: &[f32]) -> Vec<usize> {
    if buf.is_empty() {
        return Vec::new();
    }
    let (lo, hi, sum) = buf
        .iter()
        .fold((f32::MAX, f32::MIN, 0.0f64), |(lo, hi, sum), s| {
            (lo.min(*s), hi.max(*s), sum + *s as f64)
        });
    let range = hi - lo;
    if range < 1e-4 {
        return Vec::new();
    }
    let level = (sum / buf.len() as f64) as f32;
    let hyst = range * 0.1;

    let mut armed = false;
    let mut edges = Vec::new();
    for (i, s) in buf.iter().enumerate() {
        if *s < level - hyst {
            armed = true;
        } else if armed && *s >= level + hyst {
            armed = false;
            edges.push(i);
        }
    }
    edges
}

/// Pick the edge this frame's sweep starts at.
///
/// `buf` holds the newest samples and `base` is the absolute index of
/// `buf[0]`. A sweep shows `pre` samples before its edge and `post` after,
/// and only edges with room for both inside `buf` qualify — a sweep is shown
/// once it is complete, never while it is still filling.
///
/// `arm_from` is where the trigger is armed: the end of the sweep currently
/// on screen, as an absolute index. With it, the *earliest* qualifying edge
/// at or after that point is chosen, so sweeps follow one another without
/// skipping and without re-triggering inside the sweep already shown.
/// Without it — a fresh acquisition — the *newest* qualifying edge is chosen,
/// so the display starts near live.
///
/// Returns the edge's absolute index, or `None` when nothing qualifies, in
/// which case the caller keeps what it has.
pub fn select_edge(
    buf: &[f32],
    base: u64,
    pre: usize,
    post: usize,
    arm_from: Option<u64>,
) -> Option<u64> {
    let edges = rising_edges(buf);
    let qualifies = |i: usize| i >= pre && i + post <= buf.len();
    match arm_from {
        None => edges.iter().rev().copied().find(|&i| qualifies(i)),
        Some(from) => edges
            .iter()
            .copied()
            .find(|&i| qualifies(i) && base + i as u64 >= from),
    }
    .map(|i| base + i as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, period: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 / period as f32 * std::f32::consts::TAU).sin())
            .collect()
    }

    /// Deterministic "noise": dense, irregular crossings of the mean.
    fn hash_noise(n: usize) -> Vec<f32> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn edges_are_one_per_cycle_on_a_sine_and_absent_on_dc() {
        let period = 100;
        let edges = rising_edges(&sine(1000, period));
        // One rising mean-crossing per cycle, all at the same phase.
        assert_eq!(edges.len(), 9, "{edges:?}");
        for w in edges.windows(2) {
            assert_eq!(w[1] - w[0], period);
        }
        assert!(rising_edges(&[0.5; 500]).is_empty());
    }

    #[test]
    fn a_fresh_acquisition_takes_the_newest_complete_edge() {
        let period = 100;
        let buf = sine(1000, period);
        let (pre, post) = (20, 380);
        let edge = select_edge(&buf, 5000, pre, post, None).unwrap();
        // The newest edge with `post` samples after it inside the buffer,
        // at the same phase as every other edge (a few samples past the zero
        // crossing, where the trace clears the hysteresis band).
        let i = (edge - 5000) as usize;
        assert!(
            i + post <= buf.len() && i + post + period > buf.len(),
            "{i}"
        );
        assert_eq!(i % period, rising_edges(&buf)[0] % period);
    }

    #[test]
    fn sweeps_follow_one_another_at_the_same_phase() {
        let period = 100;
        let (pre, post) = (20, 380);
        // Frame 1: acquisition. Frames 2..: arm at the end of the sweep.
        let buf = sine(1000, period);
        let first = select_edge(&buf, 0, pre, post, None).unwrap();
        let next = select_edge(&buf, 0, pre, post, Some(first + post as u64));
        // Nothing qualifies after the newest edge — the caller holds.
        assert_eq!(next, None);

        // More signal arrives; the buffer window slides by 500 samples.
        let more = sine(1500, period);
        let base = 500;
        let buf2 = &more[base as usize..];
        let next = select_edge(buf2, base, pre, post, Some(first + post as u64)).unwrap();
        assert!(next >= first + post as u64);
        // Same phase as the first sweep: the trace stands still.
        assert_eq!((next - first) % period as u64, 0);
    }

    #[test]
    fn on_noise_the_trigger_steps_by_sweeps_instead_of_tracking_live() {
        // The failure this module exists for. With dense crossings, "newest
        // edge every frame" lands just before `len - post` each time, so the
        // display's distance behind live is a small constant: a roll.
        let (pre, post) = (400, 3600);
        let n = 24_000;
        let noise = hash_noise(n + 4_000);

        let frame = |written: usize| (&noise[written - n..written], (written - n) as u64);

        let (buf, base) = frame(n);
        let first = select_edge(buf, base, pre, post, None).unwrap();
        let behind_live_at_acquisition = n as u64 - (first + post as u64);
        assert!(
            behind_live_at_acquisition < 50,
            "acquisition starts near live"
        );

        // 800 samples later (one 60 fps frame at 48 kHz): armed at the end of
        // the current sweep, nothing there is complete yet, so the caller
        // holds and the trace stands still while the next sweep fills…
        let (buf, base) = frame(n + 800);
        assert_eq!(
            select_edge(buf, base, pre, post, Some(first + post as u64)),
            None
        );

        // …and once a full sweep has arrived, the next edge is the first one
        // after the previous sweep ended — a step of about one sweep, not a
        // slide of 800 samples.
        let (buf, base) = frame(n + 4_000);
        let next = select_edge(buf, base, pre, post, Some(first + post as u64)).unwrap();
        assert!(next >= first + post as u64);
        assert!(next < first + post as u64 + 200, "{next} vs {first}");
    }
}
