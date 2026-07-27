// Provider identity, /etc/paygress/provider-config.json, and the
// systemd unit that runs the provider.

use anyhow::Result;
use colored::Colorize;
use nostr_sdk::ToBech32;
use serde_json::json;
use std::io::Write;

use super::ssh::{run_ssh_command, write_remote_file};
use super::{step_banner, BootstrapArgs};
use crate::util::split_csv;

use super::ngx_l402::HTTP_BIND_ADDR;

const CONFIG_PATH: &str = "/etc/paygress/provider-config.json";
const SERVICE_PATH: &str = "/etc/systemd/system/paygress-provider.service";

const SYSTEMD_UNIT: &str = r#"[Unit]
Description=Paygress Provider Service
After=network.target pve-cluster.service

[Service]
Type=simple
ExecStart=/usr/local/bin/paygress-cli provider start --config /etc/paygress/provider-config.json
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
"#;

/// Resolve the provider's Nostr key and derive its display name. The
/// name comes from the pubkey, so it's the same on every re-run.
pub(super) fn step_nostr_identity(args: &BootstrapArgs) -> Result<(String, String)> {
    step_banner("Step 5: Configuring Nostr");

    let nostr_key = match args.nostr_key {
        Some(ref key) => {
            println!("  Using provided Nostr key");
            key.clone()
        }
        None => {
            print!("  Generating Nostr keypair... ");
            std::io::stdout().flush()?;

            let keys = nostr_sdk::Keys::generate();
            let nsec = keys
                .secret_key()
                .to_bech32()
                .map_err(|e| anyhow::anyhow!("Failed to encode key: {}", e))?;
            let npub = keys
                .public_key()
                .to_bech32()
                .map_err(|e| anyhow::anyhow!("Failed to encode public key: {}", e))?;

            println!("{}", "Done".green());
            println!("  NPUB: {}", npub.cyan());
            nsec
        }
    };

    let keys = nostr_sdk::Keys::parse(&nostr_key)
        .map_err(|e| anyhow::anyhow!("failed to parse nostr key for name derivation: {}", e))?;
    let provider_name = paygress::namegen::derive_provider_name(&keys.public_key().to_bytes());
    println!("  Provider name:  {}", provider_name.yellow().bold());
    println!();

    Ok((nostr_key, provider_name))
}

pub(super) fn step_write_config(
    args: &BootstrapArgs,
    nostr_key: &str,
    provider_name: &str,
    use_lxd: bool,
) -> Result<()> {
    step_banner("Step 6: Creating Provider Configuration");

    if args.dry_run {
        println!("  Would create {}", CONFIG_PATH);
        println!("  Would create /var/lib/paygress/ (wallet db directory)");
    } else {
        let config = render_provider_config(args, nostr_key, provider_name, use_lxd)?;
        let sudo = args.sudo();
        run_ssh_command(
            args,
            &format!(
                "{0}mkdir -p /etc/paygress && {0}mkdir -p /var/lib/paygress",
                sudo
            ),
        )?;
        write_remote_file(args, CONFIG_PATH, &config, "EOF")?;
        println!("  {} Created {}", "✓".green(), CONFIG_PATH);
        println!(
            "  {} Created /var/lib/paygress/ (wallet db directory)",
            "✓".green()
        );
    }
    println!();
    Ok(())
}

/// Serialized with serde_json so operator-supplied values (location,
/// mint URLs, Lightning address) are escaped rather than interpolated
/// straight into the document.
fn render_provider_config(
    args: &BootstrapArgs,
    nostr_key: &str,
    provider_name: &str,
    use_lxd: bool,
) -> Result<String> {
    let (backend_type, template, storage, bridge) = if use_lxd {
        ("LXD", "images:ubuntu/22.04", "default", "lxdbr0")
    } else {
        (
            "Proxmox",
            "local:vztmpl/ubuntu-22.04-standard.tar.zst",
            "local-lvm",
            "vmbr0",
        )
    };

    let config = json!({
        "backend_type": backend_type,
        "proxmox_url": "https://127.0.0.1:8006/api2/json",
        "proxmox_token_id": "root@pam!paygress",
        "proxmox_token_secret": "REPLACE_WITH_TOKEN",
        "proxmox_node": "pve",
        "proxmox_storage": storage,
        "proxmox_template": template,
        "proxmox_bridge": bridge,
        "vmid_range_start": 1000,
        "vmid_range_end": 1999,
        "nostr_private_key": nostr_key,
        "nostr_relays": ["wss://relay.damus.io", "wss://nos.lol"],
        "provider_name": provider_name,
        "provider_location": &args.location,
        "public_ip": &args.host,
        "capabilities": ["lxc", "vm"],
        "specs": [
            {
                "id": "basic",
                "name": "Basic",
                "description": "1 vCPU, 1GB RAM",
                "cpu_millicores": 1000,
                "memory_mb": 1024,
                "rate_msats_per_sec": 50,
            },
            {
                "id": "standard",
                "name": "Standard",
                "description": "2 vCPU, 2GB RAM",
                "cpu_millicores": 2000,
                "memory_mb": 2048,
                "rate_msats_per_sec": 100,
            },
        ],
        "whitelisted_mints": split_csv(&args.mints),
        "heartbeat_interval_secs": 60,
        "minimum_duration_seconds": 60,
        "cashu_wallet_db_path": "/var/lib/paygress/cashu-wallet.sqlite",
        "lightning_address": &args.lightning_address,
        // Only when ngx_l402 is deployed (step 8, which needs a Lightning
        // address). The axum backend must never run without a paywall in
        // front, and loopback is what lets it trust the paywall's headers.
        "http_bind_addr": args.lightning_address.as_ref().map(|_| HTTP_BIND_ADDR),
    });

    Ok(serde_json::to_string_pretty(&config)?)
}

pub(super) fn step_systemd_service(args: &BootstrapArgs) -> Result<()> {
    step_banner("Step 7: Setting Up Systemd Service");

    if args.dry_run {
        println!("  Would create {}", SERVICE_PATH);
    } else {
        write_remote_file(args, SERVICE_PATH, SYSTEMD_UNIT, "EOF")?;
        run_ssh_command(args, &format!("{}systemctl daemon-reload", args.sudo()))?;
        println!("  {} Created systemd service", "✓".green());
    }
    println!();
    Ok(())
}

pub(super) fn step_start_service(args: &BootstrapArgs, use_lxd: bool) -> Result<()> {
    step_banner("Step 9: Starting Provider Service");

    if args.dry_run {
        println!("  Would run: systemctl enable --now paygress-provider");
    } else if use_lxd {
        let sudo = args.sudo();
        run_ssh_command(
            args,
            &format!(
                "{0}systemctl enable paygress-provider && {0}systemctl restart paygress-provider",
                sudo
            ),
        )?;
        println!("  {} Service started successfully!", "✓".green());
    } else {
        // The Proxmox config still needs its API token pasted in.
        println!(
            "  {} Service configured (not started - needs API token)",
            "✓".green()
        );
        println!();
        println!("  To complete setup, SSH into the server and:");
        println!("    1. Get your API token: pveum user token list root@pam");
        println!("    2. Update /etc/paygress/provider-config.json");
        println!("    3. Start: systemctl enable --now paygress-provider");
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(location: Option<&str>, mints: &str) -> BootstrapArgs {
        BootstrapArgs {
            host: "203.0.113.1".to_string(),
            user: "root".to_string(),
            password: None,
            key: None,
            port: 22,
            location: location.map(str::to_string),
            nostr_key: None,
            mints: mints.to_string(),
            lightning_address: None,
            skip_proxmox: false,
            dry_run: false,
            tunnel: false,
            local_binary: None,
            sweep_interval_secs: 3600,
            sweep_min_balance_sats: 100,
            root_key: None,
        }
    }

    #[test]
    fn http_interface_is_enabled_only_behind_the_paywall() {
        // The axum backend must never be running without ngx_l402 in front:
        // it trusts the paywall's settled-amount header.
        let mut a = args(None, "https://testnut.cashu.space");
        assert!(render(&a)["http_bind_addr"].is_null());

        a.lightning_address = Some("you@getalby.com".to_string());
        assert_eq!(render(&a)["http_bind_addr"], HTTP_BIND_ADDR);
    }

    #[test]
    fn http_bind_stays_on_loopback() {
        // Any other address and the provider stops believing the header,
        // silently falling back to decoding instruments.
        assert!(paygress::provider_http::is_loopback_bind(HTTP_BIND_ADDR));
    }

    fn render(args: &BootstrapArgs) -> serde_json::Value {
        let raw = render_provider_config(args, "nsec1placeholder", "test-provider", true).unwrap();
        serde_json::from_str(&raw).expect("rendered config must be valid JSON")
    }

    #[test]
    fn quotes_in_location_do_not_break_the_document() {
        let v = render(&args(
            Some(r#"US-East "primary""#),
            "https://testnut.cashu.space",
        ));
        assert_eq!(v["provider_location"], r#"US-East "primary""#);
    }

    #[test]
    fn backslashes_in_location_are_escaped() {
        let v = render(&args(Some(r"a\b"), "https://testnut.cashu.space"));
        assert_eq!(v["provider_location"], r"a\b");
    }

    #[test]
    fn absent_location_renders_null() {
        let v = render(&args(None, "https://testnut.cashu.space"));
        assert!(v["provider_location"].is_null());
    }

    #[test]
    fn mints_are_split_and_escaped() {
        let v = render(&args(None, r#"https://a.example, https://b"x".example"#));
        assert_eq!(
            v["whitelisted_mints"],
            serde_json::json!(["https://a.example", r#"https://b"x".example"#])
        );
    }

    /// The generated file omits every field ProviderConfig defaults, so a
    /// newly-required field there would silently break bootstrap.
    #[test]
    fn rendered_config_deserializes_into_provider_config() {
        let a = args(Some("US-East"), "https://testnut.cashu.space");
        let raw = render_provider_config(&a, "nsec1placeholder", "test-provider", true).unwrap();
        let cfg: paygress::provider::ProviderConfig =
            serde_json::from_str(&raw).expect("bootstrap config must load as ProviderConfig");
        assert_eq!(cfg.provider_name, "test-provider");
        assert_eq!(cfg.provider_location.as_deref(), Some("US-East"));
        assert_eq!(cfg.public_ip, "203.0.113.1");
    }

    #[test]
    fn lxd_and_proxmox_select_different_backend_fields() {
        let a = args(None, "https://testnut.cashu.space");
        let lxd: serde_json::Value =
            serde_json::from_str(&render_provider_config(&a, "k", "n", true).unwrap()).unwrap();
        let pve: serde_json::Value =
            serde_json::from_str(&render_provider_config(&a, "k", "n", false).unwrap()).unwrap();

        assert_eq!(lxd["backend_type"], "LXD");
        assert_eq!(lxd["proxmox_storage"], "default");
        assert_eq!(lxd["proxmox_bridge"], "lxdbr0");
        assert_eq!(pve["backend_type"], "Proxmox");
        assert_eq!(pve["proxmox_storage"], "local-lvm");
        assert_eq!(pve["proxmox_bridge"], "vmbr0");
    }
}
