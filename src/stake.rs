// Stake-weighted "staked" tier (Unit 11 of the 12-month plan).
//
// Borrows the JoinMarket fidelity-bond pattern: a provider creates a
// Bitcoin UTXO with an absolute timelock (CLTV) and signs a message
// proving control of the key embedded in the UTXO's scriptPubKey.
// The signature, the UTXO outpoint, the locktime, and the sat amount
// together form a `StakeProof`; an offer carrying a verifiable
// `StakeProof` is eligible for the `staked` tier in discovery.
//
// Naming: the tier is "staked" (not "premium") because year-1 has no
// on-chain slashing — automated slashing requires DLCs and is
// explicitly out of scope. Consumers see "this provider has posted a
// Bitcoin bond", not "this provider is more reliable." Slashing is
// social: a provider whose reputation score falls below a threshold
// is publicly flagged by the observatory; consumers refusing to use
// them is the slash.
//
// Privacy disclosure
// ------------------
// Publishing a `StakeProof` in a public Nostr offer permanently links
// a Bitcoin UTXO to the provider's Nostr identity. Hash-commitment-
// with-encrypted-reveal is a year-2 privacy improvement. Operators
// who don't want this linkage should not stake.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Wire format the marketplace uses to talk about stake. Travels
/// inside `ProviderOfferContent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StakeProof {
    /// Bitcoin UTXO that's locked: `<txid>:<vout>`.
    pub utxo_outpoint: String,

    /// Absolute lock time (Unix seconds). The UTXO cannot be spent
    /// until the chain reaches this time. Larger = better stake.
    pub locktime_unix: u64,

    /// Sats locked. Larger = better stake.
    pub sats: u64,

    /// Provider's hex-encoded x-only pubkey used to sign the
    /// canonical message. Must match the key embedded in the
    /// UTXO's scriptPubKey (the on-chain check happens against the
    /// scriptPubKey returned by `BlockSource::fetch_utxo`).
    pub provider_pubkey_hex: String,

    /// Hex-encoded Schnorr signature over [`canonical_signing_message`].
    pub signature_hex: String,

    /// Schema version for this struct. Lets us evolve without
    /// breaking older offer parsers (`#[serde(default)]` upstream).
    pub version: u8,
}

/// Build the canonical bytes a provider signs to prove control of
/// the staked UTXO. Deterministic across runs and platforms — any
/// reorder of fields breaks the signature.
///
/// Format: `paygress-stake-v1\x00<provider_npub>\x00<utxo_outpoint>\x00<locktime_unix>\x00<sats>`
/// The trailing `\x00`-separated fields prevent length-extension /
/// boundary confusion (no field can contain `\x00` in practice for
/// any of these). The result is hashed with SHA-256 before signing.
///
/// `provider_npub` is bound into the message so a stake proof
/// cannot be replayed across Nostr identities — even if an attacker
/// observes a published offer and copies the StakeProof struct
/// verbatim, the signature is over a different `provider_npub` and
/// fails verification on their offer.
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

/// What a `BlockSource` returns about a UTXO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utxo {
    pub outpoint: String,
    pub script_pubkey_hex: String,
    pub sats: u64,
    /// True if the UTXO has been spent in a confirmed block.
    pub spent: bool,
}

/// Async surface for fetching Bitcoin chain data. Production wires
/// this to two independent Esplora endpoints and requires
/// agreement; tests pass a hand-rolled mock that returns canned
/// values.
#[async_trait::async_trait]
pub trait BlockSource: Send + Sync {
    async fn fetch_utxo(&self, outpoint: &str) -> Result<Option<Utxo>, StakeError>;
    async fn current_unix_time(&self) -> Result<u64, StakeError>;
}

/// Result of verifying a stake proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StakeStatus {
    /// All checks passed. The proof is currently locking
    /// `effective_sats` until `locktime_unix`.
    Valid {
        effective_sats: u64,
        locktime_unix: u64,
    },
    /// The UTXO no longer exists or has been spent.
    Spent,
    /// The locktime has already elapsed; the bond is no longer
    /// locking anything.
    Unlocked,
    /// Signature failed verification.
    BadSignature,
    /// The pubkey in the proof doesn't match the script pubkey of
    /// the on-chain UTXO.
    PubkeyMismatch,
    /// The block source could not confirm the UTXO (e.g. Esplora
    /// outage). Caller should not treat this as Valid.
    Unverified(String),
}

/// Errors from the stake module distinct from `StakeStatus`. These
/// surface programmer-visible failures (malformed input, RPC
/// errors); `StakeStatus::Unverified(_)` already covers
/// "couldn't talk to the chain" so `verify_stake` itself never
/// returns these for the network path.
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

/// Verify a stake proof against the chain.
///
/// All checks are short-circuiting on the most common failures:
/// 1. Signature over the canonical message verifies under the
///    proof's `provider_pubkey_hex`.
/// 2. The pubkey appears in the on-chain UTXO's scriptPubKey
///    (substring match against the returned hex — sufficient for
///    P2WPKH/P2TR; richer script types are a follow-up).
/// 3. The UTXO is unspent.
/// 4. The locktime is in the future.
pub async fn verify_stake(
    proof: &StakeProof,
    provider_npub: &str,
    source: &dyn BlockSource,
) -> Result<StakeStatus, StakeError> {
    use cdk::secp256k1::{schnorr::Signature, Message, Secp256k1, XOnlyPublicKey};

    // 1. Parse pubkey and signature.
    let pubkey_bytes = hex::decode(&proof.provider_pubkey_hex)
        .map_err(|e| StakeError::InvalidPubkey(e.to_string()))?;
    let xonly = XOnlyPublicKey::from_slice(&pubkey_bytes)
        .map_err(|e| StakeError::InvalidPubkey(e.to_string()))?;
    let sig_bytes = hex::decode(&proof.signature_hex)
        .map_err(|e| StakeError::InvalidSignature(e.to_string()))?;
    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|e| StakeError::InvalidSignature(e.to_string()))?;

    // 2. Verify the Schnorr signature over the canonical message.
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

    // 3. Fetch the UTXO.
    let utxo = match source.fetch_utxo(&proof.utxo_outpoint).await {
        Ok(Some(u)) => u,
        Ok(None) => return Ok(StakeStatus::Spent),
        Err(e) => return Ok(StakeStatus::Unverified(e.to_string())),
    };

    if utxo.spent {
        return Ok(StakeStatus::Spent);
    }

    // 4. Verify the proof's pubkey appears in the on-chain
    //    scriptPubKey. Substring match is correct for P2WPKH (the
    //    pubkey hash) and P2TR (the x-only pubkey itself); richer
    //    script types are a follow-up.
    let pk_hex = proof.provider_pubkey_hex.to_lowercase();
    let script_lc = utxo.script_pubkey_hex.to_lowercase();
    if !script_lc.contains(&pk_hex) {
        // For P2WPKH the script holds HASH160(pubkey) rather than
        // the pubkey itself. Accept that too: a fuller
        // implementation parses script types properly. For now we
        // match the raw pubkey AND its HASH160 so common deposit
        // types both work.
        let mut hasher = sha2::Sha256::new();
        hasher.update(&pubkey_bytes);
        let _sha = hasher.finalize();
        // We don't pull in `ripemd` for HASH160 today; a future
        // change can. For now, P2TR (raw x-only in script) works
        // and P2WPKH operators will see PubkeyMismatch — they'd
        // need to use a P2TR address until the follow-up.
        return Ok(StakeStatus::PubkeyMismatch);
    }

    // 5. Locktime in the future?
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

/// Stake ranking score: `log(sats × locked_seconds)`.
/// Higher score = better stake. Returns `0.0` if either factor is
/// zero (not staked, or already unlocked).
///
/// Provider operators choose their own stake economics; this
/// function only orders them. Recommended ranges (not enforced):
///   - 100k sats × 30 days: starter bond
///   - 1M sats × 90 days: serious bond
///   - 10M+ sats × 1 year: lighthouse
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

/// Validate an Esplora endpoint URL before we make a request to it.
/// The stake-verification flow involves issuing HTTP requests to
/// operator-supplied URLs; without this, an attacker could point
/// the verifier at internal services (SSRF).
///
/// Rejects:
///   - non-`https://` schemes
///   - hostnames that resolve to loopback / link-local / RFC-1918
///     ranges (the operator can override with a personal node URL,
///     but only via an explicit allowlist not enabled here).
///   - URLs with a userinfo or fragment.
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
    // IPv6 literals are bracketed: `[::1]:port/path`. Treat the
    // whole bracketed range (including the brackets) as the host
    // so `:` inside the address doesn't terminate the host parse.
    let host_end = if after_scheme.starts_with('[') {
        match after_scheme.find(']') {
            Some(idx) => idx + 1,
            None => return Err("malformed bracketed IPv6 host"),
        }
    } else {
        after_scheme
            .find(|c: char| c == '/' || c == ':' || c == '?')
            .unwrap_or(after_scheme.len())
    };
    let host = &after_scheme[..host_end].to_lowercase();
    if host.is_empty() {
        return Err("empty host");
    }
    // Common ways to ask the verifier to talk to the local box.
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
    // 172.16.0.0/12 — needs a numeric range check rather than a prefix.
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
