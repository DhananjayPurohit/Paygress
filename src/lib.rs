// Paygress Library
//
// Exports modules for use in binaries.
//
// Architecture — two payment paths, one shared wallet, one sweep:
//
// Path A — Nostr-DM (default, always compiled):
//   Client sends Cashu token via encrypted Nostr DM (NIP-17).
//   `ProviderService` → `CdkRedeemer` swaps the token at the mint via
//   NUT-03, storing proofs in the shared CDK wallet
//   (`/var/lib/paygress/cashu-wallet.redb`). Backend is
//   `ProxmoxClient` / `LxdBackend` / `DockerBackend` / `KvmBackend`.
//
// Path B — HTTP + ngx_l402 (always compiled, activated by `http_bind_addr`):
//   Client hits nginx; ngx_l402 redeems the Cashu token into the same
//   shared redb wallet, then forwards the authenticated request to the
//   axum backend in `src/provider_http.rs`. Bootstrap auto-generates
//   the ngx_l402 Docker Compose config (Step 8 in bootstrap.rs).
//
// Lightning sweep (both paths):
//   ngx_l402 mounts `/var/lib/paygress` as its data volume and runs a
//   periodic melt loop (`CASHU_REDEMPTION_INTERVAL_SECS`) that converts
//   ALL accumulated proofs — from both Path A and Path B — into Lightning
//   payments sent to `LNURL_ADDRESS` (= `lightning_address` in config).
//   The wallet secret is derived deterministically from the Nostr private
//   key (`derive_seed_from_nostr_key`), ensuring both sides always open
//   the same wallet.
//
// Legacy K8s pipeline (gated behind `kubernetes` Cargo feature):
//   nginx + ngx_l402 → `PodProvisioningService`. Kept compilable for
//   existing K8s users (`--features kubernetes`). Not in the default build.

// Core modules — always compiled.
pub mod blossom;
pub mod blossom_crypto;
pub mod cashu;
pub mod client;
pub mod durable_workload;
pub mod namegen;
pub mod nostr;
pub mod observatory;
pub mod reputation;
pub mod stake;
pub mod templates;
pub mod volume_encryption;

// Proxmox / LXD canonical control plane — always compiled.
pub mod compute;
pub mod discovery;
pub mod docker;
pub mod kvm;
pub mod luks;
pub mod lxd;
pub mod provider;
pub mod provider_http; // HTTP+ngx_l402 interface for Proxmox/LXD/KVM providers
pub mod proxmox;

// Legacy K8s pipeline — feature-gated behind `kubernetes`.
#[cfg(feature = "kubernetes")]
pub mod pod_provisioning;
#[cfg(feature = "kubernetes")]
pub mod sidecar_service;

// Re-export public types and functions (always-compiled surface).
pub use compute::{ComputeBackend, ContainerConfig, NodeStatus};
pub use discovery::DiscoveryClient;
pub use lxd::LxdBackend;
pub use nostr::{custom_relay_config, default_relay_config, NostrRelaySubscriber, RelayConfig};
pub use nostr::{
    AccessDetailsContent, CapacityInfo, EncryptedTopUpPodRequest, ErrorResponseContent,
    HeartbeatContent, IsolationLevel, LeaseRevocationContent, PrivateRequest, ProviderFilter,
    ProviderInfo, ProviderOfferContent, StatusRequestContent, StatusResponseContent,
    TemplateAccessPort, TopUpResponseContent, KIND_LEASE_REVOCATION, SCHEMA_VERSION,
};
pub use provider::{ProviderConfig, ProviderService};
pub use proxmox::ProxmoxClient;

// K8s-only re-export.
#[cfg(feature = "kubernetes")]
pub use cashu::initialize_cashu;
