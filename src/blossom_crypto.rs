// Client-side encryption for Blossom-stored blobs: blobs are encrypted
// before hashing, so a third-party server only ever sees ciphertext.
//
// Wire format: `nonce || XChaCha20-Poly1305 ciphertext`, where the
// 24-byte nonce is fresh per encryption and prepended so `decrypt`
// recovers it without out-of-band coordination. The AEAD tag the
// cipher appends means a wrong key fails verification instead of
// silently returning garbage.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// 32-byte symmetric key, per-blob or per-lease.
pub type EncryptionKey = [u8; 32];

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

/// Returns `nonce || ciphertext`. The fresh per-call nonce means two
/// encryptions of the same plaintext differ, so observers can't tell that
/// two checkpoints carry identical state.
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

/// Inverse of [`encrypt_for_upload`].
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

/// Blossom indexes by this value, so callers must hash the
/// post-encryption bytes — never the plaintext — when building auth
/// events or `/<hash>` URLs.
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
