use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::cashu::{validate_and_redeem, MintRedeemer, RedeemError};
use crate::compute::{ComputeBackend, ContainerConfig, PortMapping};
use crate::durable_workload::{DurableWorkload, WorkloadState, WorkloadStateMachine};
use crate::nostr::{
    AccessDetailsContent, EncryptedSpawnPodRequest, EncryptedTopUpPodRequest, ErrorResponseContent,
    NostrRelaySubscriber, PodSpec, StatusRequestContent, StatusResponseContent,
    TopUpResponseContent, WarmStandbyRole,
};
use crate::provider::config::ProviderConfig;
use crate::provider::persistence::{persist_workloads, WorkloadInfo};
use crate::provider::standby::{compute_warm_standby_role, StandbySlot};
use crate::provider::ProviderStats;
use crate::templates::{TemplateDefinition, TemplateName};

/// Everything the Nostr-DM request handlers need. Cloning is cheap: every
/// mutable field is behind an `Arc`.
#[derive(Clone)]
pub(crate) struct HandlerDeps {
    pub(crate) backend: Arc<dyn ComputeBackend>,
    pub(crate) config: ProviderConfig,
    pub(crate) nostr: NostrRelaySubscriber,
    pub(crate) redeemer: Arc<dyn MintRedeemer>,
    pub(crate) workloads: Arc<Mutex<HashMap<u32, WorkloadInfo>>>,
    pub(crate) stats: Arc<Mutex<ProviderStats>>,
    pub(crate) state_machine: Arc<Mutex<WorkloadStateMachine>>,
    pub(crate) standby_slots: Arc<Mutex<HashMap<String, StandbySlot>>>,
}

/// `Ok(None)` means an error response has already been sent to the consumer and
/// the caller should return without doing further work.
type Handled<T> = Result<Option<T>>;

/// Generate a 16-character alphanumeric SSH password. `pub(crate)` so the
/// HTTP+ngx_l402 interface can reuse it.
pub(crate) fn generate_password() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Parse a pod identifier back into the internal vmid. Accepts both
/// `container-<vmid>` (what `AccessDetailsContent` returns) and a bare number.
pub fn parse_pod_npub(pod_npub: &str) -> Option<u32> {
    if let Some(rest) = pod_npub.strip_prefix("container-") {
        rest.parse().ok()
    } else {
        pod_npub.parse().ok()
    }
}

/// Translate a `RedeemError` into the `(error_type, message)` pair sent back to
/// the consumer. The error-type strings are stable so consumers can act on them
/// (retry on `mint_network_error`, give up on `token_already_spent`, ...).
fn redeem_error_to_response(err: &RedeemError) -> (&'static str, String) {
    match err {
        RedeemError::InvalidToken(msg) => {
            ("invalid_token", format!("Invalid Cashu token: {}", msg))
        }
        RedeemError::NonWhitelistedMint { mint_url } => (
            "non_whitelisted_mint",
            format!("Mint {} is not accepted by this provider", mint_url),
        ),
        RedeemError::AlreadySpent => (
            "token_already_spent",
            "This Cashu token has already been spent at the mint".to_string(),
        ),
        RedeemError::Pending => (
            "token_pending",
            "Token is pending at the mint; retry shortly".to_string(),
        ),
        RedeemError::Network(msg) => (
            "mint_network_error",
            format!("Could not reach mint: {}", msg),
        ),
        RedeemError::UnsupportedUnit(unit) => (
            "unsupported_unit",
            format!("Token unit {} is not supported", unit),
        ),
        RedeemError::MintError(msg) => ("mint_error", format!("Mint rejected redemption: {}", msg)),
    }
}

fn unix_now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

/// Redeem `token` and report the amount in msats, replying with a structured
/// error if the mint refuses. Refusal means no container is ever created.
async fn redeem_or_respond(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    token: &str,
    context: &str,
) -> Handled<u64> {
    match validate_and_redeem(
        deps.redeemer.as_ref(),
        &deps.config.whitelisted_mints,
        token,
    )
    .await
    {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            let (error_type, err_msg) = redeem_error_to_response(&e);
            error!("{} redemption failed: {}", context, err_msg);
            deps.nostr
                .send_error_response(requester_pubkey, error_type, &err_msg, None, message_type)
                .await?;
            Ok(None)
        }
    }
}

/// Everything derived from the request that the container needs, kept together
/// so the standby and primary branches build it exactly once.
struct SpawnPlan {
    container_config: ContainerConfig,
    template: Option<TemplateDefinition>,
    password: String,
    host_port: u16,
}

/// Handle a spawn request: redeem, provision, and reply with access details.
pub(crate) async fn handle_spawn_request(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    request: EncryptedSpawnPodRequest,
) -> Result<()> {
    let config = &deps.config;
    let nostr = &deps.nostr;

    info!(
        "Processing spawn request from {} (tier: {:?})",
        requester_pubkey, request.pod_spec_id
    );

    // The consumer sends the same request shape to every provider in a
    // warm-standby set; each compares its own npub to decide its role.
    let role = compute_warm_standby_role(&nostr.get_service_public_key(), &request);
    if matches!(role, WarmStandbyRole::NotAddressed) {
        // Refuse to spend the token: the consumer designated us neither
        // primary nor standby, so they sent to the wrong provider.
        let err_msg =
            "warm-standby spawn arrived at a provider not designated as primary or standby";
        warn!("{}", err_msg);
        nostr
            .send_error_response(
                requester_pubkey,
                "not_addressed",
                err_msg,
                None,
                message_type,
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

    // Fall back to the first spec when none was named or the name is unknown.
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
            nostr
                .send_error_response(requester_pubkey, "no_specs", err_msg, None, message_type)
                .await?;
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
        nostr
            .send_error_response(
                requester_pubkey,
                "insufficient_payment",
                &err_msg,
                None,
                message_type,
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
            nostr
                .send_error_response(
                    requester_pubkey,
                    "provisioning_error",
                    &err_msg,
                    None,
                    message_type,
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
        nostr
            .send_error_response(
                requester_pubkey,
                "backend_error",
                &err_msg,
                None,
                message_type,
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

    // Starts in `Provisioning`; the orchestrator promotes it to `Live` on the
    // first observation tick that sees quorum.
    deps.state_machine.lock().await.track(DurableWorkload {
        workload_id: id,
        provider_npub: nostr.get_service_public_key(),
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

/// Resolve the template (if any) and assemble the container configuration.
async fn build_spawn_plan(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    request: &EncryptedSpawnPodRequest,
    spec: &PodSpec,
    id: u32,
) -> Handled<SpawnPlan> {
    let nostr = &deps.nostr;
    let config = &deps.config;

    let password = generate_password();
    let host_port = match config.ssh_port_start {
        Some(start) => start + (id - config.vmid_range_start) as u16,
        None => 30000 + (id % 10000) as u16,
    };

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
                nostr
                    .send_error_response(
                        requester_pubkey,
                        "unknown_template",
                        &err_msg,
                        None,
                        message_type,
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
                    protocol: "tcp",
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

    // A key on a stateless template is silently ignored rather than warned
    // about to the consumer: there is nothing to encrypt.
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
                let _ = nostr
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
            password: password.clone(),
            ssh_key: None,
            host_port: Some(host_port),
            template_ports,
            template_env,
            extra_runtime_args,
            data_path,
            volume_encryption_key,
        },
        template,
        password,
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
    let nostr = &deps.nostr;

    let workload_id = match request.workload_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            let err_msg = "warm-standby spawn missing workload_id (consumer-assigned UUID required to coordinate primary + standbys)";
            warn!("{}", err_msg);
            nostr
                .send_error_response(
                    requester_pubkey,
                    "missing_workload_id",
                    err_msg,
                    None,
                    message_type,
                )
                .await?;
            return Ok(());
        }
    };

    let now = unix_now()?;
    let self_npub = nostr.get_service_public_key();
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
    deps.standby_slots
        .lock()
        .await
        .insert(workload_id.clone(), slot);

    // Reuse AccessDetailsContent's shape with a distinguishing instructions
    // block; a dedicated content type would be a wire-schema bump for one edge
    // case.
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
    nostr
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
                protocol: p.protocol.to_string(),
                label,
            }
        })
        .collect();

    let expires_dt = chrono::DateTime::from_timestamp(expires_at as i64, 0).unwrap_or_default();

    let mut instructions = vec![
        "🚀 Workload provisioned successfully!".to_string(),
        "👤 Username: root".to_string(),
        format!("🔑 Password: {}", plan.password),
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

/// Extend an existing lease.
///
/// Redemption (a network call to the mint) happens between the two workload
/// lock windows, so the lock is never held across an external request.
pub(crate) async fn handle_topup_request(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    request: EncryptedTopUpPodRequest,
) -> Result<()> {
    let config = &deps.config;
    let nostr = &deps.nostr;

    info!(
        "Processing topup request from {} for {}",
        requester_pubkey, request.pod_npub
    );

    let vmid = match parse_pod_npub(&request.pod_npub) {
        Some(v) => v,
        None => {
            let err_msg = format!(
                "Could not parse pod identifier `{}`; expected `container-<id>` or numeric id",
                request.pod_npub
            );
            warn!("{}", err_msg);
            nostr
                .send_error_response(
                    requester_pubkey,
                    "invalid_pod_id",
                    &err_msg,
                    None,
                    message_type,
                )
                .await?;
            return Ok(());
        }
    };

    let now = unix_now()?;

    // Snapshot under a brief lock so we know how to bill before we redeem.
    let (spec_id, current_expires_at) = {
        let lock = deps.workloads.lock().await;
        match lock.get(&vmid) {
            Some(w) if w.owner_npub == requester_pubkey => (w.spec_id.clone(), w.expires_at),
            Some(_) => {
                drop(lock);
                let err_msg = "Pod not owned by requester";
                warn!("{}: vmid={}", err_msg, vmid);
                nostr
                    .send_error_response(requester_pubkey, "not_owner", err_msg, None, message_type)
                    .await?;
                return Ok(());
            }
            None => {
                drop(lock);
                let err_msg = format!("Pod {} not found", request.pod_npub);
                warn!("{}", err_msg);
                nostr
                    .send_error_response(
                        requester_pubkey,
                        "not_found",
                        &err_msg,
                        None,
                        message_type,
                    )
                    .await?;
                return Ok(());
            }
        }
    };

    if current_expires_at <= now {
        let err_msg = format!(
            "Pod {} lease has already expired; spawn a new pod instead",
            request.pod_npub
        );
        warn!("{}", err_msg);
        nostr
            .send_error_response(
                requester_pubkey,
                "lease_expired",
                &err_msg,
                None,
                message_type,
            )
            .await?;
        return Ok(());
    }

    let spec = match config.specs.iter().find(|s| s.id == spec_id) {
        Some(s) => s.clone(),
        None => {
            // Refuse rather than silently mis-bill against another spec.
            let err_msg = format!(
                "Pod {} references unknown spec `{}`; provider misconfiguration",
                request.pod_npub, spec_id
            );
            error!("{}", err_msg);
            nostr
                .send_error_response(
                    requester_pubkey,
                    "spec_unavailable",
                    &err_msg,
                    None,
                    message_type,
                )
                .await?;
            return Ok(());
        }
    };

    let payment_msats = match redeem_or_respond(
        deps,
        requester_pubkey,
        message_type,
        &request.cashu_token,
        "Topup",
    )
    .await?
    {
        Some(v) => v,
        None => return Ok(()),
    };

    let extension_secs = payment_msats / spec.rate_msats_per_sec;
    if extension_secs == 0 {
        let err_msg = format!(
            "Insufficient topup: {} msats buys 0 seconds at {} msats/sec",
            payment_msats, spec.rate_msats_per_sec
        );
        warn!("{}", err_msg);
        nostr
            .send_error_response(
                requester_pubkey,
                "insufficient_payment",
                &err_msg,
                None,
                message_type,
            )
            .await?;
        return Ok(());
    }

    // Re-check ownership and existence after re-locking: cleanup may have run
    // since the snapshot.
    let new_expires_at = {
        let mut lock = deps.workloads.lock().await;
        match lock.get_mut(&vmid) {
            Some(w) if w.owner_npub == requester_pubkey => {
                w.expires_at = w.expires_at.saturating_add(extension_secs);
                let extended = w.expires_at;
                // The consumer already paid for this extension; a restart must
                // not roll it back to the old expiry.
                persist_workloads(&lock, &config.workload_state_path);
                extended
            }
            _ => {
                // The token is already spent at the mint, so surface a distinct
                // error the CLI can explain.
                drop(lock);
                let err_msg =
                    "Pod was cleaned up before topup could be applied; token has been spent";
                error!("{}: vmid={}", err_msg, vmid);
                nostr
                    .send_error_response(requester_pubkey, "race_lost", err_msg, None, message_type)
                    .await?;
                return Ok(());
            }
        }
    };

    let new_expires_dt =
        chrono::DateTime::from_timestamp(new_expires_at as i64, 0).unwrap_or_default();
    let response = TopUpResponseContent {
        success: true,
        pod_npub: request.pod_npub.clone(),
        extended_duration_seconds: extension_secs,
        new_expires_at: new_expires_dt.to_rfc3339(),
        message: format!(
            "Lease extended by {}s ({} msats @ {} msats/sec)",
            extension_secs, payment_msats, spec.rate_msats_per_sec
        ),
    };

    nostr
        .send_topup_response_private_message(requester_pubkey, response, message_type)
        .await?;

    info!(
        "Topup applied to {}: +{}s (now expires at {})",
        request.pod_npub, extension_secs, new_expires_at
    );

    Ok(())
}

pub(crate) async fn handle_status_request(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    request: StatusRequestContent,
) -> Result<()> {
    let config = &deps.config;
    let nostr = &deps.nostr;

    info!(
        "Processing status request for pod {} from {}",
        request.pod_id, requester_pubkey
    );

    let workload = {
        let lock = deps.workloads.lock().await;
        match request.pod_id.parse::<u32>().ok() {
            Some(vmid) => lock.get(&vmid).cloned(),
            None => lock
                .values()
                .find(|w| w.owner_npub == request.pod_id || w.owner_npub == requester_pubkey)
                .cloned(),
        }
    };

    let workload = match workload {
        Some(w) => w,
        None => {
            let err_msg = format!(
                "Workload {} not found or you don't have access",
                request.pod_id
            );
            warn!("{}", err_msg);
            nostr
                .send_error_response(requester_pubkey, "not_found", &err_msg, None, message_type)
                .await?;
            return Ok(());
        }
    };

    let now = unix_now()?;
    let time_remaining = workload.expires_at.saturating_sub(now);
    let status = if time_remaining == 0 {
        "Expired"
    } else {
        "Running"
    };

    let expires_dt =
        chrono::DateTime::from_timestamp(workload.expires_at as i64, 0).unwrap_or_default();

    let spec = config.specs.iter().find(|s| s.id == workload.spec_id);
    let host_port = match config.ssh_port_start {
        Some(start) => start + (workload.vmid - config.vmid_range_start) as u16,
        None => (30000 + (workload.vmid % 10000)) as u16,
    };

    let response = StatusResponseContent {
        pod_id: workload.vmid.to_string(),
        status: status.to_string(),
        expires_at: expires_dt.to_rfc3339(),
        time_remaining_seconds: time_remaining,
        cpu_millicores: spec.map(|s| s.cpu_millicores).unwrap_or(1000),
        memory_mb: spec.map(|s| s.memory_mb).unwrap_or(1024),
        ssh_host: config.public_ip.clone(),
        ssh_port: host_port,
        ssh_username: "root".to_string(),
    };

    nostr
        .send_status_response(requester_pubkey, response, message_type)
        .await?;

    info!("Status response sent for workload {}", workload.vmid);
    Ok(())
}
