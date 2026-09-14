//! Saving a capture to disk.
//!
//! A frame worth analysing is a frame worth keeping. Each save writes the same
//! samples twice: a WAV, because every audio tool opens one, and a CSV,
//! because every spreadsheet and notebook does. Either loads back into Bench
//! by dropping it on the window (see [`source::replay`](crate::source::replay)).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where captures go unless told otherwise: `~/Documents/Bench/captures`.
///
/// One `Bench` folder, no spaces, with room beside `captures/` for anything
/// else the instrument saves later.
pub fn default_dir() -> PathBuf {
    crate::paths::bench_documents_dir().join("captures")
}

/// Write `samples` as `capture_<unix seconds>.wav` and `.csv` under `dir`,
/// creating it if needed. Returns both paths.
pub fn save_capture(
    dir: &Path,
    samples: &[f32],
    sample_rate: f64,
) -> std::io::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let wav = unique(dir, &format!("capture_{stamp}"), "wav");
    let csv = wav.with_extension("csv");

    write_wav(&wav, samples, sample_rate)?;
    write_csv(&csv, samples, sample_rate)?;
    Ok((wav, csv))
}

/// `stem.ext`, or `stem-2.ext`, `stem-3.ext` … if a save in the same second
/// already exists.
fn unique(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}.{ext}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

/// 32-bit float mono WAV — exact, no quantisation of what the ring held.
pub fn write_wav(path: &Path, samples: &[f32], sample_rate: f64) -> std::io::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: sample_rate.round() as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(to_io)?;
    for s in samples {
        writer.write_sample(*s).map_err(to_io)?;
    }
    writer.finalize().map_err(to_io)
}

/// `index,seconds,value` rows under a `# sample_rate=N` line the replay
/// loader understands.
pub fn write_csv(path: &Path, samples: &[f32], sample_rate: f64) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(out, "# sample_rate={sample_rate}")?;
    writeln!(out, "index,seconds,value")?;
    for (i, s) in samples.iter().enumerate() {
        writeln!(out, "{i},{:.6},{s}", i as f64 / sample_rate)?;
    }
    out.flush()
}

fn to_io(e: hound::Error) -> std::io::Error {
    match e {
        hound::Error::IoError(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::replay::ReplaySource;
    use crate::source::Source;

    /// A directory no other test in this process shares.
    ///
    /// Two tests starting on the same clock tick got the same nanosecond stamp
    /// and the same directory, and `save_capture`'s same-second naming then
    /// had one test's file overwritten by the other's — a round trip that
    /// came back with four samples instead of sixty-four, once in a dozen
    /// runs. The counter makes the name unique regardless of the clock.
    fn scratch() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bench-capture-test-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_saved_capture_replays_identically_from_both_files() {
        let dir = scratch();
        let samples: Vec<f32> = (0..64).map(|i| (i as f32 * 0.1).sin()).collect();
        let (wav, csv) = save_capture(&dir, &samples, 8000.0).unwrap();

        for path in [&wav, &csv] {
            let mut replay = ReplaySource::from_path(path).unwrap();
            assert_eq!(replay.sample_rate(), 8000.0, "{}", path.display());
            let mut out = Vec::new();
            replay.looping = false;
            replay.fill(&mut out, 1000);
            assert_eq!(out.len(), samples.len(), "{}", path.display());
            for (a, b) in out.iter().zip(&samples) {
                assert!(
                    (a - b).abs() < 1e-5,
                    "{} differs: {a} vs {b}",
                    path.display()
                );
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_twice_in_one_second_does_not_overwrite() {
        let dir = scratch();
        let (a, _) = save_capture(&dir, &[0.0; 4], 100.0).unwrap();
        let (b, _) = save_capture(&dir, &[1.0; 4], 100.0).unwrap();
        assert_ne!(a, b);
        let _ = std::fs::remove_dir_all(dir);
    }
}
