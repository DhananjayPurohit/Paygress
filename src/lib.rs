// Paygress Library
//
// Exports modules for use in binaries.
//
// Architecture notes:
// - **Canonical control plane (default)**: Nostr NIP-17 →
//   `ProviderService` → `ProxmoxClient` / `LxdBackend`. Always
//   compiled.
// - **Legacy K8s + ngx_l402 + HTTP control plane** (gated behind the
//   `kubernetes` Cargo feature, off by default since Unit 7): nginx
//   + ngx_l402 → `PodProvisioningService`. Kept compilable so
//   existing K8s users can opt back in with
//   `--features kubernetes`, but no longer in the default build
//   path so engineering capacity flows to the canonical plane.

// Core modules — always compiled.
pub mod blossom;
pub mod blossom_crypto;
pub mod cashu;
pub mod durable_workload;
pub mod nostr;
pub mod observatory;
pub mod reputation;
pub mod stake;
pub mod templates;

// Proxmox / LXD canonical control plane — always compiled.
pub mod compute;
pub mod discovery;
pub mod docker;
pub mod lxd;
pub mod provider;
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
    HeartbeatContent, IsolationLevel, PrivateRequest, ProviderFilter, ProviderInfo,
    ProviderOfferContent, StatusRequestContent, StatusResponseContent, TemplateAccessPort,
    TopUpResponseContent, SCHEMA_VERSION,
};
pub use provider::{ProviderConfig, ProviderService};
pub use proxmox::ProxmoxClient;

// K8s-only re-export.
#[cfg(feature = "kubernetes")]
pub use cashu::initialize_cashu;
