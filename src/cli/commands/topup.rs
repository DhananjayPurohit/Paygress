// `paygress-cli topup` — extend a workload's lease with more payment,
// single-shot via Nostr (--provider) or HTTP (--server), or streaming
// (--stream --tokens-file): one TopUp DM per tick.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Args;
use colored::Colorize;

use crate::api::{PaygressClient, TopupRequest};
use crate::util::{get_or_create_identity, parse_relays, spinner};
use paygress::discovery::DiscoveryClient;

#[derive(Args)]
pub struct TopupArgs {
    /// Pod/workload ID to top up
    #[arg(short, long)]
    pub pod_id: String,

    /// Cashu token for payment (single-shot mode only)
    #[arg(short = 'k', long)]
    pub token: Option<String>,

    /// Provider ID (Nostr mode) - if omitted, uses --server for HTTP mode
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

    /// Stream chunked top-ups: one TopUp DM per tick from --tokens-file
    #[arg(long)]
    pub stream: bool,

    /// Seconds between top-ups in streaming mode
    #[arg(long, default_value_t = 60)]
    pub tick_secs: u64,

    /// File with one Cashu token per line, streaming mode (`#` comments ignored)
    #[arg(long)]
    pub tokens_file: Option<PathBuf>,
}

pub async fn execute(mut args: TopupArgs, verbose: bool) -> Result<()> {
    if args.stream {
        return execute_stream(args).await;
    }

    let token = args
        .token
        .take()
        .ok_or_else(|| anyhow::anyhow!("--token is required (or use --stream --tokens-file)"))?;

    if let Some(provider) = args.provider.take() {
        return execute_nostr_topup(provider, args, token).await;
    }

    let server = args.server.take().ok_or_else(|| {
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

    let spinner = spinner("Processing top-up payment...");
    let client = PaygressClient::new(server);
    let response = client
        .topup_pod(TopupRequest {
            pod_id: args.pod_id,
            cashu_token: Some(token),
        })
        .await?;
    spinner.finish_and_clear();

    if !response.success {
        let error_msg = response.error.as_deref().unwrap_or("Unknown error");
        return Err(anyhow::anyhow!("Failed to top up pod: {}", error_msg));
    }

    println!("{}", "Pod topped up successfully!".green().bold());
    println!();

    if let Some(pod_id) = &response.pod_id {
        println!("  {} {}", "Pod ID:".bold(), pod_id);
    }
    if let Some(expires) = &response.new_expires_at {
        println!("  {} {}", "New Expiry:".bold(), expires);
    }
    if let Some(added) = response.added_seconds {
        println!("  {} +{}m {}s", "Added:".bold(), added / 60, added % 60);
    }
    if let Some(msg) = &response.message {
        println!("  {} {}", "Message:".bold(), msg);
    }

    Ok(())
}

/// Shared by the CLI pretty-printer and the MCP server.
#[derive(Debug, Clone)]
pub enum NostrTopupOutcome {
    Success(paygress::nostr::TopUpResponseContent),
    /// Provider rejected the topup; `error_type` strings are stable, so
    /// callers can match on them.
    ProviderError(paygress::nostr::ErrorResponseContent),
    UnknownResponse(String),
    /// No reply in the timeout window. The token MAY have been spent.
    Timeout,
}

/// No stdout I/O — pure round-trip plus structured outcome.
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

async fn execute_nostr_topup(provider_npub: String, args: TopupArgs, token: String) -> Result<()> {
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

/// Returned in file order, so per-tick spend stays predictable.
pub fn read_tokens_file(path: &std::path::Path) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {}", path.display(), e))?;
    Ok(crate::util::parse_token_lines(&raw))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSummary {
    pub chunks_sent: usize,
    pub chunks_failed: usize,
    pub exhausted: bool,
}

pub type SendFuture<'a> = Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

/// Calls `send_one` once per tick until `tokens` is empty. Errors are
/// counted but never abort the loop — the chunk is already paid for.
/// Generic over the send function so tests can pass a recording mock.
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

async fn execute_stream(args: TopupArgs) -> Result<()> {
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
    let pod_id = args.pod_id;
    let provider = provider_npub;

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
