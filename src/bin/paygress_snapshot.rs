// `paygress-snapshot` — crawl Nostr for the marketplace's live state
// and write a single JSON snapshot the static dashboard reads.
//
// Pure I/O glue: queries provider offers + recent heartbeats, drops
// receipts/consumers/stake-statuses (left as `Default::default()`
// because this snapshot is for at-a-glance dashboarding, not the full
// reputation aggregation that needs a real receipt corpus). Routes
// through `paygress::observatory::aggregator::compute_snapshot` so
// the wire format matches what other tools (and the spec) already
// expect.
//
// Usage:
//     cargo run --release --bin paygress-snapshot -- \
//         [--out snapshot.json] [--relays wss://...,wss://...]

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

    /// Heartbeat lookback window in seconds. Recent heartbeats inform
    /// the dashboard's last-seen / online indicators.
    #[arg(long, default_value_t = 600)]
    heartbeat_window_secs: u64,

    /// Anchor providers (npubs, comma-separated). Flagged in the UI.
    #[arg(long, default_value = "")]
    anchors: String,

    /// Subscription timeout per query in seconds. The default of
    /// 15s is conservative: relay cold-handshake + REQ + EOSE can
    /// easily exceed a 5s budget, and tighter timeouts silently
    /// drop events (verified empirically — at 5s the snapshot
    /// showed 0 last-seen even with the relay holding the events;
    /// at 15s it correctly populates).
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

    let nostr = NostrRelaySubscriber::new(RelayConfig {
        relays: relays.clone(),
        private_key: None,
    })
    .await
    .context("connect to relays")?;

    eprintln!("querying offers from {} relays...", relays.len());
    let offers = nostr.query_providers().await.context("query offers")?;
    eprintln!("got {} offers", offers.len());

    // Heartbeats: query each provider's recent activity. Keeps the
    // snapshot at-a-glance fast; the full uptime aggregator runs at
    // a different cadence.
    eprintln!(
        "querying heartbeats (last {}s) for {} providers...",
        args.heartbeat_window_secs,
        offers.len()
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let since = now.saturating_sub(args.heartbeat_window_secs);
    let provider_npubs: Vec<String> = offers.iter().map(|o| o.provider_npub.clone()).collect();
    let mut heartbeats = Vec::new();
    for npub in &provider_npubs {
        // Per-provider with a short timeout so a slow / silent
        // provider doesn't hold up the whole crawl.
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
