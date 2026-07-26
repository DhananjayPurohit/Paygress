// Consumer-side Rust SDK: `DiscoveryClient`'s read-only queries plus
// the spawn/topup/status DM round-trips, returning typed `*Outcome`
// enums so embedders don't hand-roll JSON parsing.
//
// The CLI still hand-rolls these flows in
// `src/cli/commands/{spawn,topup,status}.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::discovery::DiscoveryClient;
use crate::nostr::{
    AccessDetailsContent, EncryptedSpawnPodRequest, EncryptedTopUpPodRequest, ErrorResponseContent,
    ProviderInfo, StatusRequestContent, StatusResponseContent, TopUpResponseContent,
};

const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MESSAGE_TYPE: &str = "nip04";

/// Consumer SDK client: `DiscoveryClient` plus typed write-side
/// operations.
pub struct PaygressClient {
    discovery: DiscoveryClient,
    response_timeout_secs: u64,
    message_type: String,
}

impl PaygressClient {
    /// `private_key` is `nsec1…` or hex. Required for spawn / topup
    /// / status; read-only queries would work without one, but a
    /// single constructor saves callers from holding two clients.
    pub async fn new(relays: Vec<String>, private_key: String) -> Result<Self> {
        let discovery = DiscoveryClient::new_with_key(relays, private_key).await?;
        Ok(Self {
            discovery,
            response_timeout_secs: DEFAULT_RESPONSE_TIMEOUT_SECS,
            message_type: DEFAULT_MESSAGE_TYPE.to_string(),
        })
    }

    pub fn with_response_timeout_secs(mut self, secs: u64) -> Self {
        self.response_timeout_secs = secs;
        self
    }

    /// `"nip04"` (default) or `"nip17"`. NIP-17 gift-wrap is
    /// sender-anonymous but supported by fewer relays.
    pub fn with_message_type(mut self, message_type: impl Into<String>) -> Self {
        self.message_type = message_type.into();
        self
    }

    pub fn npub(&self) -> String {
        self.discovery.get_npub()
    }

    pub fn discovery(&self) -> &DiscoveryClient {
        &self.discovery
    }

    pub async fn list_offers(
        &self,
        filter: Option<crate::nostr::ProviderFilter>,
    ) -> Result<Vec<ProviderInfo>> {
        self.discovery.list_providers(filter).await
    }

    pub async fn spawn(&self, provider_npub: &str, request: SpawnRequest) -> Result<SpawnOutcome> {
        let payload = EncryptedSpawnPodRequest {
            cashu_token: request.cashu_token,
            pod_spec_id: request.pod_spec_id,
            pod_image: request.pod_image,
            ssh_username: request.ssh_username,
            ssh_password: request.ssh_password,
            template_slug: None,
            replication: None,
            primary_npub: None,
            workload_id: None,
            volume_encryption: None,
        };
        let json = serde_json::to_string(&payload)?;
        self.send_and_parse(provider_npub, json, parse_spawn_response)
            .await
    }

    pub async fn topup(&self, provider_npub: &str, request: TopupRequest) -> Result<TopupOutcome> {
        let payload = EncryptedTopUpPodRequest {
            pod_npub: request.pod_id,
            cashu_token: request.cashu_token,
        };
        let json = serde_json::to_string(&payload)?;
        self.send_and_parse(provider_npub, json, parse_topup_response)
            .await
    }

    pub async fn status(&self, provider_npub: &str, pod_id: String) -> Result<StatusOutcome> {
        let payload = StatusRequestContent { pod_id };
        let json = serde_json::to_string(&payload)?;
        self.send_and_parse(provider_npub, json, parse_status_response)
            .await
    }

    async fn send_and_parse<T, F>(
        &self,
        provider_npub: &str,
        request_json: String,
        parser: F,
    ) -> Result<T>
    where
        F: FnOnce(&str) -> Result<T>,
    {
        self.discovery
            .nostr()
            .send_encrypted_private_message(provider_npub, request_json, &self.message_type)
            .await
            .context("send DM to provider")?;

        let response = self
            .discovery
            .nostr()
            .wait_for_decrypted_message(provider_npub, self.response_timeout_secs)
            .await
            .context("wait for provider response")?;

        parser(&response.content)
    }
}

/// SDK-facing form of `EncryptedSpawnPodRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub cashu_token: String,
    /// Spec id (`basic`, `standard`, …); the provider's first spec
    /// is used when `None`.
    pub pod_spec_id: Option<String>,
    pub pod_image: String,
    pub ssh_username: String,
    pub ssh_password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopupRequest {
    /// Pod identifier from [`AccessDetailsContent::pod_npub`], e.g.
    /// `container-1234`.
    pub pod_id: String,
    pub cashu_token: String,
}

/// Result of a spawn round-trip. Anything that is neither
/// `AccessDetailsContent` nor `ErrorResponseContent` surfaces as
/// `Other(raw)`, so a provider speaking an evolved schema doesn't
/// break the caller.
#[derive(Debug, Clone)]
pub enum SpawnOutcome {
    Success(AccessDetailsContent),
    Error(ErrorResponseContent),
    Other(String),
}

#[derive(Debug, Clone)]
pub enum TopupOutcome {
    Success(TopUpResponseContent),
    Error(ErrorResponseContent),
    Other(String),
}

#[derive(Debug, Clone)]
pub enum StatusOutcome {
    Success(StatusResponseContent),
    Error(ErrorResponseContent),
    Other(String),
}

/// `Some(err)` only when the JSON carries the discriminating
/// `error_type` + `message` fields and parses cleanly.
fn try_parse_error(content: &str) -> Option<ErrorResponseContent> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    if v.get("error_type").is_none() || v.get("message").is_none() {
        return None;
    }
    serde_json::from_value(v).ok()
}

pub fn parse_spawn_response(content: &str) -> Result<SpawnOutcome> {
    if let Some(err) = try_parse_error(content) {
        return Ok(SpawnOutcome::Error(err));
    }
    if let Ok(details) = serde_json::from_str::<AccessDetailsContent>(content) {
        return Ok(SpawnOutcome::Success(details));
    }
    Ok(SpawnOutcome::Other(content.to_string()))
}

pub fn parse_topup_response(content: &str) -> Result<TopupOutcome> {
    if let Some(err) = try_parse_error(content) {
        return Ok(TopupOutcome::Error(err));
    }
    if let Ok(resp) = serde_json::from_str::<TopUpResponseContent>(content) {
        return Ok(TopupOutcome::Success(resp));
    }
    Ok(TopupOutcome::Other(content.to_string()))
}

pub fn parse_status_response(content: &str) -> Result<StatusOutcome> {
    if let Some(err) = try_parse_error(content) {
        return Ok(StatusOutcome::Error(err));
    }
    if let Ok(resp) = serde_json::from_str::<StatusResponseContent>(content) {
        return Ok(StatusOutcome::Success(resp));
    }
    Ok(StatusOutcome::Other(content.to_string()))
}

// ==================== Lease keep-alive (streaming payment) ====================
//
// Buys the lease in small pre-paid intervals auto-renewed before each
// lapses, rather than pre-paying the whole thing. Max loss on any
// failure is one interval, and failover is just "stop paying npub A,
// start paying B". `decide_tick` holds all the logic as a pure
// function; `LeaseKeepAlive::run` is the I/O driver around it.

/// Produces a fresh Cashu token worth at least `amount_msats` from
/// the consumer's funds. `CdkTokenSource` wires a real cdk wallet;
/// tests stub it.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn mint_token(&self, amount_msats: u64, mint_url: &str) -> Result<String>;
}

/// msats needed to buy `interval_secs` of lease at the provider's rate.
pub fn renewal_amount_msats(interval_secs: u64, rate_msats_per_sec: u64) -> u64 {
    interval_secs.saturating_mul(rate_msats_per_sec)
}

/// Seconds of lease remaining at `now` (0 once expired).
pub fn seconds_remaining(now: u64, expires_at: u64) -> u64 {
    expires_at.saturating_sub(now)
}

/// Renew once the remaining lease falls to/below `interval * frac`.
/// Renewing before expiry absorbs relay + mint latency, so the lease
/// never lapses and the provider's reclaim never fires.
pub fn should_renew(now: u64, expires_at: u64, interval_secs: u64, threshold_frac: f64) -> bool {
    let threshold = (interval_secs as f64 * threshold_frac).max(0.0) as u64;
    seconds_remaining(now, expires_at) <= threshold
}

/// Config for a streaming lease payer.
#[derive(Debug, Clone)]
pub struct KeepAliveConfig {
    /// Provider being paid (npub / hex).
    pub provider_npub: String,
    /// Pod id from the spawn's AccessDetails (`container-<vmid>`).
    pub pod_id: String,
    /// The chosen spec's price, sizing each interval's token.
    pub rate_msats_per_sec: u64,
    /// Mint the renewal tokens are drawn from; must be
    /// provider-whitelisted.
    pub mint_url: String,
    /// Seconds of lease each renewal buys.
    pub interval_secs: u64,
    /// Renew when remaining < interval * this (e.g. 0.4).
    pub renew_threshold_frac: f64,
    /// How often to re-check the clock.
    pub check_period: Duration,
    /// Spend cap in msats; `None` = unlimited. The payer stops
    /// before a renewal would push cumulative spend over it.
    pub budget_msats: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    /// Enough lease left — sleep and re-check.
    Sleep,
    /// Mint a token worth `amount_msats` and top up.
    Renew { amount_msats: u64 },
    /// Renewing would exceed `budget_msats`; stop.
    StopBudget,
}

pub fn decide_tick(
    now: u64,
    expires_at: u64,
    cfg: &KeepAliveConfig,
    spent_msats: u64,
) -> TickAction {
    if !should_renew(now, expires_at, cfg.interval_secs, cfg.renew_threshold_frac) {
        return TickAction::Sleep;
    }
    let amount = renewal_amount_msats(cfg.interval_secs, cfg.rate_msats_per_sec);
    match cfg.budget_msats {
        Some(cap) if spent_msats.saturating_add(amount) > cap => TickAction::StopBudget,
        _ => TickAction::Renew {
            amount_msats: amount,
        },
    }
}

/// Why the keep-alive loop ended.
#[derive(Debug, Clone)]
pub enum KeepAliveExit {
    /// Caller flipped the stop flag.
    Stopped { spent_msats: u64 },
    /// Hit the spend cap.
    BudgetExhausted { spent_msats: u64 },
    /// Provider says the lease is gone (expired / not found / race).
    LeaseGone { reason: String, spent_msats: u64 },
    /// Unrecoverable local error (mint failure past retries, etc.).
    Fatal { reason: String, spent_msats: u64 },
}

const MAX_CONSECUTIVE_ERRS: u32 = 5;

/// Streaming lease payer.
pub struct LeaseKeepAlive<T: TokenSource> {
    cfg: KeepAliveConfig,
    token_source: T,
}

impl<T: TokenSource> LeaseKeepAlive<T> {
    pub fn new(cfg: KeepAliveConfig, token_source: T) -> Self {
        Self { cfg, token_source }
    }

    /// Renew before each interval lapses until `stop` is set, the
    /// budget is exhausted, or the lease is gone.
    /// `initial_expires_at` is the unix-second expiry from the
    /// spawn's AccessDetails.
    pub async fn run(
        &self,
        client: &PaygressClient,
        initial_expires_at: u64,
        stop: Arc<AtomicBool>,
    ) -> KeepAliveExit {
        let mut expires_at = initial_expires_at;
        let mut spent_msats: u64 = 0;
        let mut consecutive_errs = 0u32;

        loop {
            if stop.load(Ordering::Relaxed) {
                return KeepAliveExit::Stopped { spent_msats };
            }
            let now = unix_now();
            match decide_tick(now, expires_at, &self.cfg, spent_msats) {
                TickAction::Sleep => {
                    tokio::time::sleep(self.cfg.check_period).await;
                }
                TickAction::StopBudget => {
                    return KeepAliveExit::BudgetExhausted { spent_msats };
                }
                TickAction::Renew { amount_msats } => {
                    match self.renew_once(client, now, amount_msats).await {
                        RenewStep::Extended(next_expiry) => {
                            consecutive_errs = 0;
                            spent_msats = spent_msats.saturating_add(amount_msats);
                            expires_at = next_expiry;
                        }
                        RenewStep::Gone(reason) => {
                            return KeepAliveExit::LeaseGone {
                                reason,
                                spent_msats,
                            };
                        }
                        RenewStep::Retry(reason) => {
                            consecutive_errs += 1;
                            if consecutive_errs >= MAX_CONSECUTIVE_ERRS {
                                return KeepAliveExit::Fatal {
                                    reason: format!("{reason} ({consecutive_errs}x)"),
                                    spent_msats,
                                };
                            }
                            tokio::time::sleep(self.cfg.check_period).await;
                        }
                    }
                }
            }
        }
    }

    /// One mint-then-topup attempt. Never sleeps or counts errors —
    /// `run` owns the retry budget.
    async fn renew_once(&self, client: &PaygressClient, now: u64, amount_msats: u64) -> RenewStep {
        let token = match self
            .token_source
            .mint_token(amount_msats, &self.cfg.mint_url)
            .await
        {
            Ok(t) => t,
            Err(e) => return RenewStep::Retry(format!("mint failed: {e}")),
        };

        let req = TopupRequest {
            pod_id: self.cfg.pod_id.clone(),
            cashu_token: token,
        };
        match client.topup(&self.cfg.provider_npub, req).await {
            Ok(TopupOutcome::Success(resp)) => RenewStep::Extended(
                parse_rfc3339_unix(&resp.new_expires_at)
                    // Provider extended but the timestamp didn't
                    // parse; advance locally so we don't hot-loop.
                    .unwrap_or_else(|| now.saturating_add(self.cfg.interval_secs)),
            ),
            Ok(TopupOutcome::Error(e)) if is_lease_gone(&e.error_type) => {
                RenewStep::Gone(format!("{}: {}", e.error_type, e.message))
            }
            Ok(TopupOutcome::Error(e)) => {
                RenewStep::Retry(format!("topup errored: {}", e.error_type))
            }
            Ok(TopupOutcome::Other(_)) | Err(_) => RenewStep::Retry("topup failed".to_string()),
        }
    }
}

/// Outcome of a single renewal attempt.
enum RenewStep {
    /// Lease now runs to this unix second.
    Extended(u64),
    /// Recoverable; counts against the retry budget.
    Retry(String),
    /// Terminal — the lease no longer exists.
    Gone(String),
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_rfc3339_unix(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp().max(0) as u64)
}

/// `error_type` strings from the provider's topup handler that mean
/// renewing is pointless.
fn is_lease_gone(error_type: &str) -> bool {
    matches!(
        error_type,
        "lease_expired" | "not_found" | "race_lost" | "not_owner"
    )
}

/// `TokenSource` backed by a cdk wallet, bound to one mint + unit at
/// construction. Assumes a `sat`-unit wallet and rounds msats *up*
/// so the provider never sees a short payment.
pub struct CdkTokenSource {
    wallet: Arc<cdk::wallet::Wallet>,
}

impl CdkTokenSource {
    pub fn new(wallet: Arc<cdk::wallet::Wallet>) -> Self {
        Self { wallet }
    }
}

#[async_trait]
impl TokenSource for CdkTokenSource {
    async fn mint_token(&self, amount_msats: u64, _mint_url: &str) -> Result<String> {
        use cdk::wallet::SendOptions;
        use cdk::Amount;
        let sats = amount_msats.div_ceil(1000).max(1);
        let prepared = self
            .wallet
            .prepare_send(Amount::from(sats), SendOptions::default())
            .await
            .map_err(|e| anyhow::anyhow!("prepare_send {sats} sat: {e}"))?;
        let token = prepared
            .confirm(None)
            .await
            .map_err(|e| anyhow::anyhow!("confirm send: {e}"))?;
        Ok(token.to_string())
    }
}

#[cfg(test)]
mod keepalive_tests {
    use super::*;

    fn cfg(interval: u64, rate: u64, frac: f64, budget: Option<u64>) -> KeepAliveConfig {
        KeepAliveConfig {
            provider_npub: "np".into(),
            pod_id: "container-1".into(),
            rate_msats_per_sec: rate,
            mint_url: "https://mint".into(),
            interval_secs: interval,
            renew_threshold_frac: frac,
            check_period: Duration::from_secs(1),
            budget_msats: budget,
        }
    }

    #[test]
    fn amount_scales_with_interval_and_rate() {
        assert_eq!(renewal_amount_msats(60, 50), 3000);
        assert_eq!(renewal_amount_msats(0, 50), 0);
    }

    #[test]
    fn should_renew_only_within_threshold() {
        // interval 60, frac 0.4 -> threshold 24s.
        assert!(!should_renew(0, 100, 60, 0.4)); // 100s left
        assert!(should_renew(80, 100, 60, 0.4)); // 20s left <= 24
        assert!(should_renew(100, 100, 60, 0.4)); // already expired
    }

    #[test]
    fn decide_sleeps_when_plenty_of_time() {
        assert_eq!(
            decide_tick(0, 1000, &cfg(60, 50, 0.4, None), 0),
            TickAction::Sleep
        );
    }

    #[test]
    fn decide_renews_near_expiry() {
        assert_eq!(
            decide_tick(980, 1000, &cfg(60, 50, 0.4, None), 0),
            TickAction::Renew { amount_msats: 3000 }
        );
    }

    #[test]
    fn decide_stops_when_next_renewal_exceeds_budget() {
        // spent 3000 + next 3000 = 6000 > cap 5000 -> stop.
        assert_eq!(
            decide_tick(980, 1000, &cfg(60, 50, 0.4, Some(5000)), 3000),
            TickAction::StopBudget
        );
    }

    #[test]
    fn decide_allows_renewal_exactly_at_budget() {
        // spent 2000 + 3000 = 5000 == cap -> allowed (strictly-greater stops).
        assert_eq!(
            decide_tick(980, 1000, &cfg(60, 50, 0.4, Some(5000)), 2000),
            TickAction::Renew { amount_msats: 3000 }
        );
    }

    #[test]
    fn rfc3339_parses_to_unix() {
        let ts = parse_rfc3339_unix("2026-04-30T00:00:00Z").unwrap();
        assert!(ts > 1_700_000_000);
        assert_eq!(parse_rfc3339_unix("not-a-date"), None);
    }

    #[test]
    fn lease_gone_matches_provider_error_types() {
        assert!(is_lease_gone("lease_expired"));
        assert!(is_lease_gone("race_lost"));
        assert!(is_lease_gone("not_owner"));
        assert!(!is_lease_gone("insufficient_payment"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_json() -> String {
        serde_json::to_string(&ErrorResponseContent {
            error_type: "token_already_spent".to_string(),
            message: "This Cashu token has already been spent".to_string(),
            details: None,
        })
        .unwrap()
    }

    fn access_json() -> String {
        serde_json::to_string(&AccessDetailsContent {
            pod_npub: "container-42".to_string(),
            node_port: 30042,
            expires_at: "2026-04-30T00:00:00Z".to_string(),
            cpu_millicores: 1000,
            memory_mb: 1024,
            pod_spec_name: "Basic".to_string(),
            pod_spec_description: "1 vCPU, 1GB".to_string(),
            instructions: vec!["ssh -p 30042 root@host".to_string()],
            host_address: "host".to_string(),
            template_ports: Vec::new(),
        })
        .unwrap()
    }

    fn topup_json() -> String {
        serde_json::to_string(&TopUpResponseContent {
            success: true,
            pod_npub: "container-42".to_string(),
            extended_duration_seconds: 3600,
            new_expires_at: "2026-04-30T01:00:00Z".to_string(),
            message: "extended".to_string(),
        })
        .unwrap()
    }

    fn status_json() -> String {
        serde_json::to_string(&StatusResponseContent {
            pod_id: "42".to_string(),
            status: "Running".to_string(),
            expires_at: "2026-04-30T00:00:00Z".to_string(),
            time_remaining_seconds: 3600,
            cpu_millicores: 1000,
            memory_mb: 1024,
            ssh_host: "1.2.3.4".to_string(),
            ssh_port: 30042,
            ssh_username: "root".to_string(),
        })
        .unwrap()
    }

    #[test]
    fn spawn_success_round_trip() {
        let out = parse_spawn_response(&access_json()).unwrap();
        match out {
            SpawnOutcome::Success(d) => assert_eq!(d.pod_npub, "container-42"),
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[test]
    fn spawn_error_routes_to_error_variant() {
        let out = parse_spawn_response(&err_json()).unwrap();
        match out {
            SpawnOutcome::Error(e) => {
                assert_eq!(e.error_type, "token_already_spent");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn spawn_unknown_payload_routes_to_other() {
        let out = parse_spawn_response(r#"{"weird":"future-thing"}"#).unwrap();
        assert!(matches!(out, SpawnOutcome::Other(_)));
    }

    #[test]
    fn topup_success_round_trip() {
        let out = parse_topup_response(&topup_json()).unwrap();
        match out {
            TopupOutcome::Success(r) => assert_eq!(r.extended_duration_seconds, 3600),
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[test]
    fn topup_error_routes_to_error_variant() {
        let out = parse_topup_response(&err_json()).unwrap();
        assert!(matches!(out, TopupOutcome::Error(_)));
    }

    #[test]
    fn status_success_round_trip() {
        let out = parse_status_response(&status_json()).unwrap();
        match out {
            StatusOutcome::Success(s) => assert_eq!(s.pod_id, "42"),
            other => panic!("expected Success, got {:?}", other),
        }
    }

    #[test]
    fn status_error_routes_to_error_variant() {
        let out = parse_status_response(&err_json()).unwrap();
        assert!(matches!(out, StatusOutcome::Error(_)));
    }

    #[test]
    fn error_with_details_parses_fully() {
        let payload = serde_json::json!({
            "error_type": "non_whitelisted_mint",
            "message": "Mint https://attacker.example is not accepted",
            "details": "operator-tunable"
        })
        .to_string();
        match parse_spawn_response(&payload).unwrap() {
            SpawnOutcome::Error(e) => {
                assert_eq!(e.error_type, "non_whitelisted_mint");
                assert_eq!(e.details.as_deref(), Some("operator-tunable"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn malformed_json_does_not_panic() {
        let out = parse_topup_response("definitely not json").unwrap();
        assert!(matches!(out, TopupOutcome::Other(_)));
    }
}
