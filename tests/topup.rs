//! Tests for the TopUp handler (Unit 2 of the 12-month plan
//! docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//!
//! The full set of scenarios in the plan (concurrent topups, full
//! cleanup_loop integration, real cashu redemption) require standing
//! up a real Nostr relay, a backend, and a mint — out of scope here.
//! What this file covers:
//!
//! - `parse_pod_npub`: round-trip with the format the spawn handler
//!   emits to consumers, plus rejection of malformed inputs.
//! - `EncryptedTopUpPodRequest` wire-format compatibility with the
//!   `#[serde(untagged)]` `PrivateRequest` dispatch — i.e. that a
//!   topup request actually deserializes as `PrivateRequest::TopUp`
//!   and not as another variant.
//!
//! The handler-level integration tests (concurrent topup,
//! cleanup-race, replay across topup) belong in a follow-up that
//! introduces a test harness exposing the spawn/topup loop with
//! mocked dependencies.

use paygress::nostr::{EncryptedTopUpPodRequest, PrivateRequest};
use paygress::provider::parse_pod_npub;

#[test]
fn parse_pod_npub_accepts_container_prefix() {
    assert_eq!(parse_pod_npub("container-1234"), Some(1234));
    assert_eq!(parse_pod_npub("container-1"), Some(1));
}

#[test]
fn parse_pod_npub_accepts_bare_number() {
    assert_eq!(parse_pod_npub("1234"), Some(1234));
}

#[test]
fn parse_pod_npub_rejects_garbage() {
    assert_eq!(parse_pod_npub(""), None);
    assert_eq!(parse_pod_npub("container-"), None);
    assert_eq!(parse_pod_npub("container-abc"), None);
    assert_eq!(parse_pod_npub("npub1xyz"), None);
}

#[test]
fn topup_request_dispatches_to_topup_variant() {
    // Wire compatibility regression. PrivateRequest is
    // `#[serde(untagged)]` and the variants share field names
    // (cashu_token, pod_id-ish). Without this test, a topup payload
    // could silently parse as Status or Spawn and the provider
    // would route it to the wrong handler.
    let req = EncryptedTopUpPodRequest {
        pod_npub: "container-42".to_string(),
        cashu_token: "cashuA...".to_string(),
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: PrivateRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        PrivateRequest::TopUp(t) => {
            assert_eq!(t.pod_npub, "container-42");
            assert_eq!(t.cashu_token, "cashuA...");
        }
        other => panic!(
            "topup request must dispatch to TopUp variant, got {:?}",
            other
        ),
    }
}
