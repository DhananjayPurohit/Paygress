// `paygress-cli status` — query a workload's lease, either via Nostr
// (`--provider`) or against a Paygress HTTP server (`--server`).

use anyhow::Result;
use clap::Args;
use colored::Colorize;

use crate::api::PaygressClient;
use crate::util::{get_or_create_identity, parse_relays, spinner};

#[derive(Args)]
pub struct StatusArgs {
    /// Pod/workload ID to check
    #[arg(short, long)]
    pub pod_id: String,

    /// Provider ID (Nostr mode)
    #[arg(long)]
    pub provider: Option<String>,

    /// HTTP server URL (e.g., http://localhost:8080)
    #[arg(long)]
    pub server: Option<String>,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

pub async fn execute(mut args: StatusArgs, verbose: bool) -> Result<()> {
    if let Some(provider) = args.provider.take() {
        return execute_nostr_status(args.pod_id, provider, args.relays, verbose).await;
    }

    let server = args.server.take().ok_or_else(|| {
        anyhow::anyhow!("Either --provider (Nostr) or --server (HTTP) is required")
    })?;

    execute_http_status(&server, args, verbose).await
}

async fn execute_http_status(server: &str, args: StatusArgs, verbose: bool) -> Result<()> {
    if verbose {
        println!("{} Checking pod status via HTTP...", "->".blue());
        println!("  Server: {}", server);
        println!("  Pod ID: {}", args.pod_id);
    }

    let spinner = spinner("Fetching pod status...");
    let client = PaygressClient::new(server);
    let response = client.get_pod_status(&args.pod_id).await?;
    spinner.finish_and_clear();

    if !response.success {
        let error_msg = response.error.as_deref().unwrap_or("Unknown error");
        return Err(anyhow::anyhow!("Failed to get pod status: {}", error_msg));
    }

    display_status(
        response.pod_id.as_deref().unwrap_or(&args.pod_id),
        response.status.as_deref().unwrap_or("Unknown"),
        response.ssh_host.as_deref(),
        response.ssh_port,
        response.ssh_username.as_deref(),
        response.expires_at.as_deref(),
        response.time_remaining_seconds.map(|t| t as u64),
    );

    Ok(())
}

/// Shared by the CLI pretty-printer and the MCP server.
#[derive(Debug, Clone)]
pub enum NostrStatusOutcome {
    Success(paygress::nostr::StatusResponseContent),
    /// Provider refused, and said why: an unknown workload, a lease that has
    /// expired, a caller who does not own it.
    ProviderError(paygress::nostr::ErrorResponseContent),
    /// Provider replied, but not with a status response we recognize.
    UnparseableResponse(String),
    Timeout,
}

/// Sort a provider's reply into an outcome. An error reply is a normal answer
/// to a status request -- ask about a workload that does not exist and this is
/// what comes back -- so it is tried before giving up on the body, the way the
/// spawn and topup round trips already do.
fn classify_status_response(content: String) -> NostrStatusOutcome {
    use paygress::nostr::{ErrorResponseContent, StatusResponseContent};

    if let Ok(status) = serde_json::from_str::<StatusResponseContent>(&content) {
        NostrStatusOutcome::Success(status)
    } else if let Ok(err) = serde_json::from_str::<ErrorResponseContent>(&content) {
        NostrStatusOutcome::ProviderError(err)
    } else {
        NostrStatusOutcome::UnparseableResponse(content)
    }
}

/// No stdout I/O — pure round-trip plus structured outcome.
pub async fn nostr_status_round_trip(
    pod_id: &str,
    provider_npub: &str,
    relays: Vec<String>,
    nostr_key: String,
    timeout_secs: u64,
) -> Result<NostrStatusOutcome> {
    use paygress::discovery::DiscoveryClient;
    use paygress::nostr::StatusRequestContent;

    let client = DiscoveryClient::new_with_key(relays, nostr_key).await?;

    let request = StatusRequestContent {
        pod_id: pod_id.to_string(),
    };
    let request_json = serde_json::to_string(&request)?;

    client
        .nostr()
        .send_encrypted_private_message(provider_npub, request_json, "nip04")
        .await?;

    match client
        .nostr()
        .wait_for_decrypted_message(provider_npub, timeout_secs)
        .await
    {
        Ok(response) => Ok(classify_status_response(response.content)),
        Err(_) => Ok(NostrStatusOutcome::Timeout),
    }
}

async fn execute_nostr_status(
    pod_id: String,
    provider_npub: String,
    relays_opt: Option<String>,
    verbose: bool,
) -> Result<()> {
    if verbose {
        println!("{} Checking workload status via Nostr...", "->".blue());
        println!("  Provider: {}", provider_npub);
        println!("  Workload ID: {}", pod_id);
    }

    let spinner = spinner("Connecting to Nostr and querying provider...");

    let nostr_key = get_or_create_identity(None)?;
    let relays = parse_relays(relays_opt);

    let outcome = nostr_status_round_trip(&pod_id, &provider_npub, relays, nostr_key, 30).await?;
    spinner.finish_and_clear();

    match outcome {
        NostrStatusOutcome::Success(status_resp) => {
            display_status(
                &status_resp.pod_id,
                &status_resp.status,
                Some(&status_resp.ssh_host),
                Some(status_resp.ssh_port),
                Some(&status_resp.ssh_username),
                Some(&status_resp.expires_at),
                Some(status_resp.time_remaining_seconds),
            );
            Ok(())
        }
        NostrStatusOutcome::ProviderError(err) => {
            println!("{}", "Status unavailable".red().bold());
            println!("  Type:    {}", err.error_type);
            println!("  Message: {}", err.message);
            if let Some(details) = err.details.as_deref() {
                println!("  Details: {}", details);
            }
            println!();
            Ok(())
        }
        NostrStatusOutcome::UnparseableResponse(body) => Err(anyhow::anyhow!(
            "Provider returned an unrecognized status response (forward-compat schema?): {}",
            body
        )),
        NostrStatusOutcome::Timeout => Err(anyhow::anyhow!(
            "Timed out waiting for status from provider"
        )),
    }
}

fn display_status(
    pod_id: &str,
    status: &str,
    ssh_host: Option<&str>,
    ssh_port: Option<u16>,
    ssh_username: Option<&str>,
    expires_at: Option<&str>,
    time_remaining: Option<u64>,
) {
    println!("{}", "Workload Status".bold());
    println!();

    println!("  {} {}", "ID:".bold(), pod_id);

    let status_colored = match status {
        "Running" | "Active" => status.green().to_string(),
        "Pending" | "Starting" => status.yellow().to_string(),
        "Failed" | "Error" => status.red().to_string(),
        "Terminated" | "Expired" => status.dimmed().to_string(),
        _ => status.to_string(),
    };
    println!("  {} {}", "Status:".bold(), status_colored);

    if let Some(host) = ssh_host {
        let username = ssh_username.unwrap_or("root");
        match ssh_port {
            Some(port) if port != 0 && port != 22 => {
                println!("  {} ssh {}@{} -p {}", "SSH:".bold(), username, host, port)
            }
            _ => println!("  {} ssh {}@{}", "SSH:".bold(), username, host),
        }
    }

    if let Some(expires) = expires_at {
        println!("  {} {}", "Expires:".bold(), expires);
    }

    if let Some(remaining) = time_remaining {
        println!("  {} {}", "Time Left:".bold(), format_time_left(remaining));
    }
    println!();
}

fn format_time_left(remaining: u64) -> String {
    if remaining == 0 {
        return "Expired".red().to_string();
    }

    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    let seconds = remaining % 60;

    let time_str = if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    };

    if remaining < 300 {
        time_str.red().to_string()
    } else if remaining < 600 {
        time_str.yellow().to_string()
    } else {
        time_str.green().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from a live provider: asking about a workload it does not have.
    /// This used to surface as "unrecognized status response".
    const NOT_FOUND: &str = r#"{"error_type":"not_found","message":"Workload 9999 not found or you don't have access","details":null}"#;

    #[test]
    fn provider_error_is_reported_as_an_error_not_an_unparseable_body() {
        match classify_status_response(NOT_FOUND.to_string()) {
            NostrStatusOutcome::ProviderError(err) => {
                assert_eq!(err.error_type, "not_found");
                assert!(err.message.contains("9999"));
            }
            other => panic!("expected ProviderError, got {:?}", other),
        }
    }

    #[test]
    fn status_response_still_wins_over_the_error_shape() {
        let body = r#"{"pod_id":"2000","status":"Running","expires_at":"2026-09-17T13:00:00Z",
            "time_remaining_seconds":600,"cpu_millicores":4000,"memory_mb":8192,
            "ssh_host":"72.61.173.244","ssh_port":2000,"ssh_username":"root"}"#;

        match classify_status_response(body.to_string()) {
            NostrStatusOutcome::Success(s) => assert_eq!(s.pod_id, "2000"),
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[test]
    fn a_body_of_neither_shape_stays_unparseable() {
        match classify_status_response("{\"hello\":\"world\"}".to_string()) {
            NostrStatusOutcome::UnparseableResponse(body) => assert!(body.contains("hello")),
            other => panic!("expected UnparseableResponse, got {:?}", other),
        }
    }
}
