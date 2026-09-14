//! Signing in, and remembering that you did.
//!
//! Bench connects as a *person*, not as a machine: it opens a browser, the user
//! consents at the gateway, and Bench keeps the resulting grant. That is why it
//! can point at `/connect`, where the server binding is chosen at consent time
//! and lives in the token rather than the URL.
//!
//! # What gets stored, and why both halves
//!
//! [`Credentials`] holds the registered client **and** the grant. The client id
//! travels with its redirect URI because an authorization server matches
//! redirect URIs exactly — a saved id replayed against a freshly-chosen loopback
//! port is rejected, and that failure only surfaces later, once the first grant
//! can no longer be refreshed.

use std::path::{Path, PathBuf};

use datagrout_conduit::authcode::{loopback, AuthCodeFlow, Grant, RegisteredClient};
use serde::{Deserialize, Serialize};

/// The default gateway. `/connect` rather than a per-server URL: the binding is
/// chosen at consent.
pub const DEFAULT_GATEWAY: &str = "https://gateway.datagrout.ai/connect";

/// How long to wait for the user to finish consenting.
const CONSENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Where the sign-in flow has got to.
///
/// Reported as it happens because the phases before the browser opens —
/// metadata discovery and dynamic client registration — are two network round
/// trips, and a UI that says nothing during them looks broken rather than busy.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Fetching protected-resource and authorization-server metadata.
    Discovering,
    /// Registering this application with the authorization server.
    Registering,
    /// Reusing a client registered on a previous run.
    ReusingClient,
    /// The consent URL is ready; the user has to visit it.
    AwaitingConsent(String),
    /// Redeeming the authorization code.
    Exchanging,
}

/// Everything Bench needs to reconnect without asking again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub gateway: String,
    pub client: RegisteredClient,
    pub grant: Grant,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("sign-in failed: {0}")]
    Flow(#[from] datagrout_conduit::authcode::AuthCodeError),
    /// The gateway accepted the consent but could not bind the saved
    /// registration to the chosen server (HTTP 5xx at the token exchange).
    ///
    /// Known trigger: re-authorizing an existing client against a different
    /// server than it was first bound to. The saved registration is unusable
    /// against that server; callers should discard it so the next Connect
    /// registers afresh — the GUI does this itself.
    #[error(
        "the gateway could not bind the saved registration to that server (HTTP {status}); \
         the saved sign-in has been discarded — press Connect again to register afresh. \
         Detail: {body}"
    )]
    RegistrationRejected { status: u16, body: String },
    #[error("could not read or write credentials: {0}")]
    Io(#[from] std::io::Error),
    #[error("stored credentials are unreadable: {0}")]
    Corrupt(#[from] serde_json::Error),
}

/// Where credentials live on disk.
///
/// A file, not a keychain — deliberately simple and deliberately called out:
/// the refresh token in here is a long-lived credential, and a real product
/// should move this to the OS keychain. Kept obvious rather than hidden behind
/// an abstraction that implies more safety than it provides.
pub fn credentials_path() -> PathBuf {
    crate::paths::config_dir()
        .join("bench")
        .join("credentials.json")
}

/// Load saved credentials, if any are readable.
///
/// A corrupt file is reported rather than swallowed: silently re-running the
/// browser flow would hide a real problem and mint a duplicate client record.
pub fn load(path: &Path) -> Result<Option<Credentials>, AuthError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write credentials, creating the directory if needed.
pub fn save(path: &Path, credentials: &Credentials) -> Result<(), AuthError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(credentials)?)?;
    Ok(())
}

/// Replace only the grant in the saved credentials, keeping the registration.
///
/// This is what a refresh handler should call. DataGrout rotates refresh
/// tokens, so after any refresh the token on disk has been consumed; if it is
/// not replaced, the next process to load the file fails with `invalid_grant`
/// and the user is sent back through the browser for no reason. The registered
/// client is unaffected and must be kept — re-registering leaves spare records.
pub fn save_refreshed_grant(path: &Path, grant: Grant) -> Result<(), AuthError> {
    let Some(mut credentials) = load(path)? else {
        // Nothing to update: the user signed out between the refresh and now.
        return Ok(());
    };
    credentials.grant = grant;
    save(path, &credentials)
}

/// Forget the saved sign-in.
pub fn clear(path: &Path) -> Result<(), AuthError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Run the full browser-consent flow.
///
/// `on_progress` is called at each phase — a GUI updates a status line and
/// opens a browser on [`Progress::AwaitingConsent`]; a CLI prints the URL.
/// Bench does not decide how a URL gets in front of a user.
///
/// Reuses `existing` when supplied, re-binding its exact redirect port. If that
/// port is taken, a new client is registered instead: authorizing against a URI
/// the server will reject is worse than leaving a spare client record behind.
pub async fn sign_in<F>(
    gateway: &str,
    existing: Option<RegisteredClient>,
    mut on_progress: F,
) -> Result<Credentials, AuthError>
where
    F: FnMut(Progress),
{
    let (listener, existing) = match &existing {
        Some(client) => match loopback::Listener::bind_for(&client.redirect_uri).await {
            Ok(listener) => (listener, existing.clone()),
            Err(_) => (loopback::Listener::bind().await?, None),
        },
        None => (loopback::Listener::bind().await?, None),
    };

    on_progress(Progress::Discovering);
    let mut flow = AuthCodeFlow::discover(gateway).await?;

    let client_was_reused = existing.is_some();
    let (flow, client) = match existing {
        Some(client) => {
            on_progress(Progress::ReusingClient);
            (flow.with_registered_client(client.clone()), client)
        }
        None => {
            on_progress(Progress::Registering);
            let client = flow.register("Bench", listener.redirect_uri()).await?;
            (flow, client)
        }
    };

    let (url, pending) = flow.authorize_url()?;
    on_progress(Progress::AwaitingConsent(url));

    let redirect = listener.wait(CONSENT_TIMEOUT).await?;

    on_progress(Progress::Exchanging);
    let reused_registration = client_was_reused;
    let grant = match flow
        .exchange(pending, &redirect.code, &redirect.state)
        .await
    {
        Ok(grant) => grant,
        // A 5xx at the exchange after the user approved is the gateway failing
        // to *bind* an existing registration to the server they just chose —
        // the known case is a client re-authorized against a different server
        // than last time. A fresh registration sidesteps it, so say so rather
        // than presenting an opaque server error.
        Err(datagrout_conduit::authcode::AuthCodeError::TokenExchange { status, body })
            if reused_registration && (500..600).contains(&status) =>
        {
            return Err(AuthError::RegistrationRejected { status, body });
        }
        Err(e) => return Err(e.into()),
    };

    Ok(Credentials {
        gateway: gateway.to_string(),
        client,
        grant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            gateway: DEFAULT_GATEWAY.into(),
            client: RegisteredClient {
                client_id: "client_abc".into(),
                redirect_uri: "http://127.0.0.1:8765/callback".into(),
            },
            grant: Grant {
                access_token: "at".into(),
                refresh_token: Some("rt".into()),
                expires_at: Some(1_800_000_000),
                client_id: "client_abc".into(),
                token_endpoint: "https://gateway.datagrout.ai/oauth/token".into(),
                scope: None,
                resource: None,
            },
        }
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("bench-auth-{}", std::process::id()));
        let path = dir.join("credentials.json");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            load(&path).unwrap().is_none(),
            "missing file is not an error"
        );

        save(&path, &credentials()).unwrap();
        let loaded = load(&path).unwrap().unwrap();

        assert_eq!(loaded.client.client_id, "client_abc");
        // The redirect URI must survive: a client id without it cannot be reused.
        assert_eq!(loaded.client.redirect_uri, "http://127.0.0.1:8765/callback");
        assert_eq!(loaded.grant.refresh_token.as_deref(), Some("rt"));

        clear(&path).unwrap();
        assert!(load(&path).unwrap().is_none());
        clear(&path).unwrap(); // clearing twice is not an error

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refreshed_grant_replaces_only_the_grant() {
        let dir = std::env::temp_dir().join(format!("bench-auth-refresh-{}", std::process::id()));
        let path = dir.join("credentials.json");
        let _ = std::fs::remove_dir_all(&dir);
        save(&path, &credentials()).unwrap();

        let rotated = Grant {
            access_token: "at2".into(),
            refresh_token: Some("rt2".into()),
            ..credentials().grant
        };
        save_refreshed_grant(&path, rotated).unwrap();

        let loaded = load(&path).unwrap().unwrap();
        // The rotated tokens are on disk…
        assert_eq!(loaded.grant.access_token, "at2");
        assert_eq!(loaded.grant.refresh_token.as_deref(), Some("rt2"));
        // …and the registration was not touched.
        assert_eq!(loaded.client.client_id, "client_abc");
        assert_eq!(loaded.client.redirect_uri, "http://127.0.0.1:8765/callback");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refreshing_after_sign_out_is_not_an_error() {
        let path =
            std::env::temp_dir().join(format!("bench-auth-none-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // The user signed out between the refresh and its persistence.
        save_refreshed_grant(&path, credentials().grant).unwrap();
        assert!(
            !path.exists(),
            "must not resurrect a sign-in the user removed"
        );
    }

    #[test]
    fn a_corrupt_file_is_reported_not_swallowed() {
        let dir = std::env::temp_dir().join(format!("bench-auth-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(&path, "{not json").unwrap();

        // Silently re-running the browser flow would hide the problem and mint
        // a duplicate client record.
        assert!(matches!(load(&path), Err(AuthError::Corrupt(_))));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credentials_path_is_under_a_config_directory() {
        let path = credentials_path();
        assert!(path.ends_with("bench/credentials.json"));
    }
}
