// `blossom check-upload` answers one question before a big blob is built:
// would this server take it? BUD-06's HEAD /upload needs a signed kind-24242
// event — an unsigned probe returns 401 without the size ever being read —
// so it goes through the client in `paygress::blossom`, not curl.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use colored::Colorize;
use nostr_sdk::Keys;

use paygress::blossom::BlossomClient;
use paygress::blossom_crypto::sha256_hex;

#[derive(Args)]
pub struct BlossomArgs {
    #[command(subcommand)]
    pub action: BlossomAction,
}

#[derive(Subcommand)]
pub enum BlossomAction {
    /// Ask servers whether they would accept a blob of a given size (BUD-06)
    CheckUpload(CheckUploadArgs),

    /// Upload a file and print its content address
    Upload(UploadArgs),

    /// Delete a blob by hash
    Delete(DeleteArgs),
}

#[derive(Args)]
pub struct UploadArgs {
    /// Blossom server to upload to.
    #[arg(long)]
    pub server: String,

    /// File to upload. Sent as-is — wrap it in `blossom_crypto` first if the
    /// server should not see the contents.
    #[arg(long)]
    pub file: std::path::PathBuf,

    /// Sign with this key instead of an ephemeral one. An ephemeral key can
    /// upload but can never delete afterwards.
    #[arg(long)]
    pub nsec: Option<String>,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// Blossom server holding the blob.
    #[arg(long)]
    pub server: String,

    /// Content address of the blob to remove.
    #[arg(long)]
    pub hash: String,

    /// Must be the key that uploaded it.
    #[arg(long)]
    pub nsec: String,
}

#[derive(Args)]
pub struct CheckUploadArgs {
    /// Blossom server to probe. Repeat to compare several.
    #[arg(long = "server", required = true)]
    pub servers: Vec<String>,

    /// Blob size in bytes. Suffixes are accepted: 183MB, 1.5GB, 200KB.
    #[arg(long, conflicts_with = "file", required_unless_present = "file")]
    pub size: Option<String>,

    /// Probe with a real blob's size and hash instead of a hypothetical one.
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,

    /// MIME type sent as `X-Content-Type`.
    #[arg(long, default_value = "application/octet-stream")]
    pub mime: String,

    /// Sign with this key instead of an ephemeral one, for servers that
    /// only accept uploads from known pubkeys.
    #[arg(long)]
    pub nsec: Option<String>,
}

pub async fn execute(args: BlossomArgs, verbose: bool) -> Result<()> {
    match args.action {
        BlossomAction::CheckUpload(a) => run_check_upload(a, verbose).await,
        BlossomAction::Upload(a) => run_upload(a).await,
        BlossomAction::Delete(a) => run_delete(a).await,
    }
}

async fn run_upload(args: UploadArgs) -> Result<()> {
    let keys = match &args.nsec {
        Some(nsec) => Keys::parse(nsec).context("parsing --nsec")?,
        None => Keys::generate(),
    };

    let bytes = std::fs::read(&args.file).with_context(|| format!("reading {:?}", args.file))?;
    eprintln!(
        "{} uploading {} bytes to {}",
        "→".blue(),
        bytes.len(),
        args.server
    );

    let expected = sha256_hex(&bytes);
    let resp = BlossomClient::new(args.server.clone(), keys)
        .put(bytes)
        .await?;
    if resp.sha256 != expected {
        anyhow::bail!(
            "server returned hash {} for bytes hashing to {}",
            resp.sha256,
            expected
        );
    }

    eprintln!("{} stored {} bytes", "✓".green(), resp.size);
    // The URL is the entire stdout contract.
    println!("{}", resp.url);
    Ok(())
}

async fn run_delete(args: DeleteArgs) -> Result<()> {
    let keys = Keys::parse(&args.nsec).context("parsing --nsec")?;
    BlossomClient::new(args.server.clone(), keys)
        .delete(&args.hash)
        .await?;
    eprintln!("{} deleted {}", "✓".green(), args.hash);
    Ok(())
}

async fn run_check_upload(args: CheckUploadArgs, verbose: bool) -> Result<()> {
    let keys = match &args.nsec {
        Some(nsec) => Keys::parse(nsec).context("parsing --nsec")?,
        None => Keys::generate(),
    };

    // A hypothetical blob still needs a hash to put in the `x` tag; servers
    // check the signature and the size, not whether the bytes exist yet.
    let (size, hash) = match &args.file {
        Some(path) => {
            let bytes = std::fs::read(path).with_context(|| format!("reading {:?}", path))?;
            (bytes.len() as u64, sha256_hex(&bytes))
        }
        None => {
            let size = parse_size(args.size.as_deref().unwrap_or_default())?;
            (
                size,
                sha256_hex(format!("paygress-probe-{}", size).as_bytes()),
            )
        }
    };

    if verbose {
        eprintln!(
            "{} probing {} bytes as {} ({})",
            "→".blue(),
            size,
            &hash[..16],
            keys.public_key().to_hex()
        );
    }

    let mut any_accepted = false;
    for server in &args.servers {
        let client = BlossomClient::new(server.clone(), keys.clone());
        match client.check_upload(&hash, size, Some(&args.mime)).await {
            Ok(check) if check.accepted() => {
                any_accepted = true;
                println!("{} {} accepts {} bytes", "✓".green(), server, size);
            }
            Ok(check) if check.unsupported() => {
                println!(
                    "{} {} does not implement BUD-06 (HTTP {}) — size limit unknown",
                    "?".yellow(),
                    server,
                    check.status
                );
            }
            Ok(check) => {
                println!(
                    "{} {} refuses {} bytes (HTTP {}{})",
                    "✗".red(),
                    server,
                    size,
                    check.status,
                    check
                        .reason
                        .as_deref()
                        .map(|r| format!(": {}", r))
                        .unwrap_or_default()
                );
            }
            Err(e) => {
                println!("{} {} unreachable: {}", "✗".red(), server, e);
            }
        }
    }

    if !any_accepted {
        anyhow::bail!("no probed server confirmed it would accept {} bytes", size);
    }
    Ok(())
}

/// Bytes, or a decimal with a KB/MB/GB suffix (powers of 1024, as every
/// blob-size limit in the wild is quoted).
fn parse_size(raw: &str) -> Result<u64> {
    let s = raw.trim();
    let (digits, multiplier) = match s.to_ascii_uppercase() {
        u if u.ends_with("GB") => (&s[..s.len() - 2], 1024u64 * 1024 * 1024),
        u if u.ends_with("MB") => (&s[..s.len() - 2], 1024 * 1024),
        u if u.ends_with("KB") => (&s[..s.len() - 2], 1024),
        u if u.ends_with('B') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };

    let value: f64 = digits
        .trim()
        .parse()
        .with_context(|| format!("cannot read {:?} as a size", raw))?;
    if !value.is_finite() || value < 0.0 {
        anyhow::bail!("size must be a positive number, got {:?}", raw);
    }
    Ok((value * multiplier as f64).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn plain_digits_are_bytes() {
        assert_eq!(parse_size("183").unwrap(), 183);
    }

    #[test]
    fn suffixes_are_binary_multiples() {
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("183MB").unwrap(), 183 * 1024 * 1024);
        assert_eq!(parse_size("1.5GB").unwrap(), 1024 * 1024 * 1024 * 3 / 2);
    }

    #[test]
    fn suffixes_are_case_insensitive() {
        assert_eq!(parse_size("2mb").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_size("big").is_err());
        assert!(parse_size("-1MB").is_err());
        assert!(parse_size("").is_err());
    }
}
