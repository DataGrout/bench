//! Bench engine — everything the instrument does, with no UI attached.
//!
//! The split that governs this crate:
//!
//! > **The trace is local, the panel is facts.**
//!
//! Samples live in a [`ring::SampleRing`] and are drawn directly at frame rate.
//! They never reach a logic cell, a smart panel, or the gateway one sample at a
//! time. What crosses the wire is a *captured frame* through a
//! [`chain::Chain`], and what lands in a cell is a handful of
//! [`features::Features`] a rule can reason about.
//!
//! ```text
//!   Source ──▶ SampleRing ──▶ scope/meter        (local, 60fps)
//!                   │
//!                   └──▶ Chain ──▶ flow.into     (per capture, 1–4 Hz)
//!                                    │
//!                                    ├──▶ Features ──▶ logic.batch
//!                                    │                     │
//!                                    │                  Spec ──▶ verdict
//!                                    └──▶ save_as_skill ──▶ skill_id + CTC
//! ```

#![forbid(unsafe_code)]

pub mod auth;
pub mod capture;
pub mod chain;
pub mod dg;
pub mod features;
pub mod paths;
pub mod profile;
pub mod ring;
pub mod session;
pub mod source;
pub mod spec;
pub mod trigger;

pub use auth::{sign_in, Credentials, Progress};
pub use chain::{Chain, Encoding, Output, Shape, Step, StepDef, StepResult};
pub use dg::{DgClient, DgError, DgResult};
pub use features::Features;
pub use profile::Profile;
pub use ring::SampleRing;
pub use session::{ChainRun, MintedSkill, Session, Verdict};
pub use source::{synth::SynthSource, Source};
pub use spec::{Cmp, Limit, Spec};

/// The logic-cell namespace Bench writes captures and specs into.
pub const DEFAULT_NAMESPACE: &str = "bench";

/// Ring capacity in samples: ~2.7 s at 48 kHz.
///
/// Sized for the longest capture worth analysing plus scroll-back for the
/// trigger, not for history — see [`ring::SampleRing`] on why the buffer is
/// deliberately lossy.
pub const DEFAULT_RING_CAPACITY: usize = 131_072;

/// Analysis cadence when auto-analysis is switched on.
///
/// **One per second, not four.** `flow.into` costs ~5 credits per call
/// (1 gateway base + 4 tool premium), so a 4 Hz tick is 20 credits a second —
/// 72,000 an hour. The instrument's realtime half is the local trace; the
/// gateway leg is a deliberate, priced action.
pub const DEFAULT_ANALYSIS_HZ: f64 = 1.0;

/// Observed cost of one `flow.into` call, for the UI's running estimate.
///
/// Measured, not assumed: 1 gateway base + 4 tool premium. When the result
/// exceeds the gateway's ~48 KB inline budget a second call fetches it from
/// the cache for one more credit — see [`CAPTURE_SAMPLES`]. Displayed so the
/// cost of leaving auto-analysis running is visible before it is spent.
pub const FLOW_INTO_CREDITS: f64 = 5.0;

/// The extra credit spent when a result has to be fetched from the cache.
pub const CACHE_FETCH_CREDITS: f64 = 1.0;

/// Samples per capture handed to the gateway.
///
/// 2048 at 48 kHz is a 43 ms window and 23.4 Hz per bin — enough to place a
/// 440 Hz tone within a few hertz rather than reading it as 468.75, which is
/// what a 512-sample frame does at 93.75 Hz per bin.
///
/// Any `signal.spectral` over ~1000 samples exceeds the gateway's ~48 KB inline
/// result budget. That is no longer a ceiling: the gateway caches the full
/// result and Bench fetches it with one extra `prism.paginate` call. It IS one
/// more credit per run, which the UI's estimate includes.
pub const CAPTURE_SAMPLES: usize = 2048;
