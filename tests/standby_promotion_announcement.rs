//! Wire-format regression tests for `StandbyPromotionAnnouncementContent`
//! (#60). Higher-indexed standbys depend on this schema for the
//! pre-emption check that prevents split-brain after a primary
//! crash, so old payloads MUST keep parsing and new payloads MUST
//! round-trip cleanly. The runtime publish/query path is exercised
//! live in `src/provider.rs::schedule_standby_promotion`, which is
//! integration-tested manually against deployed providers.

use paygress::nostr::{StandbyPromotionAnnouncementContent, KIND_STANDBY_PROMOTION_ANNOUNCEMENT};

fn sample() -> StandbyPromotionAnnouncementContent {
    StandbyPromotionAnnouncementContent {
        workload_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        new_primary_npub:
            "npub1hyr9m7zeegr98w4e07gvdpqrk25jfp3vku8029u8pcxsc48dq6nqxtwztv".to_string(),
        promoted_at: 1_780_000_000,
        version: paygress::nostr::SCHEMA_VERSION,
    }
}

#[test]
fn round_trip() {
    let v1 = sample();
    let json = serde_json::to_string(&v1).unwrap();
    let back: StandbyPromotionAnnouncementContent = serde_json::from_str(&json).unwrap();
    assert_eq!(back.workload_id, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(
        back.new_primary_npub,
        "npub1hyr9m7zeegr98w4e07gvdpqrk25jfp3vku8029u8pcxsc48dq6nqxtwztv"
    );
    assert_eq!(back.promoted_at, 1_780_000_000);
    assert_eq!(back.version, paygress::nostr::SCHEMA_VERSION);
}

#[test]
fn missing_version_defaults_to_one() {
    // Forward-compat: if a future version drops the `version` field
    // entirely, payloads from a transitional provider must still
    // parse on this version of the consumer/peer.
    let v_missing_version = serde_json::json!({
        "workload_id": "wid-promotion-7",
        "new_primary_npub": "npub1abc",
        "promoted_at": 1_780_000_001u64,
    });
    let parsed: StandbyPromotionAnnouncementContent =
        serde_json::from_value(v_missing_version).expect("payload missing `version` must parse");
    assert_eq!(parsed.workload_id, "wid-promotion-7");
    assert_eq!(parsed.version, 1, "missing version defaults to 1");
}

#[test]
fn unexpected_extra_fields_are_ignored() {
    // A peer running a newer schema (e.g. with a `replication_topology`
    // or `previous_primary_npub` field added later) must not break
    // older peers' parsing — serde defaults to ignoring unknown
    // fields, but pin the contract here so it's a deliberate decision.
    let with_extra = serde_json::json!({
        "workload_id": "wid-future",
        "new_primary_npub": "npub1xyz",
        "promoted_at": 1_780_000_002u64,
        "version": 1,
        "previous_primary_npub": "npub1old",
        "replication_topology": {"factor": 3},
    });
    let parsed: StandbyPromotionAnnouncementContent =
        serde_json::from_value(with_extra).expect("forward-compat extra fields must not break parse");
    assert_eq!(parsed.workload_id, "wid-future");
}

#[test]
fn event_kind_constant_is_38386() {
    // Pin the kind so a casual refactor can't silently change it —
    // peers identify announcement events by this kind, and a change
    // would silently break the split-brain dedup across versions.
    assert_eq!(KIND_STANDBY_PROMOTION_ANNOUNCEMENT, 38386);
}
