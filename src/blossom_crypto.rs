// Client-side encryption for Blossom-stored blobs (Unit 6 of the
// 12-month plan,
// docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//
// Blossom servers are content-addressed by SHA-256 of the *upload*
// bytes. When a workload's checkpoint sits on a third-party Blossom
// server, we don't trust the operator. We encrypt the blob
// **before** computing the hash so the server (and anyone who
// downloads by hash) sees only ciphertext.
//
// Algorithm: XChaCha20-Poly1305. 32-byte key, 24-byte nonce.
// Nonces are randomly generated per-encryption (non-deterministic
// — the proptest in tests/blossom.rs pins this) and prepended to
// the ciphertext on the wire so `decrypt` can recover them
// without out-of-band coordination.
//
// Wire format on the Blossom server: `nonce || aead-ciphertext`.
// AEAD authentication tag is appended by the chacha20poly1305 crate
// itself, so a wrong key fails AEAD verification rather than
// silently returning garbage.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// 32-byte symmetric key for XChaCha20-Poly1305. Per-blob (or
/// per-lease for checkpoint chains).
pub type EncryptionKey = [u8; 32];

/// Errors from the encryption layer. Distinct from anyhow so
/// callers can map AEAD failures (wrong key, tampered ciphertext)
/// to specific user-facing messages.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("ciphertext too short to contain a nonce")]
    Truncated,
    #[error("AEAD authentication failed (wrong key or tampered ciphertext)")]
    AuthenticationFailed,
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),
}

const NONCE_LEN: usize = 24;

/// Encrypt `plaintext` with `key`. The output is `nonce || ciphertext`,
/// where `ciphertext` includes the AEAD authentication tag.
///
/// Each call generates a fresh random nonce, so encrypting the same
/// plaintext twice produces different bytes — observers cannot
/// detect that two checkpoints carry identical state.
pub fn encrypt_for_upload(plaintext: &[u8], key: &EncryptionKey) -> Result<Vec<u8>, CryptoError> {
    use rand::RngCore;
    let cipher = XChaCha20Poly1305::new(key.into());

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt the wire format produced by [`encrypt_for_upload`]. AEAD
/// failures (wrong key, tampered bytes, truncated input) surface as
/// `AuthenticationFailed` so callers don't have to reason about
/// chacha20poly1305 internals.
pub fn decrypt_after_download(wire: &[u8], key: &EncryptionKey) -> Result<Vec<u8>, CryptoError> {
    if wire.len() < NONCE_LEN {
        return Err(CryptoError::Truncated);
    }
    let (nonce_bytes, ciphertext) = wire.split_at(NONCE_LEN);
    let nonce = XNonce::from_slice(nonce_bytes);

    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| CryptoError::AuthenticationFailed)
}

/// Compute the SHA-256 hash of the wire-format ciphertext. Blossom
/// servers index by this value, so callers must hash the
/// post-encryption bytes (not the plaintext) when constructing
/// auth events or `/<hash>` URLs.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> EncryptionKey {
        [0x42; 32]
    }

    #[test]
    fn round_trip_recovers_plaintext() {
        let pt = b"hello world".to_vec();
        let ct = encrypt_for_upload(&pt, &key()).unwrap();
        let recovered = decrypt_after_download(&ct, &key()).unwrap();
        assert_eq!(recovered, pt);
    }

    #[test]
    fn empty_blob_round_trips() {
        let pt: Vec<u8> = vec![];
        let ct = encrypt_for_upload(&pt, &key()).unwrap();
        let recovered = decrypt_after_download(&ct, &key()).unwrap();
        assert_eq!(recovered, pt);
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let pt = b"secret".to_vec();
        let ct = encrypt_for_upload(&pt, &key()).unwrap();
        let mut wrong = key();
        wrong[0] ^= 0xff;
        let err = decrypt_after_download(&ct, &wrong).unwrap_err();
        assert!(matches!(err, CryptoError::AuthenticationFailed));
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        let pt = b"secret".to_vec();
        let mut ct = encrypt_for_upload(&pt, &key()).unwrap();
        // Flip a bit in the AEAD payload (after the nonce).
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        let err = decrypt_after_download(&ct, &key()).unwrap_err();
        assert!(matches!(err, CryptoError::AuthenticationFailed));
    }

    #[test]
    fn truncated_wire_format_is_rejected_distinctly() {
        let too_short = vec![0u8; NONCE_LEN - 1];
        let err = decrypt_after_download(&too_short, &key()).unwrap_err();
        assert!(matches!(err, CryptoError::Truncated));
    }

    #[test]
    fn encryption_is_non_deterministic() {
        let pt = b"reproducibility-leak".to_vec();
        let a = encrypt_for_upload(&pt, &key()).unwrap();
        let b = encrypt_for_upload(&pt, &key()).unwrap();
        assert_ne!(a, b, "two encryptions of the same plaintext must differ");
    }

    #[test]
    fn sha256_hex_is_64_chars() {
        let h = sha256_hex(b"abc");
        assert_eq!(h.len(), 64);
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
