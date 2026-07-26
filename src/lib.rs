// Paygress library.
//
// Two payment paths share one CDK SQLite wallet
// (`/var/lib/paygress/cashu-wallet.sqlite`):
//
//   Nostr-DM (default) — client sends a Cashu token over NIP-17;
//   `ProviderService` redeems it via NUT-03 and provisions on the
//   configured `ComputeBackend`.
//
//   HTTP + ngx_l402 (activated by `http_bind_addr`) — ngx_l402 redeems
//   at the nginx layer, then forwards to the axum backend in
//   `provider_http`.
//
// ngx_l402 melts accumulated proofs from both paths to Lightning. Its
// wallet seed is derived from the provider's Nostr key, so both sides
// open the same wallet.

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

pub mod compute;
pub mod discovery;
pub mod docker;
pub mod kvm;
pub mod luks;
pub mod lxd;
pub mod provider;
pub mod provider_http;
pub mod proxmox;

// Re-exports.
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
