//! The local HTTP surface.
//!
//! Three routes, deliberately few:
//!
//! | route | purpose |
//! |---|---|
//! | `GET  /api/query?ns=&prolog=&limit=` | LC query proxy |
//! | `POST /api/action` | LC assert proxy |
//! | `POST /skills/:slug` | invoke a Bench-minted skill |
//!
//! The first two are the conventional DataGrout local-proxy contract, so a
//! reactor app written against any host offering it runs unchanged here. The
//! third is Bench's own: once a chain is minted, other local apps hit
//! `127.0.0.1:<port>/skills/<slug>` with no auth ceremony while you develop an
//! integration.
//!
//! # Binding and trust
//!
//! Binds **loopback only**. There is no authentication here on purpose — the
//! security boundary is the loopback interface plus the grant held by the
//! process. Do not make the bind address configurable without adding auth
//! first: this proxy will happily spend the logged in user's credits.

use std::net::SocketAddr;
use std::sync::Arc;

use bench_core::dg::{tools, DgClient};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A skill published to the local surface.
#[derive(Debug, Clone)]
pub struct PublishedSkill {
    /// URL segment, e.g. `spectrum-check`.
    pub slug: String,
    /// The `skill_id` returned when the chain was minted.
    pub skill_id: String,
    pub description: String,
}

/// Server configuration.
pub struct ServeConfig {
    pub port: u16,
    pub namespace: String,
    pub skills: Vec<PublishedSkill>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            // 0 = let the OS choose. Bench reports the bound port rather than
            // fighting whatever else owns a fixed one.
            port: 0,
            namespace: bench_core::DEFAULT_NAMESPACE.to_string(),
            skills: Vec::new(),
        }
    }
}

pub struct Server {
    config: Arc<ServeConfig>,
    client: Arc<dyn DgClient>,
}

impl Server {
    pub fn new(config: ServeConfig, client: Arc<dyn DgClient>) -> Self {
        Self {
            config: Arc::new(config),
            client,
        }
    }

    /// Bind and serve until dropped. Returns the bound address immediately via
    /// `on_bound` so the caller can display the real port when it chose 0.
    pub async fn run<F>(self, on_bound: F) -> anyhow::Result<()>
    where
        F: FnOnce(SocketAddr),
    {
        let listener = TcpListener::bind(("127.0.0.1", self.config.port)).await?;
        on_bound(listener.local_addr()?);

        loop {
            let (stream, _) = listener.accept().await?;
            let config = Arc::clone(&self.config);
            let client = Arc::clone(&self.client);
            tokio::spawn(async move {
                if let Err(e) = handle(stream, config, client).await {
                    tracing::debug!("bench-serve connection ended: {e}");
                }
            });
        }
    }
}

async fn handle(
    mut stream: TcpStream,
    config: Arc<ServeConfig>,
    client: Arc<dyn DgClient>,
) -> anyhow::Result<()> {
    let Some(req) = read_request(&mut stream).await? else {
        return Ok(());
    };

    let response = route(&req, &config, client.as_ref()).await;
    let (status, body) = match response {
        Ok(v) => (200, v),
        Err(msg) => (400, json!({ "error": msg })),
    };

    write_json(&mut stream, status, &body).await
}

async fn route(
    req: &Request,
    config: &ServeConfig,
    client: &dyn DgClient,
) -> Result<Value, String> {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/query") => {
            let ns = req
                .query
                .get("ns")
                .cloned()
                .unwrap_or_else(|| config.namespace.clone());
            let goal = req.query.get("prolog").ok_or("missing `prolog`")?;
            let limit = req
                .query
                .get("limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(200);

            client
                .query(&ns, goal, limit)
                .await
                .map(|rows| json!({ "results": rows }))
                .map_err(|e| e.to_string())
        }

        ("POST", "/api/action") => {
            let body: Value = serde_json::from_str(&req.body).map_err(|e| e.to_string())?;
            let ns = body
                .get("ns")
                .and_then(Value::as_str)
                .unwrap_or(&config.namespace);

            // Accept either shape a reactor client sends: structured `facts`
            // or a raw `prolog` term.
            let ops = match (body.get("facts"), body.get("prolog")) {
                (Some(facts), _) => json!([{ "op": "assert", "facts": facts }]),
                (None, Some(prolog)) => json!([{ "op": "assert_raw", "prolog": prolog }]),
                _ => return Err("body needs `facts` or `prolog`".into()),
            };

            client
                .call_tool(tools::LOGIC_BATCH, json!({ "namespace": ns, "ops": ops }))
                .await
                .map(|v| json!({ "ok": true, "result": v }))
                .map_err(|e| e.to_string())
        }

        ("POST", path) if path.starts_with("/skills/") => {
            let slug = &path["/skills/".len()..];
            let skill = config
                .skills
                .iter()
                .find(|s| s.slug == slug)
                .ok_or_else(|| format!("no skill published at /skills/{slug}"))?;

            let inputs: Value = if req.body.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&req.body).map_err(|e| e.to_string())?
            };

            client
                .call_tool(
                    tools::TOOLSMITH_INVOKE,
                    json!({ "skill_id": skill.skill_id, "inputs": inputs }),
                )
                .await
                .map_err(|e| e.to_string())
        }

        ("GET", "/skills") => Ok(json!({
            "skills": config.skills.iter().map(|s| json!({
                "slug": s.slug,
                "skill_id": s.skill_id,
                "description": s.description,
                "url": format!("/skills/{}", s.slug),
            })).collect::<Vec<_>>()
        })),

        ("GET", "/health") => Ok(json!({ "ok": true, "gateway": client.describe() })),

        _ => Err(format!("no route for {} {}", req.method, req.path)),
    }
}

// ── a very small HTTP/1.1 reader ─────────────────────────────────────────────
//
// Hand-rolled rather than pulling in a framework: three routes on loopback do
// not justify the dependency, and it keeps the promotion story simple.

#[derive(Debug, Default)]
struct Request {
    method: String,
    path: String,
    query: std::collections::BTreeMap<String, String>,
    body: String,
}

async fn read_request(stream: &mut TcpStream) -> anyhow::Result<Option<Request>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    // Read until headers are complete.
    let header_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        // A request whose headers never end is either broken or hostile.
        if buf.len() > 64 * 1024 {
            anyhow::bail!("header section too large");
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();

    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let content_length = lines
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);

    // Read the rest of the body if the headers promised more than arrived.
    let mut body_bytes = buf[header_end + 4..].to_vec();
    while body_bytes.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&chunk[..n]);
    }

    let (path, query) = split_target(&target);

    Ok(Some(Request {
        method,
        path,
        query,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    }))
}

fn split_target(target: &str) -> (String, std::collections::BTreeMap<String, String>) {
    let mut query = std::collections::BTreeMap::new();
    let (path, qs) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    for pair in qs.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(k), percent_decode(v));
    }
    (path.to_string(), query)
}

/// Decode `%XX` escapes and `+` as space. Prolog goals arrive URL-encoded and
/// are full of parens, commas and quotes, so this is not optional.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(body)?;
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         access-control-allow-origin: *\r\n\
         connection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_path_and_decodes_query() {
        let (path, query) = split_target("/api/query?ns=bench&prolog=metric%28C%2C+rms%2C+V%29");
        assert_eq!(path, "/api/query");
        assert_eq!(query.get("ns").unwrap(), "bench");
        // A Prolog goal must survive URL encoding intact.
        assert_eq!(query.get("prolog").unwrap(), "metric(C, rms, V)");
    }

    #[test]
    fn decodes_quotes_in_goals() {
        assert_eq!(
            percent_decode("spec_pass%28%27a_b%27%2C+c%29"),
            "spec_pass('a_b', c)"
        );
    }

    #[test]
    fn leaves_a_trailing_stray_percent_alone() {
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn finds_header_terminator() {
        assert_eq!(
            find_subslice(b"GET / HTTP/1.1\r\n\r\nbody", b"\r\n\r\n"),
            Some(14)
        );
        assert_eq!(find_subslice(b"no terminator", b"\r\n\r\n"), None);
    }
}
