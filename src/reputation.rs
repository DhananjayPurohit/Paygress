// Signed completion receipts and Sybil-resistant scoring (Unit 10
// of the 12-month plan,
// docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//
// Receipts that contribute to a provider's reputation must be:
//   1. Co-signed by both consumer and provider.
//   2. Bound to a verifiable Cashu spend proof (a swap-response
//      signature from the mint, captured on the provider side at
//      the moment of redemption — Unit 1 produces this).
//   3. Weighted to resist Sybil amplification: a consumer needs
//      enough history before their receipts count, and any single
//      consumer-provider pair is capped at 20% of the consumer's
//      receipt volume.
//
// This module owns the **scoring logic**. The Nostr event publish
// path and the provider co-sign flow are wired in follow-up units
// (a per-event `KIND_COMPLETION_RECEIPT = 38385` parameterized
// replaceable; provider-side co-sign on lease completion).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The Cashu spend proof carried by a receipt. Captured by the
/// provider at redemption (Unit 1) and pasted verbatim into the
/// receipt the provider co-signs. Aggregators verify this against
/// the mint's published keys before counting the receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentProof {
    /// URL of the mint that issued the swap.
    pub mint_url: String,
    /// Signature over the swap response by the mint's keys (or a
    /// hash thereof — exact bytes TBD by mint capabilities).
    pub swap_response_signature: String,
}

/// Co-signed completion receipt. The consumer signs the
/// canonicalized JSON of `(lease_id, provider_npub, consumer_npub,
/// duration_paid, duration_delivered, success_flag, payment_proof,
/// version)`; the provider returns a `provider_co_signature` over
/// the same bytes; the receipt event carries both.
///
/// Receipts missing either signature do not contribute to score
/// (see `score_provider`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionReceipt {
    pub lease_id: String,
    pub provider_npub: String,
    pub consumer_npub: String,
    /// Seconds the consumer paid for.
    pub duration_paid: u64,
    /// Seconds the workload was actually live (provider-reported,
    /// cross-checkable against heartbeat history from Unit 4).
    pub duration_delivered: u64,
    /// 1.0 = success, 0.0 = failure. Floats so future units can
    /// surface partial-credit cases (e.g. lease delivered but with
    /// SLA violations).
    pub success_flag: f32,
    pub payment_proof: PaymentProof,
    pub version: u8,
    /// Schnorr signature over the canonical content by the
    /// consumer's Nostr key. None means "consumer hasn't signed",
    /// which is invalid for scoring.
    pub consumer_signature: Option<String>,
    /// Schnorr signature over the same content by the provider's
    /// Nostr key. None means "provider hasn't co-signed".
    pub provider_co_signature: Option<String>,
    /// Unix timestamp (provider-stamped) at which this receipt was
    /// minted. Aggregators can window by this.
    pub completed_at: u64,
}

/// Heuristics the scoring function uses to defeat Sybil
/// amplification. Operator-tunable via the observatory config.
#[derive(Debug, Clone, Copy)]
pub struct SybilHeuristics {
    /// Receipts from consumers younger than this don't count.
    pub min_consumer_history_secs: u64,
    /// Cap on the share of a single consumer's receipts that can
    /// be directed at any one provider before excess is weighted
    /// to zero.
    pub max_same_counterparty_share: f32,
}

impl Default for SybilHeuristics {
    fn default() -> Self {
        Self {
            // 30 days. Plan §Unit 10. Anti-bootstrap-fakery: a
            // brand-new consumer can't single-handedly score a
            // brand-new provider.
            min_consumer_history_secs: 30 * 24 * 3600,
            // 20% per the plan's score function. Receipts past
            // this share are weighted to zero.
            max_same_counterparty_share: 0.20,
        }
    }
}

/// Per-consumer metadata the scoring function consults. Real
/// observatory builds this from the consumer's first-seen Nostr
/// activity; tests can stub it directly.
#[derive(Debug, Clone)]
pub struct ConsumerProfile {
    pub npub: String,
    /// Unix timestamp of the consumer's earliest known activity.
    pub first_seen: u64,
}

/// Receipt validity check. A `false` here means the receipt
/// MUST NOT contribute to score. The signature verification itself
/// is delegated to `verify_signatures` so the scoring function is
/// pure (no crypto side-effects); callers can supply a stub
/// verifier in tests.
fn receipt_well_formed(r: &CompletionReceipt) -> bool {
    r.consumer_signature.is_some()
        && r.provider_co_signature.is_some()
        && r.success_flag >= 0.0
        && r.success_flag <= 1.0
        && r.version > 0
}

/// Score a single provider against the receipt set in `receipts`.
/// Returns a non-negative score; magnitude is the sum of weighted
/// success flags from receipts that survive every filter.
///
/// Filters applied (in order, short-circuiting):
///   - well-formed (both signatures present, version > 0).
///   - signature verification (`verify_signatures`).
///   - payment-proof verification (`verify_payment_proof`).
///   - consumer history >= `heuristics.min_consumer_history_secs`.
///   - per-consumer Sybil cap on share of receipts directed at
///     this provider.
///
/// `verify_signatures` and `verify_payment_proof` are passed as
/// closures so tests can stub them (real implementations call into
/// nostr-sdk Schnorr verification and the cdk mint key store
/// respectively).
pub fn score_provider<S, P>(
    provider_npub: &str,
    receipts: &[CompletionReceipt],
    consumers: &HashMap<String, ConsumerProfile>,
    now: u64,
    heuristics: &SybilHeuristics,
    verify_signatures: S,
    verify_payment_proof: P,
) -> f32
where
    S: Fn(&CompletionReceipt) -> bool,
    P: Fn(&CompletionReceipt) -> bool,
{
    // First pass: pre-count each consumer's total valid receipts so
    // we can apply the Sybil cap on a per-consumer basis. We
    // pre-filter on cheap predicates only; expensive crypto checks
    // are deferred to the second pass for the receipts we're
    // actually about to count toward this provider.
    let mut per_consumer_total: HashMap<&str, u32> = HashMap::new();
    let mut per_consumer_for_provider: HashMap<&str, u32> = HashMap::new();
    for r in receipts {
        if !receipt_well_formed(r) {
            continue;
        }
        let cons = r.consumer_npub.as_str();
        *per_consumer_total.entry(cons).or_insert(0) += 1;
        if r.provider_npub == provider_npub {
            *per_consumer_for_provider.entry(cons).or_insert(0) += 1;
        }
    }

    let mut weighted_sum = 0.0f32;
    for r in receipts {
        if r.provider_npub != provider_npub {
            continue;
        }
        if !receipt_well_formed(r) {
            continue;
        }
        if !verify_signatures(r) {
            continue;
        }
        if !verify_payment_proof(r) {
            continue;
        }

        // Consumer history gate.
        let Some(profile) = consumers.get(&r.consumer_npub) else {
            continue;
        };
        let consumer_age = now.saturating_sub(profile.first_seen);
        if consumer_age < heuristics.min_consumer_history_secs {
            continue;
        }

        // Sybil cap. If this consumer has directed > max_share of
        // their receipts at this provider, excess is weighted to
        // zero so the share rounds back down to max_share.
        let total = *per_consumer_total
            .get(r.consumer_npub.as_str())
            .unwrap_or(&0);
        let same = *per_consumer_for_provider
            .get(r.consumer_npub.as_str())
            .unwrap_or(&0);
        if total == 0 {
            continue;
        }
        let share = same as f32 / total as f32;
        let weight = if share > heuristics.max_same_counterparty_share {
            // Cap weight so the *effective* contribution from this
            // consumer to this provider equals the cap.
            heuristics.max_same_counterparty_share / share
        } else {
            1.0
        };

        weighted_sum += r.success_flag * weight;
    }

    weighted_sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof() -> PaymentProof {
        PaymentProof {
            mint_url: "https://mint.example".to_string(),
            swap_response_signature: "deadbeef".to_string(),
        }
    }

    pub(super) fn signed_receipt(
        lease_id: &str,
        provider: &str,
        consumer: &str,
        success: f32,
    ) -> CompletionReceipt {
        CompletionReceipt {
            lease_id: lease_id.to_string(),
            provider_npub: provider.to_string(),
            consumer_npub: consumer.to_string(),
            duration_paid: 3600,
            duration_delivered: 3600,
            success_flag: success,
            payment_proof: proof(),
            version: 1,
            consumer_signature: Some("c-sig".to_string()),
            provider_co_signature: Some("p-sig".to_string()),
            completed_at: 1_700_000_000,
        }
    }

    fn consumer(npub: &str, first_seen: u64) -> ConsumerProfile {
        ConsumerProfile {
            npub: npub.to_string(),
            first_seen,
        }
    }

    fn always_valid(_r: &CompletionReceipt) -> bool {
        true
    }

    #[test]
    fn single_consumer_with_single_provider_is_capped_to_share() {
        // The Sybil cap is share-based: a consumer whose 100% of
        // receipts go to one provider can only contribute
        // `max_share` (= 20% by default), no matter how many
        // receipts they file. This is the intended floor — a lone
        // consumer cannot fully credit a lone provider.
        let receipts = vec![signed_receipt("l1", "P", "C", 1.0)];
        let mut consumers = HashMap::new();
        consumers.insert(
            "C".to_string(),
            consumer("C", 1_700_000_000 - 60 * 24 * 3600),
        );
        let score = score_provider(
            "P",
            &receipts,
            &consumers,
            1_700_000_000,
            &SybilHeuristics::default(),
            always_valid,
            always_valid,
        );
        assert!((score - 0.20).abs() < 1e-6, "score = {}", score);
    }

    #[test]
    fn diversified_consumers_each_contributing_one_receipt_sum() {
        // Five distinct consumers, each filing exactly one receipt
        // against P. Each is capped to 0.20; total = 1.0.
        let mut receipts = Vec::new();
        let mut consumers = HashMap::new();
        for i in 0..5 {
            let c = format!("C{}", i);
            receipts.push(signed_receipt(&format!("l{}", i), "P", &c, 1.0));
            consumers.insert(c.clone(), consumer(&c, 1_700_000_000 - 60 * 24 * 3600));
        }
        let score = score_provider(
            "P",
            &receipts,
            &consumers,
            1_700_000_000,
            &SybilHeuristics::default(),
            always_valid,
            always_valid,
        );
        assert!((score - 1.0).abs() < 1e-4, "score = {}", score);
    }

    #[test]
    fn missing_provider_co_signature_drops_receipt() {
        let mut r = signed_receipt("l1", "P", "C", 1.0);
        r.provider_co_signature = None;
        let mut consumers = HashMap::new();
        consumers.insert(
            "C".to_string(),
            consumer("C", 1_700_000_000 - 60 * 24 * 3600),
        );
        let score = score_provider(
            "P",
            &[r],
            &consumers,
            1_700_000_000,
            &SybilHeuristics::default(),
            always_valid,
            always_valid,
        );
        assert_eq!(score, 0.0);
    }

    #[test]
    fn signature_verification_failure_drops_receipt() {
        let receipts = vec![signed_receipt("l1", "P", "C", 1.0)];
        let mut consumers = HashMap::new();
        consumers.insert(
            "C".to_string(),
            consumer("C", 1_700_000_000 - 60 * 24 * 3600),
        );
        let score = score_provider(
            "P",
            &receipts,
            &consumers,
            1_700_000_000,
            &SybilHeuristics::default(),
            |_| false, // verify_signatures rejects everything
            always_valid,
        );
        assert_eq!(score, 0.0);
    }

    #[test]
    fn payment_proof_failure_drops_receipt() {
        let receipts = vec![signed_receipt("l1", "P", "C", 1.0)];
        let mut consumers = HashMap::new();
        consumers.insert(
            "C".to_string(),
            consumer("C", 1_700_000_000 - 60 * 24 * 3600),
        );
        let score = score_provider(
            "P",
            &receipts,
            &consumers,
            1_700_000_000,
            &SybilHeuristics::default(),
            always_valid,
            |_| false, // verify_payment_proof rejects everything
        );
        assert_eq!(score, 0.0);
    }

    #[test]
    fn fresh_consumer_under_min_history_does_not_count() {
        let receipts = vec![signed_receipt("l1", "P", "Cnew", 1.0)];
        let mut consumers = HashMap::new();
        // Only 1 day of history < default 30-day floor.
        consumers.insert("Cnew".to_string(), consumer("Cnew", 1_700_000_000 - 86400));
        let score = score_provider(
            "P",
            &receipts,
            &consumers,
            1_700_000_000,
            &SybilHeuristics::default(),
            always_valid,
            always_valid,
        );
        assert_eq!(score, 0.0);
    }

    #[test]
    fn same_counterparty_cap_caps_contribution() {
        // Consumer has 10 total receipts; 9 of them are against
        // provider P. Per the 20% cap, P's effective contribution
        // from this consumer is capped at 20% × 10 = 2.0, not 9.0.
        let mut receipts = Vec::new();
        for i in 0..9 {
            receipts.push(signed_receipt(&format!("lp{}", i), "P", "C", 1.0));
        }
        // One receipt against a different provider so total = 10.
        receipts.push(signed_receipt("lq", "Q", "C", 1.0));
        let mut consumers = HashMap::new();
        consumers.insert(
            "C".to_string(),
            consumer("C", 1_700_000_000 - 60 * 24 * 3600),
        );

        let score = score_provider(
            "P",
            &receipts,
            &consumers,
            1_700_000_000,
            &SybilHeuristics::default(),
            always_valid,
            always_valid,
        );

        // 9 receipts × (0.20 / 0.90) ≈ 2.0
        let expected = 9.0 * (0.20 / 0.90);
        assert!(
            (score - expected).abs() < 1e-4,
            "score should be capped near {} (got {})",
            expected,
            score
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Sybil bound: across any random set of receipts where a
        /// single consumer fires at most N receipts at one provider
        /// out of M total, that consumer can never push the score
        /// past `max_share * M`. (The cap applies per-consumer; the
        /// invariant we check is the per-consumer ceiling.)
        #[test]
        fn single_consumer_cannot_exceed_share_cap(
            same_count in 1u32..200,
            other_count in 0u32..200,
        ) {
            let consumer_npub = "C".to_string();
            let mut receipts = Vec::new();
            for i in 0..same_count {
                receipts.push(super::tests::signed_receipt(
                    &format!("p{}", i),
                    "P",
                    &consumer_npub,
                    1.0,
                ));
            }
            for i in 0..other_count {
                receipts.push(super::tests::signed_receipt(
                    &format!("q{}", i),
                    "Q",
                    &consumer_npub,
                    1.0,
                ));
            }
            let mut consumers = HashMap::new();
            consumers.insert(
                consumer_npub.clone(),
                ConsumerProfile {
                    npub: consumer_npub.clone(),
                    first_seen: 1_700_000_000 - 60 * 24 * 3600,
                },
            );
            let h = SybilHeuristics::default();
            let score = score_provider(
                "P",
                &receipts,
                &consumers,
                1_700_000_000,
                &h,
                |_| true,
                |_| true,
            );
            let total = (same_count + other_count) as f32;
            let cap = h.max_same_counterparty_share * total;
            // Allow tiny float epsilon. score should never exceed
            // the cap (when same_count is the only contribution).
            prop_assert!(
                score <= cap + 1e-3,
                "score {} exceeds Sybil cap {}",
                score,
                cap
            );
        }
    }
}
