//! The Bench application shell.
//!
//! Panes: **Scope** (live trace), **Meter** (readouts), **Chain** (the DG
//! pipeline), **Panels** (Smart Panels rendered natively).
//!
//! # The async pattern
//!
//! egui redraws synchronously; the gateway is async. So the app owns a Tokio
//! runtime, spawns work onto it, and the work reports back through a channel
//! that `update` drains once per frame. Nothing ever blocks the UI thread —
//! a scope that stutters while waiting on a network call is not a scope.
//!
//! The other half of that discipline is [`Self::analysis_in_flight`]: the tick
//! fires on a timer, so without a guard a slow round trip would let requests
//! pile up faster than they complete.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use bench_core::profile::{self, Instrument, PanelRef, Profile, SourceSpec};
use bench_core::{
    auth::{self, Credentials},
    chain::{Chain, Encoding, StepResult},
    dg::ConduitClient,
    features::Features,
    ring::SampleRing,
    session::{ChainRun, Session},
    source::{
        replay::ReplaySource,
        synth::{Preset, SynthSource, Tone, Wave},
        Source,
    },
    CACHE_FETCH_CREDITS, CAPTURE_SAMPLES, DEFAULT_ANALYSIS_HZ, DEFAULT_RING_CAPACITY,
    FLOW_INTO_CREDITS,
};
use datagrout_panels::Panel;
use datagrout_panels_egui::{render_panel_with_state, FormState, PanelAction};
use egui::{Color32, RichText};

use crate::scope::{draw_minimap, draw_scope, ScopeOutput, ScopeStyle};

/// A step argument that is a position on a chart's axis, drawn as a handle.
struct ArgHandle {
    step: usize,
    arg: String,
    hz: f64,
}

/// How much of the ring the scope shows by default. Local only, so size it
/// for the eye.
const VIEW_SAMPLES: usize = 4096;

/// Time-base choices, in samples on screen. Few samples show the waveform's
/// shape; many show its envelope — the same 440 Hz tone is a clean sine at
/// 256 and a solid band at 16384, which is why a thumbnail of the scope can
/// look more like the signal than the scope itself.
const VIEW_PRESETS: [usize; 7] = [256, 512, 1024, 2048, 4096, 8192, 16384];

/// Analysis-frame choices. A larger frame buys finer spectral bins and costs a
/// larger gateway envelope.
const CAPTURE_PRESETS: [usize; 4] = [512, 1024, 2048, 4096];

/// Text scale. egui's default is sized for dense tooling; an instrument read
/// from arm's length wants more.
const ZOOM: f32 = 1.25;

/// Something finished on the runtime and wants the UI to know.
enum Msg {
    Progress(auth::Progress),
    SignedIn(Box<Credentials>),
    Failed(String),
    /// The saved grant can no longer be refreshed. Distinct from `Failed` so
    /// the UI can drop to Offline with a "sign in again" prompt rather than
    /// showing a transport error and staying nominally connected.
    SignInExpired,
    /// The gateway refused to bind the saved registration; it has been
    /// discarded on disk and the user needs to press Connect once more.
    RegistrationDiscarded(String),
    ChainDone(Box<ChainRun>),
    PanelsLoaded(Vec<Panel>),
    /// Full rows arrived for the panel at `index` in the loaded list.
    PanelRowsLoaded {
        index: usize,
        panel: Box<Panel>,
    },
}

/// What a drag on the scope moves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeDrag {
    /// Started inside the gate: move the analysis frame along the trace.
    Gate,
    /// Started on the gate's left edge: resize the frame, right edge fixed.
    GateLeft,
    /// Started on the gate's right edge: resize the frame, left edge fixed.
    GateRight,
    /// Started outside it: pan the view through the ring's history.
    View,
}

/// Pixels either side of a gate edge that count as grabbing the edge.
const EDGE_GRAB_PX: f32 = 6.0;
/// The smallest analysis frame worth sending anywhere.
const MIN_CAPTURE: usize = 64;
/// The smallest time base: below this the trace is a handful of dots.
const MIN_VIEW: usize = 64;

/// What feeds the ring.
///
/// An enum rather than `Box<dyn Source>` so the Signal pane can reach the
/// generator's knobs and the input's channel and gain without downcasting.
enum ActiveSource {
    Synth(SynthSource),
    Replay(ReplaySource),
    #[cfg(feature = "audio")]
    Audio(bench_core::source::audio::AudioSource),
}

impl ActiveSource {
    fn as_source(&mut self) -> &mut dyn Source {
        match self {
            ActiveSource::Synth(s) => s,
            ActiveSource::Replay(r) => r,
            #[cfg(feature = "audio")]
            ActiveSource::Audio(a) => a,
        }
    }

    fn name(&self) -> String {
        match self {
            ActiveSource::Synth(s) => s.name(),
            ActiveSource::Replay(r) => r.name(),
            #[cfg(feature = "audio")]
            ActiveSource::Audio(a) => a.name(),
        }
    }

    fn sample_rate(&self) -> f64 {
        match self {
            ActiveSource::Synth(s) => s.sample_rate(),
            ActiveSource::Replay(r) => r.sample_rate(),
            #[cfg(feature = "audio")]
            ActiveSource::Audio(a) => a.sample_rate(),
        }
    }

    fn synth_mut(&mut self) -> Option<&mut SynthSource> {
        match self {
            ActiveSource::Synth(s) => Some(s),
            _ => None,
        }
    }

    fn is_synth(&self) -> bool {
        matches!(self, ActiveSource::Synth(_))
    }

    /// Sources Bench clocks itself — the generator and a replay — as opposed
    /// to a device that arrives at its own rate. Only these obey the clock
    /// menu's slow motion.
    fn is_paced(&self) -> bool {
        matches!(self, ActiveSource::Synth(_) | ActiveSource::Replay(_))
    }
}

/// Where the connection stands.
enum Connection {
    Offline,
    /// Waiting for the user to finish consenting in a browser.
    AwaitingConsent {
        url: Option<String>,
    },
    Online(Arc<Session>),
}

pub struct BenchApp {
    runtime: tokio::runtime::Runtime,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    ring: Arc<SampleRing>,
    /// What feeds the ring: the function generator or an audio input.
    source: ActiveSource,
    /// The generator's settings while an audio input is active, so switching
    /// back restores the knobs.
    parked_synth: Option<SynthSource>,
    /// Input devices as last enumerated, for the source menu.
    #[cfg(feature = "audio")]
    audio_devices: Vec<bench_core::source::audio::DeviceInfo>,
    /// Smoothed input peak for the level meter.
    #[cfg(feature = "audio")]
    input_peak: f32,
    chain: Chain,
    /// Samples on screen — the time base.
    view_samples: usize,
    /// Samples per analysis frame — what the chain is run over.
    capture_samples: usize,
    /// How far behind live the right edge of the view sits, in samples.
    /// Zero is the live edge; panning the trace increases it.
    view_back: usize,
    /// How far behind live the right edge of the analysis frame sits. The
    /// gate is a position in history, dragged along the trace; while running
    /// it holds its distance from live, so the signal scrolls under it.
    frame_back: usize,
    /// What a drag on the scope is moving, decided where the drag started.
    scope_drag: Option<ScopeDrag>,
    /// The chart handle being dragged: `(step index, argument name)`.
    arg_drag: Option<(usize, String)>,
    /// Edge trigger. While live and running, the view is aligned so a rising
    /// crossing of the signal's midpoint sits a tenth of the way in, which
    /// holds a periodic waveform still — the way every scope makes a fast
    /// signal readable. Off, the trace rolls.
    trigger: bool,
    /// Where the right edge of the view actually was this frame, behind live.
    /// Equals `view_back` unless the trigger repositioned the view.
    display_back: usize,
    /// Absolute sample index of the edge the sweep on screen starts at. The
    /// trigger re-arms only for edges after this sweep ends; until one is
    /// complete the view holds here, like a scope in normal trigger mode,
    /// instead of rolling. See `bench_core::trigger`.
    trigger_hold: Option<u64>,
    /// Why the last run produced no results, shown over the previous ones.
    last_run_error: Option<String>,
    /// Files are being dragged over the window; show where to drop them.
    files_hovering: bool,
    /// Generator clock as a fraction of real time. Below 1 the signal is
    /// produced in slow motion: same sample rate and bandwidth, fewer samples
    /// per wall-clock second, so a fast waveform can be watched evolving.
    clock_rate: f64,
    /// Per-panel "full rows have been loaded" flags, aligned with `panels`.
    panel_rows_loaded: Vec<bool>,
    connection: Connection,

    /// Latest per-step results, aligned with `chain.steps()`.
    step_results: Vec<Option<StepResult>>,
    /// True once the chain has changed since `step_results` were produced.
    ///
    /// Kept instead of clearing the results: an edited cutoff should show the
    /// old chart dimmed with a "stale" marker, not vanish into "no result",
    /// which reads as a failure rather than as "you have not re-run yet".
    results_stale: bool,
    /// Credits spent on gateway runs this session, from the observed costs.
    credits_spent: f64,
    last_features: Option<Features>,
    last_ctc: Option<String>,

    since_analysis: f64,
    analysis_hz: f64,
    /// Whether the analysis tick fires on its own.
    ///
    /// **Off by default.** Each run costs real credits, and an instrument left
    /// open on a desk would spend them unattended. The local trace and meter
    /// keep working regardless.
    auto_analysis: bool,
    /// A one-shot analysis the user asked for.
    analyse_now: bool,
    /// Gateway runs this session, for the credit estimate.
    runs: u64,
    /// Guard against overlapping runs — see the module docs.
    analysis_in_flight: bool,
    capture_seq: u64,

    running: bool,
    status: String,
    scope_style: ScopeStyle,
    /// A consent URL to hand the OS on the next frame.
    ///
    /// Deferred rather than opened inline because `open_url` is an egui output
    /// that only takes effect from inside a frame.
    pending_open: Option<String>,

    /// Smart Panels loaded from the account via `smart-panels.list`.
    ///
    /// Rendered with the same crate any other host would use; nothing here is
    /// Bench-specific except the loading.
    panels: Vec<Panel>,
    panels_loading: bool,
    /// Per-panel "rows loading" flags, aligned with `panels`.
    panel_rows_loading: Vec<bool>,
    /// Form state for rendered panels, held across frames as immediate mode
    /// requires.
    panel_forms: FormState,
    /// The last interaction a panel reported, shown until acted on.
    last_panel_action: Option<String>,
    /// Whether a credentials file exists on disk. Cached so the top bar can
    /// offer "Forget saved sign-in" without a stat per frame.
    saved_sign_in: bool,
    /// The Smart Panels picker window.
    panel_picker_open: bool,
    /// Panels placed in the dock, by `(namespace, id)` so a reload that
    /// reorders the list does not swap what is on screen.
    placed_panels: Vec<(String, String)>,
    /// The name the Profiles menu will save the current setup under. Set to
    /// the loaded profile's name so save-after-tweak keeps it by default.
    profile_name: String,
}

impl BenchApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial_profile: Option<std::path::PathBuf>,
    ) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        cc.egui_ctx.set_zoom_factor(ZOOM);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            // Two workers is plenty: the gateway calls are IO-bound and the
            // DSP that matters happens on the UI thread from the ring.
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");

        let (tx, rx) = channel();

        let mut app = Self {
            runtime,
            tx,
            rx,
            ring: Arc::new(SampleRing::new(DEFAULT_RING_CAPACITY)),
            source: ActiveSource::Synth(SynthSource::audio(48_000.0)),
            parked_synth: None,
            #[cfg(feature = "audio")]
            audio_devices: bench_core::source::audio::AudioSource::devices(),
            #[cfg(feature = "audio")]
            input_peak: 0.0,
            chain: Chain::new(),
            view_samples: VIEW_SAMPLES,
            capture_samples: CAPTURE_SAMPLES,
            view_back: 0,
            frame_back: 0,
            scope_drag: None,
            arg_drag: None,
            trigger: true,
            display_back: 0,
            trigger_hold: None,
            last_run_error: None,
            files_hovering: false,
            clock_rate: 1.0,
            panel_rows_loaded: Vec::new(),
            connection: Connection::Offline,
            step_results: Vec::new(),
            results_stale: false,
            credits_spent: 0.0,
            last_features: None,
            last_ctc: None,
            since_analysis: 0.0,
            analysis_hz: DEFAULT_ANALYSIS_HZ,
            auto_analysis: false,
            analyse_now: false,
            runs: 0,
            analysis_in_flight: false,
            capture_seq: 0,
            running: true,
            status: "offline".to_string(),
            scope_style: ScopeStyle::default(),
            pending_open: None,
            panels: Vec::new(),
            panels_loading: false,
            panel_rows_loading: Vec::new(),
            panel_forms: FormState::default(),
            last_panel_action: None,
            saved_sign_in: auth::credentials_path().exists(),
            panel_picker_open: false,
            placed_panels: Vec::new(),
            profile_name: String::new(),
        };

        // `dgbench path.bench`: the bench opens already set up. Before the
        // session restore, so a profile's placed panels are known when the
        // account's panels arrive.
        if let Some(path) = initial_profile {
            app.load_profile_path(&path);
        }

        // A saved sign-in reconnects without a browser.
        app.restore_session();
        app
    }

    fn restore_session(&mut self) {
        match auth::load(&auth::credentials_path()) {
            Ok(Some(credentials)) => self.go_online(&credentials),
            Ok(None) => {}
            // Surfaced, not swallowed: silently re-running the browser flow
            // would hide a real problem and mint a duplicate client record.
            Err(e) => self.status = format!("saved sign-in unreadable: {e}"),
        }
    }

    fn go_online(&mut self, credentials: &Credentials) {
        // Persist rotated grants, or the next launch fails with invalid_grant.
        let path = auth::credentials_path();
        let client =
            ConduitClient::connect(&credentials.gateway, credentials.grant.clone()).map(|c| {
                c.with_refresh_handler(move |grant| {
                    if let Err(e) = auth::save_refreshed_grant(&path, grant) {
                        tracing::warn!("could not persist refreshed grant: {e}");
                    }
                })
            });
        match client {
            Ok(client) => {
                let client = Arc::new(client);
                self.status = format!("connected · {}", credentials.gateway);
                self.connection = Connection::Online(Arc::new(Session::new(client.clone())));

                // A saved grant may already be expired. Refresh it now rather
                // than showing "connected" until the first analysis fails —
                // the refresh costs no credits, and a dead sign-in should be
                // reported at launch, not after the user has built a chain.
                let tx = self.tx.clone();
                self.runtime.spawn(async move {
                    if client.grant_is_expired().await {
                        if let Err(bench_core::dg::DgError::SignInExpired) =
                            client.refresh_now().await
                        {
                            let _ = tx.send(Msg::SignInExpired);
                        }
                    }
                });
            }
            Err(e) => {
                self.status = format!("connect failed: {e}");
                self.connection = Connection::Offline;
            }
        }
    }

    fn sign_in(&mut self) {
        self.connection = Connection::AwaitingConsent { url: None };
        self.status = "opening sign-in…".to_string();

        let tx = self.tx.clone();
        let existing = auth::load(&auth::credentials_path())
            .ok()
            .flatten()
            .map(|c| c.client);

        self.runtime.spawn(async move {
            let progress_tx = tx.clone();
            let result = auth::sign_in(auth::DEFAULT_GATEWAY, existing, move |p| {
                let _ = progress_tx.send(Msg::Progress(p));
            })
            .await;

            let _ = match result {
                Ok(credentials) => {
                    // Persist before reporting: a grant that reached the app
                    // but not the disk means the next launch signs in again.
                    if let Err(e) = auth::save(&auth::credentials_path(), &credentials) {
                        tracing::warn!("could not save credentials: {e}");
                    }
                    tx.send(Msg::SignedIn(Box::new(credentials)))
                }
                // The registration on disk cannot be bound to the server the
                // user just chose. Keeping it would make every further Connect
                // fail the same way, and Sign out is not offered while offline,
                // so discard it here and ask for one more click.
                Err(e @ auth::AuthError::RegistrationRejected { .. }) => {
                    if let Err(clear_err) = auth::clear(&auth::credentials_path()) {
                        tracing::warn!("could not discard rejected registration: {clear_err}");
                    }
                    tx.send(Msg::RegistrationDiscarded(e.to_string()))
                }
                Err(e) => tx.send(Msg::Failed(e.to_string())),
            };
        });
    }

    /// Drop the saved sign-in without being online — the escape hatch when a
    /// stored registration or grant has gone bad server-side.
    fn forget_saved_sign_in(&mut self) {
        let _ = auth::clear(&auth::credentials_path());
        self.saved_sign_in = false;
        self.status = "saved sign-in forgotten; Connect will register afresh".to_string();
    }

    fn sign_out(&mut self) {
        let _ = auth::clear(&auth::credentials_path());
        self.saved_sign_in = false;
        self.connection = Connection::Offline;
        self.step_results.clear();
        // Panels belong to the account that was signed in.
        self.panels.clear();
        self.panel_rows_loading.clear();
        self.panels_loading = false;
        self.status = "signed out".to_string();
    }

    /// Fetch the account's Smart Panels — one `smart-panels.list` call.
    fn load_panels(&mut self, ctx: &egui::Context) {
        let Connection::Online(session) = &self.connection else {
            return;
        };
        if self.panels_loading {
            return;
        }
        self.panels_loading = true;

        let session = Arc::clone(session);
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let msg = match session.list_panels().await {
                Ok(panels) => Msg::PanelsLoaded(panels),
                Err(bench_core::dg::DgError::SignInExpired) => Msg::SignInExpired,
                Err(e) => Msg::Failed(e.to_string()),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Fill in full rows for one loaded panel and everything under it.
    fn load_panel_rows(&mut self, index: usize, ctx: &egui::Context) {
        let Connection::Online(session) = &self.connection else {
            return;
        };
        let Some(panel) = self.panels.get(index).cloned() else {
            return;
        };
        if self.panel_rows_loading.get(index).copied().unwrap_or(false) {
            return;
        }
        if let Some(flag) = self.panel_rows_loading.get_mut(index) {
            *flag = true;
        }

        let session = Arc::clone(session);
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let msg = match session.load_panel_rows(&panel).await {
                Ok(panel) => Msg::PanelRowsLoaded {
                    index,
                    panel: Box::new(panel),
                },
                Err(bench_core::dg::DgError::SignInExpired) => Msg::SignInExpired,
                Err(e) => Msg::Failed(e.to_string()),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Drain everything the runtime reported since the last frame.
    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Progress(p) => {
                    self.status = match &p {
                        auth::Progress::Discovering => "discovering the authorization server…",
                        auth::Progress::Registering => "registering this app…",
                        auth::Progress::ReusingClient => "reusing the saved registration…",
                        auth::Progress::AwaitingConsent(_) => "waiting for browser consent…",
                        auth::Progress::Exchanging => "redeeming the authorization code…",
                    }
                    .to_string();

                    if let auth::Progress::AwaitingConsent(url) = p {
                        // Open the browser rather than making the user click:
                        // they already asked to sign in, and the extra step
                        // reads as the flow having stalled.
                        self.pending_open = Some(url.clone());
                        self.connection = Connection::AwaitingConsent { url: Some(url) };
                    }
                }
                Msg::SignedIn(credentials) => {
                    self.saved_sign_in = true;
                    self.go_online(&credentials);
                }
                Msg::RegistrationDiscarded(message) => {
                    self.saved_sign_in = false;
                    self.connection = Connection::Offline;
                    self.status = message;
                }
                Msg::PanelsLoaded(panels) => {
                    self.panels_loading = false;
                    self.panel_rows_loading = vec![false; panels.len()];
                    self.panel_rows_loaded = vec![false; panels.len()];
                    self.status = format!(
                        "loaded {} panel{}",
                        panels.len(),
                        if panels.len() == 1 { "" } else { "s" }
                    );
                    self.panels = panels;
                    // A placed panel that no longer exists on the account
                    // leaves the dock rather than rendering a stale copy.
                    self.placed_panels.retain(|key| {
                        self.panels
                            .iter()
                            .any(|p| p.namespace == key.0 && p.id == key.1)
                    });
                }
                Msg::PanelRowsLoaded { index, panel } => {
                    if let Some(flag) = self.panel_rows_loading.get_mut(index) {
                        *flag = false;
                    }
                    if let Some(flag) = self.panel_rows_loaded.get_mut(index) {
                        *flag = true;
                    }
                    if let Some(slot) = self.panels.get_mut(index) {
                        *slot = *panel;
                    }
                }
                Msg::Failed(e) => {
                    // A failed run keeps the previous results on screen and
                    // says why in their header; blanking them reads as "the
                    // signal is gone" rather than "the gateway refused".
                    if self.analysis_in_flight {
                        self.last_run_error = Some(e.clone());
                    }
                    self.analysis_in_flight = false;
                    self.panels_loading = false;
                    self.panel_rows_loading.iter_mut().for_each(|f| *f = false);
                    // A failed sign-in must not leave the consent controls up:
                    // the pending authorization is spent, so "Open sign-in
                    // page" would replay a dead URL.
                    if matches!(self.connection, Connection::AwaitingConsent { .. }) {
                        self.connection = Connection::Offline;
                    }
                    self.status = e;
                }
                Msg::SignInExpired => {
                    self.analysis_in_flight = false;
                    self.panels_loading = false;
                    self.panel_rows_loading.iter_mut().for_each(|f| *f = false);
                    self.auto_analysis = false;
                    // The registration on disk is still good and sign_in will
                    // reuse it; only the grant is dead. Nothing is deleted.
                    self.connection = Connection::Offline;
                    self.status = "sign-in expired — press Connect to sign in again".to_string();
                }
                Msg::ChainDone(run) => {
                    self.analysis_in_flight = false;
                    self.runs += 1;
                    self.credits_spent += FLOW_INTO_CREDITS
                        + if run.fetched_from_cache {
                            CACHE_FETCH_CREDITS
                        } else {
                            0.0
                        };
                    self.last_ctc = run.ctc_id.clone();
                    self.last_run_error = None;
                    self.fold_in(&run);
                    self.step_results = run.steps;
                    self.results_stale = false;
                }
            }
        }
    }

    /// Merge what the chain measured into the running feature set.
    fn fold_in(&mut self, run: &ChainRun) {
        let Some(features) = self.last_features.take() else {
            return;
        };
        let (dominant, thd) = run.spectral().unwrap_or((None, None));
        let mut features = features.with_spectral(dominant, thd);
        if let Some(regime) = run.regime() {
            features = features.with_regime(regime);
        }
        self.last_features = Some(features);
    }

    /// Pull the source forward by roughly one frame's worth of samples.
    fn pump(&mut self, dt: f64) {
        if !self.running {
            return;
        }
        // Slow motion is fewer samples per wall-clock second at the same
        // sample rate; sub-sample remainders are dropped, which at 1/1000
        // still yields 48 samples a second.
        let wanted = if self.source.is_paced() {
            let n = (self.source.sample_rate() * dt * self.clock_rate).round() as usize;
            // Cap the catch-up: a stalled window must not dump a multi-second
            // burst into the ring the moment it regains focus.
            n.clamp(0, self.view_samples.max(VIEW_SAMPLES) * 2)
        } else {
            // A real input runs at its own clock; take everything it has so
            // the trace does not lag further behind every frame.
            self.ring.capacity()
        };
        if wanted > 0 {
            bench_core::source::pump(self.source.as_source(), &self.ring, wanted);
        }
    }

    /// How far behind live the analysis frame's right edge is, as drawn.
    ///
    /// The gate is positioned relative to the view, so when the trigger has
    /// moved the view the gate moves with it.
    fn gate_back(&self) -> usize {
        self.display_back + (self.frame_back - self.view_back)
    }

    /// The trigger-aligned position for the live view: how far behind live
    /// the view's right edge should sit, or `None` to roll.
    ///
    /// The sweep on screen is held in `trigger_hold` (the edge's absolute
    /// sample index) and the trigger re-arms only for edges after that sweep
    /// ends — see `bench_core::trigger` for why "the newest edge every frame"
    /// rolls on any signal with dense crossings. A held edge that has scrolled
    /// out of the ring is dropped and the trigger acquires afresh near live.
    fn trigger_reference(&mut self) -> Option<usize> {
        let view = self.view_samples;
        let pre = view / 10;
        let post = view - pre;
        let written = self.ring.written();
        let available = self.ring.available();
        // Look back at least half a second: a 2 Hz component has no edge
        // inside a 4096-sample view, and a search that short would roll.
        let search = (view * 3).max(self.source.sample_rate() as usize / 2);
        let buf = self.ring.latest(search);
        if buf.len() < view {
            self.trigger_hold = None;
            return None;
        }
        let base = written - buf.len() as u64;

        let held = self
            .trigger_hold
            .filter(|abs| written.saturating_sub(*abs) <= available as u64);
        if held.is_none() {
            self.trigger_hold = None;
        }
        let arm_from = held.map(|abs| abs + post as u64);
        if let Some(edge) = bench_core::trigger::select_edge(&buf, base, pre, post, arm_from) {
            self.trigger_hold = Some(edge);
        }

        let abs = self.trigger_hold?;
        let back = written.checked_sub(abs + post as u64)? as usize;
        (back + view <= available).then_some(back)
    }

    /// Anything in flight that the user is waiting on.
    fn busy_with(&self) -> Option<&'static str> {
        if self.analysis_in_flight {
            Some("running the chain on DataGrout…")
        } else if self.panels_loading {
            Some("loading panels…")
        } else if self.panel_rows_loading.iter().any(|f| *f) {
            Some("loading panel rows…")
        } else {
            None
        }
    }

    /// The analysis tick: measure locally, then hand the frame to DG.
    fn analyse(&mut self, ctx: &egui::Context) {
        // Exactly the shaded gate on the scope: the same window in history,
        // frozen, live or trigger-aligned.
        let samples = self.ring.window(self.gate_back(), self.capture_samples);
        if samples.is_empty() {
            return;
        }

        self.capture_seq += 1;
        let id = format!("capture_{:04}", self.capture_seq);
        let previous = self.last_features.as_ref().map(|f| f.capture_id.clone());

        // Local measurements every tick, gateway or not: RMS, pk-pk and DC are
        // cheap and are never worth a network call.
        let mut features = Features::from_samples(id, &samples, self.source.sample_rate());
        if let Some(prev) = previous {
            features = features.following(prev);
        }
        self.last_features = Some(features);

        let Connection::Online(session) = &self.connection else {
            return;
        };
        if self.chain.is_empty() || self.analysis_in_flight {
            return;
        }

        self.analysis_in_flight = true;
        let session = Arc::clone(session);
        let chain = self.chain.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();

        self.runtime.spawn(async move {
            let msg = match session.run_chain(&chain, &samples).await {
                Ok(run) => Msg::ChainDone(Box::new(run)),
                Err(bench_core::dg::DgError::SignInExpired) => Msg::SignInExpired,
                Err(e) => Msg::Failed(e.to_string()),
            };
            let _ = tx.send(msg);
            // The result arrived off-thread; ask for a repaint so it is not
            // sitting in the channel until the next mouse move.
            ctx.request_repaint();
        });
    }
}

impl eframe::App for BenchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_messages();

        if let Some(url) = self.pending_open.take() {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }

        // A file dropped on the window: a `.bench` profile sets the bench up,
        // a WAV or CSV becomes the source.
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if let Some(path) = dropped.into_iter().next() {
            if profile::is_profile_path(&path) {
                self.load_profile_path(&path);
            } else {
                self.load_replay(&path);
            }
        }
        self.files_hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());

        let dt = ctx.input(|i| i.stable_dt).min(0.1) as f64;
        self.pump(dt);

        self.since_analysis += dt;
        let due = self.auto_analysis && self.since_analysis >= 1.0 / self.analysis_hz;
        if due || self.analyse_now {
            self.since_analysis = 0.0;
            self.analyse_now = false;
            self.analyse(ctx);
        }

        // Bench-wide keys, only when no text field has the keyboard: Space
        // runs/pauses, Enter analyses, Home returns to live. The same three
        // things a hand reaches for on a real instrument.
        if !ctx.wants_keyboard_input() {
            let (space, enter, home) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::Space),
                    i.key_pressed(egui::Key::Enter),
                    i.key_pressed(egui::Key::Home),
                )
            });
            if space {
                self.running = !self.running;
            }
            if enter && matches!(self.connection, Connection::Online(_)) && !self.chain.is_empty() {
                self.analyse_now = true;
            }
            if home {
                self.view_back = 0;
                self.frame_back = 0;
            }
        }

        self.top_bar(ctx);

        egui::SidePanel::right("bench_side")
            .default_width(340.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("bench_side_scroll")
                    .show(ui, |ui| {
                        self.signal_pane(ui);
                        ui.separator();
                        self.chain_pane(ui);
                        ui.separator();
                        self.meter_pane(ui);
                    });
            });

        // Placed panels take the strip under the scope, at the scope's width.
        self.panel_dock(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            self.clamp_offsets();
            // Triggered and running, the edge is the reference and `view_back`
            // is the horizontal-position offset from it — so a drag moves the
            // held waveform along instead of switching the display to rolling.
            // Untriggered (or nothing to trigger on), the offset is from live.
            let available = self.ring.available();
            let reference = if self.trigger && self.running {
                self.trigger_reference()
            } else {
                self.trigger_hold = None;
                None
            };
            self.display_back = match reference {
                Some(back) => {
                    (back + self.view_back).min(available.saturating_sub(self.view_samples))
                }
                None => self.view_back,
            };
            let samples = self.ring.window(self.display_back, self.view_samples);
            let shown = samples.len();
            // The gate in view coordinates: its right edge is `frame_back -
            // view_back` samples in from the right of the view.
            let end = shown.saturating_sub(self.frame_back - self.view_back);
            let start = end.saturating_sub(self.capture_samples);
            self.scope_style.gate = Some((start, end - start));
            let out = draw_scope(ui, &samples, &self.scope_style);
            if self.files_hovering {
                ui.painter().rect_filled(
                    out.rect,
                    4.0_f32,
                    Color32::from_rgba_unmultiplied(125, 211, 252, 40),
                );
                ui.painter().text(
                    out.rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "drop to replay this file",
                    egui::FontId::proportional(18.0),
                    Color32::from_rgb(226, 232, 240),
                );
            }
            self.handle_scope_input(ui, &out, &samples);
            self.minimap(ui, shown);
            self.scope_controls(ui, out.lo, out.hi, shown);

            ui.separator();
            // Work in flight dims what it is about to replace, with a spinner
            // where the new results will land.
            let busy = self.analysis_in_flight;
            ui.scope(|ui| {
                if busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("running the chain on DataGrout…").small());
                    });
                    ui.set_opacity(0.45);
                }
                egui::ScrollArea::vertical()
                    .id_salt("results_scroll")
                    .show(ui, |ui| self.results_pane(ui));
            });
        });

        self.panel_picker(ctx);

        if self.running {
            ctx.request_repaint();
        } else {
            // Paused is not idle: results, panel rows and sign-in progress
            // still arrive off-thread, and egui only repaints on input unless
            // asked. A slow heartbeat keeps the screen honest without burning
            // a core on a frozen trace.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

impl BenchApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("bench_top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Bench").heading().strong());
                ui.label(RichText::new(self.source.name()).weak().small());
                ui.separator();

                if ui
                    .button(if self.running { "⏸ Pause" } else { "▶ Run" })
                    .clicked()
                {
                    self.running = !self.running;
                }

                ui.separator();
                self.analysis_controls(ui);

                // One global "something is happening" signal, whatever it is.
                if let Some(what) = self.busy_with() {
                    ui.separator();
                    ui.spinner();
                    ui.label(
                        RichText::new(what)
                            .small()
                            .color(Color32::from_rgb(250, 204, 21)),
                    );
                }

                ui.separator();
                let placed = self.placed_panels.len();
                let label = if placed == 0 {
                    "Panels…".to_string()
                } else {
                    format!("Panels… ({placed} placed)")
                };
                if ui
                    .selectable_label(self.panel_picker_open, label)
                    .on_hover_text("browse the account's Smart Panels and place them in the dock")
                    .clicked()
                {
                    self.panel_picker_open = !self.panel_picker_open;
                }
                ui.separator();
                self.profiles_menu(ui);
                ui.separator();

                match &self.connection {
                    Connection::Offline => {
                        if ui.button("Connect to DataGrout").clicked() {
                            self.sign_in();
                        }
                        if self.saved_sign_in
                            && ui
                                .small_button("Forget saved sign-in")
                                .on_hover_text(
                                    "discard the stored registration and grant; \
                                     the next Connect registers a fresh client",
                                )
                                .clicked()
                        {
                            self.forget_saved_sign_in();
                        }
                    }
                    Connection::AwaitingConsent { url } => {
                        if let Some(url) = url.clone() {
                            // egui turns this into an open-url request that
                            // eframe hands to the system browser.
                            ui.hyperlink_to("Open sign-in page", &url);
                            if ui.small_button("Copy link").clicked() {
                                ui.ctx().copy_text(url);
                            }
                        } else {
                            ui.spinner();
                        }
                        if ui.small_button("Cancel").clicked() {
                            self.connection = Connection::Offline;
                            self.status = "sign-in cancelled".into();
                        }
                    }
                    Connection::Online(_) => {
                        if ui.button("Sign out").clicked() {
                            self.sign_out();
                        }
                    }
                }
            });

            // Status on its own row: a gateway error can be a few hundred
            // characters, and sharing a row with buttons made it overrun them.
            let colour = match self.connection {
                Connection::Online(_) => Color32::from_rgb(74, 222, 128),
                _ => Color32::from_rgb(250, 204, 21),
            };
            ui.add(egui::Label::new(RichText::new(&self.status).small().color(colour)).truncate())
                .on_hover_text(&self.status);
        });
    }

    /// The gateway-run controls. In the top bar, not the side pane: these are
    /// the one thing that costs money and the one thing a first-time user has
    /// to find, so they sit beside Run rather than below the fold.
    fn analysis_controls(&mut self, ui: &mut egui::Ui) {
        let online = matches!(self.connection, Connection::Online(_));
        let can_run = online && !self.chain.is_empty() && !self.analysis_in_flight;

        let label = if self.analysis_in_flight {
            "Analysing…"
        } else if self.results_stale {
            "Re-analyse"
        } else {
            "Analyse"
        };
        let button = egui::Button::new(RichText::new(label).strong());
        let button = if self.results_stale && can_run {
            button.fill(Color32::from_rgb(120, 80, 20))
        } else {
            button
        };
        // Fixed width: the label alternates between "Analyse" and "Analysing…"
        // once a second under auto, and a button that changes width drags the
        // controls beside it around, so the checkbox could not be hit.
        let size = egui::vec2(96.0, ui.spacing().interact_size.y);
        if ui
            .add_enabled_ui(can_run, |ui| ui.add_sized(size, button))
            .inner
            .clicked()
        {
            self.analyse_now = true;
        }

        ui.add_enabled(online, egui::Checkbox::new(&mut self.auto_analysis, "auto"));
        if self.auto_analysis {
            ui.add(
                egui::DragValue::new(&mut self.analysis_hz)
                    .speed(0.1)
                    .range(0.1..=4.0)
                    .suffix(" Hz"),
            );
            ui.label(
                RichText::new(format!(
                    "≈{:.0} credits/min",
                    self.analysis_hz * 60.0 * (FLOW_INTO_CREDITS + CACHE_FETCH_CREDITS)
                ))
                .small()
                .color(Color32::from_rgb(250, 204, 21)),
            );
        }
        if self.runs > 0 {
            ui.label(
                RichText::new(format!(
                    "{} run{} · ≈{:.0} credits",
                    self.runs,
                    if self.runs == 1 { "" } else { "s" },
                    self.credits_spent
                ))
                .weak()
                .small(),
            );
        }
    }

    fn chain_pane(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Chain").strong());
        // The TOOLS are zero-credit; the flow.into call that carries them is
        // not. Saying only the first half would be misleading.
        ui.label(
            RichText::new(format!(
                "deterministic tools · ~{:.0}–{:.0} credits per run",
                FLOW_INTO_CREDITS,
                FLOW_INTO_CREDITS + CACHE_FETCH_CREDITS
            ))
            .weak()
            .small(),
        );
        ui.add_space(4.0);

        let mut remove = None;
        let mut edits: Vec<(usize, String, String)> = Vec::new();

        // Cloned so the editors below can borrow `self.chain` mutably without
        // holding an iterator over it.
        let steps = self.chain.steps().to_vec();

        for (i, step) in steps.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).weak().small());
                ui.label(RichText::new(&step.key).monospace().small().strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").clicked() {
                        remove = Some(i);
                    }
                });
            });

            let Some(def) = bench_core::chain::step_def(&step.key) else {
                continue;
            };

            // Argument editors. Without these the chain is unusable: you
            // cannot set a filter cutoff, a spectral sample rate, or k.
            egui::Grid::new(format!("args_{i}"))
                .num_columns(2)
                .spacing([8.0, 2.0])
                .show(ui, |ui| {
                    for (name, _) in def.args {
                        let current = step.args.get(*name).cloned().unwrap_or_default();
                        let mut buffer = current.clone();

                        ui.label(RichText::new(*name).weak().small());
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut buffer)
                                .desired_width(96.0)
                                .font(egui::TextStyle::Monospace),
                        );
                        // Commit on focus loss or Enter, not per keystroke: a
                        // half-typed number would fire an analysis tick with a
                        // value the user never meant.
                        let committed = response.lost_focus()
                            || response.ctx.input(|i| i.key_pressed(egui::Key::Enter));
                        if committed && buffer != current {
                            edits.push((i, (*name).to_string(), buffer));
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
        }

        for (index, name, value) in edits {
            self.chain.set_arg(index, &name, value);
            self.results_stale = true;
        }

        if let Some(i) = remove {
            self.chain.remove(i);
            // The results no longer line up with the steps; nothing sensible
            // to keep.
            self.step_results.clear();
            self.results_stale = false;
        }

        // Only shape-compatible steps are offered, so an invalid chain cannot
        // be built (bench_core::chain).
        let available = self.chain.available_steps();
        if available.is_empty() && !self.chain.is_empty() {
            ui.label(
                RichText::new("last step is terminal — nothing chains after it")
                    .weak()
                    .small()
                    .italics(),
            );
        } else {
            egui::ComboBox::from_id_salt("add_step")
                .selected_text("+ add a step…")
                .show_ui(ui, |ui| {
                    for key in available {
                        if ui.selectable_label(false, key).clicked() {
                            let _ = self.chain.push(key);
                            // Existing results are still valid for the steps
                            // they belong to; the new step simply has none yet.
                            self.results_stale = true;
                        }
                    }
                });
        }

        if !self.chain.is_empty() {
            ui.add_space(4.0);
            ui.collapsing("the agent's call", |ui| {
                let samples = self.ring.window(self.gate_back(), self.capture_samples);
                let call = self.chain.to_mcp_call(&samples, Encoding::Base64F32);
                let text = serde_json::to_string_pretty(&call).unwrap_or_default();
                ui.add(
                    egui::TextEdit::multiline(&mut text.as_str())
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(10),
                );
            });
        }
    }

    /// Per-step results from the last run.
    fn results_pane(&mut self, ui: &mut egui::Ui) {
        if self.chain.is_empty() {
            ui.label(
                RichText::new("add a chain step to analyse the signal")
                    .weak()
                    .italics()
                    .small(),
            );
            return;
        }

        if self.step_results.is_empty() {
            let message = match self.connection {
                Connection::Online(_) => {
                    "press Analyse (top bar) to run the chain against DataGrout"
                }
                _ => "connect to DataGrout to run the chain",
            };
            ui.label(RichText::new(message).weak().italics().small());
            return;
        }

        if let Some(error) = &self.last_run_error {
            ui.label(
                RichText::new(format!("last run failed — {error}"))
                    .small()
                    .color(Color32::from_rgb(248, 113, 113)),
            );
            ui.label(
                RichText::new("the results below are from the run before it")
                    .weak()
                    .small()
                    .italics(),
            );
        }
        if self.results_stale {
            ui.label(
                RichText::new("chain changed since these results — press Re-analyse")
                    .small()
                    .color(Color32::from_rgb(250, 204, 21)),
            );
        }
        // Stale results stay visible but recede, so the eye reads them as
        // history rather than as the current state of the signal.
        let alpha = if self.results_stale { 0.45 } else { 1.0 };
        ui.set_opacity(alpha);

        // Filter cutoffs are positions on the frequency axis, so they are
        // drawn on the spectrum as handles and set by dragging. Collected up
        // front because the loop below borrows the chain.
        let fs = self.source.sample_rate();
        let handles: Vec<ArgHandle> = self
            .chain
            .steps()
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s.key.as_str(), "signal.filter" | "signal.iir"))
            .flat_map(|(i, s)| {
                let kind = s.args.get("type").map(String::as_str).unwrap_or("lowpass");
                // The FIR takes a band as `low`/`high`; the biquad takes a
                // centre `cutoff` and a `q`, so it always has one handle.
                let names: &[&str] =
                    if s.key == "signal.filter" && matches!(kind, "bandpass" | "bandstop") {
                        &["low", "high"]
                    } else {
                        &["cutoff"]
                    };
                names
                    .iter()
                    .filter_map(|name| {
                        let norm: f64 = s.args.get(*name)?.trim().parse().ok()?;
                        Some(ArgHandle {
                            step: i,
                            arg: (*name).to_string(),
                            hz: norm * fs,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut pending_arg: Option<(usize, String, String)> = None;

        for (i, (step, result)) in self
            .chain
            .steps()
            .iter()
            .zip(self.step_results.iter())
            .enumerate()
        {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).weak().small());
                ui.label(RichText::new(&step.key).monospace().small().strong());
            });

            match result {
                // A step appended after the last run has no result yet; a step
                // the gateway returned nothing for is a different situation.
                None if self.results_stale && i >= self.step_results.len() => {
                    ui.label(RichText::new("not run yet").weak().italics().small());
                }
                None => {
                    ui.label(RichText::new("no result").weak().italics().small());
                }
                // The tool refused this step. Say why, in the step's own slot,
                // instead of drawing an empty trace labelled "0 values".
                Some(StepResult::Error(_)) => {
                    ui.label(
                        RichText::new(summarise(result.as_ref().expect("matched Some")))
                            .small()
                            .color(Color32::from_rgb(248, 113, 113)),
                    );
                }
                Some(result) => {
                    if let Some(series) = result.series() {
                        let values: Vec<f32> = series.iter().map(|v| *v as f32).collect();
                        let style = ScopeStyle {
                            height: 84.0,
                            ..Default::default()
                        };
                        let out = draw_scope(ui, &values, &style);
                        if let StepResult::Spectral { freqs, .. } = result {
                            let max_hz = freqs
                                .last()
                                .copied()
                                .filter(|f| *f > 0.0)
                                .unwrap_or(fs / 2.0);
                            if let Some(change) =
                                draw_arg_handles(ui, &out, max_hz, fs, &handles, &mut self.arg_drag)
                            {
                                pending_arg = Some(change);
                            }
                            ui.label(
                                RichText::new(format!(
                                    "0 … {}{}",
                                    hz_label(max_hz),
                                    if handles.is_empty() {
                                        ""
                                    } else {
                                        " · drag a cutoff line to set the filter"
                                    }
                                ))
                                .weak()
                                .small(),
                            );
                        }
                    }
                    ui.label(RichText::new(summarise(result)).weak().small());
                }
            }
            ui.add_space(6.0);
        }

        if let Some((step, arg, value)) = pending_arg {
            self.chain.set_arg(step, &arg, value);
            self.results_stale = true;
        }

        if let Some(ctc) = &self.last_ctc {
            ui.label(RichText::new(format!("certificate {ctc}")).weak().small());
        }
    }

    /// The function generator's controls.
    ///
    /// Every knob changes the source in place, so the trace answers on the
    /// next frame and the analysis frame follows. The synth is the
    /// instrument's known-truth input: the way to learn what a chain does is
    /// to run it over a signal whose answer you already know, then turn a knob.
    fn signal_pane(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Signal").strong())
            .default_open(true)
            .show(ui, |ui| {
                self.source_picker(ui);

                let fs = self.source.sample_rate();
                let nyquist = fs / 2.0;
                ui.label(
                    RichText::new(format!(
                        "{:.0} Hz sample rate · {:.0} Hz bandwidth",
                        fs, nyquist
                    ))
                    .weak()
                    .small(),
                );

                // A source only affects samples it has not produced yet. Say
                // so, or a knob turned while parked in history looks dead.
                let hint = if !self.running {
                    Some("paused — changes show once the scope is running")
                } else if self.view_back > 0 {
                    Some("viewing history — changes appear at the live edge")
                } else {
                    None
                };
                if let Some(hint) = hint {
                    ui.label(
                        RichText::new(hint)
                            .small()
                            .color(Color32::from_rgb(250, 204, 21)),
                    );
                }

                match &self.source {
                    ActiveSource::Synth(_) => self.synth_controls(ui, nyquist),
                    ActiveSource::Replay(_) => self.replay_controls(ui),
                    #[cfg(feature = "audio")]
                    ActiveSource::Audio(_) => self.audio_controls(ui),
                }
            });
    }

    /// Choose what feeds the ring: the generator, or one of the input devices.
    fn source_picker(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("source").weak().small());
            egui::ComboBox::from_id_salt("source_pick")
                .selected_text(self.source.name())
                .width(220.0)
                .show_ui(ui, |ui| {
                    let is_synth = self.source.is_synth();
                    if ui
                        .selectable_label(is_synth, "function generator")
                        .clicked()
                        && !is_synth
                    {
                        self.select_synth();
                    }
                    if let ActiveSource::Replay(r) = &self.source {
                        let _ = ui.selectable_label(true, format!("replay: {}", r.file_name()));
                    }
                    ui.label(
                        RichText::new("drop a .wav or .csv on the window to replay it")
                            .weak()
                            .small(),
                    );
                    #[cfg(feature = "audio")]
                    {
                        for dev in self.audio_devices.clone() {
                            let label = format!(
                                "audio: {}{}",
                                dev.name,
                                if dev.is_default { " (default)" } else { "" }
                            );
                            let selected = matches!(
                                &self.source,
                                ActiveSource::Audio(a) if a.device() == dev.name
                            );
                            if ui.selectable_label(selected, label).clicked() && !selected {
                                self.select_audio(&dev.name);
                            }
                        }
                        if self.audio_devices.is_empty() {
                            ui.label(RichText::new("no input devices found").weak().small());
                        }
                    }
                    #[cfg(not(feature = "audio"))]
                    ui.label(
                        RichText::new("build with --features audio for input devices")
                            .weak()
                            .small(),
                    );
                });
            #[cfg(feature = "audio")]
            if ui
                .small_button("rescan")
                .on_hover_text("look for input devices again")
                .clicked()
            {
                self.audio_devices = bench_core::source::audio::AudioSource::devices();
            }
        });
    }

    fn select_synth(&mut self) {
        let synth = self
            .parked_synth
            .take()
            .unwrap_or_else(|| SynthSource::audio(48_000.0));
        self.source = ActiveSource::Synth(synth);
        self.after_source_switch();
    }

    /// Replace the source with a recording. Anything Bench saved, or any WAV
    /// or CSV, becomes the live signal.
    fn load_replay(&mut self, path: &std::path::Path) {
        match ReplaySource::from_path(path) {
            Ok(replay) => {
                let previous = std::mem::replace(&mut self.source, ActiveSource::Replay(replay));
                if let ActiveSource::Synth(synth) = previous {
                    self.parked_synth = Some(synth);
                }
                self.after_source_switch();
                // Start from the top, and let the whole file be seen at once.
                self.running = true;
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    // ── Profiles ─────────────────────────────────────────────────────────

    /// The Profiles menu: the shipped examples, whatever is saved under
    /// `~/Documents/Bench/profiles`, and "save the current setup as…".
    fn profiles_menu(&mut self, ui: &mut egui::Ui) {
        let mut load: Option<Profile> = None;
        let mut load_path: Option<std::path::PathBuf> = None;
        let mut save = false;

        ui.menu_button("Profiles…", |ui| {
            ui.label(RichText::new("examples").weak().small());
            for p in Profile::builtins() {
                if ui
                    .button(p.name.as_str())
                    .on_hover_text(p.description.as_str())
                    .clicked()
                {
                    load = Some(p);
                    ui.close_menu();
                }
            }

            let saved = profile::list_dir(&profile::default_dir());
            if !saved.is_empty() {
                ui.separator();
                ui.label(RichText::new("saved").weak().small());
                for path in saved {
                    let stem = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if ui
                        .button(stem.as_str())
                        .on_hover_text(path.display().to_string())
                        .clicked()
                    {
                        load_path = Some(path);
                        ui.close_menu();
                    }
                }
            }

            ui.separator();
            ui.label(RichText::new("save the current setup").weak().small());
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.profile_name)
                        .desired_width(150.0)
                        .hint_text("name"),
                );
                let named = !self.profile_name.trim().is_empty();
                if ui.add_enabled(named, egui::Button::new("Save")).clicked() {
                    save = true;
                    ui.close_menu();
                }
            });
            ui.label(
                RichText::new(format!("to {}", profile::default_dir().display()))
                    .weak()
                    .small(),
            );
            ui.label(
                RichText::new("or drop a .bench file on the window to load one")
                    .weak()
                    .small(),
            );
        });

        if let Some(p) = load {
            self.apply_profile(&p);
        }
        if let Some(path) = load_path {
            self.load_profile_path(&path);
        }
        if save {
            self.save_profile();
        }
    }

    fn load_profile_path(&mut self, path: &std::path::Path) {
        match Profile::load(path) {
            Ok(p) => self.apply_profile(&p),
            Err(e) => self.status = e.to_string(),
        }
    }

    /// Put the bench in the state a profile describes.
    ///
    /// The chain is rebuilt first, through the same shape checks the chain
    /// pane makes, so a bad file leaves the bench exactly as it was rather
    /// than half-applied. Then the source, which owns the sample rate the
    /// rest is sized against; then the knobs. Auto-analysis stays off
    /// whatever the file says: a profile that starts spending credits on
    /// open is the one thing this must not do.
    fn apply_profile(&mut self, p: &Profile) {
        let chain = match Chain::from_steps(&p.chain) {
            Ok(chain) => chain,
            Err(e) => {
                self.status = format!("profile {}: {e}", p.name);
                return;
            }
        };

        match &p.source {
            SourceSpec::Synth {
                sample_rate,
                tones,
                noise,
            } => {
                let mut synth = SynthSource::new(*sample_rate);
                synth.tones = tones.clone();
                synth.noise = *noise;
                self.source = ActiveSource::Synth(synth);
                self.parked_synth = None;
                self.after_source_switch();
            }
            SourceSpec::Replay { path, looping } => {
                self.load_replay(path);
                if let ActiveSource::Replay(replay) = &mut self.source {
                    replay.looping = *looping;
                }
            }
            SourceSpec::Audio {
                device,
                channel,
                gain,
            } => {
                #[cfg(feature = "audio")]
                {
                    let name = device.clone().or_else(|| {
                        self.audio_devices
                            .iter()
                            .find(|d| d.is_default)
                            .map(|d| d.name.clone())
                    });
                    match name {
                        Some(name) => {
                            self.select_audio(&name);
                            if let ActiveSource::Audio(audio) = &self.source {
                                audio.set_channel(*channel);
                                audio.set_gain(*gain);
                            }
                        }
                        None => {
                            self.status =
                                "profile wants audio input, but no input device is present"
                                    .to_string();
                        }
                    }
                }
                #[cfg(not(feature = "audio"))]
                {
                    let _ = (device, channel, gain);
                    self.status = "profile wants audio input, which this build was made \
                                   without (cargo run -p bench-gui --features audio)"
                        .to_string();
                }
            }
        }

        self.chain = chain;
        self.step_results.clear();
        self.last_run_error = None;
        self.view_samples = p
            .instrument
            .view_samples
            .clamp(MIN_VIEW, self.ring.capacity());
        self.set_capture(p.instrument.capture_samples);
        self.results_stale = false;
        self.trigger = p.instrument.trigger;
        self.clock_rate = p.instrument.clock_rate.clamp(0.001, 1.0);
        self.analysis_hz = p.instrument.analysis_hz.clamp(0.1, 4.0);
        self.auto_analysis = false;
        self.placed_panels = p
            .panels
            .iter()
            .map(|r| (r.namespace.clone(), r.id.clone()))
            .collect();
        self.running = true;
        self.profile_name = p.name.clone();
        self.status = format!("profile: {}", p.name);
    }

    /// The bench as it is now, as a profile.
    fn current_profile(&self, name: &str) -> Profile {
        let source = match &self.source {
            ActiveSource::Synth(s) => SourceSpec::Synth {
                sample_rate: s.sample_rate(),
                tones: s.tones.clone(),
                noise: s.noise,
            },
            ActiveSource::Replay(r) => SourceSpec::Replay {
                path: r
                    .path()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| std::path::PathBuf::from(r.name())),
                looping: r.looping,
            },
            #[cfg(feature = "audio")]
            ActiveSource::Audio(a) => SourceSpec::Audio {
                device: Some(a.device().to_string()),
                channel: a.channel(),
                gain: a.gain(),
            },
        };

        Profile {
            version: profile::FORMAT_VERSION,
            name: name.to_string(),
            description: String::new(),
            source,
            chain: self.chain.steps().to_vec(),
            instrument: Instrument {
                view_samples: self.view_samples,
                capture_samples: self.capture_samples,
                trigger: self.trigger,
                clock_rate: self.clock_rate,
                analysis_hz: self.analysis_hz,
            },
            panels: self
                .placed_panels
                .iter()
                .map(|(namespace, id)| PanelRef {
                    namespace: namespace.clone(),
                    id: id.clone(),
                })
                .collect(),
        }
    }

    fn save_profile(&mut self) {
        let name = self.profile_name.trim().to_string();
        let p = self.current_profile(&name);
        let path = profile::default_dir().join(p.file_name());
        match p.save(&path) {
            Ok(()) => self.status = format!("saved profile to {}", path.display()),
            Err(e) => self.status = e.to_string(),
        }
    }

    /// Write the analysis frame to disk as WAV and CSV.
    fn save_capture(&mut self) {
        let samples = self.ring.window(self.gate_back(), self.capture_samples);
        if samples.is_empty() {
            self.status = "nothing to save yet".to_string();
            return;
        }
        let dir = bench_core::capture::default_dir();
        match bench_core::capture::save_capture(&dir, &samples, self.source.sample_rate()) {
            Ok((wav, _csv)) => {
                self.status = format!(
                    "saved {} samples to {} (+ .csv)",
                    samples.len(),
                    wav.display()
                );
            }
            Err(e) => self.status = format!("could not save capture: {e}"),
        }
    }

    /// Replay position, loop toggle and restart.
    fn replay_controls(&mut self, ui: &mut egui::Ui) {
        let ActiveSource::Replay(replay) = &mut self.source else {
            return;
        };
        let len = replay.len().max(1);
        let secs = replay.duration_secs();
        ui.label(
            RichText::new(format!(
                "{} samples · {secs:.2} s · {:.1}% played",
                replay.len(),
                replay.position() as f64 / len as f64 * 100.0
            ))
            .weak()
            .small(),
        );
        ui.horizontal(|ui| {
            ui.checkbox(&mut replay.looping, "loop")
                .on_hover_text("wrap to the start at the end of the file");
            if ui.small_button("restart").clicked() {
                replay.restart();
            }
            if replay.is_exhausted() {
                ui.label(
                    RichText::new("finished — restart or enable loop")
                        .small()
                        .color(Color32::from_rgb(250, 204, 21)),
                );
            }
        });
    }

    #[cfg(feature = "audio")]
    fn select_audio(&mut self, name: &str) {
        match bench_core::source::audio::AudioSource::open(Some(name)) {
            Ok(audio) => {
                let previous = std::mem::replace(&mut self.source, ActiveSource::Audio(audio));
                if let ActiveSource::Synth(synth) = previous {
                    self.parked_synth = Some(synth);
                }
                self.after_source_switch();
            }
            Err(e) => self.status = format!("could not open {name}: {e}"),
        }
    }

    /// A new source means a new sample rate and unrelated history: start the
    /// ring afresh and return to live.
    fn after_source_switch(&mut self) {
        self.ring = Arc::new(SampleRing::new(DEFAULT_RING_CAPACITY));
        self.view_back = 0;
        self.frame_back = 0;
        self.results_stale = true;
        self.status = format!("source: {}", self.source.name());
    }

    /// The function generator's knobs.
    fn synth_controls(&mut self, ui: &mut egui::Ui, nyquist: f64) {
        let Some(synth) = self.source.synth_mut() else {
            return;
        };
        let mut changed = false;
        let before = (synth.tones.clone(), synth.noise);

        ui.horizontal(|ui| {
            ui.label(RichText::new("preset").weak().small());
            egui::ComboBox::from_id_salt("synth_preset")
                .selected_text("choose…")
                .show_ui(ui, |ui| {
                    for preset in Preset::ALL {
                        if ui.selectable_label(false, preset.label()).clicked() {
                            synth.apply(preset);
                            changed = true;
                        }
                    }
                });
        });

        let mut remove: Option<usize> = None;
        for (i, tone) in synth.tones.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).weak().small());
                ui.checkbox(&mut tone.enabled, "")
                    .on_hover_text("mute or unmute this component");
                egui::ComboBox::from_id_salt(("tone_wave", i))
                    .selected_text(tone.wave.label())
                    .width(84.0)
                    .show_ui(ui, |ui| {
                        for wave in Wave::ALL {
                            ui.selectable_value(&mut tone.wave, wave, wave.label());
                        }
                    });
                // Proportional speed: a drag moves 2 Hz by fractions and
                // 2 kHz by tens, so both are reachable by hand.
                let speed = (tone.freq_hz * 0.01).max(0.05);
                ui.add(
                    egui::DragValue::new(&mut tone.freq_hz)
                        .speed(speed)
                        .range(0.1..=nyquist)
                        .suffix(" Hz"),
                );
                ui.add(
                    egui::DragValue::new(&mut tone.amplitude)
                        .speed(0.01)
                        .range(0.0..=2.0)
                        .fixed_decimals(2)
                        .prefix("× "),
                );
                if ui
                    .small_button("×")
                    .on_hover_text("remove this component")
                    .clicked()
                {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            synth.tones.remove(i);
        }

        ui.horizontal(|ui| {
            if ui
                .small_button("+ component")
                .on_hover_text("stack another waveform onto the signal")
                .clicked()
            {
                let last = synth.tones.last().map(|t| t.freq_hz).unwrap_or(440.0);
                synth
                    .tones
                    .push(Tone::new(Wave::Sine, (last * 2.0).min(nyquist), 0.5));
            }
            ui.label(RichText::new("noise").weak().small());
            ui.add(egui::Slider::new(&mut synth.noise, 0.0..=1.0).fixed_decimals(2));
        });

        // Any knob turned re-renders the ring's history under the new
        // settings, so the change is on screen this frame — paused, parked in
        // history, or live. The generator is a pure function of the sample
        // index, which is what makes this a rewrite rather than a restart.
        if changed || (synth.tones.clone(), synth.noise) != before {
            let n = self.ring.available();
            if n > 0 {
                let mut history = vec![0.0f32; n];
                synth.render(synth.elapsed() - n as u64, &mut history);
                self.ring.rewrite_tail(&history);
            }
            self.results_stale = true;
        }
    }

    /// The input device's channel, gain and level meter.
    #[cfg(feature = "audio")]
    fn audio_controls(&mut self, ui: &mut egui::Ui) {
        let ActiveSource::Audio(audio) = &self.source else {
            return;
        };

        // Peak-hold meter with a fast attack and a slow fall.
        let peak = audio.take_peak();
        self.input_peak = if peak > self.input_peak {
            peak
        } else {
            self.input_peak * 0.92
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new("channel").weak().small());
            let mut channel = audio.channel();
            egui::ComboBox::from_id_salt("audio_channel")
                .selected_text(format!("{}", channel + 1))
                .width(56.0)
                .show_ui(ui, |ui| {
                    for c in 0..audio.channels() as usize {
                        ui.selectable_value(&mut channel, c, format!("{}", c + 1));
                    }
                });
            if channel != audio.channel() {
                audio.set_channel(channel);
            }

            ui.label(RichText::new("gain").weak().small());
            let mut gain = audio.gain();
            if ui
                .add(
                    egui::DragValue::new(&mut gain)
                        .speed(0.01)
                        .range(0.0..=100.0)
                        .fixed_decimals(2)
                        .prefix("× "),
                )
                .changed()
            {
                audio.set_gain(gain);
            }
        });

        ui.horizontal(|ui| {
            ui.label(RichText::new("input").weak().small());
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width().min(200.0), 10.0),
                egui::Sense::hover(),
            );
            let level = self.input_peak.clamp(0.0, 1.0);
            let colour = if self.input_peak > 0.98 {
                Color32::from_rgb(248, 113, 113)
            } else if level > 0.7 {
                Color32::from_rgb(250, 204, 21)
            } else {
                Color32::from_rgb(74, 222, 128)
            };
            let painter = ui.painter();
            painter.rect_filled(rect, 2.0_f32, Color32::from_rgb(30, 41, 59));
            painter.rect_filled(
                egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width() * level, rect.height()),
                ),
                2.0_f32,
                colour,
            );
            ui.label(
                RichText::new(format!(
                    "{:.2}{}",
                    self.input_peak,
                    if self.input_peak > 0.98 { " clip" } else { "" }
                ))
                .weak()
                .small(),
            );
        });

        if audio.dropped() > 0 {
            ui.label(
                RichText::new(format!(
                    "{} samples dropped while the UI was stalled",
                    audio.dropped()
                ))
                .weak()
                .small(),
            );
        }
        ui.label(
            RichText::new("line level is about ±1 full scale; keep inputs under ±2 V and divide 10:1 above that")
                .weak()
                .small(),
        );
    }

    /// The whole ring in a strip under the scope, with the view and gate
    /// marked. Click or drag to put the view there; the gate rides along.
    fn minimap(&mut self, ui: &mut egui::Ui, shown: usize) {
        let available = self.ring.available();
        if available == 0 {
            return;
        }
        let all = self.ring.latest(available);
        let a = available as f32;
        // Markers from the user's offsets, not the trigger-aligned position:
        // the trigger re-aligns by up to a period every frame, which is
        // invisible on the trace and a jitter on the strip.
        let view = (
            1.0 - (self.view_back + shown) as f32 / a,
            1.0 - self.view_back as f32 / a,
        );
        let gate = (
            1.0 - (self.frame_back + self.capture_samples) as f32 / a,
            1.0 - self.frame_back as f32 / a,
        );
        let (rect, response) = draw_minimap(ui, &all, view, gate);

        if response.hovered() {
            ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::AllScroll);
        }
        if response.dragged() || response.clicked() {
            if let Some(p) = response.interact_pointer_pos() {
                let frac = ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
                let centre_back = ((1.0 - frac) * a) as usize;
                let desired = centre_back.saturating_sub(self.view_samples / 2);
                // `view_back` is an offset from the reference (live or the
                // trigger edge); keep that reference and move the offset.
                let reference = self.display_back.saturating_sub(self.view_back);
                let before = self.view_back;
                self.view_back = desired.saturating_sub(reference);
                let moved = self.view_back as i64 - before as i64;
                self.frame_back = (self.frame_back as i64 + moved).max(0) as usize;
                self.clamp_offsets();
            }
        }
        response.on_hover_text("the whole ring; click or drag to move the view");
    }

    /// Keep the view inside the ring's history and the gate inside the view.
    fn clamp_offsets(&mut self) {
        let available = self.ring.available();
        self.view_back = self
            .view_back
            .min(available.saturating_sub(self.view_samples));
        let min_frame = self.view_back;
        let max_frame = (self.view_back + self.view_samples)
            .saturating_sub(self.capture_samples)
            .max(min_frame);
        self.frame_back = self.frame_back.clamp(min_frame, max_frame);
    }

    /// Turn pointer input on the scope into gate moves, resizes, pans and zooms.
    ///
    /// The idiom is the one software instruments and waveform editors share:
    /// drag the gate to move the analysis frame, drag its edges to resize it,
    /// drag elsewhere to pan through history, scroll to zoom the time base
    /// around the pointer, horizontal scroll (or Shift+scroll) to pan, and
    /// double-click to snap back to live. A crosshair reads out time and value
    /// under the pointer.
    fn handle_scope_input(&mut self, ui: &mut egui::Ui, out: &ScopeOutput, samples: &[f32]) {
        let shown = samples.len();
        if shown == 0 {
            return;
        }
        let width = out.rect.width().max(1.0);
        let samples_per_px = shown as f32 / width;

        let hit = |pos: egui::Pos2| -> ScopeDrag {
            match out.gate_rect {
                Some(g) if (pos.x - g.left()).abs() <= EDGE_GRAB_PX => ScopeDrag::GateLeft,
                Some(g) if (pos.x - g.right()).abs() <= EDGE_GRAB_PX => ScopeDrag::GateRight,
                Some(g) if g.contains(pos) => ScopeDrag::Gate,
                _ => ScopeDrag::View,
            }
        };

        if out.response.drag_started() {
            self.scope_drag = out.response.interact_pointer_pos().map(hit);
        }
        if out.response.drag_stopped() {
            self.scope_drag = None;
        }

        // Dragging right moves what is under the pointer towards "later" on
        // screen, which is towards live: fewer samples behind.
        let to_samples = |dx: f32| (dx * samples_per_px).round() as i64;
        let pan = |view_back: &mut usize, frame_back: &mut usize, d: i64| {
            // Panning carries the gate so it stays where it sits on screen.
            let before = *view_back as i64;
            *view_back = (before - d).max(0) as usize;
            let moved = *view_back as i64 - before;
            *frame_back = (*frame_back as i64 + moved).max(0) as usize;
        };

        if out.response.dragged() {
            let d = to_samples(out.response.drag_delta().x);
            match self.scope_drag {
                Some(ScopeDrag::Gate) => {
                    self.frame_back = (self.frame_back as i64 - d).max(0) as usize;
                }
                Some(ScopeDrag::GateLeft) => {
                    // Right edge fixed: the frame grows as its left edge moves left.
                    let n = (self.capture_samples as i64 - d).max(MIN_CAPTURE as i64);
                    self.set_capture(n as usize);
                }
                Some(ScopeDrag::GateRight) => {
                    // Left edge fixed: moving right takes in newer samples.
                    let n = (self.capture_samples as i64 + d).max(MIN_CAPTURE as i64);
                    let grew = n - self.capture_samples as i64;
                    self.frame_back = (self.frame_back as i64 - grew).max(0) as usize;
                    self.set_capture(n as usize);
                }
                Some(ScopeDrag::View) => pan(&mut self.view_back, &mut self.frame_back, d),
                None => {}
            }
        }

        if out.response.hovered() {
            let (scroll, shift_held) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
            let hover = out.response.hover_pos();

            // Horizontal scroll pans; so does Shift + vertical, for mice.
            let pan_px = if shift_held {
                scroll.x + scroll.y
            } else {
                scroll.x
            };
            if pan_px.abs() > 0.0 {
                pan(
                    &mut self.view_back,
                    &mut self.frame_back,
                    to_samples(pan_px),
                );
            }

            // Vertical scroll zooms the time base. The anchor is the analysis
            // frame's centre when it is on screen — the place the user dragged
            // to is the place they are looking at, and zooming out and back in
            // must return there — and the pointer otherwise.
            if !shift_held && scroll.y.abs() > 0.0 {
                if let Some(p) = hover {
                    let gate_centre = self.frame_back as f32 + self.capture_samples as f32 / 2.0;
                    let view_left = (self.view_back + shown) as f32;
                    let (anchor, frac_from_right) =
                        if gate_centre >= self.view_back as f32 && gate_centre <= view_left {
                            let f = (gate_centre - self.view_back as f32) / shown.max(1) as f32;
                            (gate_centre, f)
                        } else {
                            let frac = ((p.x - out.rect.left()) / width).clamp(0.0, 1.0);
                            let f = 1.0 - frac;
                            (self.view_back as f32 + f * shown as f32, f)
                        };
                    let factor = (-scroll.y * 0.004).exp();
                    let max_view = self.ring.capacity();
                    let new_view = ((self.view_samples as f32 * factor).round() as usize)
                        .clamp(MIN_VIEW, max_view);
                    // `view_back` is an offset from the reference (live, or
                    // the trigger edge), so the same arithmetic holds in both
                    // modes. The gate keeps its place in history, not on
                    // screen: it is a window on the signal.
                    let new_back = anchor - frac_from_right * new_view as f32;
                    self.view_back = new_back.round().max(0.0) as usize;
                    self.view_samples = new_view;
                }
            }

            let icon = match (self.scope_drag, hover) {
                (Some(ScopeDrag::GateLeft | ScopeDrag::GateRight), _) => {
                    egui::CursorIcon::ResizeHorizontal
                }
                (Some(_), _) => egui::CursorIcon::Grabbing,
                (None, Some(p)) => match hit(p) {
                    ScopeDrag::GateLeft | ScopeDrag::GateRight => {
                        egui::CursorIcon::ResizeHorizontal
                    }
                    ScopeDrag::Gate => egui::CursorIcon::Grab,
                    ScopeDrag::View => egui::CursorIcon::AllScroll,
                },
                _ => egui::CursorIcon::Default,
            };
            ui.output_mut(|o| o.cursor_icon = icon);

            // Crosshair readout: time behind live and the sample value.
            if let Some(p) = hover {
                let frac = ((p.x - out.rect.left()) / width).clamp(0.0, 1.0);
                let idx = ((frac * shown as f32) as usize).min(shown - 1);
                let behind = (self.view_back + shown - idx) as f64 / self.source.sample_rate();
                let value = samples[idx];
                let painter = ui.painter_at(out.rect);
                painter.line_segment(
                    [
                        egui::Pos2::new(p.x, out.rect.top()),
                        egui::Pos2::new(p.x, out.rect.bottom()),
                    ],
                    egui::Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(226, 232, 240, 90)),
                );
                let text = format!("−{:.2} ms   {value:+.4}", behind * 1000.0);
                let galley = painter.layout_no_wrap(
                    text,
                    egui::FontId::monospace(11.0),
                    Color32::from_rgb(226, 232, 240),
                );
                let pad = egui::vec2(6.0, 3.0);
                let mut at = egui::Pos2::new(p.x + 10.0, out.rect.top() + 18.0);
                if at.x + galley.size().x + pad.x * 2.0 > out.rect.right() {
                    at.x = p.x - 10.0 - galley.size().x - pad.x * 2.0;
                }
                let box_rect = egui::Rect::from_min_size(at, galley.size() + pad * 2.0);
                painter.rect_filled(
                    box_rect,
                    3.0_f32,
                    Color32::from_rgba_unmultiplied(15, 23, 42, 220),
                );
                painter.galley(at + pad, galley, Color32::from_rgb(226, 232, 240));
            }
        }

        if out.response.double_clicked() {
            self.view_back = 0;
            self.frame_back = 0;
        }

        self.clamp_offsets();
    }

    /// Change the analysis frame length; results from the old frame are stale.
    fn set_capture(&mut self, n: usize) {
        let n = n.clamp(MIN_CAPTURE, self.ring.capacity());
        if n != self.capture_samples {
            self.capture_samples = n;
            self.results_stale = true;
        }
    }

    /// Time base and analysis-frame controls under the trace.
    fn scope_controls(&mut self, ui: &mut egui::Ui, lo: f32, hi: f32, shown: usize) {
        let fs = self.source.sample_rate();
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.trigger, "trigger")
                .on_hover_text(
                    "hold the view on a rising edge of the signal so a periodic \
                     waveform stands still; off, the trace rolls",
                );

            ui.label(RichText::new("clock").weak().small());
            egui::ComboBox::from_id_salt("clock_rate")
                .selected_text(clock_label(self.clock_rate))
                .width(84.0)
                .show_ui(ui, |ui| {
                    for rate in [1.0, 0.1, 0.01, 0.001] {
                        ui.selectable_value(&mut self.clock_rate, rate, clock_label(rate));
                    }
                })
                .response
                .on_hover_text(
                    "slow the generator's clock: same sample rate and bandwidth, \
                     fewer samples per second, so a fast signal can be watched",
                );

            ui.label(RichText::new("time base").weak().small());
            egui::ComboBox::from_id_salt("view_samples")
                .selected_text(format!("{:.1} ms", self.view_samples as f64 / fs * 1000.0))
                .width(96.0)
                .show_ui(ui, |ui| {
                    for n in VIEW_PRESETS {
                        ui.selectable_value(
                            &mut self.view_samples,
                            n,
                            format!("{:.1} ms · {n} samples", n as f64 / fs * 1000.0),
                        );
                    }
                });

            ui.label(RichText::new("analysis frame").weak().small());
            egui::ComboBox::from_id_salt("capture_samples")
                .selected_text(format!("{} samples", self.capture_samples))
                .width(120.0)
                .show_ui(ui, |ui| {
                    for n in CAPTURE_PRESETS {
                        if ui
                            .selectable_value(
                                &mut self.capture_samples,
                                n,
                                format!("{n} samples · {:.1} Hz/bin", fs / n as f64),
                            )
                            .changed()
                        {
                            self.results_stale = true;
                        }
                    }
                });

            if ui
                .small_button("Save capture")
                .on_hover_text("write the analysis frame to ~/Documents/Bench/captures as WAV and CSV")
                .clicked()
            {
                self.save_capture();
            }

            if self.view_back > 0 || self.frame_back > 0 {
                let behind_ms = self.frame_back as f64 / fs * 1000.0;
                ui.label(
                    RichText::new(format!("frame {behind_ms:.1} ms behind live"))
                        .small()
                        .color(Color32::from_rgb(250, 204, 21)),
                );
                if ui
                    .small_button("back to live")
                    .on_hover_text("or double-click the trace")
                    .clicked()
                {
                    self.view_back = 0;
                    self.frame_back = 0;
                }
            }

            let paused = if self.running {
                ""
            } else {
                " · paused — Analyse runs over the shaded frame"
            };
            ui.label(
                RichText::new(format!(
                    "{shown} samples on screen · {lo:.3} … {hi:.3}{paused} · scroll to zoom, drag to pan, drag the frame or its edges · Space runs, Enter analyses, Home is live"
                ))
                .weak()
                .small(),
            );
        });
    }

    fn panel_index(&self, key: &(String, String)) -> Option<usize> {
        self.panels
            .iter()
            .position(|p| p.namespace == key.0 && p.id == key.1)
    }

    /// The Smart Panels picker: the account's panels, each with a Place
    /// button that drops it into the dock.
    ///
    /// Loading is explicit and priced — one `smart-panels.list` call — so it is
    /// a button rather than something that happens on connect.
    fn panel_picker(&mut self, ctx: &egui::Context) {
        if !self.panel_picker_open {
            return;
        }
        let online = matches!(self.connection, Connection::Online(_));
        let mut open = self.panel_picker_open;
        let mut load = false;
        let mut place: Option<(String, String)> = None;

        egui::Window::new("Smart Panels")
            .open(&mut open)
            .default_width(440.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let label = if self.panels_loading {
                        "Loading…"
                    } else if self.panels.is_empty() {
                        "Load panels"
                    } else {
                        "Reload"
                    };
                    if ui
                        .add_enabled(online && !self.panels_loading, egui::Button::new(label))
                        .on_hover_text("one smart_panel.list call")
                        .clicked()
                    {
                        load = true;
                    }
                    if !online {
                        ui.label(RichText::new("connect to DataGrout first").weak().small());
                    }
                });

                if self.panels.is_empty() {
                    ui.label(RichText::new("no panels loaded").weak().small().italics());
                    return;
                }
                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(440.0)
                    .show(ui, |ui| {
                        for panel in &self.panels {
                            let key = (panel.namespace.clone(), panel.id.clone());
                            let placed = self.placed_panels.contains(&key);
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(panel.title().to_string()).strong());
                                    ui.label(
                                        RichText::new(format!(
                                            "{} · {} · {} part{}",
                                            panel.kind.as_str(),
                                            panel.namespace,
                                            panel.children.len(),
                                            if panel.children.len() == 1 { "" } else { "s" }
                                        ))
                                        .weak()
                                        .small(),
                                    );
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if placed {
                                            ui.label(RichText::new("placed").weak().small());
                                        } else if ui.button("Place").clicked() {
                                            place = Some(key.clone());
                                        }
                                    },
                                );
                            });
                            ui.separator();
                        }
                    });
            });

        self.panel_picker_open = open;
        if load {
            self.load_panels(ctx);
        }
        if let Some(key) = place {
            self.placed_panels.push(key);
            self.panel_picker_open = false;
        }
    }

    /// Placed panels, side by side in a resizable strip under the scope.
    ///
    /// Interim layout: one column per placed panel, each closable, the strip's
    /// height draggable. The snap-to-slot dashboard host this grows into needs
    /// a real tiling layout rather than columns.
    fn panel_dock(&mut self, ctx: &egui::Context) {
        // Drop anything the account no longer has before drawing.
        let placed: Vec<(usize, (String, String))> = self
            .placed_panels
            .iter()
            .filter_map(|key| self.panel_index(key).map(|i| (i, key.clone())))
            .collect();
        if placed.is_empty() {
            return;
        }

        let online = matches!(self.connection, Connection::Online(_));
        let mut close: Option<(String, String)> = None;
        let mut load_rows_for: Option<usize> = None;
        let mut actions: Vec<PanelAction> = Vec::new();

        egui::TopBottomPanel::bottom("bench_dock")
            .resizable(true)
            .default_height(340.0)
            .min_height(160.0)
            .show(ctx, |ui| {
                if let Some(action) = &self.last_panel_action {
                    ui.label(
                        RichText::new(action)
                            .small()
                            .color(Color32::from_rgb(250, 204, 21)),
                    );
                }
                ui.columns(placed.len(), |cols| {
                    for (col, (i, key)) in cols.iter_mut().zip(placed.iter()) {
                        let Some(panel) = self.panels.get(*i) else {
                            continue;
                        };
                        col.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} · {}",
                                    panel.kind.as_str(),
                                    panel.namespace
                                ))
                                .weak()
                                .small(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("×")
                                        .on_hover_text("remove from the dock")
                                        .clicked()
                                    {
                                        close = Some(key.clone());
                                    }
                                    let calls = Session::row_load_calls(panel);
                                    let loading =
                                        self.panel_rows_loading.get(*i).copied().unwrap_or(false);
                                    let loaded =
                                        self.panel_rows_loaded.get(*i).copied().unwrap_or(false);
                                    if loaded {
                                        // Loading again would fetch the same
                                        // rows; the button's job is done.
                                        ui.label(RichText::new("rows loaded").weak().small());
                                    } else if calls > 0 {
                                        let label = if loading {
                                            "loading rows…".to_string()
                                        } else {
                                            format!(
                                                "Load rows ({} call{})",
                                                calls,
                                                if calls == 1 { "" } else { "s" }
                                            )
                                        };
                                        if ui
                                            .add_enabled(
                                                online && !loading,
                                                egui::Button::new(label).small(),
                                            )
                                            .on_hover_text(
                                                "the list carries a row preview; this fetches the \
                                             full rows for the panel and everything under it",
                                            )
                                            .clicked()
                                        {
                                            load_rows_for = Some(*i);
                                        }
                                    }
                                },
                            );
                        });
                        egui::ScrollArea::vertical()
                            .id_salt(("dock", key))
                            .show(col, |ui| {
                                actions.extend(render_panel_with_state(
                                    ui,
                                    panel,
                                    &mut self.panel_forms,
                                ));
                            });
                    }
                });
            });

        if let Some(key) = close {
            self.placed_panels.retain(|k| *k != key);
        }
        if let Some(i) = load_rows_for {
            self.load_panel_rows(i, ctx);
        }

        // Dispatching a form action means running its goal on the gateway;
        // that is deliberate, priced work and not wired here yet. Say so
        // rather than swallowing the click.
        if let Some(action) = actions.into_iter().next() {
            self.last_panel_action = Some(match action {
                PanelAction::Submit { panel_id, field_id } => format!(
                    "{panel_id}/{field_id} submitted — dispatch to the gateway is not wired yet"
                ),
                PanelAction::ValueChanged {
                    panel_id, field_id, ..
                } => format!("{panel_id}/{field_id} changed"),
            });
        }
    }

    /// Readouts. The local ones are measured over the shaded frame every
    /// frame, like a scope's measurement row; the spectral ones come from the
    /// last chain run, and say so.
    fn meter_pane(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Meter").strong());

        let gate = self.ring.window(self.gate_back(), self.capture_samples);
        if gate.is_empty() {
            ui.label(RichText::new("waiting for a signal").weak().small());
            return;
        }
        let local = Features::from_samples("live", &gate, self.source.sample_rate());
        let chain = self.last_features.as_ref();

        let readings: [(&str, Option<f64>, &str); 8] = [
            ("RMS", local.rms, ""),
            ("pk-pk", local.peak_to_peak, ""),
            ("DC", local.dc_offset, ""),
            ("frequency", local.frequency_hz, "Hz"),
            ("period", local.period_ms, "ms"),
            ("duty", local.duty_pct, "%"),
            ("dominant (chain)", chain.and_then(|f| f.dominant_hz), "Hz"),
            ("THD (chain)", chain.and_then(|f| f.thd_pct), "%"),
        ];

        for (label, value, unit) in readings {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).weak().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // An absent measurement shows as "—", never as 0.000:
                    // a fabricated zero is a reading an engineer would act on.
                    let text = match value {
                        Some(v) => format!("{v:.4} {unit}"),
                        None => "—".to_string(),
                    };
                    ui.label(RichText::new(text).monospace());
                });
            });
        }

        if let Some(regime) = chain.and_then(|f| f.regime.as_ref()) {
            ui.horizontal(|ui| {
                ui.label(RichText::new("regime").weak().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(regime).monospace());
                });
            });
        }
    }
}

/// Draw filter cutoffs as vertical handles on a spectrum and let them be
/// dragged. Returns `(step, argument, new value)` while a handle is moving;
/// the caller writes it into the chain, since the chain is borrowed for the
/// chart being drawn.
///
/// The idiom is the waveform editor's, on an instrument: scope cursors
/// measure, these set the parameter. The value written is the normalised
/// cutoff the tool wants, `f / fs`.
fn draw_arg_handles(
    ui: &mut egui::Ui,
    out: &ScopeOutput,
    max_hz: f64,
    fs: f64,
    handles: &[ArgHandle],
    arg_drag: &mut Option<(usize, String)>,
) -> Option<(usize, String, String)> {
    if handles.is_empty() || max_hz <= 0.0 {
        return None;
    }
    let width = out.rect.width().max(1.0);
    let x_of = |hz: f64| out.rect.left() + width * (hz / max_hz).clamp(0.0, 1.0) as f32;
    let colour = Color32::from_rgb(251, 146, 60);
    let painter = ui.painter_at(out.rect);

    for (n, h) in handles.iter().enumerate() {
        let x = x_of(h.hz);
        painter.line_segment(
            [
                egui::Pos2::new(x, out.rect.top()),
                egui::Pos2::new(x, out.rect.bottom()),
            ],
            egui::Stroke::new(1.5_f32, colour),
        );
        // Stagger labels so two handles near each other stay legible.
        let y = out.rect.top() + 3.0 + 12.0 * (n % 2) as f32;
        painter.text(
            egui::Pos2::new(x + 4.0, y),
            egui::Align2::LEFT_TOP,
            format!("{}. {} {}", h.step + 1, h.arg, hz_label(h.hz)),
            egui::FontId::proportional(10.0),
            colour,
        );
    }

    let near = |p: egui::Pos2| handles.iter().find(|h| (p.x - x_of(h.hz)).abs() <= 8.0);

    if out.response.drag_started() {
        *arg_drag = out
            .response
            .interact_pointer_pos()
            .and_then(near)
            .map(|h| (h.step, h.arg.clone()));
    }
    if out.response.drag_stopped() {
        *arg_drag = None;
    }
    if let Some(p) = out.response.hover_pos() {
        if arg_drag.is_some() || near(p).is_some() {
            ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeHorizontal);
        }
    }

    match (arg_drag.as_ref(), out.response.dragged()) {
        (Some((step, arg)), true) => {
            let p = out.response.interact_pointer_pos()?;
            let hz = ((p.x - out.rect.left()) / width).clamp(0.0, 1.0) as f64 * max_hz;
            // Normalised to the sample rate, inside the open interval the tool
            // accepts; a cutoff at exactly 0 or Nyquist is a refusal.
            let norm = (hz / fs).clamp(0.0005, 0.4995);
            Some((*step, arg.clone(), format!("{norm:.4}")))
        }
        _ => None,
    }
}

/// "440 Hz" or "4.80 kHz".
fn hz_label(hz: f64) -> String {
    if hz >= 1000.0 {
        format!("{:.2} kHz", hz / 1000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// "1×", "1/10", … for the generator clock menu.
fn clock_label(rate: f64) -> String {
    if rate >= 1.0 {
        "1× real time".to_string()
    } else {
        format!("1/{:.0}", 1.0 / rate)
    }
}

fn summarise(result: &StepResult) -> String {
    match result {
        StepResult::Values(v) => format!("{} values", v.len()),
        // Which way the filter's timing goes is the fact a live path needs:
        // a delayed trace lags the signal, a look-ahead trace answers before
        // it. Say which, in samples.
        StepResult::Filtered {
            values,
            delay_samples,
            look_ahead_samples,
            valid_from,
        } => {
            let timing = if *look_ahead_samples > 0 {
                format!("zero-phase · looks ahead {look_ahead_samples} samples")
            } else {
                format!("causal · delayed {delay_samples} samples")
            };
            format!(
                "{} values · {timing} · settled from {valid_from}",
                values.len()
            )
        }
        StepResult::Peaks {
            indices,
            mean_interval,
            rate_hz,
            ..
        } => {
            let mut text = format!("{} peaks", indices.len());
            if let Some(hz) = rate_hz {
                text.push_str(&format!(" · {hz:.2} /s"));
            }
            if let Some(gap) = mean_interval {
                text.push_str(&format!(" · mean spacing {gap:.1} samples"));
            }
            text
        }
        StepResult::Spectral {
            dominant_hz, peaks, ..
        } => match dominant_hz {
            Some(hz) => format!("dominant {hz:.2} Hz · {} peaks", peaks.len()),
            None => format!("{} peaks", peaks.len()),
        },
        StepResult::Matrix { rows, cols, .. } => {
            format!("{rows} × {cols} trajectory · row 0 shown")
        }
        StepResult::Pca {
            explained_variance_ratio,
            ..
        } => {
            let top = explained_variance_ratio.first().copied().unwrap_or(0.0) * 100.0;
            format!(
                "{} components · PC1 = {top:.1}%",
                explained_variance_ratio.len()
            )
        }
        StepResult::Cluster { k, sizes, inertia } => match inertia {
            Some(i) => format!("k={k} · sizes {sizes:?} · inertia {i:.3}"),
            None => format!("k={k} · sizes {sizes:?}"),
        },
        StepResult::Stats(pairs) => pairs
            .iter()
            .map(|(k, v)| format!("{k} {v:.3}"))
            .collect::<Vec<_>>()
            .join(" · "),
        StepResult::Error(message) => format!("step failed: {message}"),
    }
}
