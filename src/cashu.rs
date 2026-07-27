// Cashu token utilities.
//
// Two redemption paths share one CDK SQLite wallet file: the Nostr-DM path
// (`validate_and_redeem`, swaps at the mint) and the HTTP path (ngx_l402 has
// already swapped, so `extract_token_value` only decodes). ngx_l402 opens the
// same file via CASHU_WALLET_MNEMONIC and sweeps it to Lightning.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use cdk::cdk_database::{Error as DbError, WalletDatabase};
use cdk::mint_url::MintUrl;
use cdk::nuts::{CurrencyUnit, Token};
use cdk::wallet::{ReceiveOptions, Wallet};
use cdk::Amount;
use tokio::sync::Mutex;

const MSAT_PER_SAT: u64 = 1000;

const REDB_MAGIC: &[u8] = b"redb\x1a\x0a\xa9\x0d\x0a";

/// Reject a wallet file left over from the pre-SQLite (redb) storage backend.
/// The SQLite driver would otherwise report only "file is not a database".
pub fn ensure_not_legacy_redb_wallet(db_path: &Path) -> anyhow::Result<()> {
    use std::io::Read;

    // Missing file is the normal first-run case: the driver creates it.
    let Ok(mut file) = std::fs::File::open(db_path) else {
        return Ok(());
    };
    let mut header = [0u8; REDB_MAGIC.len()];
    if file.read_exact(&mut header).is_err() || header != REDB_MAGIC {
        return Ok(());
    }

    anyhow::bail!(
        "the cashu wallet at {} is a redb database; this version stores the \
         wallet in SQLite. Point `cashu_wallet_db_path` at a new file, e.g. {}",
        db_path.display(),
        db_path.with_extension("sqlite").display(),
    )
}

/// Errors from the Nostr-DM redemption path, mapped onto specific Nostr error
/// responses by the caller.
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

/// Swaps an encoded token at the mint and returns the redeemed amount in
/// **msats**. Implementors do NOT re-check the whitelist; `validate_and_redeem`
/// does that first.
#[async_trait]
pub trait MintRedeemer: Send + Sync {
    async fn redeem(&self, token_str: &str) -> Result<u64, RedeemError>;
}

/// Mint URL of a token, without redeeming it.
pub fn token_mint_url(token_str: &str) -> Result<String, RedeemError> {
    let token = Token::from_str(token_str).map_err(|e| RedeemError::InvalidToken(e.to_string()))?;
    token
        .mint_url()
        .map(|u| u.to_string())
        .map_err(|e| RedeemError::InvalidToken(format!("token has no mint URL: {}", e)))
}

/// The whitelist check happens **before** any mint contact, so a token pointed
/// at an attacker-controlled mint never causes a network call.
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

/// One lazily-created `cdk` wallet per `(mint_url, unit)`, all sharing a single
/// SQLite `WalletDatabase`. `seed` drives cdk's deterministic blinding-factor
/// derivation; see [`resolve_wallet_seed`].
pub struct CdkRedeemer {
    localstore: Arc<dyn WalletDatabase<DbError> + Send + Sync>,
    seed: [u8; 64],
    wallets: Mutex<HashMap<(String, CurrencyUnit), Arc<Wallet>>>,
}

impl CdkRedeemer {
    pub fn new(localstore: Arc<dyn WalletDatabase<DbError> + Send + Sync>, seed: [u8; 64]) -> Self {
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

        let amount = match wallet.receive(token_str, ReceiveOptions::default()).await {
            Ok(a) => a,
            Err(e) => {
                let is_keyset_err =
                    matches!(e, cdk::Error::UnknownKeySet | cdk::Error::IncorrectMint)
                        || e.to_string().to_lowercase().contains("keyset");

                if is_keyset_err {
                    // The mint rotated keys. `refresh_keysets` bypasses the
                    // metadata cache; `get_mint_keysets` would serve the same
                    // stale cache that produced this error.
                    let _ = wallet.refresh_keysets().await;
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
        // cdk has no distinct Network variant, so sniff the message.
        other => match other.to_string() {
            s if s.contains("HTTP") || s.contains("network") || s.contains("connection") => {
                RedeemError::Network(s)
            }
            s => RedeemError::MintError(s),
        },
    }
}

/// NUT-13 seed derivation with an empty passphrase. Byte-identical to
/// ngx_l402's `derive_wallet_seed`, so both processes open the *same* wallet.
pub fn derive_wallet_seed(mnemonic: &str) -> Result<[u8; 64], String> {
    let parsed = bip39::Mnemonic::parse(mnemonic.trim())
        .map_err(|e| format!("invalid BIP39 mnemonic: {}", e))?;
    Ok(parsed.to_seed_normalized(""))
}

/// Deterministic BIP39 mnemonic from the provider's Nostr private key, so the
/// wallet follows the provider identity. `bootstrap` writes this same phrase
/// into ngx_l402's `CASHU_WALLET_MNEMONIC`.
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

/// Prefers `CASHU_WALLET_MNEMONIC` (what ngx_l402 uses for the shared wallet),
/// falling back to the Nostr-key-derived mnemonic.
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
mod legacy_wallet_tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "paygress-redb-test-{}-{}",
            std::process::id(),
            name
        ));
        p
    }

    #[test]
    fn missing_file_is_fine() {
        let p = temp_path("absent.sqlite");
        let _ = std::fs::remove_file(&p);
        assert!(ensure_not_legacy_redb_wallet(&p).is_ok());
    }

    #[test]
    fn redb_wallet_is_rejected_with_guidance() {
        let p = temp_path("legacy.redb");
        std::fs::write(&p, b"redb\x1a\x0a\xa9\x0d\x0a\x03\x00\x00\x00").unwrap();

        let err = ensure_not_legacy_redb_wallet(&p)
            .expect_err("a redb wallet must not be opened as SQLite")
            .to_string();
        assert!(err.contains("redb database"), "got: {}", err);
        assert!(err.contains("legacy.sqlite"), "got: {}", err);

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sqlite_and_short_files_pass_through() {
        let sqlite = temp_path("real.sqlite");
        std::fs::write(&sqlite, b"SQLite format 3\x00rest").unwrap();
        assert!(ensure_not_legacy_redb_wallet(&sqlite).is_ok());

        let tiny = temp_path("tiny.sqlite");
        std::fs::write(&tiny, b"ab").unwrap();
        assert!(ensure_not_legacy_redb_wallet(&tiny).is_ok());

        let _ = std::fs::remove_file(&sqlite);
        let _ = std::fs::remove_file(&tiny);
    }
}

#[cfg(test)]
mod wallet_seed_tests {
    use super::*;

    // The same golden BIP39 vector pinned in ngx_l402_core.
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
        assert!(derive_wallet_seed(&a).is_ok());
        assert_ne!(a, mnemonic_from_nostr_key("other-key").unwrap());
    }
}

/// Split one Cashu token into `n` tokens via a wallet at `db_path`. The first
/// `n-1` shards get `received / n`; the last absorbs the remainder so the
/// totals reconcile exactly. `db_path` is left behind for the caller to remove.
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

    // Bail before touching the mint so a bad `n` leaves the token unspent.
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
    let db: Arc<dyn WalletDatabase<DbError> + Send + Sync> = Arc::new(db);

    // Ephemeral wallet, so a random seed is fine.
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

/// Mint a fresh token worth `amount_sats` via an ephemeral wallet at `db_path`.
///
/// Only works against a testnut-style mint whose fake Lightning backend
/// auto-pays quotes. A real mint leaves the quote `Unpaid` until the bolt11
/// invoice is paid out-of-band, so the poll below times out.
pub async fn mint_fresh_token(
    mint_url: &str,
    amount_sats: u64,
    db_path: &Path,
) -> Result<String, anyhow::Error> {
    use cdk::amount::SplitTarget;
    use cdk::nuts::{MintQuoteState, PaymentMethod};
    use rand::RngCore;

    if amount_sats == 0 {
        anyhow::bail!("cannot mint a zero-value token");
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
    let db: Arc<dyn WalletDatabase<DbError> + Send + Sync> = Arc::new(db);

    let mut seed = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut seed);

    let wallet = Wallet::new(mint_url, CurrencyUnit::Sat, db, seed, None)
        .map_err(|e| anyhow::anyhow!("wallet construction failed: {}", e))?;

    let quote = wallet
        .mint_quote(
            PaymentMethod::BOLT11,
            Some(Amount::from(amount_sats)),
            None,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("mint quote request to {} failed: {}", mint_url, e))?;

    const POLL_ATTEMPTS: u32 = 15;
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    let mut paid = false;
    for _ in 0..POLL_ATTEMPTS {
        let quote_status = wallet
            .check_mint_quote_status(&quote.id)
            .await
            .map_err(|e| anyhow::anyhow!("mint quote status check failed: {}", e))?;
        if quote_status.state == MintQuoteState::Paid {
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

    // Wrap the proofs directly: `prepare_send` would re-swap at the mint and
    // can shave fees off the face value.
    let parsed_url = MintUrl::from_str(mint_url)
        .map_err(|e| anyhow::anyhow!("invalid mint URL {}: {}", mint_url, e))?;
    let token = Token::new(parsed_url, proofs, None, CurrencyUnit::Sat);
    Ok(token.to_string())
}

/// Face-value parser for the HTTP+ngx_l402 path: sums the decoded token's proof
/// amounts, in msats, **without contacting the mint**.
///
/// ngx_l402 has already redeemed the token at the nginx layer before the
/// request reaches the axum backend; calling the mint again would double-spend.
/// The Nostr-DM path has no such upstream and uses `validate_and_redeem`.
pub async fn extract_token_value(token_str: &str) -> anyhow::Result<u64> {
    let token = Token::from_str(token_str)
        .map_err(|e| anyhow::anyhow!("Failed to decode Cashu token: {}", e))?;

    // `Token::proofs()` needs keyset metadata; `value()` does not, and the sum
    // of proof amounts is all this needs.
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
