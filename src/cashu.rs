// Cashu Token Utilities
//
// Provides:
// - `validate_and_redeem` / `MintRedeemer` / `CdkRedeemer`: the canonical
//   redemption path used by the Nostr-DM provider (`src/provider.rs`).
//   This actually swaps proofs at the mint via NUT-03, defeating
//   single- and cross-provider replay.
// - `extract_token_value`: legacy face-value parser. Still used by the
//   K8s + ngx_l402 + HTTP path (sidecar_service / pod_provisioning /
//   interfaces::http_l402). Those callers rely on ngx_l402 to perform
//   redemption at the nginx layer. Unit 7 of the 12-month plan
//   feature-gates that whole path behind the `kubernetes` Cargo
//   feature; once gated out of the default build, this function can be
//   removed.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use cdk::cdk_database::{Error as DbError, WalletDatabase};
use cdk::mint_url::MintUrl;
use cdk::nuts::{CurrencyUnit, Token};
use cdk::wallet::{ReceiveOptions, Wallet};
use cdk::Amount;
use tokio::sync::Mutex;

const MSAT_PER_SAT: u64 = 1000;

// Legacy database singleton kept so `initialize_cashu` continues to work
// for callers that haven't migrated to `CdkRedeemer` yet.
static CASHU_DB: OnceLock<Arc<cdk_redb::wallet::WalletRedbDatabase>> = OnceLock::new();

pub async fn initialize_cashu(db_path: &str) -> Result<(), String> {
    match cdk_redb::wallet::WalletRedbDatabase::new(Path::new(db_path)) {
        Ok(db) => {
            tracing::debug!("Cashu database initialized at: {}", db_path);
            let _ = CASHU_DB.set(Arc::new(db));
            Ok(())
        }
        Err(e) => {
            let error = format!("Failed to create Cashu database: {:?}", e);
            tracing::error!("{}", error);
            Err(error)
        }
    }
}

/// Errors from the Nostr-DM redemption path. Preserved as a structured
/// enum (rather than `anyhow::Error`) so callers can map specific cdk
/// failure modes onto specific Nostr error responses without string
/// matching.
#[derive(Debug, thiserror::Error)]
pub enum RedeemError {
    #[error("token could not be parsed: {0}")]
    InvalidToken(String),

    #[error("token's mint URL `{mint_url}` is not in the provider's whitelist")]
    NonWhitelistedMint { mint_url: String },

    #[error("token has already been spent at the mint")]
    AlreadySpent,

    #[error("token is in pending state at the mint; retry later")]
    Pending,

    #[error("network error talking to mint: {0}")]
    Network(String),

    #[error("token unit `{0}` is not supported by this provider")]
    UnsupportedUnit(String),

    #[error("mint rejected redemption: {0}")]
    MintError(String),
}

/// The redemption surface that `validate_and_redeem` calls into.
///
/// Implementors are responsible for swapping the encoded token at the
/// mint and returning the redeemed amount in **msats**. They do NOT
/// re-check the whitelist; that happens in `validate_and_redeem`.
#[async_trait]
pub trait MintRedeemer: Send + Sync {
    async fn redeem(&self, token_str: &str) -> Result<u64, RedeemError>;
}

/// Parse and validate the token, enforce the per-provider whitelist,
/// then delegate to the redeemer. The whitelist check happens **before**
/// any mint contact so a malicious token pointed at an attacker-
/// controlled mint never causes a network call from the provider.
pub async fn validate_and_redeem<R: MintRedeemer + ?Sized>(
    redeemer: &R,
    whitelisted_mints: &[String],
    token_str: &str,
) -> Result<u64, RedeemError> {
    let token = Token::from_str(token_str).map_err(|e| RedeemError::InvalidToken(e.to_string()))?;

    let token_mint = token
        .mint_url()
        .map_err(|e| RedeemError::InvalidToken(format!("token has no mint URL: {}", e)))?;

    let normalized_whitelist: Vec<MintUrl> = whitelisted_mints
        .iter()
        .filter_map(|s| MintUrl::from_str(s).ok())
        .collect();

    if !normalized_whitelist.iter().any(|m| m == &token_mint) {
        return Err(RedeemError::NonWhitelistedMint {
            mint_url: token_mint.to_string(),
        });
    }

    redeemer.redeem(token_str).await
}

/// Production redeemer backed by `cdk::wallet::Wallet`.
///
/// Maintains one wallet per `(mint_url, unit)` pair, lazily created on
/// first use. All wallets share a single `WalletDatabase` (a redb file)
/// so proofs, keysets, and quotes for every mint live in one place.
///
/// The `seed` is used by cdk for deterministic blinding-factor
/// derivation. See `derive_seed_from_nostr_key` for the production
/// derivation; tests can construct `CdkRedeemer` directly with any
/// 32-byte seed.
pub struct CdkRedeemer {
    localstore: Arc<dyn WalletDatabase<Err = DbError> + Send + Sync>,
    seed: [u8; 64],
    wallets: Mutex<HashMap<(String, CurrencyUnit), Arc<Wallet>>>,
}

impl CdkRedeemer {
    pub fn new(
        localstore: Arc<dyn WalletDatabase<Err = DbError> + Send + Sync>,
        seed: [u8; 64],
    ) -> Self {
        Self {
            localstore,
            seed,
            wallets: Mutex::new(HashMap::new()),
        }
    }

    async fn wallet_for(
        &self,
        mint_url: &MintUrl,
        unit: CurrencyUnit,
    ) -> Result<Arc<Wallet>, RedeemError> {
        let key = (mint_url.to_string(), unit.clone());
        let mut wallets = self.wallets.lock().await;
        if let Some(w) = wallets.get(&key) {
            return Ok(w.clone());
        }
        let wallet = Wallet::new(
            &mint_url.to_string(),
            unit,
            self.localstore.clone(),
            self.seed,
            None,
        )
        .map_err(|e| RedeemError::MintError(format!("wallet construction failed: {}", e)))?;
        let wallet = Arc::new(wallet);
        wallets.insert(key, wallet.clone());
        Ok(wallet)
    }
}

#[async_trait]
impl MintRedeemer for CdkRedeemer {
    async fn redeem(&self, token_str: &str) -> Result<u64, RedeemError> {
        let token =
            Token::from_str(token_str).map_err(|e| RedeemError::InvalidToken(e.to_string()))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| RedeemError::InvalidToken(e.to_string()))?;
        let unit = token.unit().unwrap_or(CurrencyUnit::Sat);

        let wallet = self.wallet_for(&mint_url, unit.clone()).await?;
        let amount = wallet
            .receive(token_str, ReceiveOptions::default())
            .await
            .map_err(map_cdk_error)?;
        let amount_u64: u64 = amount.into();

        match unit {
            CurrencyUnit::Sat => Ok(amount_u64
                .checked_mul(MSAT_PER_SAT)
                .ok_or_else(|| RedeemError::MintError("amount overflow".to_string()))?),
            CurrencyUnit::Msat => Ok(amount_u64),
            other => Err(RedeemError::UnsupportedUnit(format!("{:?}", other))),
        }
    }
}

fn map_cdk_error(e: cdk::Error) -> RedeemError {
    use cdk::Error as E;
    match e {
        E::TokenAlreadySpent => RedeemError::AlreadySpent,
        E::TokenPending => RedeemError::Pending,
        E::IncorrectMint => RedeemError::MintError(
            "wallet's bound mint URL does not match token's (should not happen for per-mint pool)"
                .to_string(),
        ),
        E::UnsupportedUnit => RedeemError::UnsupportedUnit("rejected by mint".to_string()),
        // cdk doesn't surface a distinct Network variant; treat
        // serialization/HTTP errors uniformly as Network so callers can
        // signal "retry later" to the consumer.
        other => match other.to_string() {
            s if s.contains("HTTP") || s.contains("network") || s.contains("connection") => {
                RedeemError::Network(s)
            }
            s => RedeemError::MintError(s),
        },
    }
}

/// Derive a 64-byte wallet seed from the provider's Nostr private key.
/// cdk's `Wallet::new` requires `[u8; 64]` (BIP-39-style seed length).
/// We hash twice with distinct domain separators so the two halves
/// are independent.
pub fn derive_seed_from_nostr_key(nostr_private_key: &str) -> [u8; 64] {
    use cdk::secp256k1::hashes::{sha256, Hash};
    let h1 =
        sha256::Hash::hash(format!("paygress-cashu-wallet-v1:a:{}", nostr_private_key).as_bytes());
    let h2 =
        sha256::Hash::hash(format!("paygress-cashu-wallet-v1:b:{}", nostr_private_key).as_bytes());
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&h1.to_byte_array());
    out[32..].copy_from_slice(&h2.to_byte_array());
    out
}

/// **Legacy face-value parser.** Returns the sum of `proof.amount` from
/// a decoded token in msats, **without contacting the mint**. This is
/// vulnerable to single- and cross-provider replay.
///
/// Used today by the K8s + ngx_l402 + HTTP path
/// (`src/sidecar_service.rs`, `src/pod_provisioning.rs`,
/// `src/interfaces/http_l402.rs`), where ngx_l402 performs Cashu
/// redemption at the nginx layer before forwarding the request. The
/// Nostr-DM path no longer calls this — it uses
/// `validate_and_redeem` instead.
///
/// Will be removed once Unit 7 feature-gates the K8s pipeline behind
/// the `kubernetes` Cargo feature.
pub async fn extract_token_value(token_str: &str) -> anyhow::Result<u64> {
    let token = Token::from_str(token_str)
        .map_err(|e| anyhow::anyhow!("Failed to decode Cashu token: {}", e))?;

    // cdk 0.14 made `Token::proofs(&keysets)` require keyset metadata,
    // but `Token::value()` still works without — it's just the sum of
    // proof amounts. That's exactly what this legacy function does.
    let amount: Amount = token
        .value()
        .map_err(|e| anyhow::anyhow!("Failed to compute token value: {}", e))?;
    let total_amount: u64 = amount.into();
    if total_amount == 0 {
        return Err(anyhow::anyhow!("Token has no proofs"));
    }

    let total_amount_msats: u64 = match token.unit().unwrap_or(CurrencyUnit::Sat) {
        CurrencyUnit::Sat => total_amount * MSAT_PER_SAT,
        CurrencyUnit::Msat => total_amount,
        unit => return Err(anyhow::anyhow!("Unsupported token unit: {:?}", unit)),
    };

    Ok(total_amount_msats)
}
