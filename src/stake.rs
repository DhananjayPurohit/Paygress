// Stake-weighted "staked" tier, on the JoinMarket fidelity-bond
// pattern: a provider timelocks a Bitcoin UTXO (CLTV) and signs a
// message proving control of the key in its scriptPubKey. An offer
// carrying a verifiable `StakeProof` is eligible for the tier.
//
// The tier is "staked", not "premium": there is no on-chain
// slashing. Consumers see "this provider posted a Bitcoin bond", not
// "this provider is more reliable".
//
// Privacy: publishing a `StakeProof` in a public offer permanently
// links a Bitcoin UTXO to the provider's Nostr identity. Operators
// who don't want that linkage should not stake.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Wire format the marketplace uses to talk about stake. Travels
/// inside `ProviderOfferContent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StakeProof {
    /// Bitcoin UTXO that's locked: `<txid>:<vout>`.
    pub utxo_outpoint: String,

    /// Absolute lock time (Unix seconds); the UTXO cannot be spent
    /// before it. Larger = better stake.
    pub locktime_unix: u64,

    /// Sats locked. Larger = better stake.
    pub sats: u64,

    /// Hex x-only pubkey that signed the canonical message. Must
    /// match the key in the on-chain UTXO's scriptPubKey.
    pub provider_pubkey_hex: String,

    /// Hex Schnorr signature over [`canonical_signing_message`].
    pub signature_hex: String,

    pub version: u8,
}

/// The bytes a provider signs to prove control of the staked UTXO:
/// SHA-256 over NUL-separated
/// `paygress-stake-v1 | provider_npub | utxo_outpoint | locktime_unix | sats`.
/// The separators prevent boundary confusion; reordering any field
/// breaks every existing signature.
///
/// `provider_npub` is bound in so a proof copied verbatim out of a
/// published offer fails verification under the thief's identity.
pub fn canonical_signing_message(
    provider_npub: &str,
    utxo_outpoint: &str,
    locktime_unix: u64,
    sats: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"paygress-stake-v1");
    hasher.update([0u8]);
    hasher.update(provider_npub.as_bytes());
    hasher.update([0u8]);
    hasher.update(utxo_outpoint.as_bytes());
    hasher.update([0u8]);
    hasher.update(locktime_unix.to_le_bytes());
    hasher.update([0u8]);
    hasher.update(sats.to_le_bytes());
    hasher.finalize().into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utxo {
    pub outpoint: String,
    pub script_pubkey_hex: String,
    pub sats: u64,
    /// Spent in a confirmed block.
    pub spent: bool,
}

/// Bitcoin chain data source. Production wires this to two
/// independent Esplora endpoints and requires agreement.
#[async_trait::async_trait]
pub trait BlockSource: Send + Sync {
    async fn fetch_utxo(&self, outpoint: &str) -> Result<Option<Utxo>, StakeError>;
    async fn current_unix_time(&self) -> Result<u64, StakeError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StakeStatus {
    Valid {
        effective_sats: u64,
        locktime_unix: u64,
    },
    /// The UTXO no longer exists or has been spent.
    Spent,
    /// The locktime elapsed; the bond locks nothing any more.
    Unlocked,
    BadSignature,
    /// The proof's pubkey isn't in the on-chain scriptPubKey.
    PubkeyMismatch,
    /// The block source could not confirm the UTXO (e.g. Esplora
    /// outage). Callers must not treat this as `Valid`.
    Unverified(String),
}

/// Malformed-input failures. "Couldn't reach the chain" is
/// `StakeStatus::Unverified` instead, so `verify_stake` never
/// returns one of these for the network path.
#[derive(Debug, thiserror::Error)]
pub enum StakeError {
    #[error("malformed UTXO outpoint: {0}")]
    InvalidOutpoint(String),
    #[error("malformed pubkey: {0}")]
    InvalidPubkey(String),
    #[error("malformed signature: {0}")]
    InvalidSignature(String),
    #[error("block source error: {0}")]
    BlockSource(String),
}

/// Verify a stake proof against the chain: signature, then pubkey
/// presence in the UTXO's scriptPubKey, then unspent, then locktime
/// still in the future.
pub async fn verify_stake(
    proof: &StakeProof,
    provider_npub: &str,
    source: &dyn BlockSource,
) -> Result<StakeStatus, StakeError> {
    use cdk::secp256k1::{schnorr::Signature, Message, Secp256k1, XOnlyPublicKey};

    let pubkey_bytes = hex::decode(&proof.provider_pubkey_hex)
        .map_err(|e| StakeError::InvalidPubkey(e.to_string()))?;
    let xonly = XOnlyPublicKey::from_slice(&pubkey_bytes)
        .map_err(|e| StakeError::InvalidPubkey(e.to_string()))?;
    let sig_bytes = hex::decode(&proof.signature_hex)
        .map_err(|e| StakeError::InvalidSignature(e.to_string()))?;
    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|e| StakeError::InvalidSignature(e.to_string()))?;

    let digest = canonical_signing_message(
        provider_npub,
        &proof.utxo_outpoint,
        proof.locktime_unix,
        proof.sats,
    );
    let msg = Message::from_digest(digest);
    let secp = Secp256k1::verification_only();
    if secp.verify_schnorr(&sig, &msg, &xonly).is_err() {
        return Ok(StakeStatus::BadSignature);
    }

    let utxo = match source.fetch_utxo(&proof.utxo_outpoint).await {
        Ok(Some(u)) => u,
        Ok(None) => return Ok(StakeStatus::Spent),
        Err(e) => return Ok(StakeStatus::Unverified(e.to_string())),
    };

    if utxo.spent {
        return Ok(StakeStatus::Spent);
    }

    // Substring match works for P2TR, where the script embeds the
    // raw x-only pubkey. P2WPKH stores HASH160(pubkey) instead, and
    // we don't compute that (no `ripemd` dependency), so P2WPKH
    // operators get PubkeyMismatch and must stake to a P2TR address.
    let pk_hex = proof.provider_pubkey_hex.to_lowercase();
    if !utxo.script_pubkey_hex.to_lowercase().contains(&pk_hex) {
        return Ok(StakeStatus::PubkeyMismatch);
    }

    let now = match source.current_unix_time().await {
        Ok(t) => t,
        Err(e) => return Ok(StakeStatus::Unverified(e.to_string())),
    };
    if proof.locktime_unix <= now {
        return Ok(StakeStatus::Unlocked);
    }

    Ok(StakeStatus::Valid {
        effective_sats: proof.sats.min(utxo.sats),
        locktime_unix: proof.locktime_unix,
    })
}

/// Ranking score `log(sats × locked_seconds)`; `0.0` when not
/// staked or already unlocked. Ordering only — operators pick their
/// own stake economics.
pub fn stake_rank(sats: u64, locktime_unix: u64, now: u64) -> f64 {
    if sats == 0 || locktime_unix <= now {
        return 0.0;
    }
    let locked_secs = locktime_unix - now;
    let product = (sats as f64) * (locked_secs as f64);
    if product <= 1.0 {
        0.0
    } else {
        product.ln()
    }
}

/// SSRF guard for the operator-supplied Esplora endpoint the
/// verifier makes requests to. Rejects non-`https://` schemes,
/// userinfo, fragments, and loopback / link-local / RFC-1918 hosts.
pub fn validate_esplora_url(url: &str) -> Result<(), &'static str> {
    if !url.starts_with("https://") {
        return Err("only https:// is allowed");
    }
    if url.contains('@') {
        return Err("userinfo in URL is not allowed");
    }
    if url.contains('#') {
        return Err("URL fragment is not allowed");
    }
    let after_scheme = &url["https://".len()..];
    // An IPv6 literal is bracketed (`[::1]:port/path`), so the whole
    // bracketed range is the host — otherwise the `:` inside the
    // address would terminate the parse early.
    let host_end = if after_scheme.starts_with('[') {
        match after_scheme.find(']') {
            Some(idx) => idx + 1,
            None => return Err("malformed bracketed IPv6 host"),
        }
    } else {
        after_scheme
            .find(['/', ':', '?'])
            .unwrap_or(after_scheme.len())
    };
    let host = &after_scheme[..host_end].to_lowercase();
    if host.is_empty() {
        return Err("empty host");
    }
    const PRIVATE_HOST_PREFIXES: &[&str] = &[
        "localhost",
        "127.",
        "169.254.",
        "10.",
        "192.168.",
        "::1",
        "[::1]",
        "[fe80",
        "[fc",
        "[fd",
    ];
    for bad in PRIVATE_HOST_PREFIXES {
        if host.starts_with(bad) {
            return Err("private/loopback hosts are not allowed");
        }
    }
    // 172.16.0.0/12 needs a numeric range check, not a prefix.
    if let Some(rest) = host.strip_prefix("172.") {
        if let Some(second_octet) = rest.split('.').next() {
            if let Ok(n) = second_octet.parse::<u8>() {
                if (16..=31).contains(&n) {
                    return Err("private/loopback hosts are not allowed");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::secp256k1::{Keypair, Message, Secp256k1, SecretKey};

    fn keypair() -> (SecretKey, String) {
        let secp = Secp256k1::new();
        let sk_bytes = [42u8; 32];
        let sk = SecretKey::from_slice(&sk_bytes).unwrap();
        let kp = Keypair::from_secret_key(&secp, &sk);
        let (xonly, _parity) = kp.x_only_public_key();
        let xonly_hex = hex::encode(xonly.serialize());
        (sk, xonly_hex)
    }

    fn sign_proof(
        sk: &SecretKey,
        provider_npub: &str,
        outpoint: &str,
        locktime: u64,
        sats: u64,
    ) -> String {
        let secp = Secp256k1::new();
        let kp = Keypair::from_secret_key(&secp, sk);
        let digest = canonical_signing_message(provider_npub, outpoint, locktime, sats);
        let msg = Message::from_digest(digest);
        let sig = secp.sign_schnorr(&msg, &kp);
        hex::encode(sig.as_ref())
    }

    struct MockChain {
        utxo: Option<Utxo>,
        now: u64,
    }

    #[async_trait::async_trait]
    impl BlockSource for MockChain {
        async fn fetch_utxo(&self, _outpoint: &str) -> Result<Option<Utxo>, StakeError> {
            Ok(self.utxo.clone())
        }
        async fn current_unix_time(&self) -> Result<u64, StakeError> {
            Ok(self.now)
        }
    }

    #[test]
    fn canonical_message_is_deterministic_and_field_sensitive() {
        let a = canonical_signing_message("npub1abc", "txid:0", 100, 1000);
        let b = canonical_signing_message("npub1abc", "txid:0", 100, 1000);
        assert_eq!(a, b, "same inputs must hash identically");

        let c = canonical_signing_message("npub1abc", "txid:0", 101, 1000);
        let d = canonical_signing_message("npub1abc", "txid:1", 100, 1000);
        let e = canonical_signing_message("npub1xyz", "txid:0", 100, 1000);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_ne!(a, e, "npub binding must affect the digest");
    }

    #[tokio::test]
    async fn happy_path_returns_valid() {
        let (sk, pk_hex) = keypair();
        let npub = "npub1provider";
        let outpoint = "abcd:0";
        let locktime = 2_000_000_000;
        let sats = 100_000;
        let signature_hex = sign_proof(&sk, npub, outpoint, locktime, sats);

        let proof = StakeProof {
            utxo_outpoint: outpoint.to_string(),
            locktime_unix: locktime,
            sats,
            provider_pubkey_hex: pk_hex.clone(),
            signature_hex,
            version: 1,
        };

        let chain = MockChain {
            utxo: Some(Utxo {
                outpoint: outpoint.to_string(),
                script_pubkey_hex: format!("5120{}", pk_hex), // P2TR-shaped
                sats,
                spent: false,
            }),
            now: 1_700_000_000,
        };

        let status = verify_stake(&proof, npub, &chain).await.unwrap();
        assert!(
            matches!(status, StakeStatus::Valid { effective_sats, locktime_unix }
                if effective_sats == sats && locktime_unix == locktime),
            "got {:?}",
            status
        );
    }

    #[tokio::test]
    async fn cross_npub_replay_fails_signature_check() {
        let (sk, pk_hex) = keypair();
        let outpoint = "abcd:0";
        let locktime = 2_000_000_000;
        let sats = 100_000;
        // Signed for npub A; replayed against npub B.
        let signature_hex = sign_proof(&sk, "npub1original", outpoint, locktime, sats);

        let proof = StakeProof {
            utxo_outpoint: outpoint.to_string(),
            locktime_unix: locktime,
            sats,
            provider_pubkey_hex: pk_hex.clone(),
            signature_hex,
            version: 1,
        };
        let chain = MockChain {
            utxo: Some(Utxo {
                outpoint: outpoint.to_string(),
                script_pubkey_hex: format!("5120{}", pk_hex),
                sats,
                spent: false,
            }),
            now: 1_700_000_000,
        };

        let status = verify_stake(&proof, "npub1impostor", &chain).await.unwrap();
        assert_eq!(status, StakeStatus::BadSignature);
    }

    #[tokio::test]
    async fn spent_utxo_is_rejected() {
        let (sk, pk_hex) = keypair();
        let npub = "npub1provider";
        let outpoint = "abcd:0";
        let locktime = 2_000_000_000;
        let sats = 100_000;
        let signature_hex = sign_proof(&sk, npub, outpoint, locktime, sats);

        let proof = StakeProof {
            utxo_outpoint: outpoint.to_string(),
            locktime_unix: locktime,
            sats,
            provider_pubkey_hex: pk_hex.clone(),
            signature_hex,
            version: 1,
        };
        let chain = MockChain {
            utxo: Some(Utxo {
                outpoint: outpoint.to_string(),
                script_pubkey_hex: format!("5120{}", pk_hex),
                sats,
                spent: true,
            }),
            now: 1_700_000_000,
        };

        let status = verify_stake(&proof, npub, &chain).await.unwrap();
        assert_eq!(status, StakeStatus::Spent);
    }

    #[tokio::test]
    async fn past_locktime_is_unlocked() {
        let (sk, pk_hex) = keypair();
        let npub = "npub1provider";
        let outpoint = "abcd:0";
        let locktime = 1_000_000_000; // already in the past
        let sats = 100_000;
        let signature_hex = sign_proof(&sk, npub, outpoint, locktime, sats);

        let proof = StakeProof {
            utxo_outpoint: outpoint.to_string(),
            locktime_unix: locktime,
            sats,
            provider_pubkey_hex: pk_hex.clone(),
            signature_hex,
            version: 1,
        };
        let chain = MockChain {
            utxo: Some(Utxo {
                outpoint: outpoint.to_string(),
                script_pubkey_hex: format!("5120{}", pk_hex),
                sats,
                spent: false,
            }),
            now: 1_700_000_000,
        };

        let status = verify_stake(&proof, npub, &chain).await.unwrap();
        assert_eq!(status, StakeStatus::Unlocked);
    }

    #[tokio::test]
    async fn pubkey_not_in_script_is_mismatch() {
        let (sk, pk_hex) = keypair();
        let npub = "npub1provider";
        let outpoint = "abcd:0";
        let locktime = 2_000_000_000;
        let sats = 100_000;
        let signature_hex = sign_proof(&sk, npub, outpoint, locktime, sats);

        let proof = StakeProof {
            utxo_outpoint: outpoint.to_string(),
            locktime_unix: locktime,
            sats,
            provider_pubkey_hex: pk_hex.clone(),
            signature_hex,
            version: 1,
        };
        // Script hex doesn't contain the proof's pubkey.
        let chain = MockChain {
            utxo: Some(Utxo {
                outpoint: outpoint.to_string(),
                script_pubkey_hex:
                    "5120deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                        .to_string(),
                sats,
                spent: false,
            }),
            now: 1_700_000_000,
        };

        let status = verify_stake(&proof, npub, &chain).await.unwrap();
        assert_eq!(status, StakeStatus::PubkeyMismatch);
    }

    #[test]
    fn rank_orders_higher_when_either_factor_is_higher() {
        let now = 1_000;
        let r1 = stake_rank(100_000, 1_000 + 30 * 86400, now); // 30d
        let r2 = stake_rank(100_000, 1_000 + 90 * 86400, now); // 90d
        let r3 = stake_rank(1_000_000, 1_000 + 30 * 86400, now); // 10x sats
        assert!(r2 > r1, "longer lock should rank higher");
        assert!(r3 > r1, "more sats should rank higher");
    }

    #[test]
    fn rank_is_zero_when_unlocked_or_no_sats() {
        let now = 2_000;
        assert_eq!(stake_rank(100_000, 1_000, now), 0.0);
        assert_eq!(stake_rank(0, now + 86400, now), 0.0);
    }

    #[test]
    fn validate_esplora_url_accepts_https_public() {
        assert!(validate_esplora_url("https://blockstream.info/api").is_ok());
        assert!(validate_esplora_url("https://mempool.space/api").is_ok());
    }

    #[test]
    fn validate_esplora_url_rejects_http_and_userinfo() {
        assert!(validate_esplora_url("http://example.com").is_err());
        assert!(validate_esplora_url("https://user:pass@example.com").is_err());
        assert!(validate_esplora_url("https://example.com/#frag").is_err());
    }

    #[test]
    fn validate_esplora_url_rejects_loopback_and_rfc1918() {
        for bad in [
            "https://localhost/",
            "https://127.0.0.1:8332",
            "https://10.0.0.1",
            "https://192.168.1.5",
            "https://169.254.1.1",
            "https://172.16.5.5",
            "https://172.31.255.255",
            "https://[::1]",
        ] {
            assert!(validate_esplora_url(bad).is_err(), "must reject {}", bad);
        }
    }

    #[test]
    fn validate_esplora_url_accepts_172_outside_rfc1918() {
        // 172.0–15 and 172.32–255 are public.
        assert!(validate_esplora_url("https://172.15.1.1/api").is_ok());
        assert!(validate_esplora_url("https://172.32.0.1/api").is_ok());
    }
}
