// Topup command - Extend workload lifetime with additional payment
//
// Modes:
//   - Single-shot Nostr (default with --provider): one TopUp DM,
//     wait for response.
//   - Single-shot HTTP (--server): one HTTP topup call.
//   - Streaming (--stream + --tokens-file): one TopUp DM per tick,
//     pulling fresh tokens from the file, until exhausted or
//     Ctrl-C. Implements R15 in its year-1 chunked form (Unit 16).

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

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

    /// Cashu token for payment (single-shot mode only).
    #[arg(short = 'k', long)]
    pub token: Option<String>,

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

    /// Stream chunked top-ups: one TopUp DM per tick, pulling fresh
    /// tokens from `--tokens-file` until exhausted or Ctrl-C.
    /// Sub-second streaming (full Cashu streaming-NUT) is out of
    /// scope for year 1 — this is "stream sats, not stream Cashu
    /// protocol bytes" (Unit 16).
    #[arg(long)]
    pub stream: bool,

    /// Seconds between top-ups in streaming mode. Smaller ticks
    /// have higher message overhead; larger ticks coarsen failover.
    #[arg(long, default_value_t = 60)]
    pub tick_secs: u64,

    /// Path to a file with one Cashu token per line (streaming mode).
    /// Blank lines and lines starting with `#` are ignored.
    #[arg(long)]
    pub tokens_file: Option<PathBuf>,
}

pub async fn execute(args: TopupArgs, verbose: bool) -> Result<()> {
    if args.stream {
        return execute_stream(args, verbose).await;
    }

    // Single-shot path: --token is required.
    let token = args
        .token
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--token is required (or use --stream --tokens-file)"))?;

    if args.provider.is_some() {
        let provider = args.provider.clone().unwrap();
        return execute_nostr_topup(provider, args, token, verbose).await;
    }

    let server = args.server.clone().ok_or_else(|| {
        anyhow::anyhow!("Either --provider (Nostr) or --server (HTTP) is required")
    })?;

    execute_http_topup(&server, args, token, verbose).await
}

async fn execute_http_topup(
    server: &str,
    args: TopupArgs,
    token: String,
    verbose: bool,
) -> Result<()> {
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
    spinner.enable_steady_tick(Duration::from_millis(100));

    let client = PaygressClient::new(server);

    let request = TopupRequest {
        pod_id: args.pod_id.clone(),
        cashu_token: Some(token),
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

/// Typed outcome of a Nostr topup round-trip. Same dual-shape pattern
/// as `NostrSpawnOutcome` — lets the pretty-print path and the MCP
/// server share one transport.
#[derive(Debug, Clone)]
pub enum NostrTopupOutcome {
    Success(paygress::nostr::TopUpResponseContent),
    /// Provider rejected the topup (insufficient payment, lease
    /// expired, not the owner, race-lost, etc.). Error type strings
    /// are stable — callers can match.
    ProviderError(paygress::nostr::ErrorResponseContent),
    /// Provider responded but the body wasn't either schema.
    UnknownResponse(String),
    /// Provider didn't respond within the timeout window. The token
    /// MAY have been spent — caller should `status` to check.
    Timeout,
}

/// Dispatch a single Nostr topup request and wait for the provider's
/// reply. No I/O on stdout — pure round-trip + structured outcome.
pub async fn nostr_topup_round_trip(
    pod_id: &str,
    token: &str,
    provider_npub: &str,
    relays: Vec<String>,
    nostr_key: String,
    timeout_secs: u64,
) -> Result<NostrTopupOutcome> {
    use paygress::nostr::{EncryptedTopUpPodRequest, ErrorResponseContent, TopUpResponseContent};

    let client = DiscoveryClient::new_with_key(relays, nostr_key).await?;

    let request = EncryptedTopUpPodRequest {
        pod_npub: pod_id.to_string(),
        cashu_token: token.to_string(),
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
        Ok(response) => {
            // Provider reply order: try TopUpResponseContent first
            // (the success path) then ErrorResponseContent. Both have
            // distinct shapes so the wrong-type parse fails cleanly.
            if let Ok(s) = serde_json::from_str::<TopUpResponseContent>(&response.content) {
                Ok(NostrTopupOutcome::Success(s))
            } else if let Ok(err) = serde_json::from_str::<ErrorResponseContent>(&response.content)
            {
                Ok(NostrTopupOutcome::ProviderError(err))
            } else {
                Ok(NostrTopupOutcome::UnknownResponse(response.content))
            }
        }
        Err(_) => Ok(NostrTopupOutcome::Timeout),
    }
}

async fn execute_nostr_topup(
    provider_npub: String,
    args: TopupArgs,
    token: String,
    _verbose: bool,
) -> Result<()> {
    println!("{}", "Topping Up Workload".blue().bold());
    println!("{}", "-".repeat(50).blue());
    println!();

    let relays = parse_relays(args.relays);
    let nostr_key = get_or_create_identity(args.nostr_key)?;

    println!("  Pod ID:   {}", args.pod_id.cyan());
    println!("  Provider: {}", provider_npub);
    println!();
    print!("  Sending topup request... ");
    println!("{}", "SENT".green());
    println!();
    println!("  Waiting for provider response (timeout: 60s)...");

    let outcome =
        nostr_topup_round_trip(&args.pod_id, &token, &provider_npub, relays, nostr_key, 60).await?;
    println!();

    match outcome {
        NostrTopupOutcome::Success(resp) => {
            println!("{}", "Topup successful!".green().bold());
            println!("  {} {}", "New Expiry:".bold(), resp.new_expires_at);
            println!("  {} +{}s", "Added:".bold(), resp.extended_duration_seconds);
            if !resp.message.is_empty() {
                println!("  {} {}", "Message:".bold(), resp.message);
            }
        }
        NostrTopupOutcome::ProviderError(err) => {
            println!("{}", "Topup failed".red().bold());
            println!("  Type:    {}", err.error_type);
            println!("  Message: {}", err.message);
            if let Some(d) = err.details {
                println!("  Details: {}", d);
            }
        }
        NostrTopupOutcome::UnknownResponse(body) => {
            println!("{}", "Unknown topup response".yellow().bold());
            println!("Body: {}", body);
        }
        NostrTopupOutcome::Timeout => {
            println!(
                "  {} {}",
                "Warning:".yellow(),
                "Provider didn't respond in time.".yellow()
            );
            println!("The topup request was sent but the provider didn't respond in time.");
            println!(
                "Check status with: paygress-cli status --pod-id {} --provider {}",
                args.pod_id, provider_npub
            );
        }
    }

    Ok(())
}

// ==================== Streaming ====================

/// Read tokens from a file (one per line). Blank lines and `#`
/// comments are ignored. Returned in file order so callers can
/// reason about per-tick spend predictably.
pub fn read_tokens_file(path: &std::path::Path) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {}", path.display(), e))?;
    Ok(raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect())
}

/// Outcome of a streaming session, useful for tests and CLI exit
/// reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSummary {
    pub chunks_sent: usize,
    pub chunks_failed: usize,
    pub exhausted: bool,
}

/// Future returned by the per-chunk send function.
pub type SendFuture<'a> = Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

/// Pure streaming loop: pulls tokens off `tokens` and calls
/// `send_one(token)` once per tick until the iterator is empty.
/// Errors from `send_one` are counted but do not abort the loop —
/// the user's wallet has already paid for the chunk; we don't get
/// it back by retrying.
///
/// The loop is generic over the send function so unit tests can
/// pass a mock that records calls (no Nostr / no provider needed).
pub async fn run_stream_loop<F>(tokens: Vec<String>, tick: Duration, send_one: F) -> StreamSummary
where
    F: for<'a> Fn(&'a str) -> SendFuture<'a> + Send + Sync,
{
    let mut chunks_sent = 0usize;
    let mut chunks_failed = 0usize;

    let mut iter = tokens.into_iter();
    while let Some(token) = iter.next() {
        match send_one(&token).await {
            Ok(()) => chunks_sent += 1,
            Err(e) => {
                chunks_failed += 1;
                tracing::warn!("streaming top-up chunk failed: {}", e);
            }
        }
        if iter.len() > 0 {
            tokio::time::sleep(tick).await;
        }
    }

    StreamSummary {
        chunks_sent,
        chunks_failed,
        exhausted: true,
    }
}

async fn execute_stream(args: TopupArgs, _verbose: bool) -> Result<()> {
    let provider_npub = args.provider.clone().ok_or_else(|| {
        anyhow::anyhow!("--stream requires --provider (HTTP streaming is not yet supported)")
    })?;
    let tokens_file = args
        .tokens_file
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--stream requires --tokens-file"))?;

    let tokens = read_tokens_file(&tokens_file)?;
    if tokens.is_empty() {
        anyhow::bail!(
            "--tokens-file {} contained no usable tokens",
            tokens_file.display()
        );
    }

    println!("{}", "Streaming Top-up".blue().bold());
    println!("  Pod ID:   {}", args.pod_id.cyan());
    println!("  Provider: {}", provider_npub);
    println!(
        "  Tokens:   {} (from {})",
        tokens.len(),
        tokens_file.display()
    );
    println!("  Tick:     {}s", args.tick_secs);
    println!();

    let relays = parse_relays(args.relays);
    let nostr_key = get_or_create_identity(args.nostr_key)?;
    let client = Arc::new(DiscoveryClient::new_with_key(relays, nostr_key).await?);
    let pod_id = args.pod_id.clone();
    let provider = provider_npub.clone();

    let summary = run_stream_loop(tokens, Duration::from_secs(args.tick_secs), move |token| {
        let client = client.clone();
        let pod_id = pod_id.clone();
        let provider = provider.clone();
        let token = token.to_string();
        Box::pin(async move {
            let request = paygress::nostr::EncryptedTopUpPodRequest {
                pod_npub: pod_id,
                cashu_token: token,
            };
            let json = serde_json::to_string(&request)?;
            client
                .nostr()
                .send_encrypted_private_message(&provider, json, "nip04")
                .await?;
            Ok(())
        })
    })
    .await;

    println!();
    println!(
        "{} {} chunk(s) sent, {} failed (token list exhausted)",
        "Streaming complete:".green().bold(),
        summary.chunks_sent,
        summary.chunks_failed
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::NamedTempFile;

    #[test]
    fn read_tokens_file_skips_comments_and_blanks() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "# comment").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "  cashuA1  ").unwrap();
        writeln!(f, "cashuA2").unwrap();
        writeln!(f, "# trailing comment").unwrap();
        f.flush().unwrap();

        let tokens = read_tokens_file(f.path()).unwrap();
        assert_eq!(tokens, vec!["cashuA1".to_string(), "cashuA2".to_string()]);
    }

    #[tokio::test]
    async fn stream_loop_invokes_send_per_token_in_order() {
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();

        let summary = run_stream_loop(
            vec!["a".into(), "b".into(), "c".into()],
            Duration::from_millis(0),
            move |t| {
                let cap = cap.clone();
                let t = t.to_string();
                Box::pin(async move {
                    cap.lock().unwrap().push(t);
                    Ok(())
                })
            },
        )
        .await;

        assert_eq!(summary.chunks_sent, 3);
        assert_eq!(summary.chunks_failed, 0);
        assert!(summary.exhausted);
        assert_eq!(*captured.lock().unwrap(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn stream_loop_counts_failures_and_keeps_going() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();

        let summary = run_stream_loop(
            vec!["good".into(), "bad".into(), "good".into()],
            Duration::from_millis(0),
            move |t| {
                let calls = calls2.clone();
                let t = t.to_string();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if t == "bad" {
                        Err(anyhow::anyhow!("simulated transient failure"))
                    } else {
                        Ok(())
                    }
                })
            },
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(summary.chunks_sent, 2);
        assert_eq!(summary.chunks_failed, 1);
    }

    #[tokio::test]
    async fn stream_loop_with_empty_token_list_is_a_noop() {
        let summary = run_stream_loop(vec![], Duration::from_secs(60), move |_t| {
            Box::pin(async { Ok(()) })
        })
        .await;
        assert_eq!(summary.chunks_sent, 0);
        assert_eq!(summary.chunks_failed, 0);
        assert!(summary.exhausted);
    }
}
