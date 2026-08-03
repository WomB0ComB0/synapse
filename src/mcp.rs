//! Model Context Protocol (MCP) tool connector.
//!
//! When configured (`MCP_ENDPOINT`), [`ConnectorImpl::Http`] EXECUTES an auto-approved
//! tool call by POSTing an MCP-style JSON-RPC `tools/call` request to the endpoint and
//! returning the tool's `result`. When unconfigured, [`ConnectorImpl::Disabled`] carries
//! no client and the gateway keeps its placeholder behavior. Chosen ONCE at startup
//! ([`default_connector`]) and shared via [`crate::state::AppState`], like the embedder.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{Config, MCP_AUTH_TOKEN_MAX_BYTES};
use crate::error::{Error, Result};

const MCP_ACCEPT: &str = "application/json, text/event-stream";
const MCP_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// The process-wide tool connector, chosen at startup from config.
pub enum ConnectorImpl {
    /// No endpoint configured — tool execution stays a placeholder (dev/CI, no network).
    Disabled,
    /// Real MCP/HTTP connector.
    Http(HttpMcpConnector),
}

impl ConnectorImpl {
    /// Whether a real connector is wired (an endpoint is configured).
    pub fn is_enabled(&self) -> bool {
        matches!(self, ConnectorImpl::Http(_))
    }

    /// Whether the operator-declared connector credential scopes satisfy a tool policy.
    pub fn supports_scopes(&self, required: &[String]) -> bool {
        match self {
            ConnectorImpl::Disabled => required.is_empty(),
            ConnectorImpl::Http(connector) => required
                .iter()
                .all(|scope| connector.scopes.contains(scope)),
        }
    }

    /// Execute a tool call, returning its JSON `result`. For [`ConnectorImpl::Disabled`]
    /// this returns the placeholder note (the gateway does not call it — it branches on
    /// [`ConnectorImpl::is_enabled`] — but the arm keeps `call` total).
    pub async fn call(
        &self,
        tool_id: &str,
        arguments: &serde_json::Value,
        idempotency_key: &str,
    ) -> Result<serde_json::Value> {
        match self {
            ConnectorImpl::Disabled => Ok(serde_json::json!({
                "note": "no MCP connector configured; call recorded as executed",
                "tool_id": tool_id,
            })),
            ConnectorImpl::Http(c) => c.call(tool_id, arguments, idempotency_key).await,
        }
    }
}

/// Build the connector from config: [`ConnectorImpl::Http`] iff `MCP_ENDPOINT` is set,
/// else [`ConnectorImpl::Disabled`].
pub fn default_connector(config: &Config) -> ConnectorImpl {
    match &config.mcp_endpoint {
        Some(endpoint) if !endpoint.trim().is_empty() => {
            ConnectorImpl::Http(HttpMcpConnector::new(
                endpoint.clone(),
                config.mcp_auth_token.clone(),
                config.mcp_auth_token_file.clone(),
                config.production_mode,
                config.mcp_scopes.clone(),
                config.mcp_timeout_secs,
                config.mcp_max_retries,
            ))
        }
        _ => ConnectorImpl::Disabled,
    }
}

/// Real connector backed by an MCP-style JSON-RPC HTTP endpoint.
///
/// POSTs `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name","arguments"}}`
/// and returns the JSON-RPC `result`. A JSON-RPC `error`, a non-2xx status, or a
/// transport fault becomes [`Error::Upstream`] (→ 502). Never follows redirects (no
/// https→http downgrade / cleartext or auth leak) and never echoes the raw body.
pub struct HttpMcpConnector {
    client: reqwest::Client,
    endpoint: String,
    auth: ConnectorAuth,
    scopes: HashSet<String>,
    max_retries: u32,
}

impl HttpMcpConnector {
    /// Construct the connector (runs once at boot; a TLS-backend failure is a legit
    /// startup fault → `expect`).
    pub fn new(
        endpoint: String,
        auth_token: Option<String>,
        auth_token_file: Option<PathBuf>,
        production_mode: bool,
        scopes: Vec<String>,
        timeout_secs: u64,
        max_retries: u32,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .connect_timeout(Duration::from_secs(10))
            // A well-behaved JSON-RPC endpoint never 3xx's; following one could downgrade
            // https→http (cleartext egress of arguments) — fail closed to an Upstream error.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build reqwest client (TLS backend?)");
        Self {
            // trim() first so a copy-pasted trailing space doesn't defeat the slash trim.
            endpoint: endpoint.trim().trim_end_matches('/').to_string(),
            client,
            auth: match (auth_token, auth_token_file) {
                (Some(token), None) => ConnectorAuth::Static(token),
                (None, Some(path)) => ConnectorAuth::File {
                    path,
                    require_private: production_mode,
                },
                (None, None) => ConnectorAuth::None,
                (Some(_), Some(_)) => {
                    unreachable!("Config rejects simultaneous MCP token and token file")
                }
            },
            scopes: scopes.into_iter().collect(),
            max_retries,
        }
    }

    /// Execute one `tools/call` with bounded retry/backoff.
    async fn call(
        &self,
        tool_id: &str,
        arguments: &serde_json::Value,
        idempotency_key: &str,
    ) -> Result<serde_json::Value> {
        let body = request_body(tool_id, arguments);
        // Resolve once per logical call, outside the retry loop. Rotation takes effect on the next
        // call, while retries for one idempotent execution use one consistent credential.
        let auth_token = self.auth.resolve().await?;
        let mut attempt: u32 = 0;
        loop {
            // The `Idempotency-Key` is the SERVICE side of a two-sided contract: it is deterministic
            // for a given tool execution and STABLE across every retry (this connector's connect-phase
            // re-send AND the orchestrator's step-level re-drive), so a COMPLIANT MCP server can dedup
            // a re-sent call and return the original result instead of re-executing. This is what makes
            // retrying a possibly-executed (non-idempotent) tool call safe.
            let request = self
                .client
                .post(&self.endpoint)
                .header(reqwest::header::ACCEPT, MCP_ACCEPT)
                .header("Idempotency-Key", idempotency_key)
                .header("Mcp-Method", "tools/call")
                .header("Mcp-Name", tool_id)
                .json(&body);
            let request = match auth_token.as_deref() {
                Some(token) => request.bearer_auth(token),
                None => request,
            };
            match request.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        // The endpoint RECEIVED the request (it may already have executed the
                        // tool), so a non-2xx is TERMINAL — never retried, since re-sending a
                        // side-effecting tool call risks a duplicate execution. Only the status
                        // code is surfaced (no provider body / URL leak).
                        return Err(Error::Upstream(format!(
                            "MCP endpoint returned HTTP {}",
                            status.as_u16()
                        )));
                    }
                    // A 2xx means the tool ran; a body-read failure now is also terminal.
                    let (content_type, bytes) = read_bounded_response(resp).await?;
                    return parse_mcp_response(content_type.as_deref(), &bytes);
                }
                Err(e) => {
                    // Retry ONLY a connect-phase error — the request provably never reached the
                    // server, so re-sending cannot double-execute the tool. Any other transport
                    // fault (post-connect timeout/reset) is AMBIGUOUS (the server may have
                    // received + executed it) and so is terminal. Details go to the log, never
                    // to the client (no target-URL leak).
                    if e.is_connect() && attempt < self.max_retries {
                        let wait = backoff(attempt);
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    tracing::warn!(error = %e, "MCP request failed");
                    return Err(Error::Upstream("MCP request failed".into()));
                }
            }
        }
    }
}

/// Connector credential source. File-backed credentials are reloaded once per logical call to
/// support zero-downtime rotation through an atomic file replacement.
enum ConnectorAuth {
    None,
    Static(String),
    File {
        path: PathBuf,
        require_private: bool,
    },
}

impl ConnectorAuth {
    async fn resolve(&self) -> Result<Option<String>> {
        let raw = match self {
            Self::None => return Ok(None),
            Self::Static(token) => return Ok(Some(token.clone())),
            Self::File {
                path,
                require_private,
            } => {
                let file = tokio::fs::File::open(path).await.map_err(|error| {
                    tracing::error!(%error, "failed to open MCP connector credential file");
                    Error::Upstream("MCP connector credential unavailable".into())
                })?;
                let metadata = file.metadata().await.map_err(|error| {
                    tracing::error!(%error, "failed to inspect MCP connector credential file");
                    Error::Upstream("MCP connector credential unavailable".into())
                })?;
                if !metadata.is_file() || metadata.len() > MCP_AUTH_TOKEN_MAX_BYTES {
                    tracing::error!(
                        "MCP connector credential path is not a regular file or exceeds the size limit"
                    );
                    return Err(Error::Upstream(
                        "MCP connector credential unavailable".into(),
                    ));
                }
                #[cfg(unix)]
                if *require_private {
                    use std::os::unix::fs::PermissionsExt as _;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        tracing::error!(
                            "MCP connector credential file has group or other permissions"
                        );
                        return Err(Error::Upstream(
                            "MCP connector credential unavailable".into(),
                        ));
                    }
                }
                #[cfg(not(unix))]
                let _ = require_private;

                use tokio::io::AsyncReadExt as _;
                let mut raw = String::new();
                file.take(MCP_AUTH_TOKEN_MAX_BYTES + 1)
                    .read_to_string(&mut raw)
                    .await
                    .map_err(|error| {
                        tracing::error!(%error, "failed to read MCP connector credential file");
                        Error::Upstream("MCP connector credential unavailable".into())
                    })?;
                raw
            }
        };
        let token = raw.trim();
        if token.is_empty() || token.len() > MCP_AUTH_TOKEN_MAX_BYTES as usize {
            tracing::error!("MCP connector credential file is empty or exceeds the size limit");
            return Err(Error::Upstream(
                "MCP connector credential unavailable".into(),
            ));
        }
        if reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).is_err() {
            tracing::error!("MCP connector credential file contains an invalid header value");
            return Err(Error::Upstream(
                "MCP connector credential unavailable".into(),
            ));
        }
        Ok(Some(token.to_string()))
    }
}

/// Read one connector response with a hard cap so a faulty upstream cannot exhaust memory.
async fn read_bounded_response(
    mut response: reqwest::Response,
) -> Result<(Option<String>, Vec<u8>)> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if response
        .content_length()
        .is_some_and(|length| length > MCP_MAX_RESPONSE_BYTES.try_into().unwrap_or(u64::MAX))
    {
        tracing::warn!("MCP response exceeded the configured body limit");
        return Err(Error::Upstream("MCP response body is too large".into()));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        tracing::warn!(%error, "MCP response body read failed");
        Error::Upstream("MCP response body read failed".into())
    })? {
        if body.len().saturating_add(chunk.len()) > MCP_MAX_RESPONSE_BYTES {
            tracing::warn!("MCP response exceeded the configured body limit");
            return Err(Error::Upstream("MCP response body is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((content_type, body))
}

/// Parse either response encoding required by MCP Streamable HTTP.
fn parse_mcp_response(content_type: Option<&str>, bytes: &[u8]) -> Result<serde_json::Value> {
    let media_type = content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if media_type.eq_ignore_ascii_case("application/json") {
        return parse_jsonrpc_result(bytes);
    }
    if media_type.eq_ignore_ascii_case("text/event-stream") {
        return parse_sse_jsonrpc_result(bytes);
    }
    tracing::warn!(
        ?content_type,
        "MCP endpoint returned an unsupported content type"
    );
    Err(Error::Upstream(
        "MCP endpoint returned an unsupported content type".into(),
    ))
}

/// Extract the JSON-RPC response from an SSE stream. Notifications and comments may precede it.
fn parse_sse_jsonrpc_result(bytes: &[u8]) -> Result<serde_json::Value> {
    let stream = std::str::from_utf8(bytes).map_err(|error| {
        tracing::warn!(%error, "failed to decode MCP event stream");
        Error::Upstream("failed to decode MCP event stream".into())
    })?;
    let mut data = String::new();

    for raw_line in stream.lines().chain(std::iter::once("")) {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            if data.is_empty() {
                continue;
            }
            let message: serde_json::Value = serde_json::from_str(&data).map_err(|error| {
                tracing::warn!(%error, "failed to decode MCP event data");
                Error::Upstream("failed to decode MCP event stream".into())
            })?;
            data.clear();
            if message.get("result").is_some() || message.get("error").is_some() {
                return parse_jsonrpc_result(message.to_string().as_bytes());
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
    }

    Err(Error::Upstream(
        "MCP event stream contained no JSON-RPC response".into(),
    ))
}

/// The JSON-RPC 2.0 `tools/call` request body.
fn request_body(tool_id: &str, arguments: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool_id, "arguments": arguments },
    })
}

/// Parse a JSON-RPC 2.0 response: return `result`, or map `error` (and a malformed
/// envelope missing both) to [`Error::Upstream`].
fn parse_jsonrpc_result(bytes: &[u8]) -> Result<serde_json::Value> {
    let resp: JsonRpcResponse = serde_json::from_slice(bytes).map_err(|e| {
        // The raw body may carry provider/tool detail — log it, return a generic error.
        tracing::warn!(error = %e, "failed to decode MCP response");
        Error::Upstream("failed to decode MCP response".into())
    })?;
    if let Some(err) = resp.error {
        // Never echo the provider's message/code to the API caller — log it instead.
        tracing::warn!(code = err.code, message = %err.message, "MCP endpoint returned a JSON-RPC error");
        return Err(Error::Upstream("MCP endpoint returned an error".into()));
    }
    let result = resp
        .result
        .ok_or_else(|| Error::Upstream("MCP response had neither result nor error".into()))?;
    // A tools/call result with isError=true is a TOOL-level failure — surface it as a failed
    // execution, not a success (the gateway records it `failed`), so a tool-reported error is
    // never mislabeled as a successful governed execution.
    if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
        return Err(Error::Upstream("MCP tool reported an error".into()));
    }
    Ok(result)
}

/// Backoff milliseconds before retry `attempt`: `200ms · 2^attempt`, capped at 5s.
fn backoff_ms(attempt: u32) -> u64 {
    200u64.saturating_mul(1u64 << attempt.min(5)).min(5_000)
}

/// Exponential backoff: 200ms, 400ms, … capped at 5s.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(backoff_ms(attempt))
}

/// A true UPPER BOUND (seconds, rounded up) on the wall-clock duration of a single [`call`]:
/// `max_retries + 1` attempts each bounded by `timeout_secs`, PLUS the connect-phase retry
/// backoff sleeps performed before each retry (`backoff(0..max_retries)`). The crash-recovery
/// staleness window MUST exceed this so a live in-flight call is never reconciled as an orphan —
/// this is the single source of truth the config validation consumes (so the connector's own
/// timeout + backoff constants can't drift out of that safety check).
pub fn worst_case_call_secs(timeout_secs: u64, max_retries: u32) -> u64 {
    let attempts = timeout_secs.saturating_mul(max_retries as u64 + 1);
    // Closed form for the cumulative `backoff(0..max_retries)` sum (in ms), so an unbounded
    // user-set MCP_MAX_RETRIES can't spin a per-retry loop at startup. `backoff` caps at 5s from
    // attempt 5, so beyond 5 retries each adds a flat 5000ms: 0,200,600,1400,3000,6200, then +5000.
    let backoff_total_ms: u64 = match max_retries {
        0 => 0,
        1 => 200,
        2 => 600,
        3 => 1400,
        4 => 3000,
        5 => 6200,
        n => 6200u64.saturating_add((n as u64 - 5).saturating_mul(5000)),
    };
    attempts.saturating_add(backoff_total_ms.div_ceil(1000))
}

/// JSON-RPC 2.0 response envelope (only the fields we consume).
#[derive(serde::Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(serde::Deserialize)]
struct JsonRpcError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_auth_reloads_an_atomically_rotated_credential() {
        let dir = std::env::temp_dir().join(format!("synapse-mcp-auth-{}", uuid::Uuid::new_v4()));
        let path = dir.join("token");
        let replacement = dir.join("token.next");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(&path, "first-token\n").await.unwrap();

        let auth = ConnectorAuth::File {
            path: path.clone(),
            require_private: false,
        };
        assert_eq!(
            auth.resolve().await.unwrap().as_deref(),
            Some("first-token")
        );

        tokio::fs::write(&replacement, "second-token\n")
            .await
            .unwrap();
        tokio::fs::rename(&replacement, &path).await.unwrap();
        assert_eq!(
            auth.resolve().await.unwrap().as_deref(),
            Some("second-token")
        );

        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn file_auth_failure_is_generic_and_does_not_leak_contents() {
        let dir = std::env::temp_dir().join(format!("synapse-mcp-auth-{}", uuid::Uuid::new_v4()));
        let path = dir.join("token");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(&path, "secret\nheader-injection")
            .await
            .unwrap();

        let auth = ConnectorAuth::File {
            path: path.clone(),
            require_private: false,
        };
        let error = auth.resolve().await.unwrap_err().to_string();
        assert!(error.contains("MCP connector credential unavailable"));
        assert!(!error.contains("secret"));
        assert!(!error.contains(path.to_string_lossy().as_ref()));

        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn production_file_auth_rejects_rotated_file_with_broad_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("synapse-mcp-auth-{}", uuid::Uuid::new_v4()));
        let path = dir.join("token");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(&path, "secret-token").await.unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let auth = ConnectorAuth::File {
            path,
            require_private: true,
        };
        assert!(matches!(auth.resolve().await, Err(Error::Upstream(_))));

        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[test]
    fn worst_case_call_secs_includes_timeouts_and_backoff() {
        // 0 retries → a single attempt, no backoff.
        assert_eq!(worst_case_call_secs(30, 0), 30);
        // 2 retries → 3×30s timeouts + ceil(0.2+0.4)=1s backoff = 91s.
        assert_eq!(worst_case_call_secs(30, 2), 91);
        // 10 retries with a 1s timeout → 11s timeouts + 31.2s→32s capped backoff = 43s
        // (backoff caps at 5s from attempt 5 on).
        assert_eq!(worst_case_call_secs(1, 10), 43);
    }

    #[test]
    fn request_body_is_jsonrpc_tools_call() {
        let body = request_body("gmail.send", &serde_json::json!({ "to": "x@y.z" }));
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "gmail.send");
        assert_eq!(body["params"]["arguments"]["to"], "x@y.z");
    }

    #[test]
    fn parse_returns_result() {
        let bytes = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}],"isError":false}}"#;
        let out = parse_jsonrpc_result(bytes).unwrap();
        assert_eq!(out["content"][0]["text"], "ok");
        assert_eq!(out["isError"], false);
    }

    #[test]
    fn parse_returns_streamable_http_sse_result() {
        let bytes = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"isError\":false}}\n\n";
        let out = parse_mcp_response(Some("text/event-stream; charset=utf-8"), bytes).unwrap();
        assert_eq!(out["content"][0]["text"], "ok");
        assert_eq!(out["isError"], false);
    }

    #[test]
    fn sse_parser_skips_notifications_before_the_response() {
        let bytes = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"isError\":false}}\n\n";
        let out = parse_mcp_response(Some("text/event-stream"), bytes).unwrap();
        assert_eq!(out["isError"], false);
    }

    #[test]
    fn parser_accepts_json_content_type_parameters() {
        let bytes = br#"{"jsonrpc":"2.0","id":1,"result":{"isError":false}}"#;
        let out = parse_mcp_response(Some("application/json; charset=utf-8"), bytes).unwrap();
        assert_eq!(out["isError"], false);
    }

    #[test]
    fn parser_rejects_unsupported_content_type() {
        let bytes = br#"{"jsonrpc":"2.0","id":1,"result":{"isError":false}}"#;
        assert!(matches!(
            parse_mcp_response(Some("text/plain"), bytes),
            Err(Error::Upstream(_))
        ));
    }

    #[test]
    fn parse_maps_jsonrpc_error_to_upstream() {
        let bytes =
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        assert!(matches!(
            parse_jsonrpc_result(bytes),
            Err(Error::Upstream(_))
        ));
    }

    #[test]
    fn parse_maps_iserror_true_to_upstream() {
        // A tools/call result reporting a tool-level failure is not a success.
        let bytes = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"boom"}],"isError":true}}"#;
        assert!(matches!(
            parse_jsonrpc_result(bytes),
            Err(Error::Upstream(_))
        ));
    }

    #[test]
    fn upstream_messages_never_echo_provider_detail() {
        // The client-facing message must not carry the provider's JSON-RPC message/code or a
        // decoded body snippet (only a generic reason).
        let err = parse_jsonrpc_result(
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"SECRET-INTERNAL-DETAIL"}}"#,
        )
        .unwrap_err();
        assert!(!err.to_string().contains("SECRET-INTERNAL-DETAIL"));
    }

    #[test]
    fn parse_rejects_envelope_missing_both() {
        let bytes = br#"{"jsonrpc":"2.0","id":1}"#;
        assert!(matches!(
            parse_jsonrpc_result(bytes),
            Err(Error::Upstream(_))
        ));
    }

    #[test]
    fn parse_rejects_malformed_json() {
        assert!(matches!(
            parse_jsonrpc_result(b"not json"),
            Err(Error::Upstream(_))
        ));
    }
}
