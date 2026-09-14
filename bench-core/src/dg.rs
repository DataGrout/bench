//! The gateway seam.
//!
//! Everything Bench needs from DataGrout, expressed as one trait. Two reasons
//! it is a trait rather than a concrete client:
//!
//! 1. **Testing.** The chain compiler, feature extraction and spec rules are
//!    all pure; a fake client keeps them testable with no network.
//! 2. **Extractability.** `bench-serve` is written against this trait, so it
//!    carries no dependency on any particular client and can move to a
//!    standalone crate that other hosts share.

use std::future::Future;
use std::pin::Pin;

use std::sync::Arc;

pub use datagrout_conduit::authcode::{AuthCodeFlow, AuthCodeProvider, Grant};
use serde_json::Value;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum DgError {
    #[error("not connected to a gateway")]
    NotConnected,
    #[error("transport: {0}")]
    Transport(String),
    #[error("tool {tool} failed: {message}")]
    Tool { tool: String, message: String },
    /// The saved grant can no longer be refreshed; the user has to sign in again.
    ///
    /// DataGrout rotates refresh tokens, so this is what happens when a grant
    /// was refreshed by one process and the rotated token was not written back
    /// — the next holder of the old file presents a token that has already been
    /// consumed. The registered client is still valid; only the grant is dead.
    #[error("sign-in expired — the saved grant can no longer be refreshed; sign in again")]
    SignInExpired,
    /// WebSocket is off for this server.
    ///
    /// Its own variant because the fix is specific and non-obvious: WS is gated
    /// per-server on `interaction_config.enabled_protocols` containing `"ws"`,
    /// and the upgrade returns HTTP 400 rather than failing at the handshake.
    /// Callers should say so and fall back to HTTP MCP.
    #[error("WebSocket is not enabled for this server — add \"ws\" to interaction_config.enabled_protocols, or use the HTTP transport")]
    WebSocketDisabled,
}

pub type DgResult<T> = Result<T, DgError>;

/// The gateway operations Bench uses.
pub trait DgClient: Send + Sync {
    /// Call a tool by fully-qualified ref.
    fn call_tool<'a>(&'a self, tool: &'a str, args: Value) -> BoxFuture<'a, DgResult<Value>>;

    /// Run a `logic.query` goal, returning solution rows.
    fn query<'a>(
        &'a self,
        namespace: &'a str,
        goal: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, DgResult<Vec<Value>>>;

    /// Human-readable description of the connection, for the status bar.
    fn describe(&self) -> String;
}

/// Well-known tool refs, so no call site spells one by hand.
pub mod tools {
    pub const FLOW_INTO: &str = "data-grout@1/flow.into@1";
    pub const LOGIC_BATCH: &str = "data-grout@1/logic.batch@1";
    pub const LOGIC_QUERY: &str = "data-grout@1/logic.query@1";
    pub const TOOLSMITH_INVOKE: &str = "data-grout@1/toolsmith.invoke@1";
    pub const REACTOR_EXPOSE: &str = "data-grout@1/reactor.expose@1";
    pub const SMART_PANEL_PUBLISH: &str = "data-grout@1/smart-panels.publish@1";
    /// Every panel with props, field ids and a row preview in one call — the
    /// intended way to load Smart Panels.
    pub const SMART_PANEL_LIST: &str = "data-grout@1/smart-panels.list@1";
    /// The raw-fetch path for a cached (headed) result. Pages are stamped
    /// `_no_head`, so they bypass the inline size clamp.
    pub const PRISM_PAGINATE: &str = "data-grout@1/prism.paginate@1";
}

/// A client that refuses every call.
///
/// The default state before a grant exists. Distinct from "no client" so the
/// UI can render its full layout while disconnected, and every path that needs
/// the gateway reports the same honest error instead of being unreachable.
pub struct Offline;

impl DgClient for Offline {
    fn call_tool<'a>(&'a self, _tool: &'a str, _args: Value) -> BoxFuture<'a, DgResult<Value>> {
        Box::pin(async { Err(DgError::NotConnected) })
    }

    fn query<'a>(
        &'a self,
        _namespace: &'a str,
        _goal: &'a str,
        _limit: usize,
    ) -> BoxFuture<'a, DgResult<Vec<Value>>> {
        Box::pin(async { Err(DgError::NotConnected) })
    }

    fn describe(&self) -> String {
        "offline".to_string()
    }
}

/// The live conduit-backed client, authenticated by browser consent.
///
/// Built from a [`Grant`] obtained through conduit's authorization-code flow —
/// not from a machine credential, because Bench acts for a person and connects
/// to `/connect`, where the server binding is chosen at consent time.
pub struct ConduitClient {
    inner: datagrout_conduit::Client,
    label: String,
    /// The token provider, shared with `inner`. Held so a rotated grant can be
    /// handed back to the application for persisting.
    provider: AuthCodeProvider,
    /// Called with the new grant whenever a refresh rotates it.
    ///
    /// Without this, a refreshed grant lives only in memory: the file on disk
    /// keeps the consumed refresh token, and the next launch — or any second
    /// process reading the same file — fails with `invalid_grant`.
    on_refresh: Option<Arc<dyn Fn(Grant) + Send + Sync>>,
    /// Whether the MCP session handshake has completed.
    ///
    /// `ClientBuilder::build` only constructs a client; the MCP transport is
    /// stateful and needs `connect()` before any tool call, or the gateway
    /// answers "Session not initialized". Tracked here so the handshake happens
    /// lazily on first use and can be redone if the session is later dropped.
    connected: tokio::sync::Mutex<bool>,
}

impl ConduitClient {
    /// Connect using a stored grant.
    ///
    /// `url` is the MCP endpoint — typically `https://gateway.datagrout.ai/connect`.
    pub fn connect(url: &str, grant: Grant) -> DgResult<Self> {
        let provider = AuthCodeProvider::new(grant);
        let inner = datagrout_conduit::ClientBuilder::new()
            .url(url)
            .auth_authorization_code_provider(provider.clone())
            .build()
            .map_err(|e| DgError::Transport(e.to_string()))?;

        Ok(Self {
            inner,
            label: url.to_string(),
            provider,
            on_refresh: None,
            connected: tokio::sync::Mutex::new(false),
        })
    }

    /// Whether the held grant's access token is at or past expiry.
    ///
    /// Local check, no network. Lets an application decide to refresh at
    /// launch rather than reporting "connected" on a grant that will fail the
    /// moment it is used.
    pub async fn grant_is_expired(&self) -> bool {
        self.provider.grant().await.is_expired()
    }

    /// Force a refresh now, persisting the rotated grant through the handler.
    ///
    /// Used at launch when the saved grant is already expired: the refresh is a
    /// token-endpoint call (no credits), and finding out *now* that the sign-in
    /// is dead beats finding out after the user has built a chain.
    pub async fn refresh_now(&self) -> DgResult<()> {
        let http = reqwest::Client::new();
        self.provider
            .get_token(&http)
            .await
            .map_err(|e| classify("refresh", e))?;
        self.persist_if_refreshed().await;
        Ok(())
    }

    /// Register a handler for rotated grants. Applications should persist
    /// what they are handed; see [`on_refresh`](Self::on_refresh) for why.
    pub fn with_refresh_handler(mut self, handler: impl Fn(Grant) + Send + Sync + 'static) -> Self {
        self.on_refresh = Some(Arc::new(handler));
        self
    }

    /// Hand a rotated grant to the handler, if one rotated.
    async fn persist_if_refreshed(&self) {
        if let Some(grant) = self.provider.take_if_dirty().await {
            match &self.on_refresh {
                Some(handler) => handler(grant),
                None => tracing::warn!(
                    "conduit grant was refreshed but no handler is registered to persist it — \
                     the saved grant is now stale"
                ),
            }
        }
    }

    /// Complete the MCP handshake if it has not happened yet.
    async fn ensure_connected(&self) -> DgResult<()> {
        let mut connected = self.connected.lock().await;
        if *connected {
            return Ok(());
        }
        self.inner
            .connect()
            .await
            .map_err(|e| classify("connect", e))?;
        *connected = true;
        // The handshake is the first place a refresh can happen.
        self.persist_if_refreshed().await;
        Ok(())
    }

    /// Mark the session dead so the next call re-handshakes.
    async fn invalidate_session(&self) {
        *self.connected.lock().await = false;
    }

    /// The underlying conduit client, for calls this seam does not wrap.
    pub fn inner(&self) -> &datagrout_conduit::Client {
        &self.inner
    }
}

impl DgClient for ConduitClient {
    fn call_tool<'a>(&'a self, tool: &'a str, args: Value) -> BoxFuture<'a, DgResult<Value>> {
        Box::pin(async move {
            self.ensure_connected().await?;

            // `Client::call_tool` is a direct `tools/call`. NOT `perform`,
            // which routes through `discovery.perform`: that adds a 4-credit
            // tool premium per call and wraps the response in an extra
            // `result` envelope. At a 4 Hz analysis tick the premium alone is
            // 20 credits a second.
            let result = self
                .inner
                .call_tool(tool, args.clone())
                .await
                .map_err(|e| classify(tool, e));

            // A dropped session is recoverable: re-handshake and try once more.
            // Without this a single expired session bricks the app until
            // restart, which is exactly the failure a long-running instrument
            // must not have.
            let result = match result {
                Err(DgError::Tool { ref message, .. }) if is_session_lost(message) => {
                    self.invalidate_session().await;
                    self.ensure_connected().await?;
                    self.inner
                        .call_tool(tool, args)
                        .await
                        .map_err(|e| classify(tool, e))
                }
                other => other,
            };

            self.persist_if_refreshed().await;
            result
        })
    }

    fn query<'a>(
        &'a self,
        namespace: &'a str,
        goal: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, DgResult<Vec<Value>>> {
        Box::pin(async move {
            let out = self
                .call_tool(
                    tools::LOGIC_QUERY,
                    serde_json::json!({
                        "namespace": namespace,
                        "query": goal,
                        "limit": limit,
                    }),
                )
                .await?;

            Ok(out
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default())
        })
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}

/// Turn a conduit error into a Bench error, recognising the WebSocket case.
///
/// The gateway gates WS per-server on `interaction_config.enabled_protocols`
/// and answers a disabled upgrade with HTTP 400 rather than a failed handshake.
/// Without this arm that surfaces as an unexplained 400 — exactly the kind of
/// dead end worth naming once.
fn is_session_lost(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("session not initialized") || m.contains("session expired")
}

fn classify(tool: &str, err: datagrout_conduit::Error) -> DgError {
    let message = err.to_string();
    // The token endpoint's wording for a consumed or revoked refresh token.
    if message.contains("invalid_grant") {
        return DgError::SignInExpired;
    }
    if message.contains("enabled_protocols") || message.contains("WebSocket protocol not enabled") {
        return DgError::WebSocketDisabled;
    }
    DgError::Tool {
        tool: tool.to_string(),
        message,
    }
}

#[cfg(test)]
pub mod fake {
    //! A scripted client for tests.

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeClient {
        pub responses: HashMap<String, Value>,
        pub calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeClient {
        pub fn with(mut self, tool: &str, response: Value) -> Self {
            self.responses.insert(tool.to_string(), response);
            self
        }

        pub fn calls_to(&self, tool: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(t, _)| t == tool)
                .map(|(_, a)| a.clone())
                .collect()
        }
    }

    impl DgClient for FakeClient {
        fn call_tool<'a>(&'a self, tool: &'a str, args: Value) -> BoxFuture<'a, DgResult<Value>> {
            self.calls.lock().unwrap().push((tool.to_string(), args));
            let response = self.responses.get(tool).cloned();
            Box::pin(async move {
                response.ok_or_else(|| DgError::Tool {
                    tool: tool.to_string(),
                    message: "no scripted response".into(),
                })
            })
        }

        fn query<'a>(
            &'a self,
            _namespace: &'a str,
            goal: &'a str,
            _limit: usize,
        ) -> BoxFuture<'a, DgResult<Vec<Value>>> {
            let response = self.responses.get(goal).cloned();
            Box::pin(async move {
                Ok(response
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default())
            })
        }

        fn describe(&self) -> String {
            "fake".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeClient;
    use super::*;
    use serde_json::json;

    #[test]
    fn a_consumed_refresh_token_is_a_sign_in_problem_not_a_transport_one() {
        let err = classify(
            "connect",
            datagrout_conduit::Error::Auth(
                "token exchange failed (HTTP 400): {\"error\":\"invalid_grant\"}".into(),
            ),
        );
        assert!(matches!(err, DgError::SignInExpired));
        assert!(err.to_string().contains("sign in again"));
    }

    #[tokio::test]
    async fn offline_refuses_every_call() {
        let client = Offline;
        let err = client
            .call_tool(tools::FLOW_INTO, json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, DgError::NotConnected));
    }

    #[tokio::test]
    async fn fake_records_calls_and_replays_responses() {
        let client = FakeClient::default().with(tools::FLOW_INTO, json!({"skill_id": "skill_x"}));

        let out = client
            .call_tool(tools::FLOW_INTO, json!({"plan": []}))
            .await
            .unwrap();

        assert_eq!(out["skill_id"], json!("skill_x"));
        assert_eq!(client.calls_to(tools::FLOW_INTO).len(), 1);
    }
}
