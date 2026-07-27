// Pure: no I/O, no clock. `now` and stake statuses are inputs, so two
// invocations with the same inputs produce byte-identical JSON anywhere.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::nostr::{HeartbeatContent, IsolationLevel, PodSpec, ProviderOfferContent};
use crate::reputation::{score_provider, CompletionReceipt, ConsumerProfile, SybilHeuristics};
use crate::stake::{stake_rank, StakeStatus};

/// Bumped so the frontend can branch without breaking old archives.
pub const SNAPSHOT_VERSION: u8 = 1;

/// Receipts older than this age out of the rolling-window score.
pub const RECEIPT_WINDOW_SECS: u64 = 30 * 24 * 3600;

/// The caller owns all the I/O (Nostr, Esplora, consumer first-seen times).
pub struct AggregatorInput {
    pub offers: Vec<ProviderOfferContent>,
    pub heartbeats: Vec<HeartbeatContent>,
    pub receipts: Vec<CompletionReceipt>,
    pub consumers: HashMap<String, ConsumerProfile>,
    /// Pre-computed against Esplora by the caller, keyed by `provider_npub`.
    pub stake_statuses: HashMap<String, StakeStatus>,
    /// Paygress-team-run providers, flagged in the UI.
    pub anchor_providers: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u8,
    pub generated_at: u64,
    pub receipt_window_secs: u64,
    /// Sorted by `npub` for byte-identical reproducibility.
    pub providers: Vec<ProviderSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSummary {
    pub npub: String,
    pub hostname: String,
    /// Present only when the offer opted in; geo is never surfaced
    /// involuntarily.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jurisdiction: Option<String>,
    pub score: f32,
    pub last_seen_unix: Option<u64>,
    /// `Some` only when the pre-computed `StakeStatus` was `Valid`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stake: Option<StakeSummary>,
    pub anchor: bool,
    pub specs: Vec<PodSpec>,
    pub isolation_level: IsolationLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StakeSummary {
    pub effective_sats: u64,
    pub locktime_unix: u64,
    /// log(sats × locked_seconds), for staked-tier ordering.
    pub rank: f64,
}

pub fn compute_snapshot(input: &AggregatorInput, now: u64) -> Snapshot {
    let heuristics = SybilHeuristics::default();
    let receipt_floor = now.saturating_sub(RECEIPT_WINDOW_SECS);

    let windowed: Vec<&CompletionReceipt> = input
        .receipts
        .iter()
        .filter(|r| r.completed_at >= receipt_floor)
        .collect();

    let mut last_seen: HashMap<&str, u64> = HashMap::new();
    for hb in &input.heartbeats {
        let cur = last_seen.entry(hb.provider_npub.as_str()).or_insert(0);
        if hb.timestamp > *cur {
            *cur = hb.timestamp;
        }
    }

    // BTreeMap: iteration ordered by npub, which is what makes the
    // output reproducible.
    let mut by_npub: BTreeMap<&str, &ProviderOfferContent> = BTreeMap::new();
    for offer in &input.offers {
        by_npub.insert(offer.provider_npub.as_str(), offer);
    }

    let scored_receipts: Vec<CompletionReceipt> = windowed.iter().map(|r| (*r).clone()).collect();

    let mut providers = Vec::with_capacity(by_npub.len());
    for (npub, offer) in by_npub {
        // Always-accept verifiers: bad receipts are dropped during
        // the Nostr crawl, before they reach this pure path.
        let score = score_provider(
            npub,
            &scored_receipts,
            &input.consumers,
            now,
            &heuristics,
            |_| true,
            |_| true,
        );

        let stake = input.stake_statuses.get(npub).and_then(|s| match s {
            StakeStatus::Valid {
                effective_sats,
                locktime_unix,
            } => Some(StakeSummary {
                effective_sats: *effective_sats,
                locktime_unix: *locktime_unix,
                rank: stake_rank(*effective_sats, *locktime_unix, now),
            }),
            _ => None,
        });

        providers.push(ProviderSummary {
            npub: npub.to_string(),
            hostname: offer.hostname.clone(),
            jurisdiction: offer.location.clone(),
            score,
            last_seen_unix: last_seen.get(npub).copied(),
            stake,
            anchor: input.anchor_providers.contains(npub),
            specs: offer.specs.clone(),
            isolation_level: offer.isolation_level,
        });
    }

    Snapshot {
        version: SNAPSHOT_VERSION,
        generated_at: now,
        receipt_window_secs: RECEIPT_WINDOW_SECS,
        providers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nostr::{CapacityInfo, IsolationLevel, SCHEMA_VERSION};
    use crate::reputation::PaymentProof;
    use crate::stake::StakeStatus;

    fn offer(npub: &str, hostname: &str, location: Option<&str>) -> ProviderOfferContent {
        ProviderOfferContent {
            provider_npub: npub.to_string(),
            hostname: hostname.to_string(),
            location: location.map(|s| s.to_string()),
            capabilities: vec!["lxc".to_string()],
            specs: vec![],
            whitelisted_mints: vec!["https://mint.example".to_string()],
            uptime_percent: 99.0,
            total_jobs_completed: 5,
            api_endpoint: None,
            version: SCHEMA_VERSION,
            isolation_level: IsolationLevel::SharedKernel,
            stake_proof: None,
        }
    }

    fn heartbeat(npub: &str, ts: u64) -> HeartbeatContent {
        HeartbeatContent {
            provider_npub: npub.to_string(),
            timestamp: ts,
            active_workloads: 0,
            available_capacity: CapacityInfo {
                cpu_available: 0,
                memory_mb_available: 0,
                storage_gb_available: 0,
            },
            version: SCHEMA_VERSION,
        }
    }

    fn receipt(provider: &str, consumer: &str, completed_at: u64) -> CompletionReceipt {
        CompletionReceipt {
            lease_id: format!("l-{}-{}-{}", provider, consumer, completed_at),
            provider_npub: provider.to_string(),
            consumer_npub: consumer.to_string(),
            duration_paid: 3600,
            duration_delivered: 3600,
            success_flag: 1.0,
            payment_proof: PaymentProof {
                mint_url: "https://mint.example".to_string(),
                swap_response_signature: "sig".to_string(),
            },
            version: 1,
            consumer_signature: Some("c".to_string()),
            provider_co_signature: Some("p".to_string()),
            completed_at,
        }
    }

    #[test]
    fn snapshot_is_byte_identical_when_inputs_match() {
        let now = 1_700_000_000;
        let input = AggregatorInput {
            offers: vec![
                offer("npubB", "host-b", None),
                offer("npubA", "host-a", Some("BER")),
            ],
            heartbeats: vec![heartbeat("npubA", now - 60), heartbeat("npubA", now - 120)],
            receipts: vec![],
            consumers: HashMap::new(),
            stake_statuses: HashMap::new(),
            anchor_providers: HashSet::new(),
        };
        let snap_a = compute_snapshot(&input, now);
        let snap_b = compute_snapshot(&input, now);
        let json_a = serde_json::to_string(&snap_a).unwrap();
        let json_b = serde_json::to_string(&snap_b).unwrap();
        assert_eq!(json_a, json_b);
    }

    #[test]
    fn providers_are_sorted_by_npub_for_reproducibility() {
        let now = 1_700_000_000;
        let input = AggregatorInput {
            offers: vec![
                offer("npubZ", "z", None),
                offer("npubA", "a", None),
                offer("npubM", "m", None),
            ],
            heartbeats: vec![],
            receipts: vec![],
            consumers: HashMap::new(),
            stake_statuses: HashMap::new(),
            anchor_providers: HashSet::new(),
        };
        let snap = compute_snapshot(&input, now);
        let order: Vec<_> = snap.providers.iter().map(|p| p.npub.as_str()).collect();
        assert_eq!(order, vec!["npubA", "npubM", "npubZ"]);
    }

    #[test]
    fn jurisdiction_is_only_emitted_if_offer_opted_in() {
        let now = 1_700_000_000;
        let input = AggregatorInput {
            offers: vec![offer("npubA", "a", Some("BER")), offer("npubB", "b", None)],
            heartbeats: vec![],
            receipts: vec![],
            consumers: HashMap::new(),
            stake_statuses: HashMap::new(),
            anchor_providers: HashSet::new(),
        };
        let snap = compute_snapshot(&input, now);
        let by: HashMap<_, _> = snap
            .providers
            .iter()
            .map(|p| (p.npub.as_str(), p))
            .collect();
        assert_eq!(by["npubA"].jurisdiction.as_deref(), Some("BER"));
        assert!(by["npubB"].jurisdiction.is_none());
    }

    #[test]
    fn old_receipts_are_aged_out_of_window() {
        let now = 1_700_000_000;
        let in_window = receipt("P", "C", now - 7 * 24 * 3600);
        let out_of_window = receipt("P", "C", now - 60 * 24 * 3600);
        let mut consumers = HashMap::new();
        consumers.insert(
            "C".to_string(),
            ConsumerProfile {
                npub: "C".to_string(),
                first_seen: now - 365 * 24 * 3600, // passes the history gate
            },
        );
        let input = AggregatorInput {
            offers: vec![offer("P", "p", None)],
            heartbeats: vec![],
            receipts: vec![in_window, out_of_window],
            consumers,
            stake_statuses: HashMap::new(),
            anchor_providers: HashSet::new(),
        };
        let snap = compute_snapshot(&input, now);
        // One consumer, one provider → capped to max_share = 0.20.
        assert!((snap.providers[0].score - 0.20).abs() < 1e-6);
    }

    #[test]
    fn anchor_providers_are_flagged() {
        let now = 1_700_000_000;
        let mut anchors = HashSet::new();
        anchors.insert("npubAnchor".to_string());
        let input = AggregatorInput {
            offers: vec![
                offer("npubAnchor", "anchor", None),
                offer("npubOther", "other", None),
            ],
            heartbeats: vec![],
            receipts: vec![],
            consumers: HashMap::new(),
            stake_statuses: HashMap::new(),
            anchor_providers: anchors,
        };
        let snap = compute_snapshot(&input, now);
        let by: HashMap<_, _> = snap
            .providers
            .iter()
            .map(|p| (p.npub.as_str(), p))
            .collect();
        assert!(by["npubAnchor"].anchor);
        assert!(!by["npubOther"].anchor);
    }

    #[test]
    fn stake_summary_only_emitted_when_status_is_valid() {
        let now = 1_700_000_000;
        let mut stake_statuses = HashMap::new();
        stake_statuses.insert(
            "npubStaked".to_string(),
            StakeStatus::Valid {
                effective_sats: 100_000,
                locktime_unix: now + 30 * 24 * 3600,
            },
        );
        stake_statuses.insert("npubSpent".to_string(), StakeStatus::Spent);
        let input = AggregatorInput {
            offers: vec![
                offer("npubStaked", "s", None),
                offer("npubSpent", "x", None),
            ],
            heartbeats: vec![],
            receipts: vec![],
            consumers: HashMap::new(),
            stake_statuses,
            anchor_providers: HashSet::new(),
        };
        let snap = compute_snapshot(&input, now);
        let by: HashMap<_, _> = snap
            .providers
            .iter()
            .map(|p| (p.npub.as_str(), p))
            .collect();
        assert!(by["npubStaked"].stake.is_some());
        assert!(by["npubSpent"].stake.is_none());
        assert!(by["npubStaked"].stake.as_ref().unwrap().rank > 0.0);
    }

    #[test]
    fn last_seen_picks_max_timestamp() {
        let now = 1_700_000_000;
        let input = AggregatorInput {
            offers: vec![offer("P", "p", None)],
            heartbeats: vec![
                heartbeat("P", now - 600),
                heartbeat("P", now - 60),
                heartbeat("P", now - 300),
            ],
            receipts: vec![],
            consumers: HashMap::new(),
            stake_statuses: HashMap::new(),
            anchor_providers: HashSet::new(),
        };
        let snap = compute_snapshot(&input, now);
        assert_eq!(snap.providers[0].last_seen_unix, Some(now - 60));
    }

    #[test]
    fn empty_input_yields_empty_provider_list() {
        let now = 1_700_000_000;
        let input = AggregatorInput {
            offers: vec![],
            heartbeats: vec![],
            receipts: vec![],
            consumers: HashMap::new(),
            stake_statuses: HashMap::new(),
            anchor_providers: HashSet::new(),
        };
        let snap = compute_snapshot(&input, now);
        assert_eq!(snap.providers.len(), 0);
        assert_eq!(snap.version, SNAPSHOT_VERSION);
        assert_eq!(snap.receipt_window_secs, RECEIPT_WINDOW_SECS);
    }
}
