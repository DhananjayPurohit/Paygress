// Shared identity and relay helpers for Nostr-based commands

use anyhow::Result;
use colored::Colorize;
use nostr_sdk::{Keys, ToBech32};
use std::io::Write;
use std::path::Path;

pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
];

pub fn parse_relays(relays: Option<String>) -> Vec<String> {
    match relays {
        Some(r) => r
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
    }
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

    // Generate new key
    println!(
        "{}",
        "  No identity found. Generating new Nostr identity...".yellow()
    );
    let keys = Keys::generate();
    let nsec = keys.secret_key()?.to_bech32()?;

    // Save to file
    let mut file = std::fs::File::create(&identity_file)?;
    file.write_all(nsec.as_bytes())?;

    // Set permissions to 600 (owner read/write only) on Unix
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
