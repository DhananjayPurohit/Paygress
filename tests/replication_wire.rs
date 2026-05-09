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
    }
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
