// Crawl Nostr for the marketplace's live state and write the JSON snapshot the
// static dashboard reads.
//
// Receipts / consumers / stake statuses are left empty: this is at-a-glance
// dashboarding, not the full reputation aggregation, which needs a real receipt
// corpus.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use paygress::nostr::{NostrRelaySubscriber, RelayConfig};
use paygress::observatory::aggregator::{compute_snapshot, AggregatorInput};

#[derive(Parser)]
#[command(name = "paygress-snapshot")]
#[command(about = "Crawl Nostr for live providers and write a dashboard JSON snapshot")]
struct Args {
    /// Output path for the snapshot JSON.
    #[arg(long, default_value = "dashboard/snapshot.json")]
    out: PathBuf,

    /// Nostr relays to query (comma-separated).
    #[arg(
        long,
        default_value = "wss://relay.damus.io,wss://nos.lol,wss://relay.nostr.band"
    )]
    relays: String,

    /// Heartbeat lookback window in seconds, feeding the last-seen column.
    #[arg(long, default_value_t = 600)]
    heartbeat_window_secs: u64,

    /// Anchor providers (npubs, comma-separated). Flagged in the UI.
    #[arg(long, default_value = "")]
    anchors: String,

    /// Subscription timeout per query in seconds. Relay cold-handshake +
    /// REQ + EOSE exceeds a 5s budget and silently drops events; measured
    /// at 0 last-seen rows on 5s, correct on 15s.
    #[arg(long, default_value_t = 15)]
    timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let relays: Vec<String> = args
        .relays
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if relays.is_empty() {
        anyhow::bail!("at least one relay is required");
    }

    let anchors: HashSet<String> = args
        .anchors
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let relay_count = relays.len();
    let nostr = NostrRelaySubscriber::new(RelayConfig {
        relays,
        private_key: None,
    })
    .await
    .context("connect to relays")?;

    eprintln!("querying offers from {} relays...", relay_count);
    let offers = nostr.query_providers().await.context("query offers")?;
    eprintln!("got {} offers", offers.len());

    eprintln!(
        "querying heartbeats (last {}s) for {} providers...",
        args.heartbeat_window_secs,
        offers.len()
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let since = now.saturating_sub(args.heartbeat_window_secs);
    let mut heartbeats = Vec::new();
    for npub in offers.iter().map(|o| o.provider_npub.as_str()) {
        // Per-provider timeout so one silent provider can't stall the crawl.
        match tokio::time::timeout(
            Duration::from_secs(args.timeout_secs),
            nostr.query_heartbeats(npub, since),
        )
        .await
        {
            Ok(Ok(mut hb)) => heartbeats.append(&mut hb),
            Ok(Err(e)) => eprintln!("  heartbeat query for {} failed: {}", npub, e),
            Err(_) => eprintln!("  heartbeat query for {} timed out", npub),
        }
    }
    eprintln!("got {} heartbeats", heartbeats.len());

    let input = AggregatorInput {
        offers,
        heartbeats,
        receipts: Vec::new(),
        consumers: HashMap::new(),
        stake_statuses: HashMap::new(),
        anchor_providers: anchors,
    };
    let snapshot = compute_snapshot(&input, now);

    if let Some(parent) = args.out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(&snapshot)?;
    std::fs::write(&args.out, json).with_context(|| format!("write {}", args.out.display()))?;
    eprintln!(
        "wrote {} ({} provider rows)",
        args.out.display(),
        snapshot.providers.len()
    );

    Ok(())
}
