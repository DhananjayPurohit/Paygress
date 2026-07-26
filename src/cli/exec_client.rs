// Typed HTTP client for the agent-sandbox exec server
// (`images/agent-sandbox/server.py`).
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

/// Where to reach an exec server, and how to authenticate to it.
#[derive(Debug, Clone, Copy)]
pub struct ExecTarget<'a> {
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
    pub pass: &'a str,
}

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

/// Accept either a bare host or a full base URL so a pasted URL
/// doesn't end up double-prefixed or double-ported.
fn normalize_endpoint(host: &str, port: u16) -> String {
    let h = host.trim();
    let (scheme, rest) = if let Some(r) = h.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = h.strip_prefix("http://") {
        ("http", r)
    } else {
        return format!("http://{}:{}", h, port);
    };

    let rest = rest.trim_end_matches('/');
    let host_part = rest.split('/').next().unwrap_or(rest);
    if host_part.contains(':') {
        format!("{}://{}", scheme, rest)
    } else {
        format!("{}://{}:{}", scheme, rest, port)
    }
}

/// HTTP Basic auth header value. Public so test harnesses can
/// build the same value the server expects.
pub fn basic_auth(user: &str, pass: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, pass));
    format!("Basic {}", encoded)
}

/// POST /exec on the agent-sandbox HTTP server. `total_timeout` covers
/// the full request including the server-side command runtime — set it
/// slightly above `request.timeout_secs` so the server can return a
/// structured `timed_out: true` before our transport gives up.
pub async fn call_exec(
    target: ExecTarget<'_>,
    request: &ExecRequest,
    total_timeout: Duration,
) -> Result<ExecResponse> {
    let url = format!("{}/exec", normalize_endpoint(target.host, target.port));
    let client = reqwest::Client::builder()
        .timeout(total_timeout)
        .build()
        .context("failed to build reqwest client")?;
    let resp = client
        .post(&url)
        .header("Authorization", basic_auth(target.user, target.pass))
        .json(request)
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
        // Pin the format: a python-side change in server.py must also
        // change this side. Both base64-encode "user:pass".
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
