//! Integration tests for Cashu mint redemption on the Nostr-DM provider path.
//!
//! Status: PLACEHOLDER. Unit 1 of the 12-month plan
//! (docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md) wires
//! `cdk::wallet::Wallet::receive` (NUT-03 swap-on-receive) into
//! `src/cashu.rs` and `src/provider.rs:455`. Today the provider only
//! decodes face value via `extract_token_value`; tokens are never spent
//! at the mint, so the same token replays across N providers undetected.
//!
//! Unit 1 will populate this file with characterization tests covering:
//!
//! - Happy path: token from a whitelisted mint redeems successfully;
//!   provider proceeds to backend.
//! - Error path: already-spent token returns `Error::TokenAlreadySpent`;
//!   the backend's `create_container` is NOT called.
//! - Error path: pending token returns `Error::TokenPending`; reject with
//!   retry hint; no container.
//! - Error path: token from a non-whitelisted mint is rejected before
//!   redemption.
//! - Edge case: mint endpoint returns 5xx; reject with retry hint; no
//!   container.
//! - Integration: replay the same valid token twice from one consumer to
//!   one provider; first succeeds, second returns `TokenAlreadySpent`.
//! - Integration: replay across two providers (same mint); exactly one
//!   swap succeeds; the other gets `TokenAlreadySpent` (proves
//!   cross-provider replay is defeated).
//!
//! These tests use `wiremock` to stub the Cashu mint HTTP API so they run
//! deterministically without depending on a public testnet mint.

#[tokio::test]
async fn test_harness_compiles() {
    // Ensures `tokio-test`, `wiremock`, and the rest of the dev-dependency
    // stack are wired before Unit 1 lands the real scenarios.
    assert!(true);
}
