//! Wire-format regression tests for `EncryptedSpawnPodRequest`'s
//! new `replication` field (Unit 5 wire-through).
//!
//! Old clients sending pre-this-PR requests MUST keep being parsed
//! by the new provider; new clients setting `replication` MUST
//! populate the state machine correctly. The runtime population
//! path is exercised live in `src/provider.rs::handle_spawn_request`.

use paygress::durable_workload::ReplicationMode;
use paygress::nostr::EncryptedSpawnPodRequest;

fn sample_v1_warm_standby() -> EncryptedSpawnPodRequest {
    EncryptedSpawnPodRequest {
        cashu_token: "tok".to_string(),
        pod_spec_id: Some("basic".to_string()),
        pod_image: "ubuntu:22.04".to_string(),
        ssh_username: "user".to_string(),
        ssh_password: "pw".to_string(),
        template_slug: Some("nostr-relay".to_string()),
        replication: Some(ReplicationMode::WarmStandby {
            standby_providers: vec!["npub1b".to_string(), "npub1c".to_string()],
        }),
        primary_npub: Some("npub1primary".to_string()),
        workload_id: Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
        volume_encryption: None,
    }
}

#[test]
fn warm_standby_carries_primary_and_workload_id() {
    let v = sample_v1_warm_standby();
    let json = serde_json::to_string(&v).unwrap();
    assert!(json.contains(r#""primary_npub":"npub1primary""#));
    assert!(json.contains(r#""workload_id":"550e8400-e29b-41d4-a716-446655440000""#));

    let back: EncryptedSpawnPodRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(back.primary_npub.as_deref(), Some("npub1primary"));
    assert_eq!(
        back.workload_id.as_deref(),
        Some("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn primary_npub_and_workload_id_skipped_when_unset() {
    // Old clients (and non-replicated spawns) don't set these.
    // skip_serializing_if keeps them off the wire so the schema
    // looks unchanged.
    let mut v = sample_v1_warm_standby();
    v.primary_npub = None;
    v.workload_id = None;
    let json = serde_json::to_string(&v).unwrap();
    assert!(!json.contains("primary_npub"));
    assert!(!json.contains("workload_id"));
}

#[test]
fn warm_standby_round_trip() {
    let v1 = sample_v1_warm_standby();
    let json = serde_json::to_string(&v1).unwrap();
    let back: EncryptedSpawnPodRequest = serde_json::from_str(&json).unwrap();
    match back.replication {
        Some(ReplicationMode::WarmStandby { standby_providers }) => {
            assert_eq!(standby_providers, vec!["npub1b", "npub1c"]);
        }
        other => panic!("expected WarmStandby, got {:?}", other),
    }
}

#[test]
fn none_replication_skipped_on_wire() {
    // Default replication path: stay off the wire so old providers
    // see no schema change at all. Same back-compat shape as
    // template_slug.
    let mut v = sample_v1_warm_standby();
    v.replication = None;
    let json = serde_json::to_string(&v).unwrap();
    assert!(
        !json.contains("replication"),
        "skip_serializing_if respected — None replication stays off the wire so non-replicated spawns look identical to old clients"
    );
}

#[test]
fn old_v0_request_without_replication_parses() {
    // Exactly what a pre-this-PR client emits. Must still parse on
    // the new code path.
    let v0 = serde_json::json!({
        "cashu_token": "tok",
        "pod_spec_id": "basic",
        "pod_image": "ubuntu:22.04",
        "ssh_username": "user",
        "ssh_password": "pw",
    });
    let parsed: EncryptedSpawnPodRequest =
        serde_json::from_value(v0).expect("v0 spawn request must parse");
    assert!(parsed.replication.is_none());
    assert!(parsed.template_slug.is_none());
}

#[test]
fn checkpointed_round_trip() {
    let mut v = sample_v1_warm_standby();
    v.replication = Some(ReplicationMode::Checkpointed);
    let json = serde_json::to_string(&v).unwrap();
    let back: EncryptedSpawnPodRequest = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        back.replication,
        Some(ReplicationMode::Checkpointed)
    ));
}

// ==================== warm_standby_role tests ====================
//
// Role-routing is what makes the same EncryptedSpawnPodRequest land
// at N+1 providers and have each pick the right path. Pin the
// matrix so the convention (primary_npub identifies the primary;
// standby_providers list contains only standbys) doesn't drift.

use paygress::nostr::{warm_standby_role, WarmStandbyRole};

#[test]
fn role_primary_when_self_matches_primary_npub() {
    let r = warm_standby_role("npub1primary", "npub1primary", &["npub1b".into()]);
    assert_eq!(r, WarmStandbyRole::Primary);
}

#[test]
fn role_standby_with_correct_index() {
    let r = warm_standby_role(
        "npub1c",
        "npub1primary",
        &["npub1b".into(), "npub1c".into(), "npub1d".into()],
    );
    assert_eq!(r, WarmStandbyRole::Standby { index: 1, count: 3 });
}

#[test]
fn role_not_addressed_when_self_unknown() {
    let r = warm_standby_role(
        "npub1stranger",
        "npub1primary",
        &["npub1b".into(), "npub1c".into()],
    );
    assert_eq!(r, WarmStandbyRole::NotAddressed);
}
