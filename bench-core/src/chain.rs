//! The DSP chain — an ordered list of DG tool steps, compiled to a
//! `flow.into` plan.
//!
//! # Shape typing
//!
//! Borrowed from Signal Lab, because it was already the right idea: two shapes
//! travel a chain — a 1-D [`Shape::Series`] and the K×L [`Shape::Matrix`] that
//! `signal.embed` produces — and some steps are [`Shape::Terminal`], emitting
//! stats that nothing can consume. A step may only be appended when its input
//! shape matches the current tail, which makes an invalid chain
//! *unrepresentable* rather than an error surfaced at run time.
//!
//! Pairwise ops (`linalg.cosine_sim`, `linalg.distance`) are deliberately
//! absent: they need a second operand the length of the running signal, which a
//! single-stream chain cannot supply, so they could only ever fail here.
//!
//! # Payload encoding
//!
//! Frames ride as base64 packed little-endian floats using DG's
//! `NumericPayload` convention — every numeric input `X` also accepts `X_b64`
//! with a `dtype` of `float32` or `float64`. A 4096-sample frame is ~22 KB of
//! base64 against ~80 KB of JSON text, and it is exact rather than
//! round-tripped through decimal. The `signal.*` and `linalg.*` suites and
//! `math.window` decode it; the other `math.*` steps get a plain array.
//!
//! # Live paths
//!
//! `signal.filter` is asked for `causal: true`. The tool's default is
//! zero-phase, which is the right answer for a file and the wrong one for a
//! running instrument: each output sample is centred on samples that have not
//! arrived yet. The result carries `delay_samples` either way, and the step
//! summary shows it. `signal.iir` is causal by construction and is the filter
//! to reach for on a live signal.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// What flows between chain steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shape {
    /// A 1-D series of samples.
    Series,
    /// A K×L matrix (`signal.embed`'s trajectory output).
    Matrix,
    /// Stats or labels — nothing chains after this.
    Terminal,
}

/// How a step's response is read.
///
/// Table-driven for the same reason `input_key` is: the tools genuinely differ
/// (`values` vs `vector` vs `records`), and scattering that knowledge across
/// the UI is how a renderer ends up silently showing an empty chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// A `values` array.
    Values,
    /// A named array — `linalg.normalize` returns `vector`, not `values`.
    ValuesAt(&'static str),
    /// A `values` array plus the filter's timing report: `delay_samples`,
    /// `look_ahead_samples` and `valid_from`. A live path needs to know which
    /// of the first two is non-zero.
    Filtered,
    /// `records[]` with `magnitude`/`freq`, plus `dominant_frequency`/`peaks`.
    Spectral,
    /// `indices` + `values` per peak, with `count`, `mean_interval`, `rate`.
    Peaks,
    /// `vectors` matrix with `n` rows and `dimension` columns.
    Matrix,
    /// `explained_variance_ratio` + `scores`.
    Pca,
    /// `k`, `sizes`, `inertia`.
    Cluster,
    /// A flat map of statistics.
    Stats,
}

/// A step available to a chain: which tool, which arg carries the incoming
/// signal, and how its shapes line up.
#[derive(Debug, Clone)]
pub struct StepDef {
    /// Short key, e.g. `"signal.filter"`.
    pub key: &'static str,
    /// Fully-qualified DG tool ref.
    pub tool: &'static str,
    /// The argument the upstream value binds to.
    pub input_key: &'static str,
    /// Whether the tool decodes `<input_key>_b64` packed floats. The
    /// `signal.*` and `linalg.*` suites do, and so does `math.window` through
    /// its `data` input; `math.normalize` and `math.describe` only read plain
    /// arrays, and handing them `values_b64` reads as "no payload provided".
    pub accepts_b64: bool,
    pub input_shape: Shape,
    pub output_shape: Shape,
    /// How to read this step's response.
    pub output: Output,
    /// The response field the next step binds to — the series (or matrix)
    /// this step produces. `flow.into` resolves `$step_N.<field>`; binding the
    /// whole result object hands the next tool a map where it wants an array.
    pub output_key: &'static str,
    /// Editable arguments as `(name, default)`. Empty string means "unset".
    pub args: &'static [(&'static str, &'static str)],
}

/// Every step Bench offers. Mirrors Signal Lab's registry so results are
/// directly comparable between the two.
pub const STEP_DEFS: &[StepDef] = &[
    StepDef {
        key: "signal.filter",
        tool: "data-grout@1/signal.filter@1",
        input_key: "data",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::Filtered,
        output_key: "values",
        // Cutoffs are NORMALISED to the sample rate, (0, 0.5]. For a real fs,
        // pass f_hz / fs — see `to_normalised_cutoff`.
        //
        // `causal` is on: the tool's default is zero-phase, which centres each
        // output sample on (taps−1)/2 *future* samples. Fine for a file,
        // wrong for a running instrument — the trace would answer before the
        // signal did. Causal filtering delays the output by that many samples
        // instead, and the result says so in `delay_samples`.
        args: &[
            ("type", "lowpass"),
            ("cutoff", "0.1"),
            ("low", ""),
            ("high", ""),
            ("num_taps", "63"),
            ("causal", "true"),
        ],
    },
    StepDef {
        key: "signal.iir",
        tool: "data-grout@1/signal.iir@1",
        input_key: "data",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::Values,
        output_key: "values",
        // The live filter. A biquad is causal by construction, with a group
        // delay of a few samples rather than the FIR's (taps−1)/2, and its
        // response is returned so the corner can be checked. `cutoff` is the
        // corner (or centre, for bandpass/notch/peaking) normalised to the
        // sample rate; `q` blank means Butterworth; `gain_db` only matters to
        // peaking and the shelves.
        args: &[
            ("type", "lowpass"),
            ("cutoff", "0.1"),
            ("q", ""),
            ("gain_db", ""),
            ("stages", "1"),
        ],
    },
    StepDef {
        key: "signal.hilbert",
        tool: "data-grout@1/signal.hilbert@1",
        input_key: "data",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::Values,
        output_key: "values",
        // `values` is the amplitude envelope, which is what chains onwards —
        // an envelope is a signal, the analytic phase is not. The transform
        // is FFT-built, so the first and last few percent ripple; read the
        // interior.
        args: &[("remove_mean", "true"), ("sample_rate", "48000")],
    },
    StepDef {
        key: "signal.resample",
        tool: "data-grout@1/signal.resample@1",
        input_key: "data",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::Values,
        output_key: "values",
        // Rational rate change by `up`/`down`, anti-alias filter included.
        // Downstream steps see a series at sample_rate · up / down — a
        // `signal.spectral` after a 1/2 decimation wants half the sample
        // rate, or its frequency axis is off by two.
        args: &[("up", "1"), ("down", "2")],
    },
    StepDef {
        key: "signal.spectral",
        tool: "data-grout@1/signal.spectral@1",
        input_key: "data",
        accepts_b64: true,
        input_shape: Shape::Series,
        // A spectrum is a result, not a signal: its `records` are
        // freq/magnitude pairs, which no series tool can consume.
        output_shape: Shape::Terminal,
        output: Output::Spectral,
        output_key: "records",
        args: &[
            ("sample_rate", "48000"),
            ("window", "hann"),
            ("top_peaks", "5"),
        ],
    },
    StepDef {
        key: "signal.convolve",
        tool: "data-grout@1/signal.convolve@1",
        input_key: "a",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::Values,
        output_key: "values",
        args: &[("b", "0.25, 0.5, 0.25"), ("mode", "same")],
    },
    StepDef {
        key: "signal.correlate",
        tool: "data-grout@1/signal.correlate@1",
        input_key: "a",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::Values,
        output_key: "values",
        // `mode` is `full`/`same`/`valid` for the autocorrelation by lag, or
        // `acf`/`pacf` for the statistician's version: mean-removed, lag 0 = 1,
        // to `max_lag` (blank = the tool's default), with a 95% band.
        args: &[("mode", "full"), ("normalize", "true"), ("max_lag", "")],
    },
    StepDef {
        key: "signal.peaks",
        tool: "data-grout@1/signal.peaks@1",
        input_key: "data",
        accepts_b64: true,
        input_shape: Shape::Series,
        // Peak positions are a result, not a signal.
        output_shape: Shape::Terminal,
        output: Output::Peaks,
        output_key: "indices",
        // Blank filters mean "every local maximum". `distance` is in samples;
        // `height` and `prominence` are in the signal's units.
        args: &[
            ("height", ""),
            ("distance", ""),
            ("prominence", ""),
            ("valleys", "false"),
            ("sample_rate", "48000"),
        ],
    },
    StepDef {
        key: "signal.embed",
        tool: "data-grout@1/signal.embed@1",
        input_key: "data",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Matrix,
        output: Output::Matrix,
        output_key: "vectors",
        args: &[("dimension", "20"), ("stride", "1")],
    },
    StepDef {
        key: "linalg.pca",
        tool: "data-grout@1/linalg.pca@1",
        input_key: "vectors",
        accepts_b64: true,
        input_shape: Shape::Matrix,
        // Scores are a matrix and the variance ratio has k entries; neither
        // is a signal for the next step.
        output_shape: Shape::Terminal,
        output: Output::Pca,
        output_key: "scores",
        args: &[("k", "4")],
    },
    StepDef {
        key: "linalg.cluster",
        tool: "data-grout@1/linalg.cluster@1",
        input_key: "vectors",
        accepts_b64: true,
        input_shape: Shape::Matrix,
        output_shape: Shape::Terminal,
        output: Output::Cluster,
        output_key: "labels",
        args: &[("k", "3")],
    },
    StepDef {
        key: "math.window",
        tool: "data-grout@1/math.window@1",
        // `data` (or `data_b64`) is the series input; `values` is the OUTPUT
        // key, and a math tool handed `values` as input treats it as a
        // payload. The rolling ops leave the first window−1 outputs null,
        // which the parser drops, so the series comes back shorter.
        input_key: "data",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::Values,
        output_key: "values",
        // `op` is one of moving_avg, moving_sum, moving_std, moving_var,
        // moving_min, moving_max, moving_median, zscore_rolling, cumsum,
        // diff, pct_change, log_return, lag, ewma.
        args: &[("op", "moving_avg"), ("window", "8")],
    },
    StepDef {
        key: "math.normalize",
        tool: "data-grout@1/math.normalize@1",
        input_key: "values",
        accepts_b64: false,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::Values,
        output_key: "values",
        args: &[("method", "minmax")],
    },
    StepDef {
        key: "linalg.normalize",
        tool: "data-grout@1/linalg.normalize@1",
        input_key: "vector",
        accepts_b64: true,
        input_shape: Shape::Series,
        output_shape: Shape::Series,
        output: Output::ValuesAt("vector"),
        output_key: "vector",
        args: &[("ord", "l2")],
    },
    StepDef {
        key: "math.describe",
        tool: "data-grout@1/math.describe@1",
        // Unlike its siblings, describe reads `payload`, not `values`; the
        // tool refuses `values` outright ("did you mean include_values?").
        input_key: "payload",
        accepts_b64: false,
        input_shape: Shape::Series,
        output_shape: Shape::Terminal,
        output: Output::Stats,
        output_key: "mean",
        args: &[],
    },
];

impl StepDef {
    /// Whether the chain asks the gateway to leave `records` out of this
    /// step's result.
    ///
    /// True for the steps whose result carries `values` and a `records`
    /// projection of the same numbers — Bench charts from `values` and never
    /// reads the projection. Not the spectrum: its records are the
    /// freq/magnitude bins and the parser reads them. Not the matrix, PCA,
    /// cluster or stats steps: they have no `values`, so the gateway would
    /// keep their records anyway and the flag would only be noise.
    pub fn declines_records(&self) -> bool {
        matches!(
            self.output,
            Output::Values | Output::ValuesAt(_) | Output::Filtered | Output::Peaks
        )
    }
}

pub fn step_def(key: &str) -> Option<&'static StepDef> {
    STEP_DEFS.iter().find(|d| d.key == key)
}

/// Convert a real cutoff in Hz to the normalised value `signal.filter` wants.
///
/// Returns `None` above Nyquist, where the request is meaningless — better a
/// refusal than a silently aliased filter.
pub fn to_normalised_cutoff(hz: f64, sample_rate: f64) -> Option<f64> {
    if sample_rate <= 0.0 || hz <= 0.0 {
        return None;
    }
    let n = hz / sample_rate;
    (n > 0.0 && n <= 0.5).then_some(n)
}

/// One configured step in a chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub key: String,
    pub args: BTreeMap<String, String>,
}

impl Step {
    /// A step with its registry defaults.
    pub fn new(key: &str) -> Option<Self> {
        let def = step_def(key)?;
        Some(Self {
            key: key.to_string(),
            args: def
                .args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
    }
}

/// An ordered, shape-valid pipeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Chain {
    steps: Vec<Step>,
}

impl Chain {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// The shape a newly-appended step would receive.
    pub fn tail_shape(&self) -> Shape {
        match self.steps.last() {
            None => Shape::Series,
            Some(step) => step_def(&step.key)
                .map(|d| d.output_shape)
                .unwrap_or(Shape::Terminal),
        }
    }

    /// Step keys that may legally be appended right now.
    pub fn available_steps(&self) -> Vec<&'static str> {
        let tail = self.tail_shape();
        if tail == Shape::Terminal {
            return Vec::new();
        }
        STEP_DEFS
            .iter()
            .filter(|d| d.input_shape == tail)
            .map(|d| d.key)
            .collect()
    }

    /// Append a step, or return it unchanged if the shape does not fit.
    pub fn push(&mut self, key: &str) -> Result<(), ChainError> {
        let def = step_def(key).ok_or_else(|| ChainError::UnknownStep(key.to_string()))?;
        let tail = self.tail_shape();
        if tail == Shape::Terminal {
            return Err(ChainError::TailIsTerminal);
        }
        if def.input_shape != tail {
            return Err(ChainError::ShapeMismatch {
                step: key.to_string(),
                wants: def.input_shape,
                got: tail,
            });
        }
        self.steps.push(Step::new(key).expect("def exists"));
        Ok(())
    }

    /// Rebuild a chain from saved steps through the same checks `push` makes.
    ///
    /// A profile's chain is data from a file; deserialising `Chain` directly
    /// would skip the shape typing and let an unknown step or an impossible
    /// order in. Each step is pushed, so the first bad one is the error, and
    /// its saved arguments are laid over the registry defaults, so an argument
    /// the file did not mention is still set.
    pub fn from_steps(steps: &[Step]) -> Result<Self, ChainError> {
        let mut chain = Self::new();
        for step in steps {
            chain.push(&step.key)?;
            let index = chain.len() - 1;
            for (name, value) in &step.args {
                chain.set_arg(index, name, value.clone());
            }
        }
        Ok(chain)
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.steps.len() {
            self.steps.remove(index);
        }
    }

    pub fn set_arg(&mut self, index: usize, name: &str, value: impl Into<String>) {
        if let Some(step) = self.steps.get_mut(index) {
            step.args.insert(name.to_string(), value.into());
        }
    }

    /// Compile to `flow.into` arguments, with `samples` as `input_data`.
    ///
    /// Step 1 reads `$input.signal`; step *n* reads `$step_{n-1}.<field>`,
    /// where the field is the previous step's [`StepDef::output_key`]. That is
    /// exactly the plan an agent would submit, which is the point: the chain
    /// you build by hand and the chain an agent builds are the same artifact.
    ///
    /// The field matters. `flow.into` resolves `$step_N` to the whole result
    /// object, and a tool handed `{"values": [...], "count": 2048}` where it
    /// wants an array fails with "provide numeric arrays" — a failure the flow
    /// still reports as `completed`, per step.
    pub fn to_flow_args(&self, samples: &[f32], encoding: Encoding) -> Value {
        // Packed floats only help a first step that can decode them; a math
        // tool at the head of the chain gets the plain array instead.
        let encoding = match self.steps.first().and_then(|s| step_def(&s.key)) {
            Some(first) if !first.accepts_b64 => Encoding::JsonArray,
            _ => encoding,
        };

        let plan: Vec<Value> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let def = step_def(&step.key).expect("validated on push");
                let n = i + 1;
                let mut args = Map::new();

                for (name, _) in def.args {
                    if let Some(v) = step.args.get(*name) {
                        if let Some(value) = coerce_arg(v) {
                            args.insert((*name).to_string(), value);
                        }
                    }
                }

                // Every series tool returns `records` — the {index, value}
                // projection of `values` for charting — alongside `values`.
                // Bench charts from `values`, so the projection is wire
                // weight: about two-thirds of a 340 KB run over a 1024-sample
                // frame. `records: false` asks the gateway to leave it out. A
                // gateway that predates the switch ignores the key.
                if def.declines_records() {
                    args.insert("records".into(), json!(false));
                }

                if i == 0 {
                    match encoding {
                        Encoding::Base64F32 | Encoding::Base64F64 => {
                            args.insert(
                                format!("{}_b64", def.input_key),
                                json!("$input.signal_b64"),
                            );
                            args.insert("dtype".into(), json!(encoding.dtype()));
                        }
                        Encoding::JsonArray => {
                            args.insert(def.input_key.to_string(), json!("$input.signal"));
                        }
                    }
                } else {
                    let previous = step_def(&self.steps[i - 1].key).expect("validated on push");
                    args.insert(
                        def.input_key.to_string(),
                        json!(format!("$step_{}.{}", n - 1, previous.output_key)),
                    );
                }

                json!({
                    "step": n,
                    "type": "tool_call",
                    "tool": def.tool,
                    "args": Value::Object(args),
                    "output": format!("s{n}"),
                })
            })
            .collect();

        let input_data = match encoding {
            Encoding::JsonArray => json!({
                "signal": samples.iter().map(|s| *s as f64).collect::<Vec<_>>()
            }),
            enc => json!({ "signal_b64": encode_samples(samples, enc) }),
        };

        json!({ "plan": plan, "input_data": input_data })
    }

    /// The same plan wrapped as an MCP `tools/call` payload — what the UI
    /// shows as "the agent's call".
    pub fn to_mcp_call(&self, samples: &[f32], encoding: Encoding) -> Value {
        json!({
            "method": "tools/call",
            "params": {
                "name": "data-grout@1/flow.into@1",
                "arguments": self.to_flow_args(samples, encoding),
            }
        })
    }

    /// Mint the chain as a reusable skill: the same plan, plus the save flags.
    pub fn to_mint_args(
        &self,
        samples: &[f32],
        encoding: Encoding,
        name: &str,
        description: &str,
    ) -> Value {
        let mut args = self.to_flow_args(samples, encoding);
        if let Some(obj) = args.as_object_mut() {
            obj.insert("save_as_skill".into(), json!(true));
            obj.insert("name".into(), json!(name));
            obj.insert("description".into(), json!(description));
        }
        args
    }
}

/// How the frame crosses the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Base64 little-endian `f32`. The default: half the bytes of `f64` and
    /// already the precision the ring holds.
    Base64F32,
    /// Base64 little-endian `f64`, for sources that genuinely carry more.
    Base64F64,
    /// Plain JSON array. Readable, much larger — for docs and debugging.
    JsonArray,
}

impl Encoding {
    pub fn dtype(&self) -> &'static str {
        match self {
            Encoding::Base64F32 => "float32",
            Encoding::Base64F64 | Encoding::JsonArray => "float64",
        }
    }
}

/// Pack samples as base64 little-endian floats per DG's `NumericPayload`.
pub fn encode_samples(samples: &[f32], encoding: Encoding) -> String {
    let bytes: Vec<u8> = match encoding {
        Encoding::Base64F64 => samples
            .iter()
            .flat_map(|s| (*s as f64).to_le_bytes())
            .collect(),
        _ => samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
    };
    base64_encode(&bytes)
}

/// Minimal standard-alphabet base64 with padding.
///
/// Hand-rolled to keep this crate's dependency list to things a reader of an
/// example app would expect. Swap for the `base64` crate if it ever appears in
/// the tree for another reason.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Numbers as numbers, `true`/`false` as bools, everything else as a string —
/// and empty as absent, so an unset optional arg is omitted rather than sent
/// as `""` for the tool to reject.
fn coerce_arg(raw: &str) -> Option<Value> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match raw {
        "true" => return Some(json!(true)),
        "false" => return Some(json!(false)),
        _ => {}
    }
    if let Ok(i) = raw.parse::<i64>() {
        return Some(json!(i));
    }
    if let Ok(f) = raw.parse::<f64>() {
        return Some(json!(f));
    }
    // A comma-separated list is a numeric vector (`signal.convolve`'s kernel).
    if raw.contains(',') {
        let parts: Vec<f64> = raw
            .split(',')
            .filter_map(|p| p.trim().parse::<f64>().ok())
            .collect();
        if parts.len() == raw.split(',').filter(|p| !p.trim().is_empty()).count() {
            return Some(json!(parts));
        }
    }
    Some(json!(raw))
}

// ---------------------------------------------------------------------------
// Reading results back
// ---------------------------------------------------------------------------

/// One step's parsed result.
#[derive(Debug, Clone, PartialEq)]
pub enum StepResult {
    /// A 1-D series, ready to plot.
    Values(Vec<f64>),
    /// A filtered series with its timing. Exactly one of `delay_samples` and
    /// `look_ahead_samples` is non-zero: causal filters delay, zero-phase
    /// filters look ahead. `valid_from` is the first sample past the
    /// start-up transient.
    Filtered {
        values: Vec<f64>,
        delay_samples: usize,
        look_ahead_samples: usize,
        valid_from: usize,
    },
    /// A one-sided spectrum.
    Spectral {
        magnitudes: Vec<f64>,
        freqs: Vec<f64>,
        dominant_hz: Option<f64>,
        peaks: Vec<(f64, f64)>,
    },
    /// A trajectory matrix; only its shape and first row are kept, since the
    /// whole thing is for the next step, not for the eye.
    Matrix {
        rows: usize,
        cols: usize,
        first_row: Vec<f64>,
    },
    /// Singular spectrum.
    Pca {
        explained_variance_ratio: Vec<f64>,
        first_component: Vec<f64>,
    },
    /// Regime clustering.
    Cluster {
        k: usize,
        sizes: Vec<usize>,
        inertia: Option<f64>,
    },
    /// Summary statistics, in registry order.
    Stats(Vec<(String, f64)>),
    /// Detected peaks (or valleys): sample index and height per peak, with
    /// the mean spacing and, given a sample rate, the rate in peaks per second.
    Peaks {
        indices: Vec<usize>,
        heights: Vec<f64>,
        mean_interval: Option<f64>,
        rate_hz: Option<f64>,
    },
    /// The step ran and the tool refused it. `flow.into` reports the flow as
    /// `completed` regardless, so this is the only place the reason survives.
    Error(String),
}

impl StepResult {
    /// The series a chart should draw, if this result has one.
    pub fn series(&self) -> Option<&[f64]> {
        match self {
            StepResult::Values(v) => Some(v),
            StepResult::Filtered { values, .. } => Some(values),
            StepResult::Spectral { magnitudes, .. } => Some(magnitudes),
            StepResult::Pca {
                explained_variance_ratio,
                ..
            } => Some(explained_variance_ratio),
            StepResult::Matrix { first_row, .. } => Some(first_row),
            _ => None,
        }
    }
}

/// Parse one step's response according to its registry descriptor.
pub fn parse_step_result(output: Output, result: &Value) -> StepResult {
    // A per-step failure arrives as a successful step whose result carries
    // `error` (and usually `status_code`). Parsing it by shape would yield an
    // empty series and hide the message.
    if let Some(message) = result.get("error").and_then(Value::as_str) {
        return StepResult::Error(match result.get("status_code").and_then(Value::as_u64) {
            Some(code) => format!("{message} (HTTP {code})"),
            None => message.to_string(),
        });
    }

    match output {
        Output::Values => StepResult::Values(numbers_at(result, "values")),
        Output::ValuesAt(key) => StepResult::Values(numbers_at(result, key)),

        Output::Filtered => StepResult::Filtered {
            values: numbers_at(result, "values"),
            delay_samples: usize_at(result, "delay_samples").unwrap_or(0),
            look_ahead_samples: usize_at(result, "look_ahead_samples").unwrap_or(0),
            valid_from: usize_at(result, "valid_from").unwrap_or(0),
        },

        Output::Peaks => StepResult::Peaks {
            indices: result
                .get("indices")
                .and_then(Value::as_array)
                .map(|v| {
                    v.iter()
                        .filter_map(|i| i.as_u64().map(|n| n as usize))
                        .collect()
                })
                .unwrap_or_default(),
            heights: numbers_at(result, "values"),
            mean_interval: num(result.get("mean_interval")),
            rate_hz: num(result.get("rate")),
        },

        Output::Spectral => {
            let records = result.get("records").and_then(Value::as_array);
            let (magnitudes, freqs) = match records {
                Some(rows) => (
                    rows.iter()
                        .filter_map(|r| num(r.get("magnitude")))
                        .collect(),
                    rows.iter().filter_map(|r| num(r.get("freq"))).collect(),
                ),
                None => (Vec::new(), Vec::new()),
            };

            let peaks = result
                .get("peaks")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|p| Some((num(p.get("freq"))?, num(p.get("magnitude"))?)))
                        .collect()
                })
                .unwrap_or_default();

            StepResult::Spectral {
                magnitudes,
                freqs,
                dominant_hz: num(result.get("dominant_frequency")),
                peaks,
            }
        }

        Output::Matrix => {
            let vectors = result.get("vectors").and_then(Value::as_array);
            let first_row = vectors
                .and_then(|v| v.first())
                .map(numbers)
                .unwrap_or_default();
            StepResult::Matrix {
                rows: usize_at(result, "n").unwrap_or_else(|| vectors.map_or(0, |v| v.len())),
                cols: usize_at(result, "dimension").unwrap_or(first_row.len()),
                first_row,
            }
        }

        Output::Pca => StepResult::Pca {
            explained_variance_ratio: numbers_at(result, "explained_variance_ratio"),
            // Scores are row-major: take PC1 across rows so the component can
            // chain onwards as a series.
            first_component: result
                .get("scores")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .map(|row| row.get(0).and_then(|v| v.as_f64()).unwrap_or(0.0))
                        .collect()
                })
                .unwrap_or_default(),
        },

        Output::Cluster => StepResult::Cluster {
            k: usize_at(result, "k").unwrap_or(0),
            sizes: result
                .get("sizes")
                .and_then(Value::as_array)
                .map(|v| {
                    v.iter()
                        .filter_map(|s| s.as_u64().map(|n| n as usize))
                        .collect()
                })
                .unwrap_or_default(),
            inertia: num(result.get("inertia")),
        },

        // A fixed key order, so the meter does not reshuffle between frames.
        Output::Stats => StepResult::Stats(
            ["count", "mean", "median", "std", "min", "max", "sum"]
                .iter()
                .filter_map(|k| Some(((*k).to_string(), num(result.get(*k))?)))
                .collect(),
        ),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn numbers(v: &Value) -> Vec<f64> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| num(Some(x))).collect())
        .unwrap_or_default()
}

fn numbers_at(result: &Value, key: &str) -> Vec<f64> {
    result.get(key).map(numbers).unwrap_or_default()
}

fn usize_at(result: &Value, key: &str) -> Option<usize> {
    result.get(key)?.as_u64().map(|n| n as usize)
}

/// Pull per-step results out of a `flow.into` response.
///
/// Three lookups, in descending reliability, because the envelope varies:
///
/// 1. `execution_result.results["step_N"]` — explicitly keyed by step number.
///    This is the path to trust.
/// 2. `execution_result.step_outputs[]` with a matching `"step"` field.
/// 3. `execution_result.step_outputs[]` **positionally**.
///
/// The fallback chain is not defensive padding. An earlier gateway returned
/// `step_outputs` entries carrying only `result` — no `step` key — so matching
/// purely by number silently found nothing and every chart came back empty.
/// Today Bench asks for `step_outputs: false` (the list duplicates the keyed
/// map byte for byte), so on a current gateway only lookup 1 has anything to
/// find; the fallbacks stay for gateways that predate the switch.
///
/// Note this needs the **full** envelope: `flow.into`'s `lean` mode returns only
/// the final result, which is enough for an agent and not enough for a UI that
/// charts every stage.
pub fn parse_flow_response(chain: &Chain, response: &Value) -> Vec<Option<StepResult>> {
    let exec = response.get("execution_result");
    let keyed = exec.and_then(|e| e.get("results"));
    let outputs = exec
        .and_then(|e| e.get("step_outputs"))
        .and_then(Value::as_array);

    chain
        .steps()
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let def = step_def(&step.key)?;
            let n = i + 1;

            let result = keyed
                .and_then(|r| r.get(format!("step_{n}")))
                .or_else(|| {
                    outputs?
                        .iter()
                        .find(|o| o.get("step").and_then(Value::as_u64) == Some(n as u64))?
                        .get("result")
                })
                .or_else(|| outputs?.get(i)?.get("result"))?;

            Some(parse_step_result(def.output, result))
        })
        .collect()
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ChainError {
    #[error("unknown step: {0}")]
    UnknownStep(String),
    #[error("the last step is terminal — nothing chains after it")]
    TailIsTerminal,
    #[error("{step} wants {wants:?} but the chain tail is {got:?}")]
    ShapeMismatch {
        step: String,
        wants: Shape,
        got: Shape,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_chain_accepts_series_steps_only() {
        let chain = Chain::new();
        assert_eq!(chain.tail_shape(), Shape::Series);
        let available = chain.available_steps();
        assert!(available.contains(&"signal.filter"));
        assert!(!available.contains(&"linalg.pca"), "pca needs a matrix");
    }

    #[test]
    fn pca_requires_embed_first() {
        let mut chain = Chain::new();
        assert_eq!(
            chain.push("linalg.pca"),
            Err(ChainError::ShapeMismatch {
                step: "linalg.pca".into(),
                wants: Shape::Matrix,
                got: Shape::Series,
            })
        );

        chain.push("signal.embed").unwrap();
        assert_eq!(chain.tail_shape(), Shape::Matrix);
        chain.push("linalg.pca").unwrap();
    }

    #[test]
    fn nothing_chains_after_a_terminal_step() {
        let mut chain = Chain::new();
        chain.push("math.describe").unwrap();
        assert_eq!(chain.tail_shape(), Shape::Terminal);
        assert!(chain.available_steps().is_empty());
        assert_eq!(chain.push("signal.filter"), Err(ChainError::TailIsTerminal));
    }

    #[test]
    fn first_step_reads_input_and_later_steps_read_bindings() {
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        chain.push("signal.spectral").unwrap();

        let args = chain.to_flow_args(&[0.0, 1.0], Encoding::JsonArray);
        let plan = args["plan"].as_array().unwrap();

        assert_eq!(plan[0]["args"]["data"], json!("$input.signal"));
        assert_eq!(plan[0]["output"], json!("s1"));
        // The gateway resolves `$step_N.field`; the field is the previous
        // step's series, not its whole result object.
        assert_eq!(plan[1]["args"]["data"], json!("$step_1.values"));
        assert_eq!(plan[1]["tool"], json!("data-grout@1/signal.spectral@1"));
    }

    #[test]
    fn bindings_follow_each_producer_s_own_output_field() {
        let mut chain = Chain::new();
        chain.push("signal.embed").unwrap();
        chain.push("linalg.pca").unwrap();
        chain.push("signal.filter").unwrap_err(); // pca is terminal

        let args = chain.to_flow_args(&[0.0, 1.0], Encoding::Base64F32);
        let plan = args["plan"].as_array().unwrap();
        assert_eq!(plan[1]["args"]["vectors"], json!("$step_1.vectors"));
    }

    #[test]
    fn a_math_tool_at_the_head_gets_a_plain_array_not_packed_floats() {
        let mut chain = Chain::new();
        chain.push("math.normalize").unwrap();
        chain.push("signal.spectral").unwrap();

        let args = chain.to_flow_args(&[0.5, -0.5], Encoding::Base64F32);
        let plan = args["plan"].as_array().unwrap();
        assert_eq!(plan[0]["args"]["values"], json!("$input.signal"));
        assert!(plan[0]["args"].get("values_b64").is_none());
        assert_eq!(args["input_data"]["signal"], json!([0.5, -0.5]));
    }

    #[test]
    fn series_steps_ask_the_gateway_to_omit_records_and_the_spectrum_does_not() {
        // `records` duplicates `values` for charting; the spectrum's records
        // are the bins themselves and the parser reads them. Sending the flag
        // to the spectrum would blank the chart; leaving it off the series
        // steps is two-thirds of the wire.
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        chain.push("math.window").unwrap();
        chain.push("signal.spectral").unwrap();

        let args = chain.to_flow_args(&[0.0], Encoding::Base64F32);
        let plan = args["plan"].as_array().unwrap();
        assert_eq!(plan[0]["args"]["records"], json!(false));
        assert_eq!(plan[1]["args"]["records"], json!(false));
        assert!(
            plan[2]["args"].get("records").is_none(),
            "the spectrum must keep its records"
        );

        // Peaks returns values (heights) plus per-peak records Bench does not
        // read, so it declines too. The matrix, PCA, cluster and stats steps
        // have no `values`, so the gateway keeps their records regardless and
        // the flag is left off rather than sent as noise.
        assert!(step_def("signal.peaks").unwrap().declines_records());
        assert!(step_def("linalg.normalize").unwrap().declines_records());
        for key in [
            "signal.spectral",
            "signal.embed",
            "math.describe",
            "linalg.pca",
            "linalg.cluster",
        ] {
            let def = step_def(key).unwrap();
            assert!(!def.declines_records(), "{key} should not send the flag");
        }
    }

    #[test]
    fn math_window_takes_packed_floats_on_its_data_input() {
        // `math.window` grew a `data`/`data_b64` input; `values` is its output
        // key and was never the right thing to hand it.
        let mut chain = Chain::new();
        chain.push("math.window").unwrap();
        chain.push("signal.spectral").unwrap();

        let args = chain.to_flow_args(&[0.5, -0.5], Encoding::Base64F32);
        let plan = args["plan"].as_array().unwrap();
        assert_eq!(plan[0]["args"]["data_b64"], json!("$input.signal_b64"));
        assert!(plan[0]["args"].get("values").is_none());
        assert_eq!(plan[1]["args"]["data"], json!("$step_1.values"));
    }

    #[test]
    fn the_fir_filter_is_causal_by_default_and_reports_its_delay() {
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        let args = chain.to_flow_args(&[0.0], Encoding::JsonArray);
        assert_eq!(args["plan"][0]["args"]["causal"], json!(true));

        let r = parse_step_result(
            Output::Filtered,
            &json!({
                "values": [0.0, 0.5, 1.0],
                "delay_samples": 31,
                "look_ahead_samples": 0,
                "valid_from": 62
            }),
        );
        assert_eq!(
            r,
            StepResult::Filtered {
                values: vec![0.0, 0.5, 1.0],
                delay_samples: 31,
                look_ahead_samples: 0,
                valid_from: 62,
            }
        );
        // The series still charts like any other.
        assert_eq!(r.series().unwrap().len(), 3);
    }

    #[test]
    fn a_zero_phase_filter_result_reports_look_ahead_not_delay() {
        let r = parse_step_result(
            Output::Filtered,
            &json!({"values": [1.0], "delay_samples": 0, "look_ahead_samples": 31, "valid_from": 31}),
        );
        match r {
            StepResult::Filtered {
                delay_samples,
                look_ahead_samples,
                ..
            } => {
                assert_eq!(delay_samples, 0);
                assert_eq!(look_ahead_samples, 31);
            }
            other => panic!("expected Filtered, got {other:?}"),
        }
    }

    #[test]
    fn the_new_series_steps_chain_on_values_and_peaks_is_terminal() {
        let mut chain = Chain::new();
        chain.push("signal.iir").unwrap();
        chain.push("signal.hilbert").unwrap();
        chain.push("signal.resample").unwrap();
        chain.push("signal.peaks").unwrap();
        assert_eq!(chain.tail_shape(), Shape::Terminal);

        let args = chain.to_flow_args(&[0.0], Encoding::Base64F32);
        let plan = args["plan"].as_array().unwrap();
        assert_eq!(plan[0]["args"]["data_b64"], json!("$input.signal_b64"));
        // Blank `q` and `gain_db` are omitted, so the tool applies Butterworth.
        assert!(plan[0]["args"].get("q").is_none());
        assert_eq!(plan[0]["args"]["stages"], json!(1));
        assert_eq!(plan[1]["args"]["data"], json!("$step_1.values"));
        assert_eq!(plan[2]["args"]["data"], json!("$step_2.values"));
        assert_eq!(plan[3]["args"]["data"], json!("$step_3.values"));
        assert_eq!(plan[3]["tool"], json!("data-grout@1/signal.peaks@1"));
    }

    #[test]
    fn parses_a_peaks_result() {
        let r = parse_step_result(
            Output::Peaks,
            &json!({
                "indices": [100, 300, 500],
                "values": [1.0, 0.9, 1.1],
                "count": 3,
                "mean_interval": 200.0,
                "rate": 0.5
            }),
        );
        assert_eq!(
            r,
            StepResult::Peaks {
                indices: vec![100, 300, 500],
                heights: vec![1.0, 0.9, 1.1],
                mean_interval: Some(200.0),
                rate_hz: Some(0.5),
            }
        );
        // Positions are not a trace.
        assert!(r.series().is_none());
    }

    #[test]
    fn a_spectrum_is_terminal() {
        let mut chain = Chain::new();
        chain.push("signal.spectral").unwrap();
        assert_eq!(chain.tail_shape(), Shape::Terminal);
    }

    #[test]
    fn a_step_error_is_reported_not_parsed_as_empty() {
        let r = parse_step_result(
            Output::Values,
            &json!({"error": "Provide both `a` and `b` as numeric arrays.", "status_code": 400}),
        );
        assert_eq!(
            r,
            StepResult::Error("Provide both `a` and `b` as numeric arrays. (HTTP 400)".into())
        );
        assert!(r.series().is_none());
    }

    #[test]
    fn base64_encoding_sets_the_b64_arg_and_dtype() {
        let mut chain = Chain::new();
        chain.push("signal.spectral").unwrap();

        let args = chain.to_flow_args(&[1.0, 2.0], Encoding::Base64F32);
        let step = &args["plan"][0];
        assert_eq!(step["args"]["data_b64"], json!("$input.signal_b64"));
        assert_eq!(step["args"]["dtype"], json!("float32"));
        // The raw array arg must NOT also be present — an explicit array wins
        // over its _b64 companion, which would silently defeat the encoding.
        assert!(step["args"].get("data").is_none());
        assert!(args["input_data"]["signal_b64"].is_string());
    }

    #[test]
    fn empty_args_are_omitted_not_sent_blank() {
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        let args = chain.to_flow_args(&[0.0], Encoding::JsonArray);
        let step_args = &args["plan"][0]["args"];

        assert_eq!(step_args["cutoff"], json!(0.1));
        assert!(
            step_args.get("low").is_none(),
            "blank 'low' must be omitted"
        );
    }

    #[test]
    fn convolve_kernel_coerces_to_a_number_list() {
        let mut chain = Chain::new();
        chain.push("signal.convolve").unwrap();
        let args = chain.to_flow_args(&[0.0], Encoding::JsonArray);
        assert_eq!(args["plan"][0]["args"]["b"], json!([0.25, 0.5, 0.25]));
    }

    #[test]
    fn minting_adds_the_save_flags_to_the_same_plan() {
        let mut chain = Chain::new();
        chain.push("signal.spectral").unwrap();
        let args = chain.to_mint_args(&[0.0], Encoding::Base64F32, "Spectrum", "desc");

        assert_eq!(args["save_as_skill"], json!(true));
        assert_eq!(args["name"], json!("Spectrum"));
        assert!(args["plan"].is_array(), "mint must reuse the plan verbatim");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"sure."), "c3VyZS4=");
    }

    // ── reading results back ─────────────────────────────────────────────

    #[test]
    fn parses_a_values_step() {
        let r = parse_step_result(Output::Values, &json!({"values": [1.0, 2.0, 3.0]}));
        assert_eq!(r, StepResult::Values(vec![1.0, 2.0, 3.0]));
        assert_eq!(r.series().unwrap().len(), 3);
    }

    #[test]
    fn linalg_normalize_reads_vector_not_values() {
        // The tools genuinely disagree on the key; reading `values` here would
        // silently produce an empty chart.
        let response = json!({"vector": [0.6, 0.8]});
        assert_eq!(
            parse_step_result(Output::ValuesAt("vector"), &response),
            StepResult::Values(vec![0.6, 0.8])
        );
        assert_eq!(
            parse_step_result(Output::Values, &response),
            StepResult::Values(vec![])
        );
    }

    #[test]
    fn parses_a_spectrum_with_peaks() {
        let response = json!({
            "records": [{"freq": 0.0, "magnitude": 0.1}, {"freq": 8.0, "magnitude": 9.0}],
            "dominant_frequency": 8.0,
            "peaks": [{"freq": 8.0, "magnitude": 9.0}]
        });
        match parse_step_result(Output::Spectral, &response) {
            StepResult::Spectral {
                magnitudes,
                freqs,
                dominant_hz,
                peaks,
            } => {
                assert_eq!(magnitudes, vec![0.1, 9.0]);
                assert_eq!(freqs, vec![0.0, 8.0]);
                assert_eq!(dominant_hz, Some(8.0));
                assert_eq!(peaks, vec![(8.0, 9.0)]);
            }
            other => panic!("expected Spectral, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_trajectory_matrix_shape() {
        let response = json!({"vectors": [[1.0, 2.0], [3.0, 4.0]], "n": 2, "dimension": 2});
        match parse_step_result(Output::Matrix, &response) {
            StepResult::Matrix {
                rows,
                cols,
                first_row,
            } => {
                assert_eq!((rows, cols), (2, 2));
                assert_eq!(first_row, vec![1.0, 2.0]);
            }
            other => panic!("expected Matrix, got {other:?}"),
        }
    }

    #[test]
    fn pca_takes_pc1_across_rows() {
        let response = json!({
            "explained_variance_ratio": [0.9, 0.1],
            "scores": [[1.0, 9.9], [2.0, 9.9], [3.0, 9.9]]
        });
        match parse_step_result(Output::Pca, &response) {
            StepResult::Pca {
                first_component, ..
            } => {
                assert_eq!(first_component, vec![1.0, 2.0, 3.0]);
            }
            other => panic!("expected Pca, got {other:?}"),
        }
    }

    #[test]
    fn stats_keep_a_fixed_order() {
        let r = parse_step_result(Output::Stats, &json!({"max": 2.0, "mean": 1.0, "count": 3}));
        match r {
            // Registry order, not JSON order — a meter that reshuffles between
            // frames is unreadable.
            StepResult::Stats(pairs) => {
                let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
                assert_eq!(keys, vec!["count", "mean", "max"]);
            }
            other => panic!("expected Stats, got {other:?}"),
        }
    }

    #[test]
    fn missing_fields_parse_to_empty_not_zero() {
        assert_eq!(
            parse_step_result(Output::Values, &json!({})),
            StepResult::Values(vec![])
        );
        match parse_step_result(Output::Spectral, &json!({})) {
            StepResult::Spectral { dominant_hz, .. } => assert!(dominant_hz.is_none()),
            other => panic!("expected Spectral, got {other:?}"),
        }
    }

    #[test]
    fn flow_response_prefers_the_step_keyed_results_map() {
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        chain.push("signal.spectral").unwrap();

        let response = json!({
            "execution_result": {
                "results": {
                    "step_1": {"values": [0.5]},
                    "step_2": {"records": [{"freq": 1.0, "magnitude": 5.0}]}
                }
            }
        });

        let parsed = parse_flow_response(&chain, &response);
        assert_eq!(
            parsed[0].as_ref().and_then(StepResult::series),
            Some(&[0.5][..])
        );
        assert!(matches!(parsed[1], Some(StepResult::Spectral { .. })));
    }

    #[test]
    fn flow_response_falls_back_to_position_when_step_outputs_are_unnumbered() {
        // This is the shape the gateway actually returns: `step_outputs`
        // entries carry only `result`. Matching purely by step number finds
        // nothing and every chart comes back empty.
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        chain.push("signal.spectral").unwrap();

        let response = json!({
            "execution_result": {
                "status": "completed",
                "step_outputs": [
                    {"result": {"values": [0.5]}},
                    {"result": {"records": [{"freq": 1.0, "magnitude": 5.0}]}}
                ]
            }
        });

        let parsed = parse_flow_response(&chain, &response);
        assert_eq!(
            parsed[0].as_ref().and_then(StepResult::series),
            Some(&[0.5][..])
        );
        assert!(matches!(parsed[1], Some(StepResult::Spectral { .. })));
    }

    #[test]
    fn numbered_step_outputs_still_match_by_number() {
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        chain.push("signal.spectral").unwrap();

        // Deliberately out of order on the wire.
        let response = json!({
            "execution_result": {
                "step_outputs": [
                    {"step": 2, "result": {"records": [{"freq": 1.0, "magnitude": 5.0}]}},
                    {"step": 1, "result": {"values": [0.5]}}
                ]
            }
        });

        let parsed = parse_flow_response(&chain, &response);
        assert_eq!(
            parsed[0].as_ref().and_then(StepResult::series),
            Some(&[0.5][..])
        );
        assert!(matches!(parsed[1], Some(StepResult::Spectral { .. })));
    }

    #[test]
    fn a_lean_flow_response_yields_no_step_results() {
        // `lean: true` returns only the final result. Bench must not request it
        // — this asserts the failure is empty slots, not a wrong chart.
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        let parsed = parse_flow_response(&chain, &json!({"result": {"values": [1.0]}}));
        assert_eq!(parsed, vec![None]);
    }

    #[test]
    fn a_partial_run_leaves_later_steps_empty() {
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();
        chain.push("signal.spectral").unwrap();

        let response = json!({
            "execution_result": {"step_outputs": [{"step": 1, "result": {"values": [1.0]}}]}
        });
        let parsed = parse_flow_response(&chain, &response);
        assert!(parsed[0].is_some());
        assert_eq!(parsed[1], None);
    }

    #[test]
    fn cutoff_normalisation_rejects_above_nyquist() {
        assert_eq!(
            to_normalised_cutoff(1000.0, 48000.0),
            Some(1000.0 / 48000.0)
        );
        assert_eq!(to_normalised_cutoff(30000.0, 48000.0), None);
        assert_eq!(to_normalised_cutoff(24000.0, 48000.0), Some(0.5));
    }
}
