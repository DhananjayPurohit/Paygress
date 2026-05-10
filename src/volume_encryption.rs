// Volume encryption — consumer-side key derivation for Phase 1.
//
// The provider creates a LUKS-encrypted volume keyed by the bytes
// shipped in `EncryptedSpawnPodRequest.volume_encryption.key_b64`.
// This module gives consumers a *deterministic* way to compute that
// key from material they already hold (their nsec + the workload id),
// so a respawn after eviction or top-up doesn't need a separate
// out-of-band key vault.
//
// Determinism is the load-bearing property:
//   derive_volume_key(nsec, workload_id) == derive_volume_key(nsec, workload_id)
// always. The consumer can recompute the same key on every respawn
// without persisting anything beyond what they already persist (the
// nsec in `~/.paygress/identity` and the workload id printed at spawn
// time).
//
// Threat model recap (mirrors the doc on `VolumeEncryption`):
//   - Defends against post-eviction disk forensics, lazy host backups,
//     co-tenant attacks on shared storage, cold-disk seizure.
//   - Does NOT defend against a live host with `CAP_SYS_PTRACE` reading
//     /proc/<pid>/mem or extracting the LUKS key from the kernel
//     keyring while the workload runs. That requires hardware
//     confidential VMs (SEV-SNP / TDX), gated behind the
//     `attested-research-tier` `IsolationLevel`.
//
// Why one-shot SHA-256 instead of HKDF: the inputs are
// already-uniform high-entropy material (a 32-byte secp256k1 secret
// key plus a UUID). HKDF's extract step exists to handle non-uniform
// input keying material; we don't have that. A domain-separated
// SHA-256 is sufficient and avoids pulling another dep just to derive
// 32 bytes.

use sha2::{Digest, Sha256};

/// Domain-separation tag for v1 volume keys. Bumping this breaks
/// every existing volume — only do so on a schema version bump
/// of `VolumeEncryption`.
const KDF_DOMAIN_V1: &[u8] = b"paygress-volume-v1\0";

/// Derive the 32-byte volume key from the consumer's nsec bytes and
/// the workload id.
///
/// Inputs:
/// - `nsec_bytes` — the consumer's 32-byte secp256k1 secret key
///   (raw bytes, not bech32-encoded).
/// - `workload_id` — the consumer-assigned workload identifier
///   (the same UUID-shaped string passed in
///   `EncryptedSpawnPodRequest.workload_id`).
///
/// The two inputs are length-prefixed implicitly via the trailing
/// NUL byte in `KDF_DOMAIN_V1` — `workload_id` cannot contain NULs
/// (it's a UUID), so collisions across the (nsec, workload_id)
/// boundary are not constructible.
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
        // The NUL separator means appending the boundary into one
        // half cannot impersonate the other half. Concretely:
        // (nsec="X..", workload="Y..") must not collide with
        // (nsec="X..Y", workload="..") or similar splits. We can't
        // construct nsecs with arbitrary bytes via the public API
        // (it's [u8; 32]), but we sanity-check that the workload-id
        // cannot back-derive the same digest by tunneling NUL.
        let k1 = derive_volume_key(&nsec(0x42), "ab");
        let k2 = derive_volume_key(&nsec(0x42), "a\0b");
        assert_ne!(
            k1, k2,
            "embedding NUL in workload_id must not collide with the canonical separator"
        );
    }
}
