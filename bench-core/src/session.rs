//! The gateway session — what Bench actually asks DataGrout to do.
//!
//! The operations, matching what the instrument needs:
//!
//! | operation | tool | when |
//! |---|---|---|
//! | [`run_chain`](Session::run_chain) | `flow.into` | every analysis tick |
//! | [`record`](Session::record) | `logic.batch` | after a capture is measured |
//! | [`mint`](Session::mint) | `flow.into` + `save_as_skill` | on demand |
//! | [`list_panels`](Session::list_panels) | `smart_panel.list` | when the Panels pane loads |
//! | [`load_panel_rows`](Session::load_panel_rows) | `logic.query` | per panel, on demand |
//!
//! All of them are `async` and none know about a UI. The caller owns the
//! runtime and decides how results get back to a frame — see `bench-gui` for
//! the egui-side pattern.

use std::sync::Arc;

use datagrout_panels::{normalize_rows, Panel, PANELS_NAMESPACE};
use serde_json::Value;

use crate::chain::{parse_flow_response, Chain, Encoding, StepResult};
use crate::dg::{tools, DgClient, DgError, DgResult};
use crate::features::Features;
use crate::spec::Spec;

/// A completed chain run.
#[derive(Debug, Clone)]
pub struct ChainRun {
    /// One entry per chain step, `None` where the step produced nothing.
    pub steps: Vec<Option<StepResult>>,
    /// Cognitive Trust Certificate id, when the plan minted one.
    pub ctc_id: Option<String>,
    /// Where the full per-step payload lives, for anything too big to inline.
    pub cache_ref: Option<String>,
    /// Whether the result exceeded the inline budget and was fetched from the
    /// cache in a second call (one extra credit).
    pub fetched_from_cache: bool,
}

/// Whether a response is the gateway's oversized-result stand-in.
///
/// `payload_key` is the field a complete response would carry; its absence
/// alongside a `cache_ref` is the tell when `_headed` is not set.
fn is_headed(response: &Value, payload_key: &str) -> bool {
    response.get("_headed").and_then(Value::as_bool) == Some(true)
        || (response.get("cache_ref").is_some() && response.get(payload_key).is_none())
}

/// The human-readable reason inside a `flow.into` `execution_error`.
///
/// Ideally that field is an object with an `error` string. The gateway also
/// emits it as one flat string in Elixir's `inspect` syntax
/// (`%{"completed_steps" => 2, "error" => "...", ...}`), so the `error` entry
/// is dug out of that text when it has to be. Anything else is passed through,
/// trimmed to a status-line length.
fn flow_error_message(err: &Value) -> String {
    if let Some(message) = err.get("error").and_then(Value::as_str) {
        return message.to_string();
    }
    let text = match err {
        Value::String(s) => s.as_str(),
        other => return other.to_string().chars().take(300).collect(),
    };
    let marker = "\"error\" => \"";
    let Some(start) = text.find(marker).map(|i| i + marker.len()) else {
        return text.chars().take(300).collect();
    };
    let rest = &text[start..];
    let end = rest.find("\", \"").unwrap_or(rest.len());
    rest[..end].replace("\\n", "\n").replace("\\\"", "\"")
}

/// Quote a panel id as a Prolog atom for interpolation into a goal.
///
/// Ids are atoms on the server; quoting is harmless for plain ones and
/// necessary for anything with a capital, a hyphen or a space.
fn atom(id: &str) -> String {
    format!("'{}'", id.replace('\'', "''"))
}

impl ChainRun {
    /// The spectral readings a chain produced, for folding into [`Features`].
    ///
    /// Scans backwards: with more than one spectral step, the last one is the
    /// most processed and the one a spec should judge.
    pub fn spectral(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.steps.iter().rev().flatten().find_map(|s| match s {
            StepResult::Spectral {
                dominant_hz, peaks, ..
            } => Some((*dominant_hz, thd_from_peaks(peaks))),
            _ => None,
        })
    }

    /// The regime label, if the chain clustered.
    pub fn regime(&self) -> Option<String> {
        self.steps.iter().rev().flatten().find_map(|s| match s {
            // The largest cluster is the prevailing regime for this capture.
            StepResult::Cluster { sizes, .. } => sizes
                .iter()
                .enumerate()
                .max_by_key(|(_, n)| **n)
                .map(|(i, _)| format!("regime_{i}")),
            _ => None,
        })
    }
}

/// Total harmonic distortion from a peak list, as a percentage.
///
/// `sqrt(sum of harmonic power) / fundamental`, where the fundamental is the
/// largest peak and a harmonic is a peak within [`HARMONIC_TOLERANCE`] of an
/// integer multiple of it.
///
/// The tolerance is the whole point. A windowed FFT smears a tone across
/// neighbouring bins, so a naive "every peak below the fundamental is a
/// harmonic" counts that leakage skirt and reports absurd distortion — 85% for
/// a clean two-tone signal, in the case that prompted this. Only peaks at
/// genuine multiples count.
///
/// Returns `None` when no harmonic is identified: with nothing above the
/// fundamental there is no distortion measurement, and 0.0 would be a claim the
/// data does not support.
fn thd_from_peaks(peaks: &[(f64, f64)]) -> Option<f64> {
    let (fundamental_hz, fundamental_mag) =
        peaks.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1))?;

    if fundamental_hz <= 0.0 || fundamental_mag <= 0.0 {
        return None;
    }

    let harmonic_power: f64 = peaks
        .iter()
        .filter(|(hz, _)| is_harmonic_of(*hz, fundamental_hz))
        .map(|(_, mag)| mag * mag)
        .sum();

    if harmonic_power <= 0.0 {
        return None;
    }
    Some(harmonic_power.sqrt() / fundamental_mag * 100.0)
}

/// How far a peak may sit from an exact multiple and still count, as a fraction
/// of the fundamental. Wide enough to survive the bin resolution of a short
/// frame, narrow enough to exclude an adjacent leakage bin.
const HARMONIC_TOLERANCE: f64 = 0.15;

fn is_harmonic_of(hz: f64, fundamental: f64) -> bool {
    if fundamental <= 0.0 {
        return false;
    }
    let ratio = hz / fundamental;
    // 2nd harmonic and above; the fundamental is not its own harmonic, and a
    // sub-fundamental peak is leakage or noise, not distortion.
    if ratio < 1.5 {
        return false;
    }
    let nearest = ratio.round();
    (ratio - nearest).abs() <= HARMONIC_TOLERANCE
}

/// A connected gateway session.
pub struct Session {
    client: Arc<dyn DgClient>,
    namespace: String,
    encoding: Encoding,
}

impl Session {
    pub fn new(client: Arc<dyn DgClient>) -> Self {
        Self {
            client,
            namespace: crate::DEFAULT_NAMESPACE.to_string(),
            encoding: Encoding::Base64F32,
        }
    }

    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    pub fn with_encoding(mut self, encoding: Encoding) -> Self {
        self.encoding = encoding;
        self
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn describe(&self) -> String {
        self.client.describe()
    }

    /// Run a chain over one captured frame.
    ///
    /// Deliberately does NOT pass `lean: true`. Lean mode returns only the final
    /// result, which suits an agent and starves a UI that charts every stage.
    ///
    /// It does pass `step_outputs: false`. The full envelope carries every
    /// step's result twice — keyed under `results.step_N` and again in the
    /// `step_outputs` list, byte for byte — and Bench reads the keyed map, so
    /// the list is half the response for nothing. A gateway that predates the
    /// switch ignores it and the parser's fallbacks still read the list.
    pub async fn run_chain(&self, chain: &Chain, samples: &[f32]) -> DgResult<ChainRun> {
        if chain.is_empty() {
            return Ok(ChainRun {
                steps: Vec::new(),
                ctc_id: None,
                cache_ref: None,
                fetched_from_cache: false,
            });
        }

        let mut args = chain.to_flow_args(samples, self.encoding);
        if let Some(obj) = args.as_object_mut() {
            obj.insert("step_outputs".into(), Value::Bool(false));
        }
        let mut response = self.client.call_tool(tools::FLOW_INTO, args).await?;
        let mut fetched_from_cache = false;

        // A flow that fails mid-way comes back with `execution_error` and no
        // `execution_result` at all — even the steps that succeeded are only
        // inside the error. Parsing that as "every step returned nothing"
        // would blank the results and hide the reason.
        if let Some(err) = response.get("execution_error") {
            return Err(DgError::Tool {
                tool: tools::FLOW_INTO.to_string(),
                message: flow_error_message(err),
            });
        }

        // The gateway swaps any result over ~48 KB for a `preview` plus a
        // `cache_ref`, to protect an agent's context window. A UI has no such
        // constraint and the full result is already computed and cached, so
        // fetch it rather than fail: `prism.paginate` on the cache_ref returns
        // the whole envelope as one record, stamped `_no_head`, for one credit.
        if is_headed(&response, "execution_result") {
            let cache_ref = response
                .get("cache_ref")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            response = self.fetch_cached(&cache_ref).await?;
            fetched_from_cache = true;
        }

        Ok(ChainRun {
            steps: parse_flow_response(chain, &response),
            ctc_id: string_at(&response, &["ctc", "id"]),
            cache_ref: response
                .get("cache_ref")
                .and_then(Value::as_str)
                .map(str::to_string),
            fetched_from_cache,
        })
    }

    /// Retrieve a headed result in full.
    async fn fetch_cached(&self, cache_ref: &str) -> DgResult<Value> {
        let page = self
            .client
            .call_tool(
                tools::PRISM_PAGINATE,
                serde_json::json!({ "cache_ref": cache_ref, "page": 1, "per_page": 1 }),
            )
            .await?;

        // A cached flow.into envelope paginates as a single record holding the
        // whole thing. Anything else means the cache held something we did not
        // expect, and guessing would produce wrong charts rather than none.
        page.get("records")
            .and_then(Value::as_array)
            .and_then(|r| r.first())
            .filter(|r| r.get("execution_result").is_some())
            .cloned()
            .ok_or_else(|| DgError::Tool {
                tool: tools::PRISM_PAGINATE.to_string(),
                message: format!("cached result {cache_ref} did not contain a flow envelope"),
            })
    }

    /// Load every Smart Panel on the account, children resolved.
    ///
    /// One `smart_panel.list` call. Rows on the returned panels are the list's
    /// `data_preview` — the first few only. [`load_panel_rows`](Self::load_panel_rows)
    /// fills a panel in on demand, because doing it for every panel up front is
    /// one `logic.query` per data panel and that is real money.
    pub async fn list_panels(&self) -> DgResult<Vec<Panel>> {
        let mut response = self
            .client
            .call_tool(tools::SMART_PANEL_LIST, serde_json::json!({ "limit": 200 }))
            .await?;

        // Smart Panels are on for every account, so this is a guard rather
        // than an expected path: the tool reports any gating as a successful
        // call carrying an `error` field, not as a tool failure. Surface it
        // as one instead of showing an empty pane.
        if let Some(message) = response.get("error").and_then(Value::as_str) {
            return Err(DgError::Tool {
                tool: tools::SMART_PANEL_LIST.to_string(),
                message: message.to_string(),
            });
        }

        if is_headed(&response, "panels") {
            let cache_ref = response
                .get("cache_ref")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            response = self.fetch_cached_list(&cache_ref).await?;
        }

        Ok(Panel::all_from_list(&response))
    }

    /// Return `panel` with full rows for it and every data panel under it.
    ///
    /// Prefers the `panel_data` snapshot — an ordered list of lists — over the
    /// live `panel_source` query. Solution rows from `logic.query` are objects
    /// whose key order is not the query's variable order, so a `[label, value]`
    /// pair can come back reversed; the snapshot cannot. The source is used only
    /// when no snapshot exists.
    ///
    /// Cost: one `logic.query` per data panel, so a three-child dashboard is
    /// three calls. Callers should say so before spending.
    pub async fn load_panel_rows(&self, panel: &Panel) -> DgResult<Panel> {
        let mut out = panel.clone();

        if panel.kind.is_data_kind() {
            let snapshot = self
                .client
                .query(
                    PANELS_NAMESPACE,
                    &format!("panel_data({}, Rows)", atom(&panel.id)),
                    1,
                )
                .await?;

            let rows = match snapshot.first().and_then(|r| r.get("Rows")) {
                Some(rows) if rows.as_array().is_some_and(|a| !a.is_empty()) => {
                    normalize_rows(rows)
                }
                _ => match &panel.source {
                    Some(source) => {
                        let solutions = self
                            .client
                            .query(&source.namespace, &source.query, 200)
                            .await?;
                        normalize_rows(&Value::Array(solutions))
                    }
                    None => Vec::new(),
                },
            };
            // Keep the preview if the cell had nothing better; an empty
            // replacement would read as data vanishing.
            if !rows.is_empty() {
                out.rows = rows;
            }
        }

        let mut children = Vec::with_capacity(panel.children.len());
        for child in &panel.children {
            children.push(Box::pin(self.load_panel_rows(child)).await?);
        }
        out.children = children;

        Ok(out)
    }

    /// How many `logic.query` calls [`load_panel_rows`](Self::load_panel_rows)
    /// would make for `panel`. Shown before spending.
    pub fn row_load_calls(panel: &Panel) -> usize {
        usize::from(panel.kind.is_data_kind())
            + panel
                .children
                .iter()
                .map(Self::row_load_calls)
                .sum::<usize>()
    }

    /// Retrieve a headed `smart_panel.list` result in full.
    ///
    /// A cached tool result paginates by its principal list, so the records may
    /// be the panel entries themselves — or, if the paginator kept the envelope
    /// whole, a single record carrying `panels`. Both are accepted.
    async fn fetch_cached_list(&self, cache_ref: &str) -> DgResult<Value> {
        let page = self
            .client
            .call_tool(
                tools::PRISM_PAGINATE,
                serde_json::json!({ "cache_ref": cache_ref, "page": 1, "per_page": 1000 }),
            )
            .await?;

        let records = page
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        match records.as_slice() {
            [single] if single.get("panels").is_some() => Ok(single.clone()),
            _ => Ok(serde_json::json!({ "panels": records })),
        }
    }

    /// Assert a capture's derived features into the logic cell.
    ///
    /// One `logic.batch` call, not one call per fact: batching exists precisely
    /// because a per-frame loop that charges per operation is unaffordable.
    pub async fn record(&self, features: &Features) -> DgResult<()> {
        self.client
            .call_tool(tools::LOGIC_BATCH, features.to_batch_args(&self.namespace))
            .await
            .map(|_| ())
    }

    /// Store a spec's rules.
    pub async fn install_spec(&self, spec: &Spec) -> DgResult<()> {
        self.client
            .call_tool(tools::LOGIC_BATCH, spec.to_batch_args(&self.namespace))
            .await
            .map(|_| ())
    }

    /// Ask the cell whether a capture passes a spec.
    ///
    /// The verdict is *derived*, not computed here — that is the point. A rule
    /// stored in the cell can equally be queried by anything else that reaches
    /// the gateway, or exposed as an HTTP endpoint, without reimplementation.
    pub async fn verdict(&self, spec: &Spec, capture_id: &str) -> DgResult<Verdict> {
        let goal = spec.verdict_goal(capture_id);
        let passed = !self
            .client
            .query(&self.namespace, &goal, 1)
            .await?
            .is_empty();

        if passed {
            return Ok(Verdict::Pass);
        }

        // On failure, name the limits that failed. "Fails spec" is not an
        // actionable bench reading.
        let rows = self
            .client
            .query(&self.namespace, &spec.violations_goal(capture_id), 20)
            .await?;

        Ok(Verdict::Fail(
            rows.iter()
                .filter_map(|r| r.get("Metric")?.as_str().map(str::to_string))
                .collect(),
        ))
    }

    /// Mint the chain as a reusable skill with its certificate.
    pub async fn mint(
        &self,
        chain: &Chain,
        samples: &[f32],
        name: &str,
        description: &str,
    ) -> DgResult<MintedSkill> {
        if chain.is_empty() {
            return Err(DgError::Tool {
                tool: tools::FLOW_INTO.to_string(),
                message: "nothing to mint — the chain is empty".into(),
            });
        }

        let args = chain.to_mint_args(samples, self.encoding, name, description);
        let response = self.client.call_tool(tools::FLOW_INTO, args).await?;

        let skill_id = response
            .get("skill_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DgError::Tool {
                tool: tools::FLOW_INTO.to_string(),
                message: "mint returned no skill_id".into(),
            })?
            .to_string();

        Ok(MintedSkill {
            skill_id,
            name: name.to_string(),
            ctc_id: string_at(&response, &["ctc", "id"]),
            ctc_url: string_at(&response, &["ctc", "viewer_url"]),
        })
    }
}

/// A spec verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Pass,
    /// The metrics whose limits were violated.
    Fail(Vec<String>),
}

impl Verdict {
    pub fn passed(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

/// A skill minted from a chain.
#[derive(Debug, Clone)]
pub struct MintedSkill {
    pub skill_id: String,
    pub name: String,
    pub ctc_id: Option<String>,
    pub ctc_url: Option<String>,
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    cursor.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dg::fake::FakeClient;
    use serde_json::json;

    fn chain_of(keys: &[&str]) -> Chain {
        let mut chain = Chain::new();
        for key in keys {
            chain.push(key).unwrap();
        }
        chain
    }

    #[tokio::test]
    async fn an_empty_chain_calls_nothing() {
        let client = FakeClient::default();
        let session = Session::new(Arc::new(client));
        let run = session.run_chain(&Chain::new(), &[1.0]).await.unwrap();
        assert!(run.steps.is_empty());
    }

    #[tokio::test]
    async fn run_chain_parses_steps_and_the_certificate() {
        let client = FakeClient::default().with(
            tools::FLOW_INTO,
            json!({
                "execution_result": {
                    "step_outputs": [{"step": 1, "result": {"values": [1.0, 2.0]}}]
                },
                "ctc": {"id": "ctc_1", "viewer_url": "https://example.com/ctc_1"},
                "cache_ref": "cache_abc"
            }),
        );
        let session = Session::new(Arc::new(client));

        let run = session
            .run_chain(&chain_of(&["signal.filter"]), &[0.1, 0.2])
            .await
            .unwrap();

        assert_eq!(
            run.steps[0].as_ref().and_then(StepResult::series),
            Some(&[1.0, 2.0][..])
        );
        assert_eq!(run.ctc_id.as_deref(), Some("ctc_1"));
        assert_eq!(run.cache_ref.as_deref(), Some("cache_abc"));
    }

    #[tokio::test]
    async fn run_chain_never_requests_lean_mode() {
        let client = Arc::new(FakeClient::default().with(tools::FLOW_INTO, json!({})));
        let session = Session::new(client.clone());
        session
            .run_chain(&chain_of(&["signal.filter"]), &[0.0])
            .await
            .unwrap();

        // Lean returns only the final result; a UI charting every stage needs
        // the full envelope.
        let args = &client.calls_to(tools::FLOW_INTO)[0];
        assert!(args.get("lean").is_none());
        assert!(args["plan"].is_array());
        // …but it does decline the duplicate per-step list: the keyed
        // `results` map is what the parser reads first.
        assert_eq!(args["step_outputs"], json!(false));
    }

    #[tokio::test]
    async fn an_oversized_result_is_fetched_from_the_cache_not_dropped() {
        // The gateway returns a preview + cache_ref for anything over ~48 KB.
        // The full result exists server-side; a UI should retrieve it.
        let client = Arc::new(
            FakeClient::default()
                .with(
                    tools::FLOW_INTO,
                    json!({
                        "_headed": true,
                        "cache_ref": "rc_big",
                        "result_bytes": 333545,
                        "preview": {}
                    }),
                )
                .with(
                    tools::PRISM_PAGINATE,
                    json!({
                        "_no_head": true,
                        "total_records": 1,
                        "records": [{
                            "execution_result": {
                                "results": {"step_1": {"values": [1.0, 2.0, 3.0]}}
                            },
                            "ctc": {"id": "ctc_7"}
                        }]
                    }),
                ),
        );
        let session = Session::new(client.clone());

        let run = session
            .run_chain(&chain_of(&["signal.filter"]), &[0.0; 8])
            .await
            .unwrap();

        assert_eq!(
            run.steps[0].as_ref().and_then(StepResult::series),
            Some(&[1.0, 2.0, 3.0][..])
        );
        assert_eq!(run.ctc_id.as_deref(), Some("ctc_7"));
        assert!(run.fetched_from_cache);

        let page_calls = client.calls_to(tools::PRISM_PAGINATE);
        assert_eq!(page_calls.len(), 1);
        assert_eq!(page_calls[0]["cache_ref"], json!("rc_big"));
    }

    #[tokio::test]
    async fn an_inline_result_makes_no_paginate_call() {
        let client = Arc::new(FakeClient::default().with(
            tools::FLOW_INTO,
            json!({"execution_result": {"results": {"step_1": {"values": [1.0]}}}}),
        ));
        let session = Session::new(client.clone());
        let run = session
            .run_chain(&chain_of(&["signal.filter"]), &[0.0])
            .await
            .unwrap();
        assert!(!run.fetched_from_cache);
        assert!(client.calls_to(tools::PRISM_PAGINATE).is_empty());
    }

    #[tokio::test]
    async fn a_cache_that_is_not_a_flow_envelope_is_an_error_not_a_wrong_chart() {
        let client = Arc::new(
            FakeClient::default()
                .with(
                    tools::FLOW_INTO,
                    json!({"_headed": true, "cache_ref": "rc_odd"}),
                )
                .with(
                    tools::PRISM_PAGINATE,
                    json!({"records": [{"something": "else"}], "total_records": 1}),
                ),
        );
        let session = Session::new(client);
        let err = session
            .run_chain(&chain_of(&["signal.filter"]), &[0.0])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not contain a flow envelope"));
    }

    fn list_response() -> Value {
        json!({
            "panels": [
                {"id": "board", "kind": "dashboard", "namespace": "pulse",
                 "props": {"title": "Pulse", "published": true},
                 "field_ids": ["total", "mix"], "data_preview": [], "source_info": null},
                {"id": "total", "kind": "metric", "namespace": "pulse",
                 "props": {"parent": "board", "published": true},
                 "field_ids": [], "data_preview": [["Total", 42]], "source_info": null},
                {"id": "mix", "kind": "bar_chart", "namespace": "pulse",
                 "props": {"parent": "board", "published": true},
                 "field_ids": [], "data_preview": [],
                 "source_info": {"namespace": "pulse", "query": "stage_count(S, N)"}}
            ],
            "total": 3
        })
    }

    #[tokio::test]
    async fn list_panels_resolves_a_dashboard_from_one_call() {
        let client = Arc::new(FakeClient::default().with(tools::SMART_PANEL_LIST, list_response()));
        let session = Session::new(client.clone());

        let panels = session.list_panels().await.unwrap();
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].children.len(), 2);
        assert_eq!(client.calls_to(tools::SMART_PANEL_LIST).len(), 1);
        assert!(client.calls_to(tools::PRISM_PAGINATE).is_empty());
    }

    #[tokio::test]
    async fn an_error_in_a_successful_result_is_a_tool_error_not_an_empty_list() {
        // Not expected in practice — panels are on for every account — but
        // the tool's error-in-result shape must not render as "no panels".
        let client = FakeClient::default().with(
            tools::SMART_PANEL_LIST,
            json!({"error": "smart_panel.list is not enabled for your account.", "code": "feature_disabled"}),
        );
        let err = Session::new(Arc::new(client))
            .list_panels()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not enabled"));
    }

    #[tokio::test]
    async fn a_headed_list_is_fetched_from_the_cache() {
        let client = Arc::new(
            FakeClient::default()
                .with(
                    tools::SMART_PANEL_LIST,
                    json!({"_headed": true, "cache_ref": "rc_list"}),
                )
                .with(
                    tools::PRISM_PAGINATE,
                    // The paginator pages the principal list: records ARE the
                    // panel entries.
                    json!({"records": list_response()["panels"], "total_records": 3}),
                ),
        );
        let session = Session::new(client.clone());
        let panels = session.list_panels().await.unwrap();
        assert_eq!(panels.len(), 1);
        assert_eq!(
            client.calls_to(tools::PRISM_PAGINATE)[0]["cache_ref"],
            json!("rc_list")
        );
    }

    #[tokio::test]
    async fn load_panel_rows_prefers_the_snapshot_and_recurses_into_children() {
        let client = Arc::new(
            FakeClient::default()
                .with(tools::SMART_PANEL_LIST, list_response())
                // `total` has a snapshot; `mix` has none and falls back to its source.
                .with(
                    "panel_data('total', Rows)",
                    json!([{"Rows": [["Total", 99], ["Other", 1]]}]),
                )
                .with("panel_data('mix', Rows)", json!([]))
                .with("stage_count(S, N)", json!([{"S": "Prospecting", "N": 4}])),
        );
        let session = Session::new(client.clone());
        let board = session.list_panels().await.unwrap().remove(0);

        assert_eq!(
            Session::row_load_calls(&board),
            2,
            "two data children, no dashboard rows"
        );

        let loaded = session.load_panel_rows(&board).await.unwrap();
        let total = loaded.children.iter().find(|c| c.id == "total").unwrap();
        assert_eq!(
            total.rows.len(),
            2,
            "the preview's single row was replaced by the snapshot"
        );
        let mix = loaded.children.iter().find(|c| c.id == "mix").unwrap();
        assert_eq!(
            mix.rows.len(),
            1,
            "no snapshot, so the live source supplied rows"
        );
    }

    #[tokio::test]
    async fn a_failed_flow_is_an_error_with_the_step_s_reason_not_empty_results() {
        // The live shape: `execution_error` as one Elixir-inspect string,
        // `execution_result` absent.
        let inspect = "%{\"completed_steps\" => 2, \"error\" => \"Tool call failed: No recognized \
                       parameters\\n  • 'values' — did you mean 'include_values'?\", \
                       \"partial_results\" => %{\"step_1\" => %{}}, \"status\" => \"failed\"}";
        let client = FakeClient::default().with(
            tools::FLOW_INTO,
            json!({"execution_error": inspect, "errors": [], "valid": true}),
        );
        let session = Session::new(Arc::new(client));
        let mut chain = Chain::new();
        chain.push("signal.filter").unwrap();

        let err = session.run_chain(&chain, &[0.0, 1.0]).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("did you mean 'include_values'"), "{text}");
        assert!(
            !text.contains("partial_results"),
            "the inspect wrapper leaked: {text}"
        );
    }

    #[test]
    fn flow_error_messages_prefer_a_structured_error() {
        assert_eq!(
            flow_error_message(&json!({"error": "boom", "completed_steps": 1})),
            "boom"
        );
        assert_eq!(flow_error_message(&json!("plain text")), "plain text");
    }

    #[test]
    fn panel_ids_are_quoted_as_atoms() {
        assert_eq!(atom("stage_mix"), "'stage_mix'");
        assert_eq!(atom("Bob's board"), "'Bob''s board'");
    }

    #[tokio::test]
    async fn record_sends_exactly_one_batch_call() {
        let client = Arc::new(FakeClient::default().with(tools::LOGIC_BATCH, json!({"ok": true})));
        let session = Session::new(client.clone()).with_namespace("bench_test");

        let features = Features::from_samples("capture_1", &[1.0, -1.0], 48000.0);
        session.record(&features).await.unwrap();

        let calls = client.calls_to(tools::LOGIC_BATCH);
        assert_eq!(calls.len(), 1, "per-fact calls would charge per fact");
        assert_eq!(calls[0]["namespace"], json!("bench_test"));
    }

    #[tokio::test]
    async fn minting_without_a_skill_id_is_an_error_not_a_silent_success() {
        let client = FakeClient::default().with(tools::FLOW_INTO, json!({"valid": true}));
        let session = Session::new(Arc::new(client));

        let err = session
            .mint(&chain_of(&["signal.filter"]), &[0.0], "S", "d")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no skill_id"));
    }

    #[tokio::test]
    async fn minting_an_empty_chain_is_refused() {
        let session = Session::new(Arc::new(FakeClient::default()));
        assert!(session.mint(&Chain::new(), &[0.0], "S", "d").await.is_err());
    }

    #[tokio::test]
    async fn mint_returns_the_certificate() {
        let client = FakeClient::default().with(
            tools::FLOW_INTO,
            json!({"skill_id": "skill_x", "ctc": {"id": "ctc_9", "viewer_url": "https://e/9"}}),
        );
        let session = Session::new(Arc::new(client));

        let minted = session
            .mint(&chain_of(&["signal.spectral"]), &[0.0], "Spectrum", "d")
            .await
            .unwrap();

        assert_eq!(minted.skill_id, "skill_x");
        assert_eq!(minted.ctc_url.as_deref(), Some("https://e/9"));
    }

    #[test]
    fn thd_needs_an_identified_harmonic() {
        // Reporting 0.0% would assert a measurement the data cannot support.
        assert_eq!(thd_from_peaks(&[(440.0, 1.0)]), None);
        assert_eq!(thd_from_peaks(&[]), None);
    }

    #[test]
    fn thd_is_harmonic_power_over_the_fundamental() {
        // A 3rd harmonic at 10% of the fundamental → 10% THD.
        let thd = thd_from_peaks(&[(440.0, 1.0), (1320.0, 0.1)]).unwrap();
        assert!((thd - 10.0).abs() < 1e-9, "got {thd}");
    }

    #[test]
    fn thd_ignores_spectral_leakage_next_to_the_fundamental() {
        // A windowed FFT smears a tone into neighbouring bins. Counting that
        // skirt as distortion reported ~85% for a clean two-tone signal.
        let peaks = [
            (468.75, 120.0), // fundamental
            (375.0, 92.8),   // leakage below
            (562.5, 36.4),   // leakage above
        ];
        assert_eq!(
            thd_from_peaks(&peaks),
            None,
            "leakage is not distortion — there is no harmonic here"
        );
    }

    #[test]
    fn harmonic_detection_accepts_near_multiples_only() {
        assert!(is_harmonic_of(880.0, 440.0)); // exact 2nd
        assert!(is_harmonic_of(1320.0, 440.0)); // exact 3rd
        assert!(is_harmonic_of(900.0, 440.0)); // 2.045× — within tolerance
        assert!(!is_harmonic_of(660.0, 440.0)); // 1.5× — not a harmonic
        assert!(!is_harmonic_of(375.0, 468.75)); // below the fundamental
        assert!(!is_harmonic_of(440.0, 440.0)); // itself
    }

    #[test]
    fn chain_run_reads_the_last_spectral_step() {
        let run = ChainRun {
            steps: vec![
                Some(StepResult::Spectral {
                    magnitudes: vec![],
                    freqs: vec![],
                    dominant_hz: Some(100.0),
                    peaks: vec![],
                }),
                Some(StepResult::Spectral {
                    magnitudes: vec![],
                    freqs: vec![],
                    dominant_hz: Some(440.0),
                    peaks: vec![],
                }),
            ],
            ctc_id: None,
            cache_ref: None,
            fetched_from_cache: false,
        };
        // The later step is the more processed one.
        assert_eq!(run.spectral().unwrap().0, Some(440.0));
    }

    #[test]
    fn regime_is_the_largest_cluster() {
        let run = ChainRun {
            steps: vec![Some(StepResult::Cluster {
                k: 3,
                sizes: vec![4, 19, 7],
                inertia: None,
            })],
            ctc_id: None,
            cache_ref: None,
            fetched_from_cache: false,
        };
        assert_eq!(run.regime().as_deref(), Some("regime_1"));
    }

    #[tokio::test]
    async fn a_verdict_names_the_failing_limits() {
        let spec = Spec::new("audio_v1").with(crate::spec::Limit::new(
            "thd_pct",
            crate::spec::Cmp::Lt,
            3.0,
        ));

        // No solution for spec_pass; one violation row comes back.
        let client = FakeClient::default().with(
            &spec.violations_goal("capture_1"),
            json!([{"Metric": "thd_pct"}]),
        );
        let session = Session::new(Arc::new(client));

        let verdict = session.verdict(&spec, "capture_1").await.unwrap();
        assert_eq!(verdict, Verdict::Fail(vec!["thd_pct".into()]));
        assert!(!verdict.passed());
    }
}
