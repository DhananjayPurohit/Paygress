// Paygress library.
//
// Two payment paths share one CDK SQLite wallet at
// `/var/lib/paygress/cashu-wallet.sqlite`: the default Nostr-DM path,
// where `ProviderService` redeems a NIP-17-delivered Cashu token via
// NUT-03, and the HTTP path (enabled by `http_bind_addr`), where
// ngx_l402 redeems at the nginx layer before forwarding to
// `provider_http`. ngx_l402 melts proofs from both paths to
// Lightning; its wallet seed is derived from the provider's Nostr
// key, which is what makes both sides open the same wallet.

pub mod blossom;
pub mod blossom_crypto;
pub mod cashu;
pub mod client;
pub mod compute;
pub mod discovery;
pub mod docker;
pub mod durable_workload;
pub mod kvm;
pub mod luks;
pub mod lxd;
pub mod namegen;
pub mod nostr;
pub mod observatory;
pub mod provider;
pub mod provider_http;
pub mod proxmox;
pub mod reputation;
pub mod stake;
pub mod templates;
pub mod volume_encryption;

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
