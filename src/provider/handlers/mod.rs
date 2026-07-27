// Nostr-DM request handlers, one module per request type, plus the pieces
// spawn/topup/status share: the dependency bundle, the error-reply helpers and
// Cashu redemption.

mod spawn;
mod status;
mod topup;

pub(crate) use spawn::handle_spawn_request;
pub(crate) use status::handle_status_request;
pub(crate) use topup::handle_topup_request;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::error;

use crate::cashu::{validate_and_redeem, MintRedeemer, RedeemError};
use crate::compute::ComputeBackend;
use crate::durable_workload::WorkloadStateMachine;
use crate::nostr::NostrRelaySubscriber;
use crate::provider::config::ProviderConfig;
use crate::provider::persistence::WorkloadInfo;
use crate::provider::standby::StandbySlot;
use crate::provider::ProviderStats;

/// Cloning is cheap: every mutable field is behind an `Arc`.
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

/// Reply to the consumer with a structured refusal. Callers log first, at the
/// level that fits the cause.
async fn send_error(
    deps: &HandlerDeps,
    requester_pubkey: &str,
    message_type: &str,
    error_type: &str,
    message: &str,
) -> Result<()> {
    deps.nostr
        .send_error_response(requester_pubkey, error_type, message, None, message_type)
        .await?;
    Ok(())
}

/// 16-character alphanumeric SSH password. `pub(crate)` so the HTTP+ngx_l402
/// interface can reuse it.
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

/// Accepts both `container-<vmid>` (what `AccessDetailsContent` returns) and a
/// bare number.
pub fn parse_pod_npub(pod_npub: &str) -> Option<u32> {
    if let Some(rest) = pod_npub.strip_prefix("container-") {
        rest.parse().ok()
    } else {
        pod_npub.parse().ok()
    }
}

/// The error-type strings are stable so consumers can act on them (retry on
/// `mint_network_error`, give up on `token_already_spent`, ...).
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

/// Redeem `token` and report the amount in msats. Refusal means no container is
/// ever created.
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
            send_error(deps, requester_pubkey, message_type, error_type, &err_msg).await?;
            Ok(None)
        }
    }
}
