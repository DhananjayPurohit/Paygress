// LUKS-on-loop helpers for consumer-encrypted persistent volumes.
//
// Host layout:
//   /var/lib/paygress/volumes/<id>.luks  sparse file, LUKS2 header + payload
//   /dev/mapper/paygress-<id>-luks       device-mapper alias after luksOpen
//   /var/lib/paygress/mounts/<id>/       ext4 mountpoint (the `-v` source)
//
// Threat model: defends against post-eviction disk forensics, operator
// backups, co-tenant access to shared storage and cold-disk seizure. It
// does NOT defend against a live host reading the key out of the kernel
// keyring or /proc/<pid>/mem — that needs SEV-SNP / TDX.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::compute::run_checked;

const VOLUME_ROOT: &str = "/var/lib/paygress";

/// Stable per `id` so cleanup can find it again after a provider crash.
fn mapper_name(id: u32) -> String {
    format!("paygress-{}-luks", id)
}

fn image_path(id: u32) -> PathBuf {
    PathBuf::from(VOLUME_ROOT)
        .join("volumes")
        .join(format!("{}.luks", id))
}

fn mount_path(id: u32) -> PathBuf {
    PathBuf::from(VOLUME_ROOT)
        .join("mounts")
        .join(id.to_string())
}

fn mapper_device(id: u32) -> PathBuf {
    PathBuf::from("/dev/mapper").join(mapper_name(id))
}

/// Deliberately has no `Drop` impl: teardown is explicit via
/// `destroy_encrypted_volume`, so retry paths can't double-destroy.
#[derive(Debug, Clone)]
pub struct EncryptedVolume {
    pub id: u32,
    pub mount_path: PathBuf,
}

/// Create + format + open + mount a LUKS volume for `id`. Rolls back partial
/// state on failure so a retry at the same id starts clean.
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

    // A crashed previous run can leave `/dev/mapper/paygress-<id>-luks`
    // behind (luksClose sees EBUSY if the lazy umount hasn't landed),
    // and luksOpen then fails with "device already exists". Destroy is
    // idempotent, so running it first makes create self-healing.
    if let Err(e) = destroy_encrypted_volume(id).await {
        warn!("pre-create cleanup of id={} returned {}; continuing", id, e);
    }

    tokio::fs::create_dir_all(img.parent().context("image path has no parent")?)
        .await
        .context("create volumes/ directory")?;
    tokio::fs::create_dir_all(&mnt)
        .await
        .context("create mountpoint directory")?;

    // Sparse: only consumes host disk on write.
    let bytes = (size_gb as u64) * 1024 * 1024 * 1024;
    let img_str = img.to_string_lossy().to_string();
    run_checked("truncate", &["-s", &bytes.to_string(), &img_str]).await?;

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
        let _ = tokio::fs::remove_file(&img).await;
        return Err(e.context("cryptsetup luksFormat"));
    }

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

    // -F forces past leftover ext4 magic from a prior tenancy at the
    // same id, which mkfs would otherwise refuse to overwrite.
    let mapper_str = mapper.to_string_lossy().to_string();
    let mnt_str = mnt.to_string_lossy().to_string();
    for (prog, args) in [
        ("mkfs.ext4", vec!["-F", mapper_str.as_str()]),
        ("mount", vec![mapper_str.as_str(), mnt_str.as_str()]),
    ] {
        if let Err(e) = run_checked(prog, &args).await {
            let _ = run_quiet("cryptsetup", &["luksClose", &mapper_n]).await;
            let _ = tokio::fs::remove_file(&img).await;
            return Err(e);
        }
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

/// Idempotent teardown — never errors on "already gone". Order matters:
/// umount releases the block device, luksClose releases the mapper entry and
/// the key from keyring memory, and luksErase overwrites every keyslot so the
/// payload is unrecoverable even from a copy taken beforehand.
pub async fn destroy_encrypted_volume(id: u32) -> Result<()> {
    let img = image_path(id);
    let mnt = mount_path(id);
    let mapper_n = mapper_name(id);
    let img_str = img.to_string_lossy().to_string();
    let mnt_str = mnt.to_string_lossy().to_string();

    debug!("Destroying LUKS volume id={}", id);

    // -l (lazy) so a container still holding a file open doesn't
    // block teardown; the kernel detaches on the last reference drop.
    if mnt.exists() {
        match Command::new("umount").args(["-l", &mnt_str]).output().await {
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

    // Non-zero on "not active", which is exactly the idempotent case.
    let _ = run_quiet("cryptsetup", &["luksClose", &mapper_n]).await;

    if img.exists() {
        if let Err(e) = run_checked("cryptsetup", &["luksErase", "--batch-mode", &img_str]).await {
            warn!("cryptsetup luksErase {} non-fatal: {}", img_str, e);
        }
        if let Err(e) = tokio::fs::remove_file(&img).await {
            warn!("remove {} non-fatal: {}", img.display(), e);
        }
    }

    if mnt.exists() {
        let _ = tokio::fs::remove_dir(&mnt).await;
    }

    Ok(())
}

/// Feed `key` to `prog` on stdin (cryptsetup `--key-file=-`) so the key bytes
/// never appear on a command line where `ps` would leak them.
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

/// Best-effort run whose failure the caller intentionally ignores.
async fn run_quiet(prog: &str, args: &[&str]) -> bool {
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

    /// Ignored: shells out to cryptsetup/umount against the real
    /// filesystem, so it runs in the VPS acceptance suite only.
    #[tokio::test]
    #[ignore]
    async fn destroy_is_a_no_op_when_nothing_exists() {
        let res = destroy_encrypted_volume(99_999).await;
        assert!(
            res.is_ok(),
            "destroy_encrypted_volume must succeed on a never-created id, got {:?}",
            res
        );
    }
}
