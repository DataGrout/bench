//! Profiles — the instrument's setup as a file.
//!
//! A profile is what a bench looks like before you touch it: which source is
//! feeding the ring and how it is dialled in, which chain is built and with
//! what arguments, the time base, the analysis frame, the trigger, and which
//! panels sit in the dock. Save one and the same setup comes back with one
//! action; ship one and a reader launches into the frame a screenshot shows.
//!
//! It is a local file, like a capture, and deliberately *not* something the
//! gateway stores: the chain half of a profile is exactly the plan a saved
//! skill holds, so a profile is the local complement of "Save as skill" —
//! the source and instrument settings a skill has no business knowing.
//!
//! # Format
//!
//! JSON with a `version`, extension `.bench`. Unknown fields are ignored so a
//! newer Bench can add settings without breaking an older reader, and a file
//! whose `version` is newer than this build refuses to load rather than
//! guessing. Chain arguments are stored as the strings the chain pane edits,
//! because that is the representation the chain compiler coerces from — a
//! profile round-trips what the user typed, not a re-serialisation of it.
//!
//! # What a profile never carries
//!
//! Auto-analysis. A profile that starts spending credits the moment it opens
//! is the "72,000 credits an hour" failure with a file extension. The rate is
//! stored; the switch is the user's.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chain::{Chain, ChainError, Step};
use crate::source::synth::Tone;
use crate::{CAPTURE_SAMPLES, DEFAULT_ANALYSIS_HZ};

/// File extension, without the dot.
pub const EXTENSION: &str = "bench";

/// The format this build writes and the newest it reads.
pub const FORMAT_VERSION: u32 = 1;

/// A saved bench setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub source: SourceSpec,
    /// The chain as `(step key, arguments)` pairs, in order.
    #[serde(default)]
    pub chain: Vec<Step>,
    #[serde(default)]
    pub instrument: Instrument,
    /// Smart Panels placed in the dock, by identity. Resolved against the
    /// account when panels load; a panel that no longer exists is dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panels: Vec<PanelRef>,
}

/// What feeds the ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceSpec {
    /// The function generator, fully specified — a profile with a synth
    /// source runs with no account and no hardware.
    Synth {
        sample_rate: f64,
        tones: Vec<Tone>,
        #[serde(default)]
        noise: f64,
    },
    /// A WAV or CSV on disk, by path.
    Replay {
        path: PathBuf,
        #[serde(default = "yes")]
        looping: bool,
    },
    /// A live input, by device name. Best effort: the device may not be
    /// present, and the build may not have the `audio` feature.
    Audio {
        #[serde(default)]
        device: Option<String>,
        #[serde(default)]
        channel: usize,
        #[serde(default = "unity")]
        gain: f32,
    },
}

fn yes() -> bool {
    true
}

fn unity() -> f32 {
    1.0
}

/// The knobs that are not the source and not the chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Instrument {
    /// Samples across the scope.
    pub view_samples: usize,
    /// Samples in the analysis frame handed to the gateway.
    pub capture_samples: usize,
    /// Hold the view on a rising edge.
    pub trigger: bool,
    /// Generator clock as a fraction of real time (1.0 = real time).
    pub clock_rate: f64,
    /// Auto-analysis cadence, for when the user switches it on.
    pub analysis_hz: f64,
}

impl Default for Instrument {
    fn default() -> Self {
        Self {
            view_samples: 4096,
            capture_samples: CAPTURE_SAMPLES,
            trigger: true,
            clock_rate: 1.0,
            analysis_hz: DEFAULT_ANALYSIS_HZ,
        }
    }
}

/// A Smart Panel's identity: `(namespace, id)`, the pair `smart_panel.list`
/// keys on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelRef {
    pub namespace: String,
    pub id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("could not read {path}: {reason}")]
    Read { path: String, reason: String },
    #[error("could not write {path}: {reason}")]
    Write { path: String, reason: String },
    #[error("not a profile: {0}")]
    Parse(String),
    #[error("profile format {found} is newer than this Bench reads ({supported})")]
    NewerFormat { found: u32, supported: u32 },
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
}

/// A profile compiled into the binary, so the Profiles menu works from a
/// bundled app with no repository beside it.
pub struct Builtin {
    /// File stem, e.g. `two-tone-spectrum`.
    pub slug: &'static str,
    pub json: &'static str,
}

/// The example profiles shipped in `bench-core/examples/profiles/`, beside the
/// crate's other examples. Cargo ignores the folder: it holds no `main.rs`.
///
/// Each is also a worked example of a chain an agent would submit, which is
/// why they are worth carrying: a reader who opens one sees a plan, not just
/// a picture. Every builtin uses the generator, so all of them run offline.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        slug: "two-tone-spectrum",
        json: include_str!("../examples/profiles/two-tone-spectrum.bench"),
    },
    Builtin {
        slug: "square-through-causal-filter",
        json: include_str!("../examples/profiles/square-through-causal-filter.bench"),
    },
    Builtin {
        slug: "tone-in-noise-peaks",
        json: include_str!("../examples/profiles/tone-in-noise-peaks.bench"),
    },
    Builtin {
        slug: "regime-embedding",
        json: include_str!("../examples/profiles/regime-embedding.bench"),
    },
];

impl Profile {
    /// Parse a profile from JSON, refusing a format newer than this build.
    pub fn from_json(json: &str) -> Result<Self, ProfileError> {
        let profile: Profile =
            serde_json::from_str(json).map_err(|e| ProfileError::Parse(e.to_string()))?;
        if profile.version > FORMAT_VERSION {
            return Err(ProfileError::NewerFormat {
                found: profile.version,
                supported: FORMAT_VERSION,
            });
        }
        Ok(profile)
    }

    /// Pretty JSON with a trailing newline, the way a file should end.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("profile serialises");
        s.push('\n');
        s
    }

    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        let json = std::fs::read_to_string(path).map_err(|e| ProfileError::Read {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Self::from_json(&json)
    }

    /// Write to `path`, creating parent directories.
    pub fn save(&self, path: &Path) -> Result<(), ProfileError> {
        let write = |p: &Path| -> std::io::Result<()> {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(p, self.to_json())
        };
        write(path).map_err(|e| ProfileError::Write {
            path: path.display().to_string(),
            reason: e.to_string(),
        })
    }

    /// Every compiled-in example, parsed. A builtin that fails to parse is a
    /// build defect, not a runtime condition, so this panics rather than
    /// returning a Result — and a test asserts it never does.
    pub fn builtins() -> Vec<Profile> {
        BUILTINS
            .iter()
            .map(|b| {
                Self::from_json(b.json)
                    .unwrap_or_else(|e| panic!("builtin profile {} is invalid: {e}", b.slug))
            })
            .collect()
    }

    /// Rebuild the chain through the same shape-checked path the chain pane
    /// uses, so a hand-edited profile with an unknown step or an impossible
    /// order is refused with the reason rather than loaded half-way.
    pub fn chain(&self) -> Result<Chain, ChainError> {
        Chain::from_steps(&self.chain)
    }

    /// `name` as a file stem: lowercase, hyphens, nothing a shell minds.
    pub fn slug(&self) -> String {
        slugify(&self.name)
    }

    /// `<slug>.bench`.
    pub fn file_name(&self) -> String {
        format!("{}.{EXTENSION}", self.slug())
    }

    /// Whether the source needs nothing beyond this process to run.
    pub fn runs_offline(&self) -> bool {
        matches!(self.source, SourceSpec::Synth { .. })
    }
}

/// Where saved profiles go unless told otherwise: `~/Documents/Bench/profiles`,
/// beside `captures/`.
pub fn default_dir() -> PathBuf {
    crate::paths::bench_documents_dir().join("profiles")
}

/// The `.bench` files in `dir`, sorted by name. An unreadable or missing
/// directory is an empty list, not an error: the menu simply has nothing to
/// offer yet.
pub fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_profile_path(p))
        .collect();
    files.sort();
    files
}

/// Whether a path looks like a profile — the drop handler's test, since a WAV
/// or CSV dropped on the window is a replay and a `.bench` is a setup. Plain
/// `.json` counts too: nothing else Bench would be handed is JSON.
pub fn is_profile_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .as_deref(),
        Some(EXTENSION) | Some("json")
    )
}

fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "profile".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::synth::Wave;
    use std::collections::BTreeMap;

    fn sample() -> Profile {
        let mut filter = Step::new("signal.filter").unwrap();
        filter.args.insert("cutoff".into(), "0.05".into());
        Profile {
            version: FORMAT_VERSION,
            name: "Two-tone spectrum".into(),
            description: "a 440 Hz tone and its third harmonic".into(),
            source: SourceSpec::Synth {
                sample_rate: 48_000.0,
                tones: vec![
                    Tone::sine(440.0, 1.0),
                    Tone::new(Wave::Square, 1320.0, 0.15),
                ],
                noise: 0.01,
            },
            chain: vec![filter, Step::new("signal.spectral").unwrap()],
            instrument: Instrument {
                view_samples: 2048,
                ..Instrument::default()
            },
            panels: vec![PanelRef {
                namespace: "_panels".into(),
                id: "opp_stage".into(),
            }],
        }
    }

    #[test]
    fn round_trips_through_json_exactly() {
        let p = sample();
        let back = Profile::from_json(&p.to_json()).unwrap();
        assert_eq!(back, p);
        assert!(p.to_json().ends_with('\n'));
    }

    #[test]
    fn the_chain_rebuilds_through_the_shape_checked_path() {
        let chain = sample().chain().unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain.steps()[0].args["cutoff"], "0.05");
        // A default the profile did not mention is still there.
        assert_eq!(chain.steps()[0].args["causal"], "true");
    }

    #[test]
    fn an_unknown_step_or_an_impossible_order_is_refused_with_the_reason() {
        let mut p = sample();
        p.chain = vec![Step {
            key: "signal.nope".into(),
            args: BTreeMap::new(),
        }];
        assert!(matches!(p.chain(), Err(ChainError::UnknownStep(_))));

        // pca wants a matrix; a series cannot feed it directly.
        p.chain = vec![Step::new("linalg.pca").unwrap()];
        assert!(matches!(p.chain(), Err(ChainError::ShapeMismatch { .. })));
    }

    #[test]
    fn unknown_fields_are_ignored_and_a_newer_format_is_refused() {
        let json = r#"{
            "version": 1,
            "name": "x",
            "source": {"kind": "synth", "sample_rate": 8000.0, "tones": []},
            "some_future_setting": {"a": 1}
        }"#;
        let p = Profile::from_json(json).unwrap();
        assert!(p.chain.is_empty());
        assert_eq!(p.instrument, Instrument::default());

        let newer = json.replace("\"version\": 1", "\"version\": 99");
        assert!(matches!(
            Profile::from_json(&newer),
            Err(ProfileError::NewerFormat {
                found: 99,
                supported: FORMAT_VERSION
            })
        ));
    }

    #[test]
    fn every_builtin_parses_builds_its_chain_and_runs_offline() {
        let builtins = Profile::builtins();
        assert_eq!(builtins.len(), BUILTINS.len());
        for (b, p) in BUILTINS.iter().zip(&builtins) {
            assert_eq!(p.version, FORMAT_VERSION, "{}", b.slug);
            assert_eq!(p.slug(), b.slug, "file stem and name disagree");
            assert!(!p.description.is_empty(), "{} needs a description", b.slug);
            assert!(p.runs_offline(), "{} must use the generator", b.slug);
            let chain = p.chain().unwrap_or_else(|e| panic!("{}: {e}", b.slug));
            assert!(!chain.is_empty(), "{} has no chain", b.slug);
            // Stored as formatted, so a diff of the file is a diff of the setup.
            // Line endings are the checkout's business, not the format's: a
            // Windows clone with autocrlf hands `include_str!` CRLF bytes.
            assert_eq!(
                b.json.replace("\r\n", "\n"),
                p.to_json(),
                "{} is not in canonical form",
                b.slug
            );
        }
    }

    #[test]
    fn save_and_load_and_list() {
        let dir = std::env::temp_dir().join(format!(
            "bench-profile-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let p = sample();
        let path = dir.join("nested").join(p.file_name());
        p.save(&path).unwrap();
        assert_eq!(path.file_name().unwrap(), "two-tone-spectrum.bench");
        assert_eq!(Profile::load(&path).unwrap(), p);

        std::fs::write(dir.join("nested").join("notes.txt"), "x").unwrap();
        let listed = list_dir(&dir.join("nested"));
        assert_eq!(listed, vec![path.clone()]);
        assert!(list_dir(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn slugs_are_shell_safe() {
        assert_eq!(slugify("Two-tone spectrum"), "two-tone-spectrum");
        assert_eq!(slugify("  Sq / Filter!! "), "sq-filter");
        assert_eq!(slugify("***"), "profile");
    }

    #[test]
    fn profile_paths_are_bench_or_json() {
        assert!(is_profile_path(Path::new("a/b.bench")));
        assert!(is_profile_path(Path::new("a/B.JSON")));
        assert!(!is_profile_path(Path::new("a/b.wav")));
        assert!(!is_profile_path(Path::new("a/b")));
    }
}
