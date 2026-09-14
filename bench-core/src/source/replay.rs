//! File replay — a WAV or CSV played back as if it were live.
//!
//! Just another producer: the ring, the scope, the gate and the chain do not
//! know the difference between a microphone and a recording. That is the point
//! of the source trait, and it is what lets a capture saved from one session be
//! analysed in another.

use std::path::{Path, PathBuf};

use super::Source;

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("could not read {path}: {reason}")]
    Read { path: String, reason: String },
    #[error("{0} has no samples")]
    Empty(String),
    #[error("unsupported file type {0:?}; use .wav or .csv")]
    Unsupported(String),
}

/// A recording, played back at its own sample rate, looping by default.
pub struct ReplaySource {
    name: String,
    /// Where the file came from, when it came from one — a profile saves
    /// the path, not the samples.
    path: Option<PathBuf>,
    sample_rate: f64,
    samples: Vec<f32>,
    position: usize,
    /// Wrap at the end rather than stopping. A scope replaying a file loops;
    /// a spec run over a file should not.
    pub looping: bool,
}

/// Sample rate assumed for a CSV that does not declare one.
pub const DEFAULT_CSV_RATE: f64 = 48_000.0;

impl ReplaySource {
    /// Load a `.wav` (any channel count, averaged to mono) or a `.csv` (last
    /// numeric column; a leading `# sample_rate=N` line sets the rate).
    pub fn from_path(path: &Path) -> Result<Self, ReplayError> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let (sample_rate, samples) = match ext.as_str() {
            "wav" => read_wav(path).map_err(|reason| ReplayError::Read {
                path: name.clone(),
                reason,
            })?,
            "csv" | "txt" => {
                let text = std::fs::read_to_string(path).map_err(|e| ReplayError::Read {
                    path: name.clone(),
                    reason: e.to_string(),
                })?;
                parse_csv(&text)
            }
            other => return Err(ReplayError::Unsupported(other.to_string())),
        };
        if samples.is_empty() {
            return Err(ReplayError::Empty(name));
        }
        Ok(Self {
            path: Some(path.to_path_buf()),
            ..Self::from_samples(name, sample_rate, samples)
        })
    }

    pub fn from_samples(name: impl Into<String>, sample_rate: f64, samples: Vec<f32>) -> Self {
        Self {
            name: name.into(),
            path: None,
            sample_rate,
            samples,
            position: 0,
            looping: true,
        }
    }

    /// The file this replay was loaded from, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Playback position, in samples from the start.
    pub fn position(&self) -> usize {
        self.position
    }

    pub fn restart(&mut self) {
        self.position = 0;
    }

    pub fn file_name(&self) -> &str {
        &self.name
    }

    pub fn duration_secs(&self) -> f64 {
        self.samples.len() as f64 / self.sample_rate
    }
}

fn read_wav(path: &Path) -> Result<(f64, Vec<f32>), String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let mono: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => {
            let all: Result<Vec<f32>, _> = reader.samples::<f32>().collect();
            downmix(&all.map_err(|e| e.to_string())?, channels)
        }
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample.saturating_sub(1))) as f32;
            let all: Result<Vec<i32>, _> = reader.samples::<i32>().collect();
            let floats: Vec<f32> = all
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|s| s as f32 / scale)
                .collect();
            downmix(&floats, channels)
        }
    };
    Ok((spec.sample_rate as f64, mono))
}

/// Average interleaved channels to one.
fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// Parse a CSV: the last numeric field of each row is the sample. Rows that
/// do not parse (headers) are skipped. `# sample_rate=N` sets the rate.
pub fn parse_csv(text: &str) -> (f64, Vec<f32>) {
    let mut rate = DEFAULT_CSV_RATE;
    let mut samples = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            if let Some(v) = rest.trim().strip_prefix("sample_rate=") {
                if let Ok(r) = v.trim().parse::<f64>() {
                    rate = r;
                }
            }
            continue;
        }
        let value = line
            .split([',', ';', '\t', ' '])
            .filter(|f| !f.is_empty())
            .filter_map(|f| f.trim().parse::<f32>().ok())
            .next_back();
        if let Some(v) = value {
            samples.push(v);
        }
    }
    (rate, samples)
}

impl Source for ReplaySource {
    fn name(&self) -> String {
        format!("replay: {}", self.name)
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn fill(&mut self, out: &mut Vec<f32>, max: usize) -> usize {
        out.clear();
        if self.samples.is_empty() {
            return 0;
        }
        while out.len() < max {
            if self.position >= self.samples.len() {
                if !self.looping {
                    break;
                }
                self.position = 0;
            }
            let take = (max - out.len()).min(self.samples.len() - self.position);
            out.extend_from_slice(&self.samples[self.position..self.position + take]);
            self.position += take;
        }
        out.len()
    }

    fn is_exhausted(&self) -> bool {
        !self.looping && self.position >= self.samples.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_takes_the_last_numeric_column_and_honours_the_rate_line() {
        let text =
            "# sample_rate=1000\nindex,seconds,value\n0,0.000,0.5\n1,0.001,-0.5\n2,0.002,0.25\n";
        let (rate, samples) = parse_csv(text);
        assert_eq!(rate, 1000.0);
        assert_eq!(samples, vec![0.5, -0.5, 0.25]);
    }

    #[test]
    fn replay_loops_by_default_and_stops_when_asked() {
        let mut r = ReplaySource::from_samples("t", 100.0, vec![1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        assert_eq!(r.fill(&mut out, 5), 5);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 1.0, 2.0]);
        assert!(!r.is_exhausted());

        r.looping = false;
        r.restart();
        assert_eq!(r.fill(&mut out, 5), 3);
        assert!(r.is_exhausted());
        assert_eq!(r.fill(&mut out, 5), 0, "an exhausted replay yields nothing");
    }

    #[test]
    fn stereo_wav_is_averaged_to_mono() {
        assert_eq!(downmix(&[1.0, 3.0, -1.0, 1.0], 2), vec![2.0, 0.0]);
    }
}
