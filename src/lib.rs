// Paygress Library
//
// Exports modules for use in binaries
// Payment verification handled by ngx_l402 at nginx layer

// Core modules
pub mod cashu;
pub mod nostr;
pub mod pod_provisioning;
pub mod sidecar_service;

// Proxmox integration modules
pub mod compute;
pub mod discovery;
pub mod lxd;
pub mod provider;
pub mod proxmox;

// Re-export public types and functions
pub use cashu::initialize_cashu;
pub use compute::{ComputeBackend, ContainerConfig, NodeStatus};
pub use discovery::DiscoveryClient;
pub use lxd::LxdBackend;
pub use nostr::{custom_relay_config, default_relay_config, NostrRelaySubscriber, RelayConfig};
pub use nostr::{
    AccessDetailsContent, CapacityInfo, EncryptedTopUpPodRequest, ErrorResponseContent,
    HeartbeatContent, IsolationLevel, PrivateRequest, ProviderFilter, ProviderInfo,
    ProviderOfferContent, StatusRequestContent, StatusResponseContent, TopUpResponseContent,
    SCHEMA_VERSION,
};
pub use provider::{ProviderConfig, ProviderService};
pub use proxmox::ProxmoxClient;

// Architecture notes:
// - K8s mode: nginx + ngx_l402 → PodProvisioningService
// - Proxmox mode: Nostr NIP-17 → ProviderService → ProxmoxClient
