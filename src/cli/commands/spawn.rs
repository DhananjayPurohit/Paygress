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

/// Decode a Nostr secret key (`nsec1...` bech32 or 64-hex) to its
/// raw 32-byte form. Used by `--encrypt-volume` to feed the consumer's
/// nsec into the volume-key KDF.
fn nsec_to_bytes(nostr_key: &str) -> Result<[u8; 32]> {
    use std::str::FromStr;
    let secret = nostr_sdk::SecretKey::from_str(nostr_key)
        .map_err(|e| anyhow::anyhow!("invalid nsec/hex secret key: {}", e))?;
    Ok(secret
        .as_secret_bytes()
        .try_into()
        .map_err(|_| anyhow::anyhow!("nostr_sdk::SecretKey returned a non-32-byte secret"))?)
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
    #[arg(long, default_value = "none")]
    pub replication: String,

    /// Comma-separated standby provider npubs (warm-standby only).
    /// Ignored when `--replication` is not `warm-standby`.
    #[arg(long)]
    pub standby: Option<String>,

    /// Primary provider's npub (warm-standby only). When you spawn
    /// AGAINST a standby with `--provider <standby-npub>`, this tells
    /// the standby who the primary is so it can listen for the right
    /// `LeaseRevocation`. When you spawn AGAINST the primary, set
    /// this to the same value as `--provider` so the primary
    /// recognizes itself.
    #[arg(long)]
    pub primary_npub: Option<String>,

    /// Consumer-assigned workload identifier (UUID-shaped string,
    /// warm-standby only). The same value MUST be passed when
    /// spawning against the primary AND each standby — it ties the
    /// N+1 spawns into one logical workload. The
    /// `LeaseRevocation` event uses this id, and standbys look up
    /// their reserved slot by it on receipt.
    #[arg(long)]
    pub workload_id: Option<String>,

    /// Encrypt the workload's persistent data volume with a key
    /// derived from your nsec + workload-id. Defends against
    /// post-eviction disk forensics, lazy host backups, co-tenant
    /// attacks on shared storage, and cold-disk seizure. Does NOT
    /// defend against a live host kernel reading process memory or
    /// extracting the LUKS key from the keyring — that requires
    /// a confidential VM (`isolation-level=attested-research-tier`).
    ///
    /// When set, `--workload-id` becomes effectively required (the
    /// CLI generates a UUID if you don't supply one and prints it
    /// so you can re-supply it on respawn / top-up to recover the
    /// same key).
    #[arg(long)]
    pub encrypt_volume: bool,
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
                anyhow::anyhow!("--replication warm-standby requires --standby <npub1,npub2,...>")
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
#[allow(clippy::too_many_arguments)]
pub async fn nostr_spawn_round_trip(
    provider_npub: &str,
    tier: &str,
    token: &str,
    image: String,
    ssh_user: String,
    ssh_pass: String,
    template_slug: Option<String>,
    replication: Option<paygress::durable_workload::ReplicationMode>,
    primary_npub: Option<String>,
    workload_id: Option<String>,
    volume_encryption: Option<paygress::nostr::VolumeEncryption>,
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
        replication,
        primary_npub,
        workload_id,
        volume_encryption,
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

    let replication = parse_replication_arg(&args.replication, args.standby.as_deref())?;
    // For warm-standby, primary_npub + workload_id are required so
    // each receiving provider can self-determine role and so the
    // standbys share one stable id with the primary.
    if let Some(paygress::durable_workload::ReplicationMode::WarmStandby { .. }) =
        replication.as_ref()
    {
        if args.primary_npub.is_none() {
            anyhow::bail!("--replication warm-standby requires --primary-npub <primary's npub>");
        }
        if args.workload_id.is_none() {
            anyhow::bail!(
                "--replication warm-standby requires --workload-id <consumer-assigned uuid>"
            );
        }
    }

    // Volume encryption: derive a 32-byte key from (nsec, workload_id)
    // and ship it inside the encrypted spawn DM. The provider creates
    // a LUKS-encrypted volume keyed by these bytes (Phase 2 of the
    // confidentiality plan; today the provider parses but doesn't
    // yet act). If the consumer didn't supply --workload-id, mint a
    // UUIDv4 so the key is reproducible on respawn.
    let mut effective_workload_id = args.workload_id.clone();
    let volume_encryption = if args.encrypt_volume {
        if effective_workload_id.is_none() {
            let id = uuid::Uuid::new_v4().to_string();
            println!(
                "  {} {}",
                "Generated workload-id (save this for respawn):".bold(),
                id.cyan()
            );
            effective_workload_id = Some(id);
        }
        let workload_id = effective_workload_id.as_ref().unwrap();
        let nsec_bytes = nsec_to_bytes(&nostr_key)?;
        let key = paygress::volume_encryption::derive_volume_key(&nsec_bytes, workload_id);
        Some(paygress::nostr::VolumeEncryption::v1(key))
    } else {
        None
    };

    let outcome = nostr_spawn_round_trip(
        &provider_npub,
        &args.tier,
        &args.token,
        args.image.clone(),
        ssh_user.clone(),
        ssh_pass.clone(),
        args.template_slug.clone(),
        replication,
        args.primary_npub.clone(),
        effective_workload_id,
        volume_encryption,
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

#[cfg(test)]
mod tests {
    use super::parse_replication_arg;
    use paygress::durable_workload::ReplicationMode;

    #[test]
    fn replication_none_default_returns_no_wire_field() {
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
        assert!(err
            .to_string()
            .contains("only valid with --replication warm-standby"));
    }

    #[test]
    fn replication_unknown_value_errors() {
        let err = parse_replication_arg("multi-master", None).unwrap_err();
        assert!(err.to_string().contains("unknown --replication value"));
    }
}
