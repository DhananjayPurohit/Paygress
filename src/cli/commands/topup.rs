// Topup command - Extend workload lifetime with additional payment
//
// Unified command that works in both modes:
//   - Nostr mode (default): sends encrypted topup to a provider via Nostr
//   - HTTP mode (--server): calls a Paygress HTTP server directly

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use super::identity::{get_or_create_identity, parse_relays};
use crate::api::{PaygressClient, TopupRequest};
use paygress::discovery::DiscoveryClient;

#[derive(Args)]
pub struct TopupArgs {
    /// Pod/workload ID to top up
    #[arg(short, long)]
    pub pod_id: String,

    /// Cashu token for payment
    #[arg(short = 'k', long)]
    pub token: String,

    /// Provider npub (Nostr mode) - if omitted, uses --server for HTTP mode
    #[arg(long)]
    pub provider: Option<String>,

    /// HTTP server URL (e.g., http://localhost:8080) - used when --provider is not set
    #[arg(long)]
    pub server: Option<String>,

    /// Your Nostr private key (nsec) - uses ~/.paygress/identity if not provided
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

pub async fn execute(args: TopupArgs, verbose: bool) -> Result<()> {
    if args.provider.is_some() {
        let provider = args.provider.clone().unwrap();
        return execute_nostr_topup(provider, args, verbose).await;
    }

    let server = args.server.clone().ok_or_else(|| {
        anyhow::anyhow!("Either --provider (Nostr) or --server (HTTP) is required")
    })?;

    execute_http_topup(&server, args, verbose).await
}

async fn execute_http_topup(server: &str, args: TopupArgs, verbose: bool) -> Result<()> {
    if verbose {
        println!("{} Topping up pod via HTTP...", "->".blue());
        println!("  Server: {}", server);
        println!("  Pod ID: {}", args.pod_id);
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    spinner.set_message("Processing top-up payment...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let client = PaygressClient::new(server);

    let request = TopupRequest {
        pod_id: args.pod_id.clone(),
        cashu_token: Some(args.token),
    };

    let response = client.topup_pod(request).await?;
    spinner.finish_and_clear();

    if response.success {
        println!("{}", "Pod topped up successfully!".green().bold());
        println!();

        if let Some(pod_id) = &response.pod_id {
            println!("  {} {}", "Pod ID:".bold(), pod_id);
        }
        if let Some(expires) = &response.new_expires_at {
            println!("  {} {}", "New Expiry:".bold(), expires);
        }
        if let Some(added) = response.added_seconds {
            let minutes = added / 60;
            let seconds = added % 60;
            println!("  {} +{}m {}s", "Added:".bold(), minutes, seconds);
        }
        if let Some(msg) = &response.message {
            println!("  {} {}", "Message:".bold(), msg);
        }
    } else {
        let error_msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow::anyhow!("Failed to top up pod: {}", error_msg));
    }

    Ok(())
}

async fn execute_nostr_topup(provider_npub: String, args: TopupArgs, _verbose: bool) -> Result<()> {
    println!("{}", "Topping Up Workload".blue().bold());
    println!("{}", "-".repeat(50).blue());
    println!();

    let relays = parse_relays(args.relays);
    let nostr_key = get_or_create_identity(args.nostr_key)?;

    let client = DiscoveryClient::new_with_key(relays, nostr_key).await?;

    println!("  Pod ID:   {}", args.pod_id.cyan());
    println!("  Provider: {}", provider_npub);
    println!();

    // Build topup request as the structured EncryptedTopUpPodRequest
    // payload — provider.rs uses #[serde(untagged)] PrivateRequest, so
    // the field set must match the TopUp variant exactly. Sending a
    // free-form JSON would silently misroute (the provider would
    // either fail to parse or match the wrong variant).
    let request = paygress::nostr::EncryptedTopUpPodRequest {
        pod_npub: args.pod_id.clone(),
        cashu_token: args.token,
    };

    print!("  Sending topup request... ");

    let request_json = serde_json::to_string(&request)?;
    client
        .nostr()
        .send_encrypted_private_message(&provider_npub, request_json, "nip04")
        .await?;

    println!("{}", "SENT".green());
    println!();
    println!("  Waiting for provider response (timeout: 60s)...");

    match client
        .nostr()
        .wait_for_decrypted_message(&provider_npub, 60)
        .await
    {
        Ok(response) => {
            println!();

            // Try to parse response
            if let Ok(resp) = serde_json::from_str::<serde_json::Value>(&response.content) {
                if resp.get("error").is_some() {
                    println!("{}", "Topup failed".red().bold());
                    println!("  {}", resp["error"].as_str().unwrap_or("Unknown error"));
                } else {
                    println!("{}", "Topup successful!".green().bold());
                    if let Some(expires) = resp.get("expires_at") {
                        println!("  {} {}", "New Expiry:".bold(), expires);
                    }
                    if let Some(added) = resp.get("added_seconds") {
                        println!("  {} +{}s", "Added:".bold(), added);
                    }
                }
            } else {
                println!("Provider response: {}", response.content);
            }
        }
        Err(e) => {
            println!();
            println!("  {} {}", "Warning:".yellow(), e.to_string().yellow());
            println!("The topup request was sent but the provider didn't respond in time.");
            println!(
                "Check status with: paygress-cli status --pod-id {} --provider {}",
                args.pod_id, provider_npub
            );
        }
    }

    Ok(())
}
