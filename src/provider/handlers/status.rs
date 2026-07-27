use anyhow::Result;
use tracing::{info, warn};

use crate::nostr::{StatusRequestContent, StatusResponseContent};

use super::{send_error, unix_now, HandlerDeps};

pub(crate) async fn handle_status_request(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    request: StatusRequestContent,
) -> Result<()> {
    let config = &deps.config;

    info!(
        "Processing status request for pod {} from {}",
        request.pod_id, requester_pubkey
    );

    let found = {
        let lock = deps.workloads.lock().await;
        match request.pod_id.parse::<u32>().ok() {
            Some(vmid) => lock.get(&vmid).cloned(),
            None => lock
                .values()
                .find(|w| w.owner_npub == request.pod_id || w.owner_npub == requester_pubkey)
                .cloned(),
        }
    };

    let Some(workload) = found else {
        let err_msg = format!(
            "Workload {} not found or you don't have access",
            request.pod_id
        );
        warn!("{}", err_msg);
        send_error(deps, requester_pubkey, message_type, "not_found", &err_msg).await?;
        return Ok(());
    };

    let time_remaining = workload.expires_at.saturating_sub(unix_now()?);
    let expires_dt =
        chrono::DateTime::from_timestamp(workload.expires_at as i64, 0).unwrap_or_default();
    let spec = config.specs.iter().find(|s| s.id == workload.spec_id);

    let response = StatusResponseContent {
        pod_id: workload.vmid.to_string(),
        status: if time_remaining == 0 {
            "Expired"
        } else {
            "Running"
        }
        .to_string(),
        expires_at: expires_dt.to_rfc3339(),
        time_remaining_seconds: time_remaining,
        cpu_millicores: spec.map(|s| s.cpu_millicores).unwrap_or(1000),
        memory_mb: spec.map(|s| s.memory_mb).unwrap_or(1024),
        ssh_host: config.public_ip.clone(),
        ssh_port: config.ssh_host_port(workload.vmid),
        ssh_username: "root".to_string(),
    };

    deps.nostr
        .send_status_response(requester_pubkey, response, message_type)
        .await?;

    info!("Status response sent for workload {}", workload.vmid);
    Ok(())
}
