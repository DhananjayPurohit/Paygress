//! `parse_pod_npub` plus `EncryptedTopUpPodRequest`'s compatibility with
//! the `#[serde(untagged)]` `PrivateRequest` dispatch.
//!
//! Handler-level scenarios (concurrent topups, cleanup races, real cashu
//! redemption) need a live relay, backend and mint, so they live elsewhere.

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
    // `PrivateRequest` is untagged and its variants share field names, so a
    // topup payload could silently parse as Status or Spawn.
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
