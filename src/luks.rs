// LUKS-on-loop helpers for consumer-encrypted persistent volumes.
//
// Phase 2 of the volume-encryption work. Phase 1 (PR #46) shipped the
// wire format + KDF; this module is what actually encrypts the bytes
// on disk so the host operator's post-eviction `tar` reveals only
// ciphertext.
//
// Layout on the host
// ------------------
// /var/lib/paygress/volumes/<id>.luks   — sparse file, LUKS2 header + payload
// /dev/mapper/paygress-<id>-luks        — kernel device-mapper alias (after luksOpen)
// /var/lib/paygress/mounts/<id>/        — ext4 mountpoint (the `-v` bind source)
//
// Lifecycle
// ---------
// `create_encrypted_volume` does the full create-format-open-mkfs-mount
// dance, returning a handle whose `mount_path` the docker backend
// bind-mounts into the container. `destroy_encrypted_volume` is the
// inverse: umount, luksClose, luksErase (overwrites all keyslots so
// the file's ciphertext is unrecoverable even by the host operator
// who held the disk image), then rm.
//
// Idempotency
// -----------
// Both creation and destruction are best-effort idempotent:
//   - create rolls back any partial state on failure (so a half-
//     formatted file doesn't trap a future spawn at the same id),
//   - destroy never errors on "not present" — a half-leaked mapper
//     entry from a crashed previous run gets cleaned up on the next
//     `delete_container`.
//
// Why shell-out to cryptsetup
// ---------------------------
// libcryptsetup-rs exists, but it links against libcryptsetup (the
// system C library) and hauls a large unsafe surface into the
// process. Shelling out to `/sbin/cryptsetup` keeps the LUKS code
// path entirely in a child process — easier to audit, easier to
// strace, and matches how every other paygress subprocess (docker,
// nginx) is invoked. Performance is irrelevant: we exec cryptsetup
// twice per workload lifetime (create + destroy).
//
// Threat model recap (mirrors the wire-format doc on
// `nostr::VolumeEncryption`):
//   - Defends: post-eviction disk forensics, lazy host-operator
//     backups, co-tenant attacks on shared storage, cold-disk
//     seizure.
//   - Does NOT defend: live host kernel reading /proc/<pid>/mem or
//     extracting the LUKS key from the kernel keyring while the
//     workload runs. That requires hardware confidential VMs
//     (SEV-SNP / TDX), gated behind the `attested-research-tier`
//     `IsolationLevel`.
//   - The key is fed to `cryptsetup` via stdin (key-file=-) so it
//     never appears on the command line (where `ps` would leak it).
//     Provider holds the key only in memory, dropped when
//     `ContainerConfig` goes out of scope.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Root directory for paygress-managed encrypted volumes. Two
/// subdirectories live here:
/// - `volumes/<id>.luks` — sparse files holding LUKS2 containers.
/// - `mounts/<id>/`     — ext4 mountpoints bind-mounted into the
///                        container at `data_path`.
const VOLUME_ROOT: &str = "/var/lib/paygress";

/// Kernel device-mapper name for a workload's open LUKS volume.
/// Stable per `id` so cleanup can find it after a provider crash.
fn mapper_name(id: u32) -> String {
    format!("paygress-{}-luks", id)
}

/// Sparse file backing the LUKS container.
fn image_path(id: u32) -> PathBuf {
    PathBuf::from(VOLUME_ROOT)
        .join("volumes")
        .join(format!("{}.luks", id))
}

/// Mountpoint where the open LUKS volume's ext4 lives.
fn mount_path(id: u32) -> PathBuf {
    PathBuf::from(VOLUME_ROOT)
        .join("mounts")
        .join(id.to_string())
}

/// Fully-resolved /dev/mapper path (what `mount` and Docker bind
/// mounts care about).
fn mapper_device(id: u32) -> PathBuf {
    PathBuf::from("/dev/mapper").join(mapper_name(id))
}

/// Created + open + mounted handle to an encrypted volume. The
/// `mount_path` is what the Docker backend bind-mounts at
/// `data_path` inside the container. Drop semantics: do NOT do
/// anything on drop — destruction is explicit via
/// `destroy_encrypted_volume`, which the docker backend calls from
/// `delete_container`. (Doing it on drop would risk
/// double-destruction on retry paths.)
#[derive(Debug, Clone)]
pub struct EncryptedVolume {
    pub id: u32,
    pub mount_path: PathBuf,
}

/// Verify cryptsetup is on PATH. Provider should call this at
/// startup if any template it serves has `data_path: Some(_)` and
/// the operator has not opted out of consumer-encrypted volumes.
/// Returns the version string so the operator can log what they
/// got.
pub async fn check_cryptsetup_available() -> Result<String> {
    let out = Command::new("cryptsetup")
        .arg("--version")
        .output()
        .await
        .context(
            "cryptsetup binary not found on PATH; install cryptsetup or disable encrypted-volume support",
        )?;
    if !out.status.success() {
        anyhow::bail!(
            "cryptsetup --version returned non-zero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Create + format + open + mount a LUKS-encrypted volume for the
/// given workload id. Returns the mount path the caller should bind
/// into the container.
///
/// On failure, attempts to roll back any partial state (close mapper,
/// rm sparse file) so a retry at the same id starts clean.
pub async fn create_encrypted_volume(
    id: u32,
    size_gb: u32,
    key: &[u8; 32],
) -> Result<EncryptedVolume> {
    let img = image_path(id);
    let mnt = mount_path(id);
    let mapper = mapper_device(id);
    let mapper_n = mapper_name(id);

    info!(
        "Creating LUKS-encrypted data volume: id={} size={}G image={}",
        id,
        size_gb,
        img.display()
    );

    // 1. mkdir -p the parent directories. Both volumes/ and mounts/
    //    must exist before the next steps; they survive across
    //    spawns (best-effort once-per-host).
    tokio::fs::create_dir_all(img.parent().unwrap())
        .await
        .context("create volumes/ directory")?;
    tokio::fs::create_dir_all(&mnt)
        .await
        .context("create mountpoint directory")?;

    // 2. Truncate to size. Sparse — only consumes disk on write.
    //    `truncate -s` is portable across the GNU coreutils on
    //    every Linux paygress runs on.
    let bytes = (size_gb as u64) * 1024 * 1024 * 1024;
    let img_str = img.to_string_lossy().to_string();
    let trunc = Command::new("truncate")
        .args(["-s", &bytes.to_string(), &img_str])
        .output()
        .await
        .context("invoke truncate")?;
    if !trunc.status.success() {
        anyhow::bail!(
            "truncate failed: {}",
            String::from_utf8_lossy(&trunc.stderr)
        );
    }

    // 3. luksFormat with the consumer key on stdin (--key-file=-).
    //    --batch-mode skips the interactive "are you sure" prompt;
    //    --type luks2 picks the modern header format with proper
    //    PBKDF2 + AEAD; defaults are fine for AES-XTS-Plain64.
    if let Err(e) = run_with_key_stdin(
        "cryptsetup",
        &[
            "luksFormat",
            "--type",
            "luks2",
            "--batch-mode",
            "--key-file=-",
            &img_str,
        ],
        key,
    )
    .await
    {
        // Roll back: the truncate-d file is unusable junk. Don't
        // leave it behind.
        let _ = tokio::fs::remove_file(&img).await;
        return Err(e.context("cryptsetup luksFormat"));
    }

    // 4. luksOpen → /dev/mapper/paygress-<id>-luks. Same key on
    //    stdin. After this the kernel device-mapper holds the key
    //    in keyring memory (visible to root via `dmsetup info`,
    //    which is exactly the threat-model boundary we documented).
    if let Err(e) = run_with_key_stdin(
        "cryptsetup",
        &["luksOpen", "--key-file=-", &img_str, &mapper_n],
        key,
    )
    .await
    {
        let _ = tokio::fs::remove_file(&img).await;
        return Err(e.context("cryptsetup luksOpen"));
    }

    // 5. mkfs.ext4 on the mapper device. -F forces over any stale
    //    signature (a re-spawn at the same id with a new key would
    //    otherwise see leftover ext4 magic from a prior tenancy and
    //    refuse to reformat).
    let mapper_str = mapper.to_string_lossy().to_string();
    let mkfs = Command::new("mkfs.ext4")
        .args(["-F", &mapper_str])
        .output()
        .await
        .context("invoke mkfs.ext4")?;
    if !mkfs.status.success() {
        // Roll back: close the mapper, then drop the file.
        let _ = run("cryptsetup", &["luksClose", &mapper_n]).await;
        let _ = tokio::fs::remove_file(&img).await;
        anyhow::bail!(
            "mkfs.ext4 failed: {}",
            String::from_utf8_lossy(&mkfs.stderr)
        );
    }

    // 6. mount to /var/lib/paygress/mounts/<id>. The Docker backend
    //    bind-mounts this path at the template's `data_path`.
    let mnt_str = mnt.to_string_lossy().to_string();
    let mount = Command::new("mount")
        .args([&mapper_str, &mnt_str])
        .output()
        .await
        .context("invoke mount")?;
    if !mount.status.success() {
        let _ = run("cryptsetup", &["luksClose", &mapper_n]).await;
        let _ = tokio::fs::remove_file(&img).await;
        anyhow::bail!("mount failed: {}", String::from_utf8_lossy(&mount.stderr));
    }

    info!(
        "LUKS volume id={} ready: mounted at {} (mapper {})",
        id,
        mnt.display(),
        mapper.display()
    );
    Ok(EncryptedVolume {
        id,
        mount_path: mnt,
    })
}

/// Tear down everything `create_encrypted_volume` set up. Idempotent
/// — never errors on "already gone". Order matters:
/// 1. umount the ext4 (releases the kernel block device handle)
/// 2. luksClose (releases the mapper entry + the LUKS key from
///    keyring memory)
/// 3. luksErase (overwrites all keyslots → the underlying file's
///    ciphertext is unrecoverable, even if the operator copied the
///    file before this step ran)
/// 4. rm the sparse file (free disk space; defense-in-depth even
///    after luksErase)
/// 5. rmdir the mountpoint (cosmetic; keeps /var/lib/paygress/mounts
///    tidy)
pub async fn destroy_encrypted_volume(id: u32) -> Result<()> {
    let img = image_path(id);
    let mnt = mount_path(id);
    let mapper_n = mapper_name(id);
    let img_str = img.to_string_lossy().to_string();
    let mnt_str = mnt.to_string_lossy().to_string();

    debug!("Destroying LUKS volume id={}", id);

    // 1. umount. -l (lazy) handles the case where the container is
    //    still holding a file open during teardown — the kernel
    //    detaches the mount the moment the last reference drops.
    if mnt.exists() {
        let out = Command::new("umount").args(["-l", &mnt_str]).output().await;
        match out {
            Ok(o) if !o.status.success() => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if !stderr.contains("not mounted") {
                    warn!("umount {} non-fatal error: {}", mnt_str, stderr.trim());
                }
            }
            Err(e) => warn!("umount {} could not exec: {}", mnt_str, e),
            _ => {}
        }
    }

    // 2. luksClose. Idempotent: cryptsetup returns 0 on success and
    //    a non-zero on "not active", which we tolerate.
    let _ = run("cryptsetup", &["luksClose", &mapper_n]).await;

    // 3. luksErase wipes ALL keyslots without needing the original
    //    key (--batch-mode bypasses the "are you really sure" prompt).
    //    After this, the LUKS header has no recoverable keyslot;
    //    even if the operator extracted the file before step 4,
    //    the AES-XTS payload is unreachable.
    if img.exists() {
        let out = Command::new("cryptsetup")
            .args(["luksErase", "--batch-mode", &img_str])
            .output()
            .await;
        if let Ok(o) = out {
            if !o.status.success() {
                warn!(
                    "cryptsetup luksErase {} non-fatal: {}",
                    img_str,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
        }
    }

    // 4. rm the sparse file. Best-effort; the disk space matters
    //    more than the ciphertext (which is keyless after step 3).
    if img.exists() {
        if let Err(e) = tokio::fs::remove_file(&img).await {
            warn!("remove {} non-fatal: {}", img.display(), e);
        }
    }

    // 5. rmdir the mountpoint. Cosmetic.
    if mnt.exists() {
        let _ = tokio::fs::remove_dir(&mnt).await;
    }

    Ok(())
}

/// Spawn `prog` with `args` and feed `key` on stdin (for cryptsetup
/// `--key-file=-`). The key bytes never appear on the command line
/// (where `ps` would expose them) or in any log.
async fn run_with_key_stdin(prog: &str, args: &[&str], key: &[u8; 32]) -> Result<()> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", prog))?;
    {
        let stdin = child.stdin.as_mut().context("child stdin not piped")?;
        stdin.write_all(key).await.context("write key to stdin")?;
        stdin.shutdown().await.context("close key stdin")?;
    }
    let out = child
        .wait_with_output()
        .await
        .with_context(|| format!("wait for {}", prog))?;
    if !out.status.success() {
        anyhow::bail!(
            "{} {:?} failed: {}",
            prog,
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Spawn `prog` with `args` (no stdin), best-effort silent. Returns
/// the success bool so callers can log without short-circuiting on
/// "not present" cleanups.
async fn run(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_id_scoped_and_under_volume_root() {
        let img = image_path(42);
        let mnt = mount_path(42);
        let dev = mapper_device(42);
        assert!(
            img.starts_with(VOLUME_ROOT),
            "image not under VOLUME_ROOT: {}",
            img.display()
        );
        assert!(
            mnt.starts_with(VOLUME_ROOT),
            "mount not under VOLUME_ROOT: {}",
            mnt.display()
        );
        assert_eq!(img.file_name().unwrap(), "42.luks");
        assert_eq!(mnt.file_name().unwrap(), "42");
        assert_eq!(dev, PathBuf::from("/dev/mapper/paygress-42-luks"));
    }

    #[test]
    fn mapper_name_is_distinct_per_id() {
        assert_ne!(mapper_name(1), mapper_name(2));
        assert_eq!(mapper_name(7), "paygress-7-luks");
    }

    #[test]
    fn paths_for_different_ids_do_not_collide() {
        assert_ne!(image_path(1), image_path(2));
        assert_ne!(mount_path(1), mount_path(2));
    }
}
