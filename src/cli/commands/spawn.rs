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

/// Translate the `--replication` + `--standby` CLI flags into the
/// wire-format `Option<ReplicationMode>` the spawn request carries.
/// Returns `Ok(None)` for the default (no replication) so the wire
/// stays empty and old providers don't see a redundant field. Pure
/// function — exposed for unit-testing the validation matrix.
pub fn parse_replication_arg(
    mode: &str,
    standby_csv: Option<&str>,
) -> anyhow::Result<Option<paygress::durable_workload::ReplicationMode>> {
    use paygress::durable_workload::ReplicationMode;
    match mode {
        "none" => {
            if standby_csv.is_some() {
                anyhow::bail!(
                    "--standby is only valid with --replication warm-standby (got --replication none)"
                );
            }
            Ok(None)
        }
        "checkpointed" => {
            if standby_csv.is_some() {
                anyhow::bail!(
                    "--standby is only valid with --replication warm-standby (got --replication checkpointed)"
                );
            }
            Ok(Some(ReplicationMode::Checkpointed))
        }
        "warm-standby" => {
            let csv = standby_csv.ok_or_else(|| {
                anyhow::anyhow!(
                    "--replication warm-standby requires --standby <npub1,npub2,...>"
                )
            })?;
            let standby_providers: Vec<String> = csv
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if standby_providers.is_empty() {
                anyhow::bail!("--standby must list at least one provider npub");
            }
            Ok(Some(ReplicationMode::WarmStandby { standby_providers }))
        }
        other => anyhow::bail!(
            "unknown --replication value `{}` (expected: none | checkpointed | warm-standby)",
            other
        ),
    }
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

    /// Replication mode: `none` (default), `checkpointed`, or
    /// `warm-standby`. Warm-standby additionally requires `--standby`
    /// listing the standby providers' npubs.
    ///
    /// Warm-standby semantics: send the SAME spawn invocation to
    /// every standby provider too (same `--token` is invalid since
    /// each pod is paid separately; use `--token <token-i>` per
    /// provider). The orchestrator on the primary will publish a
    /// `LeaseRevocation` to the standbys on quorum-loss.
    #[arg(long, default_value = "none")]
    pub replication: String,

    /// Comma-separated standby provider npubs (warm-standby only).
    /// Ignored when `--replication` is not `warm-standby`.
    #[arg(long)]
    pub standby: Option<String>,
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

async fn execute_nostr_spawn(
    provider_npub: String,
    args: SpawnArgs,
    ssh_user: String,
    ssh_pass: String,
    verbose: bool,
) -> Result<()> {
    println!("{}", "Spawning Workload".blue().bold());
    println!("{}", "-".repeat(50).blue());
    println!();

    let relays = parse_relays(args.relays);
    let nostr_key = get_or_create_identity(args.nostr_key)?;

    let client = DiscoveryClient::new_with_key(relays, nostr_key).await?;

    println!("  Your NPUB: {}", client.get_npub().cyan());
    println!();

    // Check if provider is online
    print!("  Checking provider status... ");
    if !client.is_provider_online(&provider_npub).await {
        println!("{}", "OFFLINE".red());
        println!();
        println!("{}", "Provider appears to be offline.".red());
        println!("Try a different provider or wait for this one to come online.");
        return Ok(());
    }
    println!("{}", "ONLINE".green());

    // Get provider info and verify tier
    let provider = client
        .get_provider(&provider_npub)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Provider not found"))?;

    let spec = provider
        .specs
        .iter()
        .find(|s| s.id == args.tier)
        .ok_or_else(|| anyhow::anyhow!("Tier '{}' not available on this provider", args.tier))?;

    println!(
        "  {} Found tier: {} ({} msat/sec)",
        "OK".green(),
        spec.name,
        spec.rate_msats_per_sec
    );

    // Build and send spawn request
    println!(
        "  {} user: {}, pass: {}",
        "SSH Credentials:".bold(),
        ssh_user.cyan(),
        ssh_pass.cyan()
    );

    let replication = parse_replication_arg(&args.replication, args.standby.as_deref())?;
    let request = EncryptedSpawnPodRequest {
        cashu_token: args.token.clone(),
        pod_spec_id: Some(args.tier.clone()),
        pod_image: args.image,
        ssh_username: ssh_user,
        ssh_password: ssh_pass,
        template_slug: args.template_slug.clone(),
        replication,
    };

    println!();
    print!("  Sending spawn request... ");

    let request_json = serde_json::to_string(&request)?;
    let _event_id = client
        .nostr()
        .send_encrypted_private_message(&provider.npub, request_json, "nip04")
        .await?;

    println!("{}", "SENT".green());
    println!();
    println!("  Waiting for provider to provision container (timeout: 120s)...");

    match client
        .nostr()
        .wait_for_decrypted_message(&provider.npub, 120)
        .await
    {
        Ok(response) => {
            println!();
            println!("{}", "-".repeat(50).blue());

            if let Ok(access) = serde_json::from_str::<AccessDetailsContent>(&response.content) {
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
            } else if let Ok(err) = serde_json::from_str::<ErrorResponseContent>(&response.content)
            {
                println!("{}", "Provider Error".red().bold());
                println!();
                println!("  Type:    {}", err.error_type);
                println!("  Message: {}", err.message);
                if let Some(details) = err.details {
                    println!("  Details: {}", details);
                }
            } else {
                println!("{}", "Received Unknown Response".yellow().bold());
                println!();
                println!("Content: {}", response.content);
            }
        }
        Err(e) => {
            println!();
            println!("{}", "-".repeat(50).blue());
            println!("  {} {}", "Warning:".yellow(), e.to_string().yellow());
            println!();
            println!("The request was sent, but the provider didn't respond in time.");
            println!("You may check your status later with: paygress-cli status --pod-id <ID> --provider <npub>");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_replication_arg;
    use paygress::durable_workload::ReplicationMode;

    #[test]
    fn replication_none_default_returns_no_wire_field() {
        // Default path: empty wire payload so old providers see
        // no schema change.
        assert!(parse_replication_arg("none", None).unwrap().is_none());
    }

    #[test]
    fn replication_checkpointed_passes_through() {
        let r = parse_replication_arg("checkpointed", None).unwrap();
        assert!(matches!(r, Some(ReplicationMode::Checkpointed)));
    }

    #[test]
    fn replication_warm_standby_parses_csv() {
        let r = parse_replication_arg("warm-standby", Some("npub1a, npub1b ,npub1c"))
            .unwrap()
            .unwrap();
        match r {
            ReplicationMode::WarmStandby { standby_providers } => {
                assert_eq!(standby_providers, vec!["npub1a", "npub1b", "npub1c"]);
            }
            _ => panic!("expected WarmStandby, got {:?}", r),
        }
    }

    #[test]
    fn replication_warm_standby_requires_standby_flag() {
        let err = parse_replication_arg("warm-standby", None).unwrap_err();
        assert!(err.to_string().contains("warm-standby requires --standby"));
    }

    #[test]
    fn replication_warm_standby_rejects_empty_list() {
        let err = parse_replication_arg("warm-standby", Some(" , , ")).unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn replication_none_rejects_standby_flag() {
        let err = parse_replication_arg("none", Some("npub1x")).unwrap_err();
        assert!(err.to_string().contains("only valid with --replication warm-standby"));
    }

    #[test]
    fn replication_unknown_value_errors() {
        let err = parse_replication_arg("multi-master", None).unwrap_err();
        assert!(err.to_string().contains("unknown --replication value"));
    }
}
