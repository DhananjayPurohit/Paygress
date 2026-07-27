// `paygress-cli batch` — fan out N pods in parallel.
//
// Writes per-shard subdirs at <output>/shard-<i>/ plus
// <output>/shards.json. Exits non-zero if any shard failed.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use serde::Serialize;

use super::spawn::{nostr_spawn_round_trip, NostrSpawnOutcome, NostrSpawnParams};
use crate::util::{generate_password, get_or_create_identity, parse_isolation_level, parse_relays};
use paygress::nostr::{
    AccessDetailsContent, ErrorResponseContent, IsolationLevel, TemplateAccessPort,
};

#[derive(Args)]
pub struct BatchArgs {
    /// Provider ID
    #[arg(long)]
    pub provider: String,

    /// Comma-separated Cashu tokens, one per shard
    #[arg(long, conflicts_with_all = ["tokens_file", "split_token"])]
    pub tokens: Option<String>,

    /// File with one Cashu token per line (`#` comments and blanks ignored)
    #[arg(long, conflicts_with_all = ["tokens", "split_token"])]
    pub tokens_file: Option<PathBuf>,

    /// One large Cashu token to split into `--shards` tokens before fanning out
    #[arg(long)]
    pub split_token: Option<String>,

    /// Number of shards to split `--split-token` into
    #[arg(long, requires = "split_token")]
    pub shards: Option<usize>,

    /// Tier on the provider's offer
    #[arg(short, long, default_value = "basic")]
    pub tier: String,

    /// Template slug
    #[arg(long, default_value = "agent-sandbox")]
    pub template: String,

    /// Output directory holding `shard-<i>/` subdirs and `shards.json`
    #[arg(long, default_value = "./paygress-batch")]
    pub output: PathBuf,

    /// Per-shard spawn timeout (seconds)
    #[arg(long, default_value_t = 120)]
    pub timeout_secs: u64,

    /// Container image; ignored when the provider resolves the template slug
    #[arg(long, default_value = "ubuntu:22.04")]
    pub image: String,

    /// Your Nostr private key (nsec) — uses ~/.paygress/identity if unset
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,

    /// Minimum isolation tier, applied to every shard
    #[arg(long, value_parser = parse_isolation_level)]
    pub isolation_level: Option<IsolationLevel>,
}

/// Serialized names are a stable contract — scripts match on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShardStatus {
    Spawned,
    Offline,
    Timeout,
    ProviderError,
    UnknownResponse,
    TransportError,
    JoinError,
}

impl ShardStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ShardStatus::Spawned => "spawned",
            ShardStatus::Offline => "offline",
            ShardStatus::Timeout => "timeout",
            ShardStatus::ProviderError => "provider_error",
            ShardStatus::UnknownResponse => "unknown_response",
            ShardStatus::TransportError => "transport_error",
            ShardStatus::JoinError => "join_error",
        }
    }
}

/// Stable schema.
#[derive(Debug, Clone, Serialize)]
pub struct ShardManifestEntry {
    pub index: usize,
    pub status: ShardStatus,
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

/// The static token list from --tokens or --tokens-file; `--split-token`
/// needs a mint round-trip and lives in [`materialize_tokens`].
pub fn parse_tokens(args: &BatchArgs) -> Result<Vec<String>> {
    if let Some(s) = &args.tokens {
        let v = crate::util::split_csv(s);
        if v.is_empty() {
            anyhow::bail!("--tokens must contain at least one non-empty token");
        }
        return Ok(v);
    }
    if let Some(p) = &args.tokens_file {
        let content = std::fs::read_to_string(p)
            .with_context(|| format!("failed to read tokens file {}", p.display()))?;
        let v = crate::util::parse_token_lines(&content);
        if v.is_empty() {
            anyhow::bail!(
                "token file {} contains no tokens (after stripping comments + blank lines)",
                p.display()
            );
        }
        return Ok(v);
    }
    anyhow::bail!("one of --tokens, --tokens-file, or --split-token is required");
}

/// Performs the ephemeral wallet round-trip when `--split-token` was
/// supplied, otherwise defers to [`parse_tokens`].
pub async fn materialize_tokens(args: &BatchArgs) -> Result<Vec<String>> {
    let Some(big_token) = &args.split_token else {
        return parse_tokens(args);
    };

    let n = args
        .shards
        .ok_or_else(|| anyhow::anyhow!("--shards is required when --split-token is set"))?;
    if n == 0 {
        anyhow::bail!("--shards must be >= 1");
    }

    // Unique filename so concurrent batch invocations don't collide.
    let mut db_path = std::env::temp_dir();
    db_path.push(format!(
        "paygress-batch-split-{}.sqlite",
        uuid::Uuid::new_v4()
    ));

    let result = paygress::cashu::split_token_into_n(big_token, n, &db_path).await;
    // Always remove the temp wallet — it holds the input token's proofs.
    let _ = std::fs::remove_file(&db_path);
    result
}

pub struct FanOutConfig {
    pub provider: String,
    pub tier: String,
    pub template: String,
    pub image: String,
    pub timeout_secs: u64,
    pub isolation_level: Option<IsolationLevel>,
    pub relays: Vec<String>,
    pub nostr_key: String,
}

pub struct ShardSpawn {
    pub index: usize,
    pub ssh_user: String,
    pub ssh_pass: String,
    pub outcome: Result<NostrSpawnOutcome>,
}

pub enum ShardResult {
    Done(Box<ShardSpawn>),
    JoinError(String),
}

/// One pod per token, concurrently; each shard opens its own relay
/// connection. Results are unordered — the caller sorts by index.
pub async fn fan_out_spawns(cfg: &FanOutConfig, tokens: Vec<String>) -> Vec<ShardResult> {
    let mut handles = Vec::with_capacity(tokens.len());
    for (index, token) in tokens.into_iter().enumerate() {
        let provider = cfg.provider.clone();
        let params = NostrSpawnParams {
            tier: cfg.tier.clone(),
            token,
            image: cfg.image.clone(),
            ssh_user: "user".to_string(),
            ssh_pass: generate_password(16),
            template_slug: Some(cfg.template.clone()),
            isolation_level: cfg.isolation_level,
            ..Default::default()
        };
        let relays = cfg.relays.clone();
        let nostr_key = cfg.nostr_key.clone();
        let timeout = cfg.timeout_secs;

        handles.push(tokio::spawn(async move {
            let ssh_user = params.ssh_user.clone();
            let ssh_pass = params.ssh_pass.clone();
            let outcome =
                nostr_spawn_round_trip(&provider, params, relays, nostr_key, timeout).await;
            ShardSpawn {
                index,
                ssh_user,
                ssh_pass,
                outcome,
            }
        }));
    }

    let mut results = Vec::with_capacity(handles.len());
    for h in handles {
        results.push(match h.await {
            Ok(v) => ShardResult::Done(Box::new(v)),
            Err(e) => ShardResult::JoinError(format!("tokio join error: {}", e)),
        });
    }
    results
}

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
        status: ShardStatus::Spawned,
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
    status: ShardStatus,
    err: ErrorResponseContent,
) -> ShardManifestEntry {
    ShardManifestEntry {
        error_type: Some(err.error_type),
        error_message: Some(err.message),
        ..manifest_entry_status_only(index, status, None)
    }
}

fn manifest_entry_status_only(
    index: usize,
    status: ShardStatus,
    message: Option<String>,
) -> ShardManifestEntry {
    ShardManifestEntry {
        index,
        status,
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

fn manifest_entry(cfg: &FanOutConfig, result: ShardResult) -> ShardManifestEntry {
    let spawn = match result {
        ShardResult::Done(s) => *s,
        // The index is lost with the task, so a joined-panic shard
        // reports as shard 0.
        ShardResult::JoinError(msg) => {
            return manifest_entry_status_only(0, ShardStatus::JoinError, Some(msg))
        }
    };
    let ShardSpawn {
        index,
        ssh_user,
        ssh_pass,
        outcome,
    } = spawn;

    match outcome {
        Ok(NostrSpawnOutcome::Success(access)) => {
            manifest_entry_from_success(index, &cfg.provider, &ssh_user, &ssh_pass, access)
        }
        Ok(NostrSpawnOutcome::ProviderError(err)) => {
            manifest_entry_from_error(index, ShardStatus::ProviderError, err)
        }
        Ok(NostrSpawnOutcome::ProviderOffline) => manifest_entry_status_only(
            index,
            ShardStatus::Offline,
            Some("provider's heartbeat did not appear within the live window".to_string()),
        ),
        Ok(NostrSpawnOutcome::Timeout) => manifest_entry_status_only(
            index,
            ShardStatus::Timeout,
            Some(format!(
                "no response within {}s; token may have been spent",
                cfg.timeout_secs
            )),
        ),
        Ok(NostrSpawnOutcome::UnknownResponse(s)) => manifest_entry_status_only(
            index,
            ShardStatus::UnknownResponse,
            Some(format!("body: {}", s.chars().take(200).collect::<String>())),
        ),
        Err(e) => {
            manifest_entry_status_only(index, ShardStatus::TransportError, Some(e.to_string()))
        }
    }
}

/// Created up front so downstream scripts don't race the coordinator;
/// failed shards get an empty subdir rather than none.
fn scaffold_output_dirs(output: &std::path::Path, shards: usize) -> Result<()> {
    std::fs::create_dir_all(output)
        .with_context(|| format!("failed to create output dir {}", output.display()))?;
    for i in 0..shards {
        let p = output.join(format!("shard-{}", i));
        std::fs::create_dir_all(&p)
            .with_context(|| format!("failed to create shard subdir {}", p.display()))?;
    }
    Ok(())
}

pub async fn execute(args: BatchArgs, _verbose: bool) -> Result<()> {
    let tokens = materialize_tokens(&args).await?;
    let n = tokens.len();
    let BatchArgs {
        provider,
        tier,
        template,
        image,
        output,
        timeout_secs,
        isolation_level,
        nostr_key,
        relays,
        ..
    } = args;
    let cfg = FanOutConfig {
        provider: provider.clone(),
        tier: tier.clone(),
        template: template.clone(),
        image,
        timeout_secs,
        isolation_level,
        relays: parse_relays(relays),
        nostr_key: get_or_create_identity(nostr_key)?,
    };

    println!("{}", "Paygress Batch Coordinator".blue().bold());
    println!("{}", "-".repeat(50).blue());
    println!("  Provider:    {}", provider.cyan());
    println!("  Template:    {}", template.cyan());
    println!("  Tier:        {}", tier);
    println!("  Shards:      {}", n);
    println!("  Output dir:  {}", output.display());
    println!();

    scaffold_output_dirs(&output, n)?;

    let mut entries: Vec<ShardManifestEntry> = fan_out_spawns(&cfg, tokens)
        .await
        .into_iter()
        .map(|r| manifest_entry(&cfg, r))
        .collect();
    // Stable order so the JSON manifest matches the shard index.
    entries.sort_by_key(|e| e.index);

    let spawned_count = entries
        .iter()
        .filter(|e| e.status == ShardStatus::Spawned)
        .count();
    let manifest = ShardManifest {
        provider_npub: provider,
        template,
        tier,
        shard_count: n,
        spawned_count,
        shards: entries,
    };

    let manifest_path = output.join("shards.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;

    print_summary(&manifest, &manifest_path);

    if spawned_count < n {
        anyhow::bail!(
            "{} of {} shards failed to spawn (see manifest for details)",
            n - spawned_count,
            n
        );
    }

    Ok(())
}

fn print_summary(manifest: &ShardManifest, manifest_path: &std::path::Path) {
    println!();
    println!("{}", "-".repeat(50).blue());
    println!(
        "{}: {}/{} shards spawned",
        "Result".bold(),
        manifest.spawned_count.to_string().green(),
        manifest.shard_count
    );
    println!("  Manifest: {}", manifest_path.display());
    println!();
    println!("{}", "Per-shard summary:".bold());
    for e in &manifest.shards {
        let status_label = match e.status {
            ShardStatus::Spawned => e.status.as_str().green().to_string(),
            _ => e.status.as_str().red().to_string(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> BatchArgs {
        BatchArgs {
            provider: "npub1abc".to_string(),
            tokens: None,
            tokens_file: None,
            split_token: None,
            shards: None,
            tier: "basic".to_string(),
            template: "agent-sandbox".to_string(),
            output: PathBuf::from("/tmp/paygress-batch-test"),
            timeout_secs: 120,
            image: "ubuntu:22.04".to_string(),
            nostr_key: None,
            relays: None,
            isolation_level: None,
        }
    }

    fn args_with_tokens(s: &str) -> BatchArgs {
        BatchArgs {
            tokens: Some(s.to_string()),
            ..base_args()
        }
    }

    fn args_with_file(p: PathBuf) -> BatchArgs {
        BatchArgs {
            tokens_file: Some(p),
            ..base_args()
        }
    }

    fn args_with_split(token: &str, shards: Option<usize>) -> BatchArgs {
        BatchArgs {
            split_token: Some(token.to_string()),
            shards,
            ..base_args()
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
        // Trailing comma is a common copy/paste artifact.
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
    fn shard_status_serializes_to_the_documented_strings() {
        assert_eq!(
            serde_json::to_string(&ShardStatus::ProviderError).unwrap(),
            "\"provider_error\""
        );
        assert_eq!(
            serde_json::to_string(&ShardStatus::Spawned).unwrap(),
            "\"spawned\""
        );
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
        assert_eq!(e.status, ShardStatus::Spawned);
        assert_eq!(e.host.as_deref(), Some("10.0.0.7"));
        assert_eq!(e.ssh_port, Some(30042));
        assert_eq!(e.ssh_user.as_deref(), Some("user"));
        assert_eq!(e.ssh_pass.as_deref(), Some("pw"));
        assert!(e.error_type.is_none());
    }

    #[test]
    fn manifest_entry_success_falls_back_when_host_address_empty() {
        // Old providers don't set host_address; the manifest must still
        // expose a usable host so downstream scripts can SSH.
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

    #[tokio::test]
    async fn materialize_tokens_split_without_shards_errors() {
        // clap's `requires = "split_token"` covers the other direction.
        // "--split-token without --shards" is a runtime check because
        // clap can't express a mutual requirement without making both
        // flags mandatory.
        let args = args_with_split("dummy-token-not-used", None);
        let err = materialize_tokens(&args).await.unwrap_err();
        assert!(
            err.to_string().contains("--shards is required"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn materialize_tokens_split_zero_shards_errors() {
        let args = args_with_split("dummy-token-not-used", Some(0));
        let err = materialize_tokens(&args).await.unwrap_err();
        assert!(err.to_string().contains(">= 1"), "got: {}", err);
    }

    #[tokio::test]
    async fn materialize_tokens_split_invalid_token_errors_fast() {
        // Must fail at Token::from_str BEFORE the wallet attempts a
        // mint round-trip (slow, and would leak a sqlite file).
        let args = args_with_split("not-a-real-cashu-token", Some(3));
        let err = materialize_tokens(&args).await.unwrap_err();
        assert!(
            err.to_string().contains("invalid input token"),
            "got: {}",
            err
        );
    }

    #[tokio::test]
    async fn materialize_tokens_falls_through_to_parse_tokens_for_static_input() {
        let args = args_with_tokens("a,b,c");
        let v = materialize_tokens(&args).await.unwrap();
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn manifest_entry_error_carries_error_fields() {
        let err = ErrorResponseContent {
            error_type: "token_already_spent".to_string(),
            message: "this Cashu token was already redeemed".to_string(),
            details: None,
        };
        let e = manifest_entry_from_error(2, ShardStatus::ProviderError, err);
        assert_eq!(e.index, 2);
        assert_eq!(e.status, ShardStatus::ProviderError);
        assert_eq!(e.error_type.as_deref(), Some("token_already_spent"));
        assert!(e.host.is_none());
        assert!(e.ssh_port.is_none());
    }
}
