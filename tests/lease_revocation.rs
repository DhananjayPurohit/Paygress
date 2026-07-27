//! Wire-format regression tests for `LeaseRevocationContent`. Standby
//! providers read this schema for cold-start replay, so old payloads must
//! keep parsing and new ones must round-trip.

use paygress::nostr::LeaseRevocationContent;

fn sample() -> LeaseRevocationContent {
    LeaseRevocationContent {
        // Consumer-assigned UUID — matches the standby's slot key.
        workload_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
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
    assert_eq!(back.workload_id, "550e8400-e29b-41d4-a716-446655440000");
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
        "None state_uri must stay off the wire, not serialize as null"
    );
}

#[test]
fn v0_without_version_field_parses() {
    let v0 = serde_json::json!({
        "workload_id": "wid-7",
        "primary_provider_npub": "npub1abc",
        "standby_providers": ["npub1xyz"],
        "reason": "self-eviction",
        "revoked_at": 1_780_000_000u64,
    });
    let parsed: LeaseRevocationContent =
        serde_json::from_value(v0).expect("v0 revocation must parse");
    assert_eq!(parsed.workload_id, "wid-7");
    assert_eq!(parsed.standby_providers.len(), 1);
    assert!(parsed.state_uri.is_none());
    assert_eq!(parsed.version, 1, "missing version defaults to 1");
}

#[test]
fn empty_standby_list_round_trips() {
    // Nothing emits this today, but the schema must support it so broadcast
    // revocations don't need a wire bump later.
    let mut v = sample();
    v.standby_providers.clear();
    let json = serde_json::to_string(&v).unwrap();
    let back: LeaseRevocationContent = serde_json::from_str(&json).unwrap();
    assert!(back.standby_providers.is_empty());
}

use paygress::nostr::{parse_revocation_event, NostrEvent, KIND_LEASE_REVOCATION};

fn make_event(kind: u32, content: String) -> NostrEvent {
    NostrEvent {
        id: "id".to_string(),
        pubkey: "primary-pub".to_string(),
        created_at: 1_780_000_000,
        kind,
        tags: vec![],
        content,
        sig: "sig".to_string(),
        message_type: "lease_revocation".to_string(),
    }
}

#[test]
fn parse_revocation_event_returns_some_for_matching_kind_and_body() {
    let body = serde_json::to_string(&sample()).unwrap();
    let ev = make_event(KIND_LEASE_REVOCATION as u32, body);
    let parsed = parse_revocation_event(&ev).expect("must parse");
    assert_eq!(parsed.workload_id, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(parsed.standby_providers.len(), 2);
}

#[test]
fn parse_revocation_event_returns_none_for_wrong_kind() {
    // The dispatcher relies on this to fall through to the DM path.
    let body = serde_json::to_string(&sample()).unwrap();
    let ev = make_event(4, body); // Kind::EncryptedDirectMessage = 4
    assert!(parse_revocation_event(&ev).is_none());
}

#[test]
fn parse_revocation_event_returns_none_for_malformed_body() {
    let ev = make_event(KIND_LEASE_REVOCATION as u32, "{not json".to_string());
    assert!(parse_revocation_event(&ev).is_none());
}
