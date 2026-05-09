//! Wire-format regression tests for `AccessDetailsContent`'s new
//! `host_address` + `template_ports` fields.
//!
//! Old clients sending pre-this-PR responses MUST keep parsing on
//! the new code path; new responses MUST round-trip cleanly. The
//! consumer-facing UX of these fields is exercised live in
//! `src/cli/commands/spawn.rs` (covered by manual demo).

use paygress::nostr::{AccessDetailsContent, TemplateAccessPort};

fn sample_v1() -> AccessDetailsContent {
    AccessDetailsContent {
        pod_npub: "container-42".to_string(),
        node_port: 30042,
        expires_at: "2026-04-30T00:00:00Z".to_string(),
        cpu_millicores: 1000,
        memory_mb: 1024,
        pod_spec_name: "Basic".to_string(),
        pod_spec_description: "1 vCPU, 1GB".to_string(),
        instructions: vec!["ssh -p 30042 root@host".to_string()],
        host_address: "72.61.173.244".to_string(),
        template_ports: vec![TemplateAccessPort {
            host_port: 30043,
            container_port: 7777,
            protocol: "tcp".to_string(),
            label: "relay-ws".to_string(),
        }],
    }
}

#[test]
fn template_ports_round_trip() {
    let v1 = sample_v1();
    let json = serde_json::to_string(&v1).unwrap();
    let back: AccessDetailsContent = serde_json::from_str(&json).unwrap();
    assert_eq!(back.template_ports.len(), 1);
    assert_eq!(back.template_ports[0].host_port, 30043);
    assert_eq!(back.template_ports[0].label, "relay-ws");
    assert_eq!(back.host_address, "72.61.173.244");
}

#[test]
fn empty_template_ports_skipped_on_wire() {
    let mut v = sample_v1();
    v.template_ports.clear();
    let json = serde_json::to_string(&v).unwrap();
    assert!(
        !json.contains("template_ports"),
        "skip_serializing_if respected — empty Vec stays off the wire so non-template spawns look identical to old clients"
    );
}

#[test]
fn empty_host_address_skipped_on_wire() {
    let mut v = sample_v1();
    v.host_address.clear();
    let json = serde_json::to_string(&v).unwrap();
    assert!(
        !json.contains("host_address"),
        "skip_serializing_if respected for empty String"
    );
}

#[test]
fn old_v0_response_without_new_fields_parses() {
    // Exactly what a pre-this-PR provider emits.
    let v0_json = serde_json::json!({
        "pod_npub": "container-1",
        "node_port": 30001,
        "expires_at": "2026-04-30T00:00:00Z",
        "cpu_millicores": 1000,
        "memory_mb": 1024,
        "pod_spec_name": "Basic",
        "pod_spec_description": "—",
        "instructions": ["ssh -p 30001 root@host"]
    });
    let parsed: AccessDetailsContent = serde_json::from_value(v0_json).expect("v0 must parse");
    assert_eq!(parsed.host_address, "");
    assert!(parsed.template_ports.is_empty());
}

#[test]
fn template_port_label_is_routable() {
    // Pin the label values produced for each template's first port,
    // since the consumer SDK uses these to route by role
    // (e.g. "give me the http port" vs scraping by container_port).
    use paygress::templates::{TemplateDefinition, TemplateName};
    let relay = TemplateDefinition::lookup(TemplateName::NostrRelay);
    assert_eq!(relay.ports[0].label, "relay-ws");
    let infer = TemplateDefinition::lookup(TemplateName::InferenceEndpoint);
    assert_eq!(infer.ports[0].label, "ollama-http");
    let bitcoind = TemplateDefinition::lookup(TemplateName::BitcoinNode);
    assert_eq!(bitcoind.ports[0].label, "rpc");
}
