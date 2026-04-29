// Status command - Get workload status
//
// Unified command that works in both modes:
//   - Nostr mode (--provider): queries a provider via Nostr
//   - HTTP mode (--server): queries a Paygress HTTP server

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use super::identity::{get_or_create_identity, parse_relays};
use crate::api::PaygressClient;

#[derive(Args)]
pub struct StatusArgs {
    /// Pod/workload ID to check
    #[arg(short, long)]
    pub pod_id: String,

    /// Provider npub (Nostr mode)
    #[arg(long)]
    pub provider: Option<String>,

    /// HTTP server URL (e.g., http://localhost:8080)
    #[arg(long)]
    pub server: Option<String>,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

pub async fn execute(args: StatusArgs, verbose: bool) -> Result<()> {
    if args.provider.is_some() {
        let provider = args.provider.clone().unwrap();
        return execute_nostr_status(args.pod_id.clone(), provider, args.relays.clone(), verbose)
            .await;
    }

    let server = args.server.clone().ok_or_else(|| {
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

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    spinner.set_message("Fetching pod status...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let client = PaygressClient::new(server);
    let response = client.get_pod_status(&args.pod_id).await?;
    spinner.finish_and_clear();

    if response.success {
        display_status(
            response.pod_id.as_deref().unwrap_or(&args.pod_id),
            response.status.as_deref().unwrap_or("Unknown"),
            response.ssh_host.as_deref(),
            response.ssh_port,
            response.ssh_username.as_deref(),
            response.expires_at.as_deref(),
            response.time_remaining_seconds.map(|t| t as u64),
        );
    } else {
        let error_msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow::anyhow!("Failed to get pod status: {}", error_msg));
    }

    Ok(())
}

async fn execute_nostr_status(
    pod_id: String,
    provider_npub: String,
    relays_opt: Option<String>,
    verbose: bool,
) -> Result<()> {
    use paygress::nostr::{
        NostrRelaySubscriber, RelayConfig, StatusRequestContent, StatusResponseContent,
    };

    if verbose {
        println!("{} Checking workload status via Nostr...", "->".blue());
        println!("  Provider: {}", provider_npub);
        println!("  Workload ID: {}", pod_id);
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    spinner.set_message("Connecting to Nostr and querying provider...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let nostr_key = get_or_create_identity(None)?;
    let relays = parse_relays(relays_opt);
    let relay_config = RelayConfig {
        relays,
        private_key: Some(nostr_key),
    };

    let client = NostrRelaySubscriber::new(relay_config).await?;

    client
        .subscribe_to_pod_events(|_| Box::pin(async { Ok(()) }))
        .await?;

    let request = StatusRequestContent {
        pod_id: pod_id.clone(),
    };
    let content = serde_json::to_string(&request)?;

    client
        .send_encrypted_private_message(&provider_npub, content, "nip17")
        .await?;

    spinner.set_message("Waiting for provider response...");

    match client.wait_for_decrypted_message(&provider_npub, 30).await {
        Ok(response_event) => {
            spinner.finish_and_clear();

            let status_resp: StatusResponseContent = serde_json::from_str(&response_event.content)?;

            display_status(
                &status_resp.pod_id,
                &status_resp.status,
                Some(&status_resp.ssh_host),
                Some(status_resp.ssh_port),
                Some(&status_resp.ssh_username),
                Some(&status_resp.expires_at),
                Some(status_resp.time_remaining_seconds),
            );
        }
        Err(e) => {
            spinner.finish_and_clear();
            return Err(anyhow::anyhow!(
                "Timed out waiting for status from provider: {}",
                e
            ));
        }
    }

    Ok(())
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
        if let Some(port) = ssh_port {
            if port != 0 && port != 22 {
                println!("  {} ssh {}@{} -p {}", "SSH:".bold(), username, host, port);
            } else {
                println!("  {} ssh {}@{}", "SSH:".bold(), username, host);
            }
        } else {
            println!("  {} ssh {}@{}", "SSH:".bold(), username, host);
        }
    }

    if let Some(expires) = expires_at {
        println!("  {} {}", "Expires:".bold(), expires);
    }

    if let Some(remaining) = time_remaining {
        if remaining > 0 {
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

            let time_colored = if remaining < 300 {
                time_str.red().to_string()
            } else if remaining < 600 {
                time_str.yellow().to_string()
            } else {
                time_str.green().to_string()
            };

            println!("  {} {}", "Time Left:".bold(), time_colored);
        } else {
            println!("  {} {}", "Time Left:".bold(), "Expired".red());
        }
    }
    println!();
}
