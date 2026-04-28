//! Integration tests for Cashu mint redemption on the Nostr-DM provider path.
//!
//! Covers Unit 1 of the 12-month plan
//! (docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//!
//! These tests inject a `MintRedeemer` mock so they never hit a real mint.
//! Stubbing the mint's HTTP API with `wiremock` was rejected because the
//! cdk wallet's swap protocol requires cryptographically-valid blinded
//! signatures, which can't be faithfully synthesized without the mint's
//! private keys. The trait seam exercises the same code path
//! (whitelist enforcement, error mapping, replay detection) without
//! coupling tests to cdk's HTTP wire format.
//!
//! See `docs/solutions/patterns/critical-patterns.md` for the historical
//! footgun this fix closes.
//!
//! Test scenarios per Unit 1 plan:
//! 1. Happy path: whitelisted mint, never-spent → success.
//! 2. Already-spent token → `RedeemError::AlreadySpent`, no backend call.
//! 3. Non-whitelisted mint → rejected before redeemer is contacted.
//! 4. Pending token → `RedeemError::Pending`.
//! 5. Mint network failure → `RedeemError::Network`.
//! 6. In-provider replay: same token twice → first OK, second AlreadySpent.
//! 7. Cross-provider replay: shared mint mock → exactly one swap succeeds.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use paygress::cashu::{validate_and_redeem, MintRedeemer, RedeemError};

const WHITELISTED_MINT: &str = "https://testnut.cashu.space";
const OTHER_MINT: &str = "https://attacker.example";

/// Build a synthetic Cashu V3 token string. Body is plain JSON so cdk's
/// `Token::from_str` parses it; proof signatures are dummy hex because no
/// local crypto verification happens before `Wallet::receive` would hit
/// the mint, and our tests never reach the wallet.
fn make_token(mint_url: &str, amount_sat: u64, secret: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let body = serde_json::json!({
        "token": [{
            "mint": mint_url,
            "proofs": [{
                "amount": amount_sat,
                "secret": secret,
                "C": "023be53e8c60530eea9b3943fda1a2ce71c7b3f0cf0dc6d846fa765aaf779fa81d",
                "id": "009a1f293253e41e",
            }],
        }],
        "unit": "sat",
    });

    let json = serde_json::to_string(&body).expect("synthetic token body");
    format!("cashuA{}", URL_SAFE_NO_PAD.encode(json.as_bytes()))
}

/// Stateful mock that records redeemed token strings so we can model
/// replay detection (in-provider and cross-provider via a shared `Arc`).
struct MockRedeemer {
    redeemed: Arc<Mutex<HashSet<String>>>,
    next_error: Arc<Mutex<Option<RedeemError>>>,
    call_count: Arc<AtomicUsize>,
}

impl MockRedeemer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            redeemed: Arc::new(Mutex::new(HashSet::new())),
            next_error: Arc::new(Mutex::new(None)),
            call_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Cause the next call to fail with the given error. After the call,
    /// the slot resets and subsequent calls behave normally.
    async fn fail_next_with(self: &Arc<Self>, err: RedeemError) {
        *self.next_error.lock().await = Some(err);
    }

    fn calls(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MintRedeemer for MockRedeemer {
    async fn redeem(&self, token_str: &str) -> Result<u64, RedeemError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

        if let Some(err) = self.next_error.lock().await.take() {
            return Err(err);
        }

        let mut redeemed = self.redeemed.lock().await;
        if !redeemed.insert(token_str.to_string()) {
            return Err(RedeemError::AlreadySpent);
        }

        // Mirror real cdk: parse token, return its amount in msats.
        use std::str::FromStr;
        let token = cdk::nuts::Token::from_str(token_str)
            .map_err(|e| RedeemError::InvalidToken(e.to_string()))?;
        let total: u64 = token.proofs().iter().map(|p| u64::from(p.amount)).sum();
        let unit = token.unit().unwrap_or(cdk::nuts::CurrencyUnit::Sat);
        let msats = match unit {
            cdk::nuts::CurrencyUnit::Sat => total * 1000,
            cdk::nuts::CurrencyUnit::Msat => total,
            other => return Err(RedeemError::UnsupportedUnit(format!("{:?}", other))),
        };
        Ok(msats)
    }
}

fn whitelist() -> Vec<String> {
    vec![WHITELISTED_MINT.to_string()]
}

#[tokio::test]
async fn happy_path_whitelisted_mint_returns_msats() {
    let redeemer = MockRedeemer::new();
    let token = make_token(WHITELISTED_MINT, 100, "secret-happy-path");

    let amount = validate_and_redeem(redeemer.as_ref(), &whitelist(), &token)
        .await
        .expect("redemption should succeed");

    assert_eq!(amount, 100 * 1000, "100 sat should become 100_000 msat");
    assert_eq!(redeemer.calls(), 1, "redeemer should be called once");
}

#[tokio::test]
async fn already_spent_token_is_rejected() {
    let redeemer = MockRedeemer::new();
    redeemer.fail_next_with(RedeemError::AlreadySpent).await;
    let token = make_token(WHITELISTED_MINT, 50, "secret-already-spent");

    let err = validate_and_redeem(redeemer.as_ref(), &whitelist(), &token)
        .await
        .expect_err("must reject double-spent token");

    assert!(matches!(err, RedeemError::AlreadySpent));
    assert_eq!(
        redeemer.calls(),
        1,
        "the mint was contacted; the rejection came from there"
    );
}

#[tokio::test]
async fn non_whitelisted_mint_short_circuits_before_redeemer() {
    let redeemer = MockRedeemer::new();
    let token = make_token(OTHER_MINT, 100, "secret-wrong-mint");

    let err = validate_and_redeem(redeemer.as_ref(), &whitelist(), &token)
        .await
        .expect_err("must reject token from non-whitelisted mint");

    match err {
        RedeemError::NonWhitelistedMint { mint_url } => {
            assert!(mint_url.contains("attacker.example"));
        }
        other => panic!("expected NonWhitelistedMint, got {:?}", other),
    }
    assert_eq!(
        redeemer.calls(),
        0,
        "redeemer must NOT be contacted for blacklisted mint"
    );
}

#[tokio::test]
async fn pending_token_is_rejected_with_retry_hint() {
    let redeemer = MockRedeemer::new();
    redeemer.fail_next_with(RedeemError::Pending).await;
    let token = make_token(WHITELISTED_MINT, 100, "secret-pending");

    let err = validate_and_redeem(redeemer.as_ref(), &whitelist(), &token)
        .await
        .expect_err("must reject pending token");

    assert!(matches!(err, RedeemError::Pending));
}

#[tokio::test]
async fn mint_network_error_propagates_as_network_error() {
    let redeemer = MockRedeemer::new();
    redeemer
        .fail_next_with(RedeemError::Network("HTTP 503 from mint".to_string()))
        .await;
    let token = make_token(WHITELISTED_MINT, 100, "secret-network-fail");

    let err = validate_and_redeem(redeemer.as_ref(), &whitelist(), &token)
        .await
        .expect_err("must propagate mint outage as Network error");

    match err {
        RedeemError::Network(msg) => assert!(msg.contains("503")),
        other => panic!("expected Network, got {:?}", other),
    }
}

#[tokio::test]
async fn malformed_token_is_rejected_as_invalid_token() {
    let redeemer = MockRedeemer::new();

    let err = validate_and_redeem(
        redeemer.as_ref(),
        &whitelist(),
        "cashuA-not-base64-and-not-json",
    )
    .await
    .expect_err("must reject unparseable token");

    assert!(matches!(err, RedeemError::InvalidToken(_)));
    assert_eq!(
        redeemer.calls(),
        0,
        "redeemer must NOT be contacted for unparseable token"
    );
}

#[tokio::test]
async fn in_provider_replay_second_call_fails_already_spent() {
    let redeemer = MockRedeemer::new();
    let token = make_token(WHITELISTED_MINT, 100, "secret-replay-same-provider");

    let first = validate_and_redeem(redeemer.as_ref(), &whitelist(), &token).await;
    let second = validate_and_redeem(redeemer.as_ref(), &whitelist(), &token).await;

    assert!(first.is_ok(), "first redemption should succeed");
    assert!(
        matches!(second, Err(RedeemError::AlreadySpent)),
        "replay must be rejected: {:?}",
        second
    );
    assert_eq!(redeemer.calls(), 2, "both attempts hit the mint");
}

#[tokio::test]
async fn cross_provider_replay_only_one_swap_succeeds() {
    // Two providers whose `MintRedeemer` instances share state — modelling
    // the real-world property that the *mint* serializes spend attempts.
    let shared_mint = MockRedeemer::new();
    let provider_a = shared_mint.clone();
    let provider_b = shared_mint.clone();
    let token = make_token(WHITELISTED_MINT, 100, "secret-replay-cross-provider");

    let res_a = validate_and_redeem(provider_a.as_ref(), &whitelist(), &token).await;
    let res_b = validate_and_redeem(provider_b.as_ref(), &whitelist(), &token).await;

    let oks = [res_a.is_ok(), res_b.is_ok()]
        .iter()
        .filter(|x| **x)
        .count();
    let already_spent = [&res_a, &res_b]
        .iter()
        .filter(|r| matches!(r, Err(RedeemError::AlreadySpent)))
        .count();

    assert_eq!(oks, 1, "exactly one provider's swap should succeed");
    assert_eq!(already_spent, 1, "the other should see AlreadySpent");
}
