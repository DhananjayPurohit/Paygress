// Helpers shared across the CLI commands: local Nostr identity,
// relay lists, password generation, spinners, isolation-level parsing.

use anyhow::Result;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use nostr_sdk::{Keys, ToBech32};
use paygress::nostr::IsolationLevel;
use rand::Rng;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
];

pub fn parse_relays(relays: Option<String>) -> Vec<String> {
    match relays {
        Some(r) => split_csv(&r),
        None => DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Split a comma-separated flag value, trimming and dropping blanks.
pub fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

pub fn get_or_create_identity(explicit_key: Option<String>) -> Result<String> {
    if let Some(key) = explicit_key {
        return Ok(key);
    }

    let home =
        std::env::var("HOME").map_err(|_| anyhow::anyhow!("Could not determine home directory"))?;
    let paygress_dir = Path::new(&home).join(".paygress");
    if !paygress_dir.exists() {
        std::fs::create_dir_all(&paygress_dir)?;
    }

    let identity_file = paygress_dir.join("identity");
    if identity_file.exists() {
        let key = std::fs::read_to_string(&identity_file)?.trim().to_string();
        println!(
            "  Using identity from {}",
            identity_file.display().to_string().dimmed()
        );
        return Ok(key);
    }

    println!(
        "{}",
        "  No identity found. Generating new Nostr identity...".yellow()
    );
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;

    let mut file = std::fs::File::create(&identity_file)?;
    file.write_all(nsec.as_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(0o600);
        file.set_permissions(perms)?;
    }

    println!(
        "  {} Created new identity at {}",
        "✓".green(),
        identity_file.display()
    );
    println!("  {} {}", "NSEC:".bold(), nsec.red());
    println!("  {}", "Make sure to back up this key!".yellow());
    println!();

    Ok(nsec)
}

pub fn generate_password(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

/// clap value-parser for `--isolation-level`, shared by `list`,
/// `spawn`, and `batch`.
pub fn parse_isolation_level(s: &str) -> std::result::Result<IsolationLevel, String> {
    IsolationLevel::from_slug(s).ok_or_else(|| {
        format!(
            "unknown isolation level `{}` (expected one of: \
             shared-kernel, dedicated-host, attested-research-tier)",
            s
        )
    })
}

/// Start a steady-ticking spinner in the CLI's house style.
pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.blue} {msg}")
            .expect("static spinner template is valid"),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}
