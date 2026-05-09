//! Wire-format regression tests for `LeaseRevocationContent` (Unit 5
//! orchestrator wiring). Standby providers depend on this schema for
//! cold-start replay, so old payloads MUST keep parsing and new
//! payloads MUST round-trip cleanly. The runtime publish/subscribe
//! path is exercised live in `src/provider.rs` (orchestrator_loop),
//! which is integration-tested manually against deployed providers.

use paygress::nostr::LeaseRevocationContent;

fn sample() -> LeaseRevocationContent {
    LeaseRevocationContent {
        workload_id: 1042,
        primary_provider_npub: "npub1primary".to_string(),
        standby_providers: vec!["npub1standby1".to_string(), "npub1standby2".to_string()],
        reason: "heartbeat-quorum-lost-past-t2".to_string(),
        revoked_at: 1_780_000_000,
        state_uri: Some("blossom://abc123".to_string()),
        version: paygress::nostr::SCHEMA_VERSION,
    }
}

#[test]
fn round_trip() {
    let v1 = sample();
    let json = serde_json::to_string(&v1).unwrap();
    let back: LeaseRevocationContent = serde_json::from_str(&json).unwrap();
    assert_eq!(back.workload_id, 1042);
    assert_eq!(back.primary_provider_npub, "npub1primary");
    assert_eq!(back.standby_providers.len(), 2);
    assert_eq!(back.reason, "heartbeat-quorum-lost-past-t2");
    assert_eq!(back.revoked_at, 1_780_000_000);
    assert_eq!(back.state_uri.as_deref(), Some("blossom://abc123"));
    assert_eq!(back.version, paygress::nostr::SCHEMA_VERSION);
}

#[test]
fn empty_state_uri_skipped_on_wire() {
    let mut v = sample();
    v.state_uri = None;
    let json = serde_json::to_string(&v).unwrap();
    assert!(
        !json.contains("state_uri"),
        "skip_serializing_if respected — None state_uri stays off the wire so non-checkpointed revocations don't carry a noisy null"
    );
}

#[test]
fn v0_without_version_field_parses() {
    // A pre-this-PR provider would never have published a revocation,
    // but for forward-compat (a future version dropping `version`)
    // we want #[serde(default)] to keep working.
    let v0 = serde_json::json!({
        "workload_id": 7,
        "primary_provider_npub": "npub1abc",
        "standby_providers": ["npub1xyz"],
        "reason": "self-eviction",
        "revoked_at": 1_780_000_000u64,
    });
    let parsed: LeaseRevocationContent =
        serde_json::from_value(v0).expect("v0 revocation must parse");
    assert_eq!(parsed.workload_id, 7);
    assert_eq!(parsed.standby_providers.len(), 1);
    assert!(parsed.state_uri.is_none());
    assert_eq!(parsed.version, 1, "missing version defaults to 1");
}

#[test]
fn empty_standby_list_round_trips() {
    // Defensive: a primary self-evicting on a non-warm-standby
    // workload would still emit a revocation (currently it doesn't,
    // but the schema must support it cleanly so a future expansion —
    // e.g. broadcast revocations — doesn't need a wire bump).
    let mut v = sample();
    v.standby_providers.clear();
    let json = serde_json::to_string(&v).unwrap();
    let back: LeaseRevocationContent = serde_json::from_str(&json).unwrap();
    assert!(back.standby_providers.is_empty());
}
