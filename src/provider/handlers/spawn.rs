use std::collections::HashMap;

use anyhow::Result;
use tracing::{debug, error, info, warn};

use crate::compute::{ContainerConfig, PortMapping};
use crate::durable_workload::{DurableWorkload, WorkloadState};
use crate::nostr::{
    AccessDetailsContent, EncryptedSpawnPodRequest, ErrorResponseContent, PodSpec, WarmStandbyRole,
};
use crate::provider::config::BackendType;
use crate::provider::persistence::{persist_standby_slots, persist_workloads, WorkloadInfo};
use crate::provider::standby::{compute_warm_standby_role, StandbySlot};
use crate::templates::{TemplateDefinition, TemplateName};

use super::{generate_password, redeem_or_respond, send_error, unix_now, Handled, HandlerDeps};

/// Everything derived from the request that the container needs, kept together
/// so the standby and primary branches build it exactly once.
struct SpawnPlan {
    container_config: ContainerConfig,
    template: Option<TemplateDefinition>,
    host_port: u16,
}

/// The consumer chose this inside the encrypted spawn request. Honouring it is
/// what lets an automated caller reach the box at all: the generated password
/// is only ever returned inside the human-readable `instructions`, which no
/// program should be parsing.
///
/// Rejected unless plain alphanumeric — the KVM backend embeds it in cloud-init
/// YAML and LXD pipes `root:<password>` to `chpasswd`, so a newline would set
/// other accounts' passwords.
fn usable_ssh_password(requested: &str) -> Option<String> {
    let ok = (12..=64).contains(&requested.len())
        && requested.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| requested.to_string())
}

/// Templates name public Docker images. LXD cannot launch one, and KVM cuts
/// every VM from one operator-chosen base image and ignores the field
/// entirely — on both, a template spawn would take the token and hand back a
/// bare box. Those backends serve a sandbox with the toolchain baked in
/// instead (`images/ci-sandbox/`).
fn serves_templates(backend: BackendType) -> bool {
    matches!(backend, BackendType::Docker | BackendType::Proxmox)
}

/// Redeem, provision, and reply with access details.
pub(crate) async fn handle_spawn_request(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    request: EncryptedSpawnPodRequest,
) -> Result<()> {
    let config = &deps.config;

    info!(
        "Processing spawn request from {} (tier: {:?})",
        requester_pubkey, request.pod_spec_id
    );

    // The consumer sends the same request shape to every provider in a
    // warm-standby set; each compares its own npub to decide its role.
    let role = compute_warm_standby_role(&deps.nostr.get_service_public_key(), &request);
    if matches!(role, WarmStandbyRole::NotAddressed) {
        // Refuse to spend the token: the consumer designated us neither
        // primary nor standby, so they sent to the wrong provider.
        let err_msg =
            "warm-standby spawn arrived at a provider not designated as primary or standby";
        warn!("{}", err_msg);
        send_error(
            deps,
            requester_pubkey,
            message_type,
            "not_addressed",
            err_msg,
        )
        .await?;
        return Ok(());
    }

    // Before redemption: accepting a template this backend cannot materialize
    // would take the token and hand back a bare box instead.
    if request.template_slug.is_some() && !serves_templates(config.backend_type) {
        let err_msg = "this provider's backend serves its own image and cannot run templates";
        warn!("{}", err_msg);
        send_error(
            deps,
            requester_pubkey,
            message_type,
            "template_unsupported",
            err_msg,
        )
        .await?;
        return Ok(());
    }

    let payment_msats = match redeem_or_respond(
        deps,
        requester_pubkey,
        message_type,
        &request.cashu_token,
        "Cashu",
    )
    .await?
    {
        Some(v) => v,
        None => return Ok(()),
    };

    let spec = match config
        .specs
        .iter()
        .find(|s| request.pod_spec_id.as_deref() == Some(s.id.as_str()))
        .or_else(|| config.specs.first())
    {
        Some(s) => s.clone(),
        None => {
            let err_msg = "No pod specifications available on this provider";
            error!("{}", err_msg);
            send_error(deps, requester_pubkey, message_type, "no_specs", err_msg).await?;
            return Ok(());
        }
    };

    let duration_secs = payment_msats / spec.rate_msats_per_sec;
    if duration_secs < config.minimum_duration_seconds {
        let err_msg = format!(
            "Insufficient payment for minimum duration. Required: {} msats for {}s",
            config.minimum_duration_seconds * spec.rate_msats_per_sec,
            config.minimum_duration_seconds
        );
        warn!("{}", err_msg);
        send_error(
            deps,
            requester_pubkey,
            message_type,
            "insufficient_payment",
            &err_msg,
        )
        .await?;
        return Ok(());
    }

    info!(
        "Validated payment: {} msats for {}s on tier {}",
        payment_msats, duration_secs, spec.name
    );

    let id = match deps
        .backend
        .find_available_id(config.vmid_range_start, config.vmid_range_end)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            let err_msg = format!("Failed to find available ID: {}", e);
            error!("{}", err_msg);
            send_error(
                deps,
                requester_pubkey,
                message_type,
                "provisioning_error",
                &err_msg,
            )
            .await?;
            return Ok(());
        }
    };

    let plan =
        match build_spawn_plan(deps, requester_pubkey, message_type, &request, &spec, id).await? {
            Some(p) => p,
            None => return Ok(()),
        };

    // A standby doesn't create the container yet: it reserves the slot and
    // waits for a LeaseRevocation. The token is already redeemed, so the
    // provider earns for offering the capacity even if failover never happens.
    if let WarmStandbyRole::Standby { index, count } = role {
        return reserve_standby_slot(
            deps,
            requester_pubkey,
            message_type,
            &request,
            &spec,
            &plan,
            index,
            count,
            duration_secs,
        )
        .await;
    }

    debug!("Calling backend.create_container for workload {}", id);
    if let Err(e) = deps.backend.create_container(&plan.container_config).await {
        let err_msg = format!("Backend failed to create workload: {}", e);
        error!("{}", err_msg);
        send_error(
            deps,
            requester_pubkey,
            message_type,
            "backend_error",
            &err_msg,
        )
        .await?;
        return Ok(());
    }
    debug!("Successfully created container {}", id);

    let now = unix_now()?;
    let workload = WorkloadInfo {
        vmid: id,
        workload_type: "lxc".to_string(),
        spec_id: spec.id.clone(),
        created_at: now,
        expires_at: now + duration_secs,
        owner_npub: requester_pubkey.to_string(),
        replication: request.replication.clone().unwrap_or_default(),
        restart_policy: crate::durable_workload::RestartPolicy::default(),
        state_uri: None,
        consumer_workload_id: request.workload_id.clone().filter(|s| !s.is_empty()),
    };

    {
        let mut lock = deps.workloads.lock().await;
        lock.insert(id, workload.clone());
        persist_workloads(&lock, &config.workload_state_path);
    }

    deps.state_machine.lock().await.track(DurableWorkload {
        workload_id: id,
        provider_npub: deps.nostr.get_service_public_key(),
        state: WorkloadState::Provisioning { since: now },
        replication: workload.replication.clone(),
        restart_policy: workload.restart_policy,
        state_uri: workload.state_uri.clone(),
        created_at: now,
        expires_at: workload.expires_at,
    });

    deps.stats.lock().await.total_jobs_completed += 1;

    send_spawn_access_details(
        deps,
        requester_pubkey,
        message_type,
        &spec,
        &plan,
        id,
        workload.expires_at,
    )
    .await?;

    info!("Workload {} provisioned for {} seconds", id, duration_secs);
    Ok(())
}

async fn build_spawn_plan(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    request: &EncryptedSpawnPodRequest,
    spec: &PodSpec,
    id: u32,
) -> Handled<SpawnPlan> {
    let config = &deps.config;

    let password = usable_ssh_password(&request.ssh_password).unwrap_or_else(generate_password);
    let host_port = config.ssh_host_port(id);

    // Image, ports and env come from the provider's own registry, never from
    // consumer bytes. Unknown slugs are rejected rather than ignored so a
    // consumer can't probe for accepted templates.
    let template = match request.template_slug.as_deref() {
        Some(slug) => match TemplateName::from_slug(slug) {
            Some(name) => Some(TemplateDefinition::lookup(name)),
            None => {
                let err_msg = format!(
                    "Unknown template `{}` — provider does not advertise it",
                    slug
                );
                warn!("{}", err_msg);
                send_error(
                    deps,
                    requester_pubkey,
                    message_type,
                    "unknown_template",
                    &err_msg,
                )
                .await?;
                return Ok(None);
            }
        },
        None => None,
    };

    let image = template
        .as_ref()
        .map(|t| t.image.to_string())
        .unwrap_or_else(|| request.pod_image.clone());

    // Template port `i` is published on `host_port + 1 + i`; `host_port` itself
    // stays reserved for SSH, which some backends care about.
    let template_ports: Vec<PortMapping> = template
        .as_ref()
        .map(|t| {
            t.ports
                .iter()
                .enumerate()
                .map(|(i, p)| PortMapping {
                    host_port: host_port.saturating_add(1 + i as u16),
                    container_port: p.container_port,
                    protocol: "tcp".to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let mut template_env: HashMap<String, String> = template
        .as_ref()
        .map(|t| {
            t.env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Templates that bake in the paygress-exec server default EXEC_USER/PASS to
    // empty and return 503 until they're set. Overlaying the same credentials
    // the consumer sees in AccessDetails is what unlocks /exec, and keeps it to
    // one secret per spawn.
    if let Some(t) = template.as_ref() {
        if t.env.contains_key("EXEC_USER") {
            template_env.insert("EXEC_USER".to_string(), "root".to_string());
        }
        if t.env.contains_key("EXEC_PASS") {
            template_env.insert("EXEC_PASS".to_string(), password.clone());
        }
    }

    let extra_runtime_args: Vec<String> = template
        .as_ref()
        .map(|t| t.extra_docker_args.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();

    let data_path: Option<String> = template
        .as_ref()
        .and_then(|t| t.data_path.map(|p| p.to_string()));

    let volume_encryption_key = match (&data_path, request.volume_encryption.as_ref()) {
        (Some(_), Some(ve)) => match ve.decoded_key() {
            Ok(key) => {
                info!(
                    "Spawn request includes volume_encryption (algorithm={}, version={}); will create LUKS-encrypted data volume",
                    ve.algorithm, ve.version
                );
                Some(key)
            }
            Err(e) => {
                error!(
                    "Rejecting spawn: malformed volume_encryption.key_b64: {}",
                    e
                );
                let err_payload = ErrorResponseContent {
                    error_type: "invalid_volume_encryption".to_string(),
                    message: format!("volume_encryption rejected: {}", e),
                    details: None,
                };
                let _ = deps
                    .nostr
                    .send_error_response_private_message(
                        requester_pubkey,
                        err_payload,
                        message_type,
                    )
                    .await;
                return Ok(None);
            }
        },
        (None, Some(_)) => {
            warn!(
                "Spawn request set volume_encryption but template has no data_path; encryption is a no-op for stateless workloads"
            );
            None
        }
        _ => None,
    };

    Ok(Some(SpawnPlan {
        container_config: ContainerConfig {
            id,
            name: format!("paygress-{}", id),
            image,
            cpu_cores: (spec.cpu_millicores / 1000).max(1) as u32,
            memory_mb: spec.memory_mb as u32,
            storage_gb: 10,
            password,
            ssh_key: None,
            host_port: Some(host_port),
            template_ports,
            template_env,
            extra_runtime_args,
            data_path,
            volume_encryption_key,
        },
        template,
        host_port,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn reserve_standby_slot(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    request: &EncryptedSpawnPodRequest,
    spec: &PodSpec,
    plan: &SpawnPlan,
    index: usize,
    count: usize,
    duration_secs: u64,
) -> Result<()> {
    let workload_id = match request.workload_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            let err_msg = "warm-standby spawn missing workload_id (consumer-assigned UUID required to coordinate primary + standbys)";
            warn!("{}", err_msg);
            send_error(
                deps,
                requester_pubkey,
                message_type,
                "missing_workload_id",
                err_msg,
            )
            .await?;
            return Ok(());
        }
    };

    let now = unix_now()?;
    let self_npub = deps.nostr.get_service_public_key();
    let peer_standby_npubs: Vec<String> = match request.replication.as_ref() {
        Some(crate::durable_workload::ReplicationMode::WarmStandby { standby_providers }) => {
            standby_providers
                .iter()
                .filter(|p| !crate::nostr::npubs_equal(p, &self_npub))
                .cloned()
                .collect()
        }
        _ => Vec::new(),
    };

    let slot = StandbySlot {
        workload_id: workload_id.clone(),
        primary_npub: request.primary_npub.clone().unwrap_or_default(),
        standby_index: index,
        standby_count: count,
        container_config: plan.container_config.clone(),
        spec_id: spec.id.clone(),
        expires_at: now + duration_secs,
        owner_npub: requester_pubkey.to_string(),
        created_at: now,
        peer_standby_npubs,
    };
    info!(
        "Reserved standby slot for workload_id={} (index {}/{}, expires at {})",
        workload_id, index, count, slot.expires_at
    );
    {
        let mut slots = deps.standby_slots.lock().await;
        slots.insert(workload_id.clone(), slot);
        persist_standby_slots(&slots, &deps.config.standby_state_path);
    }

    // Reuses AccessDetailsContent's shape; a dedicated content type would be a
    // wire-schema bump for one edge case.
    let expires_dt =
        chrono::DateTime::from_timestamp((now + duration_secs) as i64, 0).unwrap_or_default();
    let details = AccessDetailsContent {
        pod_npub: format!("standby-slot-{}", workload_id),
        node_port: 0, // no live container yet; 0 signals "reserved, not running"
        expires_at: expires_dt.to_rfc3339(),
        cpu_millicores: spec.cpu_millicores,
        memory_mb: spec.memory_mb,
        pod_spec_name: spec.name.clone(),
        pod_spec_description: spec.description.clone(),
        instructions: vec![
            format!(
                "🛏️  Standby slot reserved (index {}/{} for workload {}).",
                index, count, workload_id
            ),
            format!(
                "Will promote on LeaseRevocation event from primary {}.",
                request.primary_npub.as_deref().unwrap_or("(unset)")
            ),
            format!(
                "Expected promotion delay: {} seconds (index * 30s backoff).",
                index * 30
            ),
        ],
        host_address: deps.config.public_ip.clone(),
        template_ports: Vec::new(),
    };
    deps.nostr
        .send_access_details_private_message(requester_pubkey, details, message_type)
        .await?;
    Ok(())
}

async fn send_spawn_access_details(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    spec: &PodSpec,
    plan: &SpawnPlan,
    id: u32,
    expires_at: u64,
) -> Result<()> {
    let host = &deps.config.public_ip;

    // Match each mapping back to its template port by container_port (unique
    // within a template today) so the consumer gets the label rather than
    // having to remember the host_port + 1 + i rule.
    let template_access_ports: Vec<crate::nostr::TemplateAccessPort> = plan
        .container_config
        .template_ports
        .iter()
        .map(|p| {
            let label = plan
                .template
                .as_ref()
                .and_then(|t| {
                    t.ports
                        .iter()
                        .find(|tp| tp.container_port == p.container_port)
                })
                .map(|tp| tp.label.to_string())
                .unwrap_or_else(|| format!("port-{}", p.container_port));
            crate::nostr::TemplateAccessPort {
                host_port: p.host_port,
                container_port: p.container_port,
                protocol: p.protocol.clone(),
                label,
            }
        })
        .collect();

    let expires_dt = chrono::DateTime::from_timestamp(expires_at as i64, 0).unwrap_or_default();

    let mut instructions = vec![
        "🚀 Workload provisioned successfully!".to_string(),
        "👤 Username: root".to_string(),
        format!("🔑 Password: {}", plan.container_config.password),
        format!("⌛ Expires: {}", expires_dt.format("%Y-%m-%d %H:%M:%S UTC")),
        "Access: You can connect to the container using SSH.".to_string(),
        format!("  ssh -p {} root@{}", plan.host_port, host),
    ];
    if !template_access_ports.is_empty() {
        instructions.push("Workload ports:".to_string());
        for p in &template_access_ports {
            instructions.push(format!(
                "  {} ({}): {}://{}:{}",
                p.label, p.protocol, p.protocol, host, p.host_port
            ));
        }
    }

    let details = AccessDetailsContent {
        pod_npub: format!("container-{}", id),
        node_port: plan.host_port,
        expires_at: expires_dt.to_rfc3339(),
        cpu_millicores: spec.cpu_millicores,
        memory_mb: spec.memory_mb,
        pod_spec_name: spec.name.clone(),
        pod_spec_description: spec.description.clone(),
        instructions,
        host_address: host.clone(),
        template_ports: template_access_ports,
    };

    debug!("Sending access details to {}", requester_pubkey);
    deps.nostr
        .send_access_details_private_message(requester_pubkey, details, message_type)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sane_requested_password_is_honoured() {
        let requested = "W9ZG2GIUOWiPkYID";
        assert_eq!(
            usable_ssh_password(requested).as_deref(),
            Some(requested),
            "otherwise the consumer's only copy of the password is wrong"
        );
    }

    #[test]
    fn injection_shaped_passwords_fall_back_to_a_generated_one() {
        for bad in [
            "short",
            "with space1234",
            // chpasswd reads one account per line.
            "abcdefghijkl\nroot:hunter2",
            // cloud-init user-data is YAML.
            "abcdefghijkl\n  ssh_pwauth: true",
            "",
        ] {
            assert!(
                usable_ssh_password(bad).is_none(),
                "`{}` must not reach a backend",
                bad.escape_debug()
            );
        }
    }

    #[test]
    fn only_image_backends_serve_templates() {
        assert!(serves_templates(BackendType::Docker));
        assert!(serves_templates(BackendType::Proxmox));
        // Both would take the token and then fail to launch the image.
        assert!(!serves_templates(BackendType::LXD));
        assert!(!serves_templates(BackendType::Kvm));
    }
}
