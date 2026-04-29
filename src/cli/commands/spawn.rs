// Spawn command - Create a new workload with Cashu payment
//
// Unified command that works in both modes:
//   - Nostr mode (default): sends encrypted spawn request to a provider via Nostr
//   - HTTP mode (--server): calls a Paygress HTTP server directly

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rand::Rng;

use super::identity::{get_or_create_identity, parse_relays};
use crate::api::{PaygressClient, SpawnRequest};
use paygress::discovery::DiscoveryClient;
use paygress::nostr::{AccessDetailsContent, EncryptedSpawnPodRequest, ErrorResponseContent};

fn generate_password(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

#[derive(Args)]
pub struct SpawnArgs {
    /// Provider npub (Nostr mode) - if omitted, uses --server for HTTP mode
    #[arg(long)]
    pub provider: Option<String>,

    /// HTTP server URL (e.g., http://localhost:8080) - used when --provider is not set
    #[arg(long)]
    pub server: Option<String>,

    /// Pod tier/specification ID (e.g., basic, standard, premium)
    #[arg(short, long, default_value = "basic")]
    pub tier: String,

    /// Cashu token for payment
    #[arg(short = 'k', long)]
    pub token: String,

    /// Container image (HTTP mode only)
    #[arg(short, long, default_value = "ubuntu:22.04")]
    pub image: String,

    /// SSH username (default: "user")
    #[arg(short = 'u', long)]
    pub ssh_user: Option<String>,

    /// SSH password (auto-generated if not provided)
    #[arg(short = 'p', long)]
    pub ssh_pass: Option<String>,

    /// Your Nostr private key (nsec) - uses ~/.paygress/identity if not provided
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,

    /// Template slug (e.g. `nostr-relay`). When set, the provider
    /// materializes image/ports/env from its OWN template registry
    /// and ignores `--image`. Normally set by `paygress deploy`,
    /// not by users directly.
    #[arg(long, hide = true)]
    pub template_slug: Option<String>,
}

pub async fn execute(mut args: SpawnArgs, verbose: bool) -> Result<()> {
    // Auto-generate SSH credentials if not provided
    let ssh_user = args.ssh_user.take().unwrap_or_else(|| "user".to_string());
    let ssh_pass = args
        .ssh_pass
        .take()
        .unwrap_or_else(|| generate_password(16));

    // If --provider is given, use Nostr mode
    if args.provider.is_some() {
        let provider = args.provider.clone().unwrap();
        return execute_nostr_spawn(provider, args, ssh_user, ssh_pass, verbose).await;
    }

    // Otherwise require --server for HTTP mode
    let server = args.server.clone().ok_or_else(|| {
        anyhow::anyhow!("Either --provider (Nostr) or --server (HTTP) is required")
    })?;

    execute_http_spawn(&server, args, ssh_user, ssh_pass, verbose).await
}

async fn execute_http_spawn(
    server: &str,
    args: SpawnArgs,
    ssh_user: String,
    ssh_pass: String,
    verbose: bool,
) -> Result<()> {
    if verbose {
        println!("{} Spawning pod via HTTP...", "->".blue());
        println!("  Server: {}", server);
        println!("  Tier: {}", args.tier);
        println!("  Image: {}", args.image);
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    spinner.set_message("Connecting to Paygress server...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let client = PaygressClient::new(server);

    spinner.set_message("Checking server health...");
    client.health().await?;

    spinner.set_message("Spawning pod with Cashu payment...");

    let request = SpawnRequest {
        pod_spec_id: args.tier,
        pod_image: args.image,
        ssh_username: ssh_user,
        ssh_password: ssh_pass,
        cashu_token: Some(args.token),
    };

    let response = client.spawn_pod(request).await?;
    spinner.finish_and_clear();

    if response.success {
        println!("{}", "Pod spawned successfully!".green().bold());
        println!();

        if let Some(pod_id) = &response.pod_id {
            println!("  {} {}", "Pod ID:".bold(), pod_id);
        }
        if let Some(host) = &response.ssh_host {
            if let Some(port) = response.ssh_port {
                println!(
                    "  {} ssh {}@{} -p {}",
                    "SSH:".bold(),
                    response.ssh_username.as_deref().unwrap_or("user"),
                    host,
                    port
                );
            }
        }
        if let Some(expires) = &response.expires_at {
            println!("  {} {}", "Expires:".bold(), expires);
        }
        if let Some(duration) = response.duration_seconds {
            let minutes = duration / 60;
            let seconds = duration % 60;
            println!("  {} {}m {}s", "Duration:".bold(), minutes, seconds);
        }

        println!();
        println!(
            "{}",
            "Tip: Use 'paygress-cli status --pod-id <ID> --server <URL>' to check status".dimmed()
        );
        println!(
            "{}",
            "Tip: Use 'paygress-cli topup --pod-id <ID> --server <URL> --token <TOKEN>' to extend"
                .dimmed()
        );
    } else {
        let error_msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow::anyhow!("Failed to spawn pod: {}", error_msg));
    }

    Ok(())
}

/// Typed outcome of a single Nostr spawn round-trip. Lets the
/// pretty-print wrapper (`execute_nostr_spawn`) and the batch
/// coordinator share one transport path while differing on what to
/// do with the result.
#[derive(Debug, Clone)]
pub enum NostrSpawnOutcome {
    /// Provider's heartbeat says it's offline; no spawn attempted,
    /// no token spent.
    ProviderOffline,
    /// Provider responded with provisioned access details.
    Success(AccessDetailsContent),
    /// Provider responded with a structured error (token already
    /// spent, unknown template, insufficient payment, etc.).
    ProviderError(ErrorResponseContent),
    /// Provider responded but the body wasn't either schema. Likely
    /// a forward-compat shape from a newer provider.
    UnknownResponse(String),
    /// Provider didn't respond within the timeout window. The token
    /// MAY have been spent — caller should check via `status`.
    Timeout,
}

/// Dispatch a single Nostr spawn request and wait for the response.
/// No I/O on stdout — pure round-trip + structured outcome. Used by
/// the interactive `spawn` command (via `execute_nostr_spawn`) and
/// by the batch coordinator (`commands::batch`) which needs a
/// machine-readable result per shard.
pub async fn nostr_spawn_round_trip(
    provider_npub: &str,
    tier: &str,
    token: &str,
    image: String,
    ssh_user: String,
    ssh_pass: String,
    template_slug: Option<String>,
    relays: Vec<String>,
    nostr_key: String,
    timeout_secs: u64,
) -> Result<NostrSpawnOutcome> {
    let client = DiscoveryClient::new_with_key(relays, nostr_key).await?;

    if !client.is_provider_online(provider_npub).await {
        return Ok(NostrSpawnOutcome::ProviderOffline);
    }

    let provider = client
        .get_provider(provider_npub)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Provider not found"))?;

    // Verify the tier exists on this provider so we fail before
    // spending the token rather than after.
    if !provider.specs.iter().any(|s| s.id == tier) {
        anyhow::bail!("Tier '{}' not available on this provider", tier);
    }

    let request = EncryptedSpawnPodRequest {
        cashu_token: token.to_string(),
        pod_spec_id: Some(tier.to_string()),
        pod_image: image,
        ssh_username: ssh_user,
        ssh_password: ssh_pass,
        template_slug,
    };
    let request_json = serde_json::to_string(&request)?;

    let _event_id = client
        .nostr()
        .send_encrypted_private_message(&provider.npub, request_json, "nip04")
        .await?;

    match client
        .nostr()
        .wait_for_decrypted_message(&provider.npub, timeout_secs)
        .await
    {
        Ok(response) => {
            if let Ok(access) = serde_json::from_str::<AccessDetailsContent>(&response.content) {
                Ok(NostrSpawnOutcome::Success(access))
            } else if let Ok(err) = serde_json::from_str::<ErrorResponseContent>(&response.content)
            {
                Ok(NostrSpawnOutcome::ProviderError(err))
            } else {
                Ok(NostrSpawnOutcome::UnknownResponse(response.content))
            }
        }
        Err(_) => Ok(NostrSpawnOutcome::Timeout),
    }
}

async fn execute_nostr_spawn(
    provider_npub: String,
    args: SpawnArgs,
    ssh_user: String,
    ssh_pass: String,
    _verbose: bool,
) -> Result<()> {
    println!("{}", "Spawning Workload".blue().bold());
    println!("{}", "-".repeat(50).blue());
    println!();

    let relays = parse_relays(args.relays);
    let nostr_key = get_or_create_identity(args.nostr_key)?;

    println!("  Your NPUB: derived from your Nostr identity");
    println!();

    print!("  Checking provider status... ");

    println!(
        "  {} user: {}, pass: {}",
        "SSH Credentials:".bold(),
        ssh_user.cyan(),
        ssh_pass.cyan()
    );

    let outcome = nostr_spawn_round_trip(
        &provider_npub,
        &args.tier,
        &args.token,
        args.image.clone(),
        ssh_user.clone(),
        ssh_pass.clone(),
        args.template_slug.clone(),
        relays,
        nostr_key,
        120,
    )
    .await?;

    println!();
    println!("{}", "-".repeat(50).blue());

    match outcome {
        NostrSpawnOutcome::ProviderOffline => {
            println!("{}", "Provider appears to be offline.".red());
            println!("Try a different provider or wait for this one to come online.");
        }
        NostrSpawnOutcome::Success(access) => {
            println!("{}", "Workload Provisioned Successfully!".green().bold());
            println!();
            println!("  {}   {}", "Pod ID:".bold(), access.pod_npub.cyan());
            if !access.host_address.is_empty() {
                println!("  {}   {}", "Host:".bold(), access.host_address.cyan());
            }
            println!("  {}   {}", "Expires:".bold(), access.expires_at.yellow());
            println!(
                "  {}   {} vCPU, {} MB RAM",
                "Spec:".bold(),
                access.cpu_millicores / 1000,
                access.memory_mb
            );
            if !access.template_ports.is_empty() {
                println!();
                println!("{}", "Workload Ports:".bold());
                for p in &access.template_ports {
                    println!(
                        "  {} ({}) → {}://{}:{}",
                        p.label.cyan(),
                        p.protocol,
                        p.protocol,
                        access.host_address,
                        p.host_port
                    );
                }
            }
            println!();
            println!("{}", "Connection Instructions:".bold());
            for inst in access.instructions {
                println!("  - {}", inst);
            }
        }
        NostrSpawnOutcome::ProviderError(err) => {
            println!("{}", "Provider Error".red().bold());
            println!();
            println!("  Type:    {}", err.error_type);
            println!("  Message: {}", err.message);
            if let Some(details) = err.details {
                println!("  Details: {}", details);
            }
        }
        NostrSpawnOutcome::UnknownResponse(content) => {
            println!("{}", "Received Unknown Response".yellow().bold());
            println!();
            println!("Content: {}", content);
        }
        NostrSpawnOutcome::Timeout => {
            println!(
                "  {} {}",
                "Warning:".yellow(),
                "Provider didn't respond in time.".yellow()
            );
            println!();
            println!("The request was sent, but the provider didn't respond in time.");
            println!("You may check your status later with: paygress-cli status --pod-id <ID> --provider <npub>");
        }
    }

    Ok(())
}
