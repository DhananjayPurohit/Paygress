// Wire-format pin for `EncryptedSpawnPodRequest.volume_encryption`.
//
// This file freezes the JSON shape so a future serde drift breaks
// the test rather than silently changing what providers receive on
// the wire. New fields on `VolumeEncryption` MUST go through a
// schema-version bump (and update the v1 fixture in this file).

use paygress::nostr::{EncryptedSpawnPodRequest, VolumeEncryption};
use paygress::volume_encryption::derive_volume_key;

fn base_request() -> EncryptedSpawnPodRequest {
    EncryptedSpawnPodRequest {
        cashu_token: "cashuA-fixture".to_string(),
        pod_spec_id: Some("basic".to_string()),
        pod_image: "ubuntu:22.04".to_string(),
        ssh_username: "root".to_string(),
        ssh_password: "p".to_string(),
        template_slug: None,
        replication: None,
        primary_npub: None,
        workload_id: Some("wid-test".to_string()),
        volume_encryption: None,
    }
}

#[test]
fn unspecified_volume_encryption_is_skipped_on_wire() {
    let req = base_request();
    let json = serde_json::to_string(&req).unwrap();
    assert!(
        !json.contains("volume_encryption"),
        "volume_encryption=None must be skipped (skip_serializing_if) so old providers see no schema change; got {json}"
    );
}

#[test]
fn v1_round_trip_preserves_key() {
    let key = [0x42u8; 32];
    let mut req = base_request();
    req.volume_encryption = Some(VolumeEncryption::v1(key));

    let json = serde_json::to_string(&req).unwrap();
    let back: EncryptedSpawnPodRequest = serde_json::from_str(&json).unwrap();
    let recovered = back.volume_encryption.unwrap();
    assert_eq!(recovered.version, VolumeEncryption::VERSION_V1);
    assert_eq!(recovered.algorithm, VolumeEncryption::ALGORITHM_V1);
    assert_eq!(recovered.decoded_key().unwrap(), key);
}

#[test]
fn v0_back_compat_no_field_present() {
    // A pre-volume-encryption client serializes without the field at
    // all. Provider deserialization must succeed and treat it as None.
    let v0_json = r#"{
        "cashu_token": "cashuA-old",
        "pod_spec_id": "basic",
        "pod_image": "ubuntu:22.04",
        "ssh_username": "u",
        "ssh_password": "p"
    }"#;
    let req: EncryptedSpawnPodRequest = serde_json::from_str(v0_json).unwrap();
    assert!(req.volume_encryption.is_none());
}

#[test]
fn unknown_algorithm_round_trips_so_provider_can_reject_it_explicitly() {
    // A future client that picks `xchacha20-poly1305` and ships it
    // to a today-provider must still deserialize successfully — the
    // provider then rejects with a structured error rather than a
    // serde failure. (The reject-path itself lives in the provider
    // when LUKS plumbing lands; here we just pin the wire shape.)
    let mut req = base_request();
    req.volume_encryption = Some(VolumeEncryption {
        version: 7,
        algorithm: "future-algorithm-tag".to_string(),
        key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
    });
    let json = serde_json::to_string(&req).unwrap();
    let back: EncryptedSpawnPodRequest = serde_json::from_str(&json).unwrap();
    let v = back.volume_encryption.unwrap();
    assert_eq!(v.version, 7);
    assert_eq!(v.algorithm, "future-algorithm-tag");
}

#[test]
fn decoded_key_rejects_wrong_length() {
    let v = VolumeEncryption {
        version: VolumeEncryption::VERSION_V1,
        algorithm: VolumeEncryption::ALGORITHM_V1.to_string(),
        // 16 bytes, not 32.
        key_b64: "AAAAAAAAAAAAAAAAAAAAAA".to_string(),
    };
    let err = v.decoded_key().unwrap_err().to_string();
    assert!(
        err.contains("expected 32"),
        "wrong-length keys must fail loud, got: {err}"
    );
}

#[test]
fn decoded_key_rejects_invalid_base64() {
    let v = VolumeEncryption {
        version: VolumeEncryption::VERSION_V1,
        algorithm: VolumeEncryption::ALGORITHM_V1.to_string(),
        key_b64: "!!! not base64 !!!".to_string(),
    };
    assert!(v.decoded_key().is_err());
}

#[test]
fn kdf_matches_v1_helper_end_to_end() {
    // The CLI derives the key, encodes with v1(), and ships it. The
    // provider decodes with decoded_key(). End-to-end equality is the
    // whole point — pin it.
    let nsec = [0x99u8; 32];
    let workload_id = "deadbeef-cafe";
    let key = derive_volume_key(&nsec, workload_id);
    let v = VolumeEncryption::v1(key);
    assert_eq!(v.decoded_key().unwrap(), key);
}

#[test]
fn json_field_name_is_volume_encryption_when_present() {
    let req = EncryptedSpawnPodRequest {
        volume_encryption: Some(VolumeEncryption::v1([1u8; 32])),
        ..base_request()
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(
        json.contains("\"volume_encryption\":"),
        "field name on the wire must be `volume_encryption`; got {json}"
    );
}
