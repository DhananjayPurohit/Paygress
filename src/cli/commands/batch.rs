// Batch coordinator — fan-out N agent-sandbox (or any template)
// pods in parallel for map-reduce shards, CI matrices, and
// embarrassingly-parallel batch workloads.
//
// Scope of v1 (this command):
//   - Parses N Cashu tokens from --tokens / --tokens-file.
//   - Spawns N pods concurrently via the shared
//     `spawn::nostr_spawn_round_trip` helper.
//   - Writes per-shard subdirs at <output>/shard-<i>/ so the caller
//     can scp results in, plus a top-level <output>/shards.json
//     describing every shard's access details.
//   - Prints a tabular summary; exits non-zero if any shard failed
//     so CI can react.
//
// What this command does NOT do (deliberate v1 scope cut):
//   - SSH/scp automation. Ship the spawnable surface first; add
//     auto-exec/collect once we have a clean Rust SSH client
//     story (libssh2 vs russh trade-off TBD). Today the caller can
//     parse shards.json and run their own ssh fan-out.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use rand::Rng;
use serde::Serialize;

use super::identity::{get_or_create_identity, parse_relays};
use super::spawn::{nostr_spawn_round_trip, NostrSpawnOutcome};
use paygress::nostr::{AccessDetailsContent, ErrorResponseContent, TemplateAccessPort};

#[derive(Args)]
pub struct BatchArgs {
    /// Provider npub. Auto-discovery of the cheapest provider lands
    /// with the observatory; until then, this flag is required.
    #[arg(long)]
    pub provider: String,

    /// Comma-separated Cashu tokens, one per shard. Each token is
    /// redeemed independently; a single shard's token failure does
    /// not cancel the others.
    #[arg(long, conflicts_with = "tokens_file")]
    pub tokens: Option<String>,

    /// Path to a file with one Cashu token per line. Lines starting
    /// with `#` and blank lines are ignored.
    #[arg(long, conflicts_with = "tokens")]
    pub tokens_file: Option<PathBuf>,

    /// Tier on the provider's offer.
    #[arg(short, long, default_value = "basic")]
    pub tier: String,

    /// Template slug. Defaults to `agent-sandbox` because it's the
    /// generic compute primitive batch shards usually want; override
    /// with `--template inference-endpoint` etc. for specialized
    /// shards.
    #[arg(long, default_value = "agent-sandbox")]
    pub template: String,

    /// Output directory. Per-shard subdirs are created as
    /// `<output>/shard-<i>/` (the caller's downstream scp/ssh script
    /// drops results in there); `<output>/shards.json` is the
    /// machine-readable manifest.
    #[arg(long, default_value = "./paygress-batch")]
    pub output: PathBuf,

    /// Per-shard spawn timeout (seconds).
    #[arg(long, default_value_t = 120)]
    pub timeout_secs: u64,

    /// Container image. Ignored when the provider resolves a known
    /// template slug (the default path); only matters for
    /// non-template spawns where the provider trusts consumer bytes.
    #[arg(long, default_value = "ubuntu:22.04")]
    pub image: String,

    /// Your Nostr private key (nsec). Falls back to
    /// `~/.paygress/identity` if not provided.
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated). Falls back to the
    /// CLI's default relay list.
    #[arg(long)]
    pub relays: Option<String>,
}

/// Per-shard outcome that lands in the JSON manifest. Fields are a
/// strict subset of `AccessDetailsContent` plus shard-coordinator
/// metadata (index, ssh creds, status). Stable schema — downstream
/// scripts pin to it.
#[derive(Debug, Clone, Serialize)]
pub struct ShardManifestEntry {
    pub index: usize,
    pub status: String, // "spawned" | "provider_error" | "offline" | "timeout" | "unknown_response"
    pub host: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_user: Option<String>,
    pub ssh_pass: Option<String>,
    pub pod_id: Option<String>,
    pub expires_at: Option<String>,
    pub template_ports: Vec<TemplateAccessPort>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShardManifest {
    pub provider_npub: String,
    pub template: String,
    pub tier: String,
    pub shard_count: usize,
    pub spawned_count: usize,
    pub shards: Vec<ShardManifestEntry>,
}

fn generate_password(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

/// Parse the token list from either --tokens or --tokens-file.
/// Surfaced for unit-testing without spinning up the runtime.
pub fn parse_tokens(args: &BatchArgs) -> Result<Vec<String>> {
    if let Some(s) = &args.tokens {
        let v: Vec<String> = s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if v.is_empty() {
            anyhow::bail!("--tokens must contain at least one non-empty token");
        }
        return Ok(v);
    }
    if let Some(p) = &args.tokens_file {
        let content = std::fs::read_to_string(p)
            .with_context(|| format!("failed to read tokens file {}", p.display()))?;
        let v: Vec<String> = content
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        if v.is_empty() {
            anyhow::bail!(
                "token file {} contains no tokens (after stripping comments + blank lines)",
                p.display()
            );
        }
        return Ok(v);
    }
    anyhow::bail!("either --tokens or --tokens-file is required");
}

/// Build a shard manifest entry from a successful access-details
/// response. Pure transform — split out so it can be unit-tested
/// without the runtime.
fn manifest_entry_from_success(
    index: usize,
    host_address_fallback: &str,
    ssh_user: &str,
    ssh_pass: &str,
    access: AccessDetailsContent,
) -> ShardManifestEntry {
    let host = if access.host_address.is_empty() {
        host_address_fallback.to_string()
    } else {
        access.host_address
    };
    ShardManifestEntry {
        index,
        status: "spawned".to_string(),
        host: Some(host),
        ssh_port: Some(access.node_port),
        ssh_user: Some(ssh_user.to_string()),
        ssh_pass: Some(ssh_pass.to_string()),
        pod_id: Some(access.pod_npub),
        expires_at: Some(access.expires_at),
        template_ports: access.template_ports,
        error_type: None,
        error_message: None,
    }
}

fn manifest_entry_from_error(
    index: usize,
    status: &str,
    err: ErrorResponseContent,
) -> ShardManifestEntry {
    ShardManifestEntry {
        index,
        status: status.to_string(),
        host: None,
        ssh_port: None,
        ssh_user: None,
        ssh_pass: None,
        pod_id: None,
        expires_at: None,
        template_ports: Vec::new(),
        error_type: Some(err.error_type),
        error_message: Some(err.message),
    }
}

fn manifest_entry_status_only(
    index: usize,
    status: &str,
    message: Option<String>,
) -> ShardManifestEntry {
    ShardManifestEntry {
        index,
        status: status.to_string(),
        host: None,
        ssh_port: None,
        ssh_user: None,
        ssh_pass: None,
        pod_id: None,
        expires_at: None,
        template_ports: Vec::new(),
        error_type: None,
        error_message: message,
    }
}

pub async fn execute(args: BatchArgs, _verbose: bool) -> Result<()> {
    let tokens = parse_tokens(&args)?;
    let n = tokens.len();
    let relays = parse_relays(args.relays.clone());
    let nostr_key = get_or_create_identity(args.nostr_key.clone())?;

    println!("{}", "Paygress Batch Coordinator".blue().bold());
    println!("{}", "-".repeat(50).blue());
    println!("  Provider:    {}", args.provider.cyan());
    println!("  Template:    {}", args.template.cyan());
    println!("  Tier:        {}", args.tier);
    println!("  Shards:      {}", n);
    println!("  Output dir:  {}", args.output.display());
    println!();

    // Scaffold the output dir up front so a downstream script can
    // start writing into per-shard subdirs without racing the
    // coordinator. We create the shard subdir even for failed
    // shards because users will look for them; an empty subdir is
    // less surprising than a missing one.
    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("failed to create output dir {}", args.output.display()))?;
    for i in 0..n {
        let p = args.output.join(format!("shard-{}", i));
        std::fs::create_dir_all(&p)
            .with_context(|| format!("failed to create shard subdir {}", p.display()))?;
    }

    // Fire off all spawns concurrently. Each gets its own
    // DiscoveryClient inside `nostr_spawn_round_trip`; for batch
    // sizes typical of map-reduce (tens, not thousands) the
    // per-shard relay handshake is fine. A connection-pooling
    // refactor lands when shard counts justify it.
    let mut handles = Vec::with_capacity(n);
    for (i, token) in tokens.into_iter().enumerate() {
        let provider = args.provider.clone();
        let tier = args.tier.clone();
        let image = args.image.clone();
        let template = Some(args.template.clone());
        let relays = relays.clone();
        let nostr_key = nostr_key.clone();
        let timeout = args.timeout_secs;
        let ssh_user = "user".to_string();
        let ssh_pass = generate_password(16);

        let handle = tokio::spawn(async move {
            let outcome = nostr_spawn_round_trip(
                &provider,
                &tier,
                &token,
                image,
                ssh_user.clone(),
                ssh_pass.clone(),
                template,
                relays,
                nostr_key,
                timeout,
            )
            .await;
            (i, ssh_user, ssh_pass, outcome)
        });
        handles.push(handle);
    }

    let mut entries: Vec<ShardManifestEntry> = Vec::with_capacity(n);
    for h in handles {
        let (i, ssh_user, ssh_pass, outcome) = match h.await {
            Ok(v) => v,
            Err(e) => {
                entries.push(manifest_entry_status_only(
                    0,
                    "join_error",
                    Some(format!("tokio join error: {}", e)),
                ));
                continue;
            }
        };

        let entry = match outcome {
            Ok(NostrSpawnOutcome::Success(access)) => {
                manifest_entry_from_success(i, &args.provider, &ssh_user, &ssh_pass, access)
            }
            Ok(NostrSpawnOutcome::ProviderError(err)) => {
                manifest_entry_from_error(i, "provider_error", err)
            }
            Ok(NostrSpawnOutcome::ProviderOffline) => manifest_entry_status_only(
                i,
                "offline",
                Some("provider's heartbeat did not appear within the live window".to_string()),
            ),
            Ok(NostrSpawnOutcome::Timeout) => manifest_entry_status_only(
                i,
                "timeout",
                Some(format!(
                    "no response within {}s; token may have been spent",
                    args.timeout_secs
                )),
            ),
            Ok(NostrSpawnOutcome::UnknownResponse(s)) => manifest_entry_status_only(
                i,
                "unknown_response",
                Some(format!("body: {}", s.chars().take(200).collect::<String>())),
            ),
            Err(e) => manifest_entry_status_only(i, "transport_error", Some(e.to_string())),
        };
        entries.push(entry);
    }

    // Stable order so the JSON manifest matches the shard index.
    entries.sort_by_key(|e| e.index);

    let spawned_count = entries.iter().filter(|e| e.status == "spawned").count();
    let manifest = ShardManifest {
        provider_npub: args.provider.clone(),
        template: args.template.clone(),
        tier: args.tier.clone(),
        shard_count: n,
        spawned_count,
        shards: entries.clone(),
    };

    let manifest_path = args.output.join("shards.json");
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(&manifest_path, manifest_json)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;

    println!();
    println!("{}", "-".repeat(50).blue());
    println!(
        "{}: {}/{} shards spawned",
        "Result".bold(),
        spawned_count.to_string().green(),
        n
    );
    println!("  Manifest: {}", manifest_path.display());
    println!();
    println!("{}", "Per-shard summary:".bold());
    for e in &entries {
        let status_label = match e.status.as_str() {
            "spawned" => "spawned".green().to_string(),
            "offline" => "offline".red().to_string(),
            "timeout" => "timeout".red().to_string(),
            "provider_error" | "unknown_response" | "transport_error" | "join_error" => {
                e.status.red().to_string()
            }
            _ => e.status.to_string(),
        };
        match (&e.host, e.ssh_port) {
            (Some(host), Some(port)) => println!(
                "  shard-{:<3} {:<10} {}:{}",
                e.index, status_label, host, port
            ),
            _ => {
                let detail = e
                    .error_message
                    .as_deref()
                    .or(e.error_type.as_deref())
                    .unwrap_or("");
                println!("  shard-{:<3} {:<10} {}", e.index, status_label, detail);
            }
        }
    }

    if spawned_count < n {
        anyhow::bail!(
            "{} of {} shards failed to spawn (see manifest for details)",
            n - spawned_count,
            n
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with_tokens(s: &str) -> BatchArgs {
        BatchArgs {
            provider: "npub1abc".to_string(),
            tokens: Some(s.to_string()),
            tokens_file: None,
            tier: "basic".to_string(),
            template: "agent-sandbox".to_string(),
            output: PathBuf::from("/tmp/paygress-batch-test"),
            timeout_secs: 120,
            image: "ubuntu:22.04".to_string(),
            nostr_key: None,
            relays: None,
        }
    }

    fn args_with_file(p: PathBuf) -> BatchArgs {
        BatchArgs {
            provider: "npub1abc".to_string(),
            tokens: None,
            tokens_file: Some(p),
            tier: "basic".to_string(),
            template: "agent-sandbox".to_string(),
            output: PathBuf::from("/tmp/paygress-batch-test"),
            timeout_secs: 120,
            image: "ubuntu:22.04".to_string(),
            nostr_key: None,
            relays: None,
        }
    }

    #[test]
    fn parse_tokens_comma_list() {
        let args = args_with_tokens("a,b,c");
        let v = parse_tokens(&args).unwrap();
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_tokens_strips_whitespace() {
        let args = args_with_tokens("  a , b  ,c  ");
        let v = parse_tokens(&args).unwrap();
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_tokens_drops_empty_entries() {
        // Trailing comma is a common copy/paste artifact; don't
        // explode on it.
        let args = args_with_tokens("a,,b,");
        let v = parse_tokens(&args).unwrap();
        assert_eq!(v, vec!["a", "b"]);
    }

    #[test]
    fn parse_tokens_rejects_empty_input() {
        let args = args_with_tokens("");
        assert!(parse_tokens(&args).is_err());
        let args = args_with_tokens(" , , ");
        assert!(parse_tokens(&args).is_err());
    }

    #[test]
    fn parse_tokens_from_file_with_comments() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tokens.txt");
        std::fs::write(
            &p,
            "# header comment\ntoken-a\n\n  token-b  \n# trailing comment\ntoken-c\n",
        )
        .unwrap();
        let args = args_with_file(p);
        let v = parse_tokens(&args).unwrap();
        assert_eq!(v, vec!["token-a", "token-b", "token-c"]);
    }

    #[test]
    fn parse_tokens_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.txt");
        std::fs::write(&p, "# only a comment\n\n").unwrap();
        let args = args_with_file(p);
        assert!(parse_tokens(&args).is_err());
    }

    #[test]
    fn manifest_entry_success_carries_access_fields() {
        let access = AccessDetailsContent {
            pod_npub: "container-42".to_string(),
            node_port: 30042,
            expires_at: "2026-04-30T00:00:00Z".to_string(),
            cpu_millicores: 1000,
            memory_mb: 1024,
            pod_spec_name: "Basic".to_string(),
            pod_spec_description: "1 vCPU".to_string(),
            instructions: vec!["ssh -p 30042 root@host".to_string()],
            host_address: "10.0.0.7".to_string(),
            template_ports: vec![],
        };
        let e = manifest_entry_from_success(3, "fallback-host", "user", "pw", access);
        assert_eq!(e.index, 3);
        assert_eq!(e.status, "spawned");
        assert_eq!(e.host.as_deref(), Some("10.0.0.7"));
        assert_eq!(e.ssh_port, Some(30042));
        assert_eq!(e.ssh_user.as_deref(), Some("user"));
        assert_eq!(e.ssh_pass.as_deref(), Some("pw"));
        assert!(e.error_type.is_none());
    }

    #[test]
    fn manifest_entry_success_falls_back_when_host_address_empty() {
        // Old providers don't set host_address (skip_serializing_if
        // empty String); the manifest must still expose a usable
        // host so downstream scripts can SSH.
        let access = AccessDetailsContent {
            pod_npub: "container-1".to_string(),
            node_port: 30001,
            expires_at: "2026-04-30T00:00:00Z".to_string(),
            cpu_millicores: 500,
            memory_mb: 512,
            pod_spec_name: "Basic".to_string(),
            pod_spec_description: "—".to_string(),
            instructions: vec![],
            host_address: String::new(),
            template_ports: vec![],
        };
        let e = manifest_entry_from_success(0, "provider-public-ip", "user", "pw", access);
        assert_eq!(e.host.as_deref(), Some("provider-public-ip"));
    }

    #[test]
    fn manifest_entry_error_carries_error_fields() {
        let err = ErrorResponseContent {
            error_type: "token_already_spent".to_string(),
            message: "this Cashu token was already redeemed".to_string(),
            details: None,
        };
        let e = manifest_entry_from_error(2, "provider_error", err);
        assert_eq!(e.index, 2);
        assert_eq!(e.status, "provider_error");
        assert_eq!(e.error_type.as_deref(), Some("token_already_spent"));
        assert!(e.host.is_none());
        assert!(e.ssh_port.is_none());
    }
}
