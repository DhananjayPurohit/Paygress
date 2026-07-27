// Streaming lease payment: buy the lease in small pre-paid intervals,
// auto-renewed before each lapses, rather than pre-paying the whole thing. Max
// loss on any failure is one interval, and failover is just "stop paying npub
// A, start paying B". `decide_tick` is the pure decision function;
// `LeaseKeepAlive::run` is the I/O driver around it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::{PaygressClient, TopupOutcome, TopupRequest};

/// Produces a fresh Cashu token worth at least `amount_msats` from the
/// consumer's funds. `CdkTokenSource` wires a real cdk wallet; tests stub it.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn mint_token(&self, amount_msats: u64, mint_url: &str) -> Result<String>;
}

pub fn renewal_amount_msats(interval_secs: u64, rate_msats_per_sec: u64) -> u64 {
    interval_secs.saturating_mul(rate_msats_per_sec)
}

pub fn seconds_remaining(now: u64, expires_at: u64) -> u64 {
    expires_at.saturating_sub(now)
}

/// Renew once the remaining lease falls to/below `interval * frac`. Renewing
/// before expiry absorbs relay + mint latency, so the lease never lapses and
/// the provider's reclaim never fires.
pub fn should_renew(now: u64, expires_at: u64, interval_secs: u64, threshold_frac: f64) -> bool {
    let threshold = (interval_secs as f64 * threshold_frac).max(0.0) as u64;
    seconds_remaining(now, expires_at) <= threshold
}

#[derive(Debug, Clone)]
pub struct KeepAliveConfig {
    pub provider_npub: String,
    /// Pod id from the spawn's AccessDetails (`container-<vmid>`).
    pub pod_id: String,
    pub rate_msats_per_sec: u64,
    /// Must be one of the provider's whitelisted mints.
    pub mint_url: String,
    /// Seconds of lease each renewal buys.
    pub interval_secs: u64,
    /// Renew when remaining < interval * this (e.g. 0.4).
    pub renew_threshold_frac: f64,
    pub check_period: Duration,
    /// `None` = unlimited. The payer stops before a renewal would push
    /// cumulative spend over it.
    pub budget_msats: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    Sleep,
    Renew { amount_msats: u64 },
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

#[derive(Debug, Clone)]
pub enum KeepAliveExit {
    Stopped {
        spent_msats: u64,
    },
    BudgetExhausted {
        spent_msats: u64,
    },
    /// Provider says the lease is gone (expired / not found / race).
    LeaseGone {
        reason: String,
        spent_msats: u64,
    },
    /// Unrecoverable local error (mint failure past retries, etc.).
    Fatal {
        reason: String,
        spent_msats: u64,
    },
}

const MAX_CONSECUTIVE_ERRS: u32 = 5;

pub struct LeaseKeepAlive<T: TokenSource> {
    cfg: KeepAliveConfig,
    token_source: T,
}

impl<T: TokenSource> LeaseKeepAlive<T> {
    pub fn new(cfg: KeepAliveConfig, token_source: T) -> Self {
        Self { cfg, token_source }
    }

    /// Renew before each interval lapses until `stop` is set, the budget is
    /// exhausted, or the lease is gone. `initial_expires_at` is the unix-second
    /// expiry from the spawn's AccessDetails.
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

    /// One mint-then-topup attempt. Never sleeps or counts errors — `run` owns
    /// the retry budget.
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
                    // Provider extended but the timestamp didn't parse; advance
                    // locally so we don't hot-loop.
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

/// `error_type` strings from the provider's topup handler that mean renewing is
/// pointless.
fn is_lease_gone(error_type: &str) -> bool {
    matches!(
        error_type,
        "lease_expired" | "not_found" | "race_lost" | "not_owner"
    )
}

/// `TokenSource` backed by a cdk wallet. Assumes a `sat`-unit wallet and rounds
/// msats *up* so the provider never sees a short payment.
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
