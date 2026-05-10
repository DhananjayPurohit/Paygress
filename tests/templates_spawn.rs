//! Wire-format + template-resolution tests for the spawn flow with
//! the new `template_slug` field. The full end-to-end (DM → provider
//! → docker run) is exercised live on the VPS, not here — these tests
//! cover the parts that are deterministic on a build host.

use std::collections::HashMap;

use paygress::compute::{ContainerConfig, PortMapping};
use paygress::nostr::EncryptedSpawnPodRequest;
use paygress::templates::{TemplateDefinition, TemplateName};

#[test]
fn spawn_request_omits_template_slug_when_none() {
    let req = EncryptedSpawnPodRequest {
        cashu_token: "cashuA...".to_string(),
        pod_spec_id: Some("basic".to_string()),
        pod_image: "ubuntu:22.04".to_string(),
        ssh_username: "u".to_string(),
        ssh_password: "p".to_string(),
        template_slug: None,
        replication: None,
        primary_npub: None,
        workload_id: None,
        volume_encryption: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(
        !json.contains("template_slug"),
        "skip_serializing_if = None means the field stays off the wire for old consumers"
    );
}

#[test]
fn spawn_request_includes_template_slug_when_set() {
    let req = EncryptedSpawnPodRequest {
        cashu_token: "cashuA...".to_string(),
        pod_spec_id: Some("basic".to_string()),
        pod_image: "ignored".to_string(),
        ssh_username: "u".to_string(),
        ssh_password: "p".to_string(),
        template_slug: Some("nostr-relay".to_string()),
        replication: None,
        primary_npub: None,
        workload_id: None,
        volume_encryption: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains(r#""template_slug":"nostr-relay""#));
}

#[test]
fn old_v0_payload_without_template_slug_still_parses() {
    // What an old CLI (pre-this PR) sends: no template_slug field.
    // The provider must continue to deserialize it cleanly thanks
    // to `#[serde(default)]`.
    let v0 = serde_json::json!({
        "cashu_token": "cashuA...",
        "pod_spec_id": "basic",
        "pod_image": "ubuntu:22.04",
        "ssh_username": "u",
        "ssh_password": "p"
    });
    let parsed: EncryptedSpawnPodRequest = serde_json::from_value(v0).unwrap();
    assert!(parsed.template_slug.is_none());
}

#[test]
fn nostr_relay_definition_resolves_via_slug() {
    let name = TemplateName::from_slug("nostr-relay").expect("slug round-trips");
    let def = TemplateDefinition::lookup(name);
    assert!(def.image.contains("strfry"));
    assert_eq!(def.ports.len(), 1);
    assert_eq!(def.ports[0].container_port, 7777);
}

#[test]
fn unknown_slug_does_not_resolve() {
    assert!(TemplateName::from_slug("malicious-template").is_none());
}

/// Mirrors the port-allocation rule in `handle_spawn_request`: each
/// template port published on `host_port + i + 1`. Pinning this
/// here so a refactor that breaks the formula (e.g. drops the +1
/// offset and collides with SSH) trips a test.
#[test]
fn template_ports_offset_from_ssh_host_port() {
    let host_port: u16 = 30042;
    let def = TemplateDefinition::lookup(TemplateName::HeadlessBrowser);
    let allocated: Vec<PortMapping> = def
        .ports
        .iter()
        .enumerate()
        .map(|(i, p)| PortMapping {
            host_port: host_port.saturating_add(1 + i as u16),
            container_port: p.container_port,
            protocol: "tcp",
        })
        .collect();
    assert_eq!(allocated.len(), 2);
    assert_ne!(
        allocated[0].host_port, host_port,
        "must NOT collide with SSH port"
    );
    assert_eq!(allocated[0].host_port, 30043);
    assert_eq!(allocated[1].host_port, 30044);
}

#[test]
fn container_config_carries_template_data() {
    let def = TemplateDefinition::lookup(TemplateName::NostrRelay);
    let mut env: HashMap<String, String> = HashMap::new();
    for (k, v) in &def.env {
        env.insert(k.to_string(), v.to_string());
    }
    let cfg = ContainerConfig {
        id: 1234,
        name: "paygress-1234".to_string(),
        image: def.image.to_string(),
        cpu_cores: 1,
        memory_mb: 512,
        storage_gb: 5,
        password: "x".to_string(),
        ssh_key: None,
        host_port: Some(30042),
        template_ports: def
            .ports
            .iter()
            .enumerate()
            .map(|(i, p)| PortMapping {
                host_port: 30042u16.saturating_add(1 + i as u16),
                container_port: p.container_port,
                protocol: "tcp",
            })
            .collect(),
        template_env: env,
        extra_runtime_args: def
            .extra_docker_args
            .iter()
            .map(|s| s.to_string())
            .collect(),
        data_path: def.data_path.map(|p| p.to_string()),
        volume_encryption_key: None,
    };
    assert_eq!(cfg.template_ports.len(), 1);
    assert_eq!(cfg.template_ports[0].container_port, 7777);
    assert!(cfg.template_env.contains_key("STRFRY_DB_PATH"));
}
