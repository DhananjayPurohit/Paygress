use anyhow::Result;
use tracing::{error, info, warn};

use crate::nostr::{EncryptedTopUpPodRequest, TopUpResponseContent};
use crate::provider::persistence::persist_workloads;

use super::{parse_pod_npub, redeem_or_respond, send_error, unix_now, HandlerDeps};

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

    info!(
        "Processing topup request from {} for {}",
        requester_pubkey, request.pod_npub
    );

    let Some(vmid) = parse_pod_npub(&request.pod_npub) else {
        let err_msg = format!(
            "Could not parse pod identifier `{}`; expected `container-<id>` or numeric id",
            request.pod_npub
        );
        warn!("{}", err_msg);
        send_error(
            deps,
            requester_pubkey,
            message_type,
            "invalid_pod_id",
            &err_msg,
        )
        .await?;
        return Ok(());
    };

    let now = unix_now()?;

    // Snapshot under a brief lock so we know how to bill before we redeem.
    let snapshot = {
        let lock = deps.workloads.lock().await;
        lock.get(&vmid).map(|w| {
            (
                w.owner_npub == requester_pubkey,
                w.spec_id.clone(),
                w.expires_at,
            )
        })
    };
    let (spec_id, current_expires_at) = match snapshot {
        Some((true, spec_id, expires_at)) => (spec_id, expires_at),
        Some((false, _, _)) => {
            let err_msg = "Pod not owned by requester";
            warn!("{}: vmid={}", err_msg, vmid);
            send_error(deps, requester_pubkey, message_type, "not_owner", err_msg).await?;
            return Ok(());
        }
        None => {
            let err_msg = format!("Pod {} not found", request.pod_npub);
            warn!("{}", err_msg);
            send_error(deps, requester_pubkey, message_type, "not_found", &err_msg).await?;
            return Ok(());
        }
    };

    if current_expires_at <= now {
        let err_msg = format!(
            "Pod {} lease has already expired; spawn a new pod instead",
            request.pod_npub
        );
        warn!("{}", err_msg);
        send_error(
            deps,
            requester_pubkey,
            message_type,
            "lease_expired",
            &err_msg,
        )
        .await?;
        return Ok(());
    }

    // Refuse rather than silently mis-bill against another spec.
    let spec = match config.specs.iter().find(|s| s.id == spec_id) {
        Some(s) => s.clone(),
        None => {
            let err_msg = format!(
                "Pod {} references unknown spec `{}`; provider misconfiguration",
                request.pod_npub, spec_id
            );
            error!("{}", err_msg);
            send_error(
                deps,
                requester_pubkey,
                message_type,
                "spec_unavailable",
                &err_msg,
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

    // Re-check ownership and existence after re-locking: cleanup may have run
    // since the snapshot.
    let extended = {
        let mut lock = deps.workloads.lock().await;
        match lock.get_mut(&vmid) {
            Some(w) if w.owner_npub == requester_pubkey => {
                w.expires_at = w.expires_at.saturating_add(extension_secs);
                let extended = w.expires_at;
                // The consumer already paid for this extension; a restart must
                // not roll it back to the old expiry.
                persist_workloads(&lock, &config.workload_state_path);
                Some(extended)
            }
            _ => None,
        }
    };
    let Some(new_expires_at) = extended else {
        // The token is already spent at the mint, so surface a distinct error
        // the CLI can explain.
        let err_msg = "Pod was cleaned up before topup could be applied; token has been spent";
        error!("{}: vmid={}", err_msg, vmid);
        send_error(deps, requester_pubkey, message_type, "race_lost", err_msg).await?;
        return Ok(());
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

    deps.nostr
        .send_topup_response_private_message(requester_pubkey, response, message_type)
        .await?;

    info!(
        "Topup applied to {}: +{}s (now expires at {})",
        request.pod_npub, extension_secs, new_expires_at
    );

    Ok(())
}
