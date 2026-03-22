# [Paygress](https://paygress.net)

**Pay-per-use compute with Lightning + Nostr. No accounts, no signups.**

https://github.com/user-attachments/assets/627d2bb1-1a9b-4e66-bc42-7c91a1804fe1

Paygress is a marketplace where anyone can buy or sell compute resources using Cashu ecash tokens. Providers advertise on Nostr, consumers discover and pay - all anonymous, all instant.

## Prerequisites

Install Rust via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Install required system libraries (Ubuntu/Debian):

```bash
sudo apt update && sudo apt install -y pkg-config libssl-dev
```

---

## Install

```bash
cargo install paygress-cli
```

---

## For Consumers

### 1. Find a provider

```bash
# Browse all providers on Nostr
paygress-cli list

# Filter and sort
paygress-cli list --online-only --sort price

# Get details on a specific provider
paygress-cli list info <PROVIDER_NPUB>
```

### 2. Spawn a workload

Get a Cashu token from a wallet like [Nutstash](https://nutstash.app/) or [Minibits](https://www.minibits.cash/), then:

```bash
paygress-cli spawn \
  --provider <PROVIDER_NPUB> \
  --tier basic \
  --token "cashuA..."
```

SSH credentials are auto-generated and displayed after provisioning. You can also set them explicitly with `--ssh-user` and `--ssh-pass`.

The CLI auto-generates a Nostr identity at `~/.paygress/identity` on first use.

### 3. Connect

```bash
ssh -p <PORT> root@<PROVIDER_IP>
```

### 4. Check status

```bash
paygress-cli status --pod-id <ID> --provider <NPUB>
```

### HTTP Mode

For centralized deployments (Kubernetes + Nginx L402 paywall), pass `--server` instead of `--provider`:

```bash
paygress-cli list --server http://my-server:8080
paygress-cli spawn --server http://my-server:8080 --tier basic --token "cashuA..."
paygress-cli status --server http://my-server:8080 --pod-id <ID>
```

---

## For Providers

### Quick Start: One-Click Bootstrap

Set up any Linux VPS as a provider with a single command:

```bash
# With SSH password (requires sshpass: apt install sshpass / brew install hudochenkov/sshpass/sshpass)
paygress-cli bootstrap \
  --host <YOUR_SERVER_IP> \
  --user root \
  --password "your-ssh-password" \
  --name "My Node" \
  --mints "https://testnut.cashu.space"

# With SSH key (no extra dependencies)
paygress-cli bootstrap \
  --host <YOUR_SERVER_IP> \
  --user root \
  --key ~/.ssh/id_rsa \
  --name "My Node" \
  --mints "https://testnut.cashu.space"
```

This will SSH into your server, install LXD (on Ubuntu) or Proxmox (on Debian), compile Paygress, configure a systemd service, and start broadcasting offers to Nostr.

**Requirements:** Linux with systemd, root/sudo access. Public IP recommended (or use WireGuard tunnel below).

### Manual Setup

```bash
# 1. Setup (generates config at provider-config.json)
paygress-cli provider setup \
  --proxmox-url https://127.0.0.1:8006/api2/json \
  --token-id "root@pam!paygress" \
  --token-secret "<SECRET>" \
  --name "My Provider" \
  --mints "https://testnut.cashu.space"

# 2. Start
paygress-cli provider start --config provider-config.json

# 3. Check status
paygress-cli provider status
```

### Provider Management

```bash
# Stop the service
paygress-cli provider stop

# View live logs
journalctl -u paygress-provider -f

# Reset (remove all Paygress data from a server)
paygress-cli system reset --host <IP> --user root
```

### Running Behind NAT (No Public IP)

If your machine doesn't have a public IP (e.g., home server behind a router), use a WireGuard VPN tunnel to get one:

```bash
# Install WireGuard (Ubuntu/Debian)
sudo apt install wireguard wireguard-tools
```

```bash
# Pay for a VPN tunnel with a Cashu token
paygress-cli provider tunnel \
  --vpn-url https://vpn.cashu.icu \
  --token "cashuA..."
```

This installs WireGuard (if needed), downloads a VPN config, starts the tunnel, and updates your provider config with the public IP and port range. Restart the provider service after:

```bash
systemctl restart paygress-provider
```

Your provider is now reachable through the VPN tunnel. Consumers SSH to the tunnel's public IP.

---

## Supported Backends

| Backend | Best For | Status |
|---------|----------|--------|
| **LXD** | Ubuntu VPS, bare metal | Verified |
| **Proxmox** | Home labs, Debian servers | Verified |
| **Kubernetes** | Scalable cloud (HTTP/L402 mode) | Beta |

## Architecture

**Decentralized (Nostr + LXD/Proxmox):**
Provider publishes offers (Kind 38383) and heartbeats (Kind 38384) to Nostr relays. Consumer sends encrypted spawn request with Cashu token. Provider verifies payment, creates container, returns SSH credentials - all via encrypted Nostr DMs.

**Centralized (Kubernetes):**
Nginx with `ngx_l402` validates Cashu tokens. Paygress provisions K8s pods with SSH access. Clients interact via HTTP API.

---
