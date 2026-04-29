//! Schema round-trip tests for the offer/heartbeat payloads
//! introduced/extended in Unit 4 of the 12-month plan
//! (docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//!
//! These tests pin the wire format we publish on Nostr so that:
//! - Old (v0) payloads from providers running 0.1.x continue to
//!   parse on consumers running this revision
//!   (`#[serde(default)]` fields).
//! - New v1 payloads round-trip cleanly.
//! - Unknown future fields are tolerated by today's parser
//!   (forward-compatibility).
//!
//! These are pure schema tests — they do not stand up a relay or
//! exercise the publish path. Live-relay integration tests are out
//! of scope here.

use paygress::nostr::{
    CapacityInfo, HeartbeatContent, IsolationLevel, PodSpec, ProviderOfferContent, SCHEMA_VERSION,
};

fn sample_offer() -> ProviderOfferContent {
    ProviderOfferContent {
        provider_npub: "npub1example".to_string(),
        hostname: "host.example".to_string(),
        location: Some("BER".to_string()),
        capabilities: vec!["lxc".to_string()],
        specs: vec![PodSpec {
            id: "basic".to_string(),
            name: "Basic".to_string(),
            description: "1 vCPU".to_string(),
            cpu_millicores: 1000,
            memory_mb: 1024,
            rate_msats_per_sec: 50,
        }],
        whitelisted_mints: vec!["https://mint.example".to_string()],
        uptime_percent: 99.5,
        total_jobs_completed: 7,
        api_endpoint: None,
        version: SCHEMA_VERSION,
        isolation_level: IsolationLevel::SharedKernel,
        stake_proof: None,
    }
}

fn sample_heartbeat() -> HeartbeatContent {
    HeartbeatContent {
        provider_npub: "npub1example".to_string(),
        timestamp: 1_700_000_000,
        active_workloads: 3,
        available_capacity: CapacityInfo {
            cpu_available: 4000,
            memory_mb_available: 8192,
            storage_gb_available: 100,
        },
        version: SCHEMA_VERSION,
    }
}

#[test]
fn offer_v1_roundtrip() {
    let offer = sample_offer();
    let json = serde_json::to_string(&offer).expect("serialize");
    let back: ProviderOfferContent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.provider_npub, offer.provider_npub);
    assert_eq!(back.version, SCHEMA_VERSION);
    assert_eq!(back.isolation_level, IsolationLevel::SharedKernel);
}

#[test]
fn heartbeat_v1_roundtrip() {
    let hb = sample_heartbeat();
    let json = serde_json::to_string(&hb).expect("serialize");
    let back: HeartbeatContent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.timestamp, hb.timestamp);
    assert_eq!(back.version, SCHEMA_VERSION);
}

#[test]
fn offer_v0_payload_defaults_to_v1_schema() {
    // A pre-Unit-4 publisher emits no `version` and no
    // `isolation_level`. Today's parser must accept it.
    let v0_json = serde_json::json!({
        "provider_npub": "npub1old",
        "hostname": "old.example",
        "location": null,
        "capabilities": ["lxc"],
        "specs": [],
        "whitelisted_mints": [],
        "uptime_percent": 90.0,
        "total_jobs_completed": 0,
        "api_endpoint": null,
    });

    let parsed: ProviderOfferContent = serde_json::from_value(v0_json).expect("v0 must parse");
    assert_eq!(parsed.version, SCHEMA_VERSION);
    assert_eq!(parsed.isolation_level, IsolationLevel::SharedKernel);
}

#[test]
fn heartbeat_v0_payload_defaults_to_v1_schema() {
    let v0_json = serde_json::json!({
        "provider_npub": "npub1old",
        "timestamp": 1_600_000_000u64,
        "active_workloads": 0,
        "available_capacity": {
            "cpu_available": 0,
            "memory_mb_available": 0,
            "storage_gb_available": 0,
        },
    });

    let parsed: HeartbeatContent = serde_json::from_value(v0_json).expect("v0 must parse");
    assert_eq!(parsed.version, SCHEMA_VERSION);
}

#[test]
fn offer_with_unknown_future_field_is_tolerated() {
    let json = serde_json::json!({
        "provider_npub": "npub1future",
        "hostname": "fut.example",
        "location": null,
        "capabilities": ["lxc"],
        "specs": [],
        "whitelisted_mints": [],
        "uptime_percent": 100.0,
        "total_jobs_completed": 0,
        "api_endpoint": null,
        "version": 1,
        "isolation_level": "shared-kernel",
        "future_field_we_have_no_clue_about": {"k": "v"},
    });

    serde_json::from_value::<ProviderOfferContent>(json)
        .expect("unknown fields must not break parsing");
}

#[test]
fn isolation_level_serializes_as_kebab_case() {
    let s = serde_json::to_string(&IsolationLevel::AttestedResearchTier).unwrap();
    assert_eq!(s, "\"attested-research-tier\"");

    let s = serde_json::to_string(&IsolationLevel::DedicatedHost).unwrap();
    assert_eq!(s, "\"dedicated-host\"");
}

#[test]
fn isolation_level_deserializes_kebab_case() {
    let level: IsolationLevel = serde_json::from_str("\"shared-kernel\"").unwrap();
    assert_eq!(level, IsolationLevel::SharedKernel);

    let level: IsolationLevel = serde_json::from_str("\"attested-research-tier\"").unwrap();
    assert_eq!(level, IsolationLevel::AttestedResearchTier);
}
