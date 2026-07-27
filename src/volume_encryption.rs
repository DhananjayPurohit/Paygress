// Consumer-side derivation of the LUKS volume key the provider
// receives in `EncryptedSpawnPodRequest.volume_encryption.key_b64`.
//
// Determinism is the load-bearing property: the consumer recomputes
// the same key on every respawn from material they already hold
// (the nsec in `~/.paygress/identity` plus the workload id printed
// at spawn), so no separate key vault is needed.

use sha2::{Digest, Sha256};

/// Bumping this breaks every existing volume — only do so alongside
/// a `VolumeEncryption` schema version bump.
const KDF_DOMAIN_V1: &[u8] = b"paygress-volume-v1\0";

/// Derive the 32-byte volume key from the consumer's raw (not bech32)
/// secp256k1 secret key and the workload id. The NUL separator domain-separates
/// the two inputs, so no collision across the boundary is constructible.
pub fn derive_volume_key(nsec_bytes: &[u8; 32], workload_id: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(KDF_DOMAIN_V1);
    hasher.update(nsec_bytes);
    hasher.update(b"\0");
    hasher.update(workload_id.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nsec(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn derivation_is_deterministic() {
        let k1 = derive_volume_key(&nsec(0x42), "workload-abc");
        let k2 = derive_volume_key(&nsec(0x42), "workload-abc");
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_workload_ids_yield_different_keys() {
        let k1 = derive_volume_key(&nsec(0x42), "workload-a");
        let k2 = derive_volume_key(&nsec(0x42), "workload-b");
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_nsecs_yield_different_keys() {
        let k1 = derive_volume_key(&nsec(0x01), "workload-x");
        let k2 = derive_volume_key(&nsec(0x02), "workload-x");
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_is_thirty_two_bytes() {
        let k = derive_volume_key(&nsec(0x00), "");
        assert_eq!(k.len(), 32);
    }

    #[test]
    fn boundary_collision_is_not_constructible() {
        let k1 = derive_volume_key(&nsec(0x42), "ab");
        let k2 = derive_volume_key(&nsec(0x42), "a\0b");
        assert_ne!(
            k1, k2,
            "embedding NUL in workload_id must not collide with the canonical separator"
        );
    }
}
