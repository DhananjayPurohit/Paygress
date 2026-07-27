// `paygress-cli spawn` — create a workload against a provider.
//
// Nostr mode (default): encrypted spawn request DM to a provider.
// HTTP mode (--server): direct call to a Paygress HTTP server.

use anyhow::Result;
use clap::Args;
use colored::Colorize;

use crate::api::{PaygressClient, SpawnRequest};
use crate::util::{
    generate_password, get_or_create_identity, parse_isolation_level, parse_relays, spinner,
    split_csv,
};
use paygress::discovery::DiscoveryClient;
use paygress::durable_workload::ReplicationMode;
use paygress::nostr::{
    AccessDetailsContent, EncryptedSpawnPodRequest, ErrorResponseContent, IsolationLevel,
    VolumeEncryption,
};

/// `nsec1...` bech32 or 64-hex to raw bytes, for the volume-key KDF.
fn nsec_to_bytes(nostr_key: &str) -> Result<[u8; 32]> {
    use std::str::FromStr;
    let secret = nostr_sdk::SecretKey::from_str(nostr_key)
        .map_err(|e| anyhow::anyhow!("invalid nsec/hex secret key: {}", e))?;
    secret
        .as_secret_bytes()
        .try_into()
        .map_err(|_| anyhow::anyhow!("nostr_sdk::SecretKey returned a non-32-byte secret"))
}

#[derive(Args)]
pub struct SpawnArgs {
    /// Provider ID (Nostr mode) - if omitted, uses --server for HTTP mode
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

    /// Template slug; the provider resolves image/ports/env and ignores --image
    #[arg(long, hide = true)]
    pub template_slug: Option<String>,

    /// Replication mode: none, checkpointed, or warm-standby
    #[arg(long, default_value = "none")]
    pub replication: String,

    /// Comma-separated standby provider IDs (warm-standby only)
    #[arg(long)]
    pub standby: Option<String>,

    /// Primary provider's ID (warm-standby only)
    #[arg(long)]
    pub primary_id: Option<String>,

    /// Shared workload identifier, passed to the primary and each standby
    #[arg(long)]
    pub workload_id: Option<String>,

    /// Encrypt the persistent data volume (key derived from nsec + workload-id)
    #[arg(long, conflicts_with = "no_encrypt_volume")]
    pub encrypt_volume: bool,

    /// Opt out of the per-template default for encrypted volumes
    #[arg(long, conflicts_with = "encrypt_volume")]
    pub no_encrypt_volume: bool,

    /// Minimum isolation tier, verified before the token is sent
    #[arg(long, value_parser = parse_isolation_level)]
    pub isolation_level: Option<IsolationLevel>,
}

/// `Ok(None)` for the default, so the wire field stays absent for
/// providers that predate it.
pub fn parse_replication_arg(
    mode: &str,
    standby_csv: Option<&str>,
) -> Result<Option<ReplicationMode>> {
    match mode {
        "none" | "checkpointed" => {
            if standby_csv.is_some() {
                anyhow::bail!(
                    "--standby is only valid with --replication warm-standby (got --replication {})",
                    mode
                );
            }
            Ok(match mode {
                "checkpointed" => Some(ReplicationMode::Checkpointed),
                _ => None,
            })
        }
        "warm-standby" => {
            let csv = standby_csv.ok_or_else(|| {
                anyhow::anyhow!("--replication warm-standby requires --standby <npub1,npub2,...>")
            })?;
            let standby_providers = split_csv(csv);
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
    let ssh_user = args.ssh_user.take().unwrap_or_else(|| "user".to_string());
    let ssh_pass = args
        .ssh_pass
        .take()
        .unwrap_or_else(|| generate_password(16));

    if let Some(provider) = args.provider.take() {
        return execute_nostr_spawn(provider, args, ssh_user, ssh_pass).await;
    }

    let server = args.server.take().ok_or_else(|| {
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

    let spinner = spinner("Connecting to Paygress server...");
    let client = PaygressClient::new(server);

    spinner.set_message("Checking server health...");
    client.health().await?;

    spinner.set_message("Spawning pod with Cashu payment...");
    let response = client
        .spawn_pod(SpawnRequest {
            pod_spec_id: args.tier,
            pod_image: args.image,
            ssh_username: ssh_user,
            ssh_password: ssh_pass,
            cashu_token: Some(args.token),
        })
        .await?;
    spinner.finish_and_clear();

    if !response.success {
        let error_msg = response.error.as_deref().unwrap_or("Unknown error");
        return Err(anyhow::anyhow!("Failed to spawn pod: {}", error_msg));
    }

    println!("{}", "Pod spawned successfully!".green().bold());
    println!();

    if let Some(pod_id) = &response.pod_id {
        println!("  {} {}", "Pod ID:".bold(), pod_id);
    }
    if let (Some(host), Some(port)) = (&response.ssh_host, response.ssh_port) {
        println!(
            "  {} ssh {}@{} -p {}",
            "SSH:".bold(),
            response.ssh_username.as_deref().unwrap_or("user"),
            host,
            port
        );
    }
    if let Some(expires) = &response.expires_at {
        println!("  {} {}", "Expires:".bold(), expires);
    }
    if let Some(duration) = response.duration_seconds {
        println!(
            "  {} {}m {}s",
            "Duration:".bold(),
            duration / 60,
            duration % 60
        );
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

    Ok(())
}

/// Shared by the pretty-printer, the batch coordinator, and MCP.
#[derive(Debug, Clone)]
pub enum NostrSpawnOutcome {
    /// Provider's heartbeat says it's offline; no token spent.
    ProviderOffline,
    Success(AccessDetailsContent),
    /// Structured provider error (token spent, unknown template, ...).
    ProviderError(ErrorResponseContent),
    /// Provider replied with neither schema — likely a newer provider.
    UnknownResponse(String),
    /// No reply in the timeout window. The token MAY have been spent.
    Timeout,
}

#[derive(Debug, Clone, Default)]
pub struct NostrSpawnParams {
    pub tier: String,
    pub token: String,
    pub image: String,
    pub ssh_user: String,
    pub ssh_pass: String,
    pub template_slug: Option<String>,
    pub replication: Option<ReplicationMode>,
    pub primary_id: Option<String>,
    pub workload_id: Option<String>,
    pub volume_encryption: Option<VolumeEncryption>,
    pub isolation_level: Option<IsolationLevel>,
}

/// No stdout I/O — pure round-trip plus structured outcome.
pub async fn nostr_spawn_round_trip(
    provider_npub: &str,
    params: NostrSpawnParams,
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

    // Every check below runs before the token is sent, so a mismatch
    // never spends it.
    if !provider.specs.iter().any(|s| s.id == params.tier) {
        anyhow::bail!("Tier '{}' not available on this provider", params.tier);
    }

    if let Some(min_iso) = params.isolation_level {
        if !provider.isolation_level.meets(min_iso) {
            anyhow::bail!(
                "provider's isolation tier `{}` does not meet requested minimum `{}`; \
                 try `paygress-cli list --isolation-level {}` to discover providers that do",
                provider.isolation_level.slug(),
                min_iso.slug(),
                min_iso.slug(),
            );
        }
    }

    if !provider.whitelisted_mints.is_empty() {
        let token_mint = paygress::cashu::token_mint_url(&params.token)
            .map_err(|e| anyhow::anyhow!("could not read token: {}", e))?;

        let token_norm = token_mint.trim_end_matches('/');
        let accepted = provider
            .whitelisted_mints
            .iter()
            .any(|m| m.trim_end_matches('/').eq_ignore_ascii_case(token_norm));

        if !accepted {
            let list = provider
                .whitelisted_mints
                .iter()
                .map(|m| format!("  • {}", m))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "token is from mint '{}' which is not accepted by provider '{}'.\n\
                 Accepted mints:\n{}",
                token_mint,
                provider.hostname,
                list
            );
        }
    }

    let request = EncryptedSpawnPodRequest {
        cashu_token: params.token,
        pod_spec_id: Some(params.tier),
        pod_image: params.image,
        ssh_username: params.ssh_user,
        ssh_password: params.ssh_pass,
        template_slug: params.template_slug,
        replication: params.replication,
        primary_npub: params.primary_id,
        workload_id: params.workload_id,
        volume_encryption: params.volume_encryption,
    };
    let request_json = serde_json::to_string(&request)?;

    client
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

/// Precedence: `--no-encrypt-volume` beats `--encrypt-volume` beats the
/// template default; custom-image spawns (no slug) stay off. Returns the
/// resolved workload id, which encryption mints when unset.
fn resolve_volume_encryption(
    encrypt_volume: bool,
    no_encrypt_volume: bool,
    template_slug: Option<&str>,
    workload_id: Option<String>,
    nostr_key: &str,
) -> Result<(Option<VolumeEncryption>, Option<String>)> {
    let template_default = template_slug
        .and_then(paygress::templates::TemplateName::from_slug)
        .map(paygress::templates::template_default_encrypts_volume)
        .unwrap_or(false);

    let should_encrypt = !no_encrypt_volume && (encrypt_volume || template_default);
    if !should_encrypt {
        return Ok((None, workload_id));
    }

    let workload_id = workload_id.unwrap_or_else(|| {
        let id = uuid::Uuid::new_v4().to_string();
        println!(
            "  {} {}",
            "Generated workload-id (save this for respawn):".bold(),
            id.cyan()
        );
        id
    });

    let nsec_bytes = nsec_to_bytes(nostr_key)?;
    let key = paygress::volume_encryption::derive_volume_key(&nsec_bytes, &workload_id);
    if !encrypt_volume && template_default {
        println!(
            "  {} (template default; pass --no-encrypt-volume to skip)",
            "Encrypting persistent data volume".green()
        );
    }
    Ok((Some(VolumeEncryption::v1(key)), Some(workload_id)))
}

async fn execute_nostr_spawn(
    provider_npub: String,
    args: SpawnArgs,
    ssh_user: String,
    ssh_pass: String,
) -> Result<()> {
    println!("{}", "Spawning Workload".blue().bold());
    println!("{}", "-".repeat(50).blue());
    println!();

    let SpawnArgs {
        tier,
        token,
        image,
        nostr_key,
        relays,
        template_slug,
        replication,
        standby,
        primary_id,
        workload_id,
        encrypt_volume,
        no_encrypt_volume,
        isolation_level,
        ..
    } = args;

    let relays = parse_relays(relays);
    let nostr_key = get_or_create_identity(nostr_key)?;

    print!("  Checking provider {}... ", provider_npub.cyan());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let replication = parse_replication_arg(&replication, standby.as_deref())?;
    // Both ids let each receiving provider determine its own role.
    if matches!(replication, Some(ReplicationMode::WarmStandby { .. })) {
        if primary_id.is_none() {
            anyhow::bail!("--replication warm-standby requires --primary-id <primary provider ID>");
        }
        if workload_id.is_none() {
            anyhow::bail!(
                "--replication warm-standby requires --workload-id <consumer-assigned uuid>"
            );
        }
    }

    let (volume_encryption, workload_id) = resolve_volume_encryption(
        encrypt_volume,
        no_encrypt_volume,
        template_slug.as_deref(),
        workload_id,
        &nostr_key,
    )?;

    let outcome = nostr_spawn_round_trip(
        &provider_npub,
        NostrSpawnParams {
            tier,
            token,
            image,
            ssh_user: ssh_user.clone(),
            ssh_pass: ssh_pass.clone(),
            template_slug,
            replication,
            primary_id,
            workload_id,
            volume_encryption,
            isolation_level,
        },
        relays,
        nostr_key,
        120,
    )
    .await?;

    println!();
    println!("{}", "-".repeat(50).blue());
    print_spawn_outcome(outcome, &ssh_user, &ssh_pass);
    Ok(())
}

fn print_spawn_outcome(outcome: NostrSpawnOutcome, ssh_user: &str, ssh_pass: &str) {
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
            println!(
                "  {}   {} / {}",
                "SSH:".bold(),
                ssh_user.cyan(),
                ssh_pass.cyan()
            );
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
            println!("You may check your status later with: paygress-cli status --pod-id <ID> --provider <id>");
        }
    }
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
