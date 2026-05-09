// Typed HTTP client for the agent-sandbox exec server
// (`images/agent-sandbox/server.py`).
//
// Used by:
//   - `paygress-cli exec`              (interactive shell convenience)
//   - `paygress-cli mcp` `run_command` (agent-driven exec)
//
// Wire format mirrors the server's:
//   POST http://<host>:<port>/exec
//   Authorization: Basic <base64(user:pass)>
//   Body: {"command": "<bash command>", "timeout_secs": 60, "working_dir": "/workspace"}
//   200 OK: {"stdout": "...", "stderr": "...", "exit_code": 0,
//            "duration_ms": 12, "timed_out": false}

use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct ExecRequest {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// Server response from POST /exec. Stable schema — agents and the
/// MCP `run_command` tool depend on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
}

/// Endpoint construction is deliberately schemed: a stray `https://`
/// in the host (because the user pasted a URL) shouldn't double-prefix.
/// We accept either bare host or full base URL and normalize.
fn normalize_endpoint(host: &str, port: u16) -> String {
    let h = host.trim();
    if h.starts_with("http://") || h.starts_with("https://") {
        // Already a URL — assume the user knows what port they want
        // and only append the port if the input has no port section.
        // Heuristic: if there's a colon AFTER the scheme delimiter,
        // there's already a port.
        if let Some(rest) = h
            .strip_prefix("http://")
            .or_else(|| h.strip_prefix("https://"))
        {
            let scheme = if h.starts_with("https://") {
                "https"
            } else {
                "http"
            };
            let host_part = rest.split('/').next().unwrap_or(rest);
            if host_part.contains(':') {
                return format!("{}://{}", scheme, rest.trim_end_matches('/'));
            }
            return format!("{}://{}:{}", scheme, rest.trim_end_matches('/'), port);
        }
    }
    format!("http://{}:{}", h, port)
}

/// HTTP Basic auth header value. Public so test harnesses can
/// build the same value the server expects.
pub fn basic_auth(user: &str, pass: &str) -> String {
    let creds = format!("{}:{}", user, pass);
    let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
    format!("Basic {}", encoded)
}

/// POST /exec on the agent-sandbox HTTP server. Returns the typed
/// response. `total_timeout` covers the full HTTP request including
/// the server-side command runtime — set it slightly above the
/// `timeout_secs` body field to allow the server to surface a
/// `timed_out: true` response rather than hitting the client's
/// transport timeout first.
pub async fn call_exec(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    command: &str,
    timeout_secs: Option<u64>,
    working_dir: Option<&str>,
    total_timeout: Duration,
) -> Result<ExecResponse> {
    let url = format!("{}/exec", normalize_endpoint(host, port));
    let body = ExecRequest {
        command: command.to_string(),
        timeout_secs,
        working_dir: working_dir.map(|s| s.to_string()),
    };
    let client = reqwest::Client::builder()
        .timeout(total_timeout)
        .build()
        .context("failed to build reqwest client")?;
    let resp = client
        .post(&url)
        .header("Authorization", basic_auth(user, pass))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {} failed", url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("exec server returned HTTP {}: {}", status, body);
    }
    resp.json::<ExecResponse>()
        .await
        .context("exec server response was not the expected JSON shape")
}

/// Health check for the exec server. Unauthenticated by design (so
/// the provider can liveness-probe). Returns Ok(()) on 2xx, error
/// otherwise.
pub async fn call_health(host: &str, port: u16, total_timeout: Duration) -> Result<()> {
    let url = format!("{}/health", normalize_endpoint(host, port));
    let client = reqwest::Client::builder()
        .timeout(total_timeout)
        .build()
        .context("failed to build reqwest client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {} failed", url))?;
    if !resp.status().is_success() {
        anyhow::bail!("health check returned HTTP {}", resp.status());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_bare_host_gets_http_prefix() {
        assert_eq!(normalize_endpoint("1.2.3.4", 8080), "http://1.2.3.4:8080");
        assert_eq!(
            normalize_endpoint("example.com", 8080),
            "http://example.com:8080"
        );
    }

    #[test]
    fn endpoint_keeps_explicit_scheme() {
        assert_eq!(
            normalize_endpoint("http://1.2.3.4", 8080),
            "http://1.2.3.4:8080"
        );
        assert_eq!(
            normalize_endpoint("https://example.com", 9090),
            "https://example.com:9090"
        );
    }

    #[test]
    fn endpoint_keeps_explicit_port_in_url() {
        // If the URL already has a port, we don't override it.
        assert_eq!(
            normalize_endpoint("http://1.2.3.4:7777", 8080),
            "http://1.2.3.4:7777"
        );
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        assert_eq!(
            normalize_endpoint("http://example.com/", 8080),
            "http://example.com:8080"
        );
    }

    #[test]
    fn basic_auth_matches_servers_python_format() {
        // Pin the format so a python-side change in server.py would
        // also need a Rust-side change: both must base64-encode
        // "user:pass" with standard alphabet.
        assert_eq!(basic_auth("root", "hunter2"), "Basic cm9vdDpodW50ZXIy");
    }

    #[test]
    fn exec_request_omits_optional_fields_when_none() {
        let r = ExecRequest {
            command: "ls".to_string(),
            timeout_secs: None,
            working_dir: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("timeout_secs"));
        assert!(!json.contains("working_dir"));
        assert!(json.contains(r#""command":"ls""#));
    }
}
