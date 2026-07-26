// Cashu Token Utilities
//
// Two redemption paths, one shared SQLite wallet, one sweep.
//
// Path A — Nostr-DM (src/provider.rs):
//   `validate_and_redeem` / `MintRedeemer` / `CdkRedeemer` — swaps the
//   token at the mint via NUT-03, defeating single- and cross-provider
//   replay. Proofs land in the shared CDK SQLite wallet
//   (`cashu_wallet_db_path`, default `/var/lib/paygress/cashu-wallet.sqlite`).
//
// Path B — HTTP + ngx_l402 (src/provider_http.rs):
//   ngx_l402 verifies the Cashu token at the nginx layer before forwarding
//   the request. The backend calls `extract_token_value` to read the
//   already-redeemed face value without a second mint call. ngx_l402 uses
//   the SAME SQLite file (`cashu-wallet.sqlite`) via CASHU_WALLET_MNEMONIC,
//   so both paths write into one shared wallet.
//
// Lightning sweep:
//   ngx_l402 sweeps the shared SQLite wallet to Lightning periodically
//   (CASHU_REDEMPTION_INTERVAL_SECS / LNURL_ADDRESS), draining ecash
//   accumulated from BOTH Nostr-DM and HTTP-path payments.

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

// Singleton CDK wallet database. Used by the HTTP+ngx_l402 path
// (`initialize_cashu` / `extract_token_value`) and feature-gated behind
// `kubernetes` for the legacy K8s pipeline.
static CASHU_DB: OnceLock<Arc<cdk_sqlite::WalletSqliteDatabase>> = OnceLock::new();

pub async fn initialize_cashu(db_path: &str) -> Result<(), String> {
    match cdk_sqlite::WalletSqliteDatabase::new(std::path::PathBuf::from(db_path)).await {
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

/// Extract the mint URL from a Cashu token string without redeeming it.
///
/// Used by the consumer CLI to validate the token's mint against the
/// provider's whitelist **before** sending the token, so a mismatch
/// fails fast with a clear error instead of a round-trip rejection.
pub fn token_mint_url(token_str: &str) -> Result<String, RedeemError> {
    let token = Token::from_str(token_str).map_err(|e| RedeemError::InvalidToken(e.to_string()))?;
    token
        .mint_url()
        .map(|u| u.to_string())
        .map_err(|e| RedeemError::InvalidToken(format!("token has no mint URL: {}", e)))
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
/// first use. All wallets share a single `WalletDatabase` (a SQLite file)
/// so proofs, keysets, and quotes for every mint live in one place.
///
/// The `seed` is used by cdk for deterministic blinding-factor
/// derivation. See `resolve_wallet_seed` for the production derivation
/// (BIP39 / NUT-13 standard); tests can construct `CdkRedeemer` directly
/// with any 64-byte seed.
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
    /// Swap the encoded token at the mint and return the received amount
    /// in millisatoshis. Proofs are stored in the per-mint CDK wallet.
    async fn redeem(&self, token_str: &str) -> Result<u64, RedeemError> {
        let token =
            Token::from_str(token_str).map_err(|e| RedeemError::InvalidToken(e.to_string()))?;
        let mint_url = token
            .mint_url()
            .map_err(|e| RedeemError::InvalidToken(e.to_string()))?;
        let unit = token.unit().unwrap_or(CurrencyUnit::Sat);

        let wallet = self.wallet_for(&mint_url, unit.clone()).await?;

        // Try redemption with cached keysets first (zero extra latency).
        // On a keyset-related error (mint rotated its keys), refresh once
        // from the mint and retry — adds one extra round-trip only when
        // rotation actually occurred rather than on every call.
        let amount = match wallet.receive(token_str, ReceiveOptions::default()).await {
            Ok(a) => a,
            Err(e) => {
                let is_keyset_err =
                    matches!(e, cdk::Error::UnknownKeySet | cdk::Error::IncorrectMint)
                        || e.to_string().to_lowercase().contains("keyset");

                if is_keyset_err {
                    // Refresh keysets from the live mint, then retry once.
                    let _ = wallet.get_mint_keysets().await;
                    wallet
                        .receive(token_str, ReceiveOptions::default())
                        .await
                        .map_err(map_cdk_error)?
                } else {
                    return Err(map_cdk_error(e));
                }
            }
        };
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
///
/// DEPRECATED: this is the legacy, non-standard derivation. New wallets use the
/// Cashu/NUT-13 standard via [`resolve_wallet_seed`] / [`derive_wallet_seed`].
/// Kept only so an already-deployed legacy wallet remains openable.
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

/// Derive the deterministic 64-byte Cashu wallet seed from a BIP39 mnemonic,
/// per NUT-13, using an empty passphrase. This is the Cashu standard and is
/// byte-identical to ngx_l402's `ngx_l402_core::derive_wallet_seed`, so both
/// processes open the *same* shared wallet from the same mnemonic.
pub fn derive_wallet_seed(mnemonic: &str) -> Result<[u8; 64], String> {
    let parsed = bip39::Mnemonic::parse(mnemonic.trim())
        .map_err(|e| format!("invalid BIP39 mnemonic: {}", e))?;
    Ok(parsed.to_seed_normalized(""))
}

/// Derive a *deterministic, standard* BIP39 mnemonic from the provider's Nostr
/// private key. Domain-separated SHA-256 of the key gives 128 bits of entropy,
/// which BIP39 turns into a 12-word phrase. This preserves the "wallet follows
/// the provider's Nostr identity" model while producing a real, restorable
/// mnemonic. `bootstrap` writes this same phrase into ngx_l402's
/// `CASHU_WALLET_MNEMONIC` so both sides converge on one wallet.
pub fn mnemonic_from_nostr_key(nostr_private_key: &str) -> Result<String, String> {
    use cdk::secp256k1::hashes::{sha256, Hash};
    let h = sha256::Hash::hash(
        format!("paygress-cashu-wallet-bip39-v1:{}", nostr_private_key).as_bytes(),
    );
    let entropy = &h.to_byte_array()[..16]; // 128 bits -> 12 words
    bip39::Mnemonic::from_entropy(entropy)
        .map(|m| m.to_string())
        .map_err(|e| format!("mnemonic from entropy: {}", e))
}

/// Resolve the 64-byte wallet seed for the redeemer.
///
/// Prefers an explicit `CASHU_WALLET_MNEMONIC` (the canonical Cashu/NUT-13
/// source, and the value ngx_l402 uses for the shared wallet). Falls back to a
/// deterministic BIP39 mnemonic derived from the Nostr key so a provider keeps a
/// stable wallet across restarts even without explicit configuration.
pub fn resolve_wallet_seed(nostr_private_key: &str) -> Result<[u8; 64], String> {
    if let Ok(env_mnemonic) = std::env::var("CASHU_WALLET_MNEMONIC") {
        let m = env_mnemonic.trim();
        if !m.is_empty() {
            return derive_wallet_seed(m);
        }
    }
    let mnemonic = mnemonic_from_nostr_key(nostr_private_key)?;
    derive_wallet_seed(&mnemonic)
}

#[cfg(test)]
mod wallet_seed_tests {
    use super::*;

    // Canonical BIP39 vector (all-zero 128-bit entropy), empty passphrase —
    // identical to the golden vector pinned in ngx_l402_core, proving both
    // projects derive the same seed from the same mnemonic.
    const VECTOR_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const VECTOR_SEED_HEX: &str = "5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc19a5ac40b389cd370d086206dec8aa6c43daea6690f20ad3d8d48b2d2ce9e38e4";

    #[test]
    fn derives_canonical_bip39_seed() {
        let seed = derive_wallet_seed(VECTOR_MNEMONIC).expect("valid mnemonic");
        assert_eq!(hex::encode(seed), VECTOR_SEED_HEX);
    }

    #[test]
    fn invalid_mnemonic_is_rejected() {
        assert!(derive_wallet_seed("").is_err());
        assert!(derive_wallet_seed("not a real mnemonic at all").is_err());
    }

    #[test]
    fn nostr_mnemonic_is_deterministic_and_valid() {
        let a = mnemonic_from_nostr_key("nsec-example-key").unwrap();
        let b = mnemonic_from_nostr_key("nsec-example-key").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.split_whitespace().count(), 12);
        // Must round-trip through the standard derivation.
        assert!(derive_wallet_seed(&a).is_ok());
        // Different identities -> different wallets.
        assert_ne!(a, mnemonic_from_nostr_key("other-key").unwrap());
    }
}

/// Split one Cashu token into N tokens of approximately equal face
/// value. Used by `paygress batch --split-token ... --shards N` so
/// users don't have to hand-mint N tokens before fanning out.
///
/// Flow: open an ephemeral wallet at `db_path`, swap the input token
/// in (mint round-trip), then prepare+send N tokens whose face
/// values sum to the received amount. The first `N-1` shards each
/// get `received / N` (integer floor); the final shard absorbs any
/// remainder so the totals reconcile exactly.
///
/// Caveats:
///   - Exercised end-to-end against `testnut.cashu.space` only.
///     The bundled cdk 0.14 wallet supports v2 (66-char) keyset
///     IDs in code, so mainnet mints (e.g. `mint.minibits.cash`)
///     are expected to work for receive+split, but that path has
///     not been verified against a live mainnet mint yet.
///   - The wallet's localstore at `db_path` is left in place after
///     the split; callers wanting truly ephemeral semantics should
///     remove it. The batch coordinator does.
pub async fn split_token_into_n(
    token_str: &str,
    n: usize,
    db_path: &Path,
) -> Result<Vec<String>, anyhow::Error> {
    use cdk::wallet::SendOptions;
    use cdk::Amount;
    use rand::RngCore;

    if n == 0 {
        anyhow::bail!("cannot split into 0 shards");
    }

    let token =
        Token::from_str(token_str).map_err(|e| anyhow::anyhow!("invalid input token: {}", e))?;
    let mint_url = token
        .mint_url()
        .map_err(|e| anyhow::anyhow!("token has no mint URL: {}", e))?;
    let unit = token.unit().unwrap_or(CurrencyUnit::Sat);

    // Face-value pre-check: bail before touching the mint if N is
    // mathematically infeasible. Keeps the error fast and the token
    // unspent on bad input.
    let face_value: u64 = token
        .value()
        .map_err(|e| anyhow::anyhow!("failed to compute token value: {}", e))?
        .into();
    if face_value == 0 {
        anyhow::bail!("input token has zero face value");
    }
    if (face_value as usize) < n {
        anyhow::bail!(
            "input token face value ({} {:?}) cannot be split into {} shards (minimum 1 per shard)",
            face_value,
            unit,
            n
        );
    }

    let db = cdk_sqlite::WalletSqliteDatabase::new(db_path.to_path_buf())
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to open ephemeral wallet db at {}: {}",
                db_path.display(),
                e
            )
        })?;
    let db: Arc<dyn WalletDatabase<Err = DbError> + Send + Sync> = Arc::new(db);

    // Random seed — the wallet is ephemeral, so deterministic
    // derivation buys us nothing. cdk's Wallet::new requires [u8; 64].
    let mut seed = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut seed);

    let wallet = Wallet::new(&mint_url.to_string(), unit, db, seed, None)
        .map_err(|e| anyhow::anyhow!("wallet construction failed: {}", e))?;

    let received = wallet
        .receive(token_str, ReceiveOptions::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to receive input token: {}", e))?;
    let received_value: u64 = received.into();
    if (received_value as usize) < n {
        anyhow::bail!(
            "received amount ({}) less than shard count ({}); mint may have charged fees",
            received_value,
            n
        );
    }

    let per_shard_floor = received_value / n as u64;
    let final_shard = received_value - per_shard_floor * (n as u64 - 1);

    let mut tokens: Vec<String> = Vec::with_capacity(n);
    for i in 0..n {
        let amount = if i + 1 == n {
            final_shard
        } else {
            per_shard_floor
        };
        let prepared = wallet
            .prepare_send(Amount::from(amount), SendOptions::default())
            .await
            .map_err(|e| anyhow::anyhow!("prepare_send shard {}/{}: {}", i + 1, n, e))?;
        let token = prepared
            .confirm(None)
            .await
            .map_err(|e| anyhow::anyhow!("confirm send shard {}/{}: {}", i + 1, n, e))?;
        tokens.push(token.to_string());
    }

    Ok(tokens)
}

/// Mint a fresh token worth `amount_sats` from `mint_url`.
///
/// Built for CI funding against a testnut-style mint whose fake
/// Lightning backend auto-pays quotes — the poll loop below settles in
/// one or two rounds there. Against a real mint the quote stays
/// `Unpaid` until someone pays the bolt11 invoice out-of-band, so the
/// timeout fires; that path is deliberately unsupported here (an
/// unattended CI job has no way to pay an invoice).
///
/// Same ephemeral-wallet convention as `split_token_into_n`: throwaway
/// redb at `db_path`, random seed, nothing persisted worth keeping.
/// The minted proofs are wrapped directly in a `Token` rather than
/// round-tripped through `prepare_send` — that would re-swap at the
/// mint for no benefit and can shave fees off the face value.
pub async fn mint_fresh_token(
    mint_url: &str,
    amount_sats: u64,
    db_path: &Path,
) -> Result<String, anyhow::Error> {
    use cdk::amount::SplitTarget;
    use cdk::nuts::MintQuoteState;
    use rand::RngCore;

    if amount_sats == 0 {
        anyhow::bail!("cannot mint a zero-value token");
    }

    let db = cdk_redb::wallet::WalletRedbDatabase::new(db_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to open ephemeral wallet db at {}: {}",
            db_path.display(),
            e
        )
    })?;
    let db: Arc<dyn WalletDatabase<Err = DbError> + Send + Sync> = Arc::new(db);

    let mut seed = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut seed);

    let wallet = Wallet::new(mint_url, CurrencyUnit::Sat, db, seed, None)
        .map_err(|e| anyhow::anyhow!("wallet construction failed: {}", e))?;

    let quote = wallet
        .mint_quote(Amount::from(amount_sats), None)
        .await
        .map_err(|e| anyhow::anyhow!("mint quote request to {} failed: {}", mint_url, e))?;

    // Poll until the mint reports the quote paid. 15 × 2s covers a
    // slow testnut round-trip with margin; a real mint never gets
    // there (see doc comment).
    const POLL_ATTEMPTS: u32 = 15;
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    let mut paid = false;
    for _ in 0..POLL_ATTEMPTS {
        let state = wallet
            .mint_quote_state(&quote.id)
            .await
            .map_err(|e| anyhow::anyhow!("mint quote status check failed: {}", e))?;
        if state.state == MintQuoteState::Paid {
            paid = true;
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    if !paid {
        anyhow::bail!(
            "mint quote at {} not paid after {}s — is this a testnut-style mint \
             that auto-pays quotes? Real mints need the invoice paid out-of-band.",
            mint_url,
            POLL_ATTEMPTS as u64 * POLL_INTERVAL.as_secs(),
        );
    }

    let proofs = wallet
        .mint(&quote.id, SplitTarget::default(), None)
        .await
        .map_err(|e| anyhow::anyhow!("minting proofs failed: {}", e))?;

    let parsed_url = MintUrl::from_str(mint_url)
        .map_err(|e| anyhow::anyhow!("invalid mint URL {}: {}", mint_url, e))?;
    let token = Token::new(parsed_url, proofs, None, CurrencyUnit::Sat);
    Ok(token.to_string())
}

/// Face-value parser for the HTTP+ngx_l402 path.
///
/// Returns the sum of `proof.amount` from a decoded token in msats,
/// **without contacting the mint**. This is intentionally safe here
/// because ngx_l402 has *already* redeemed the token at the nginx layer
/// (NUT-03 swap + replay guard) before forwarding the request to the
/// axum backend. Calling the mint a second time would double-spend.
///
/// The Nostr-DM path uses `validate_and_redeem` instead (which does
/// contact the mint), because there is no upstream nginx layer to
/// pre-redeem for it.
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
