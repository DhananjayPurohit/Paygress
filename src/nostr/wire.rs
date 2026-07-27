// Payload types carried in Nostr event bodies and encrypted DMs.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::kinds::{KIND_LEASE_REVOCATION, SCHEMA_VERSION};

fn default_schema_version() -> u8 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NostrEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
    /// `nip04`, `nip17`, or `lease_revocation`.
    pub message_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodSpec {
    /// e.g. `basic`, `standard`, `premium`.
    pub id: String,
    pub name: String,
    pub description: String,
    pub cpu_millicores: u64,
    pub memory_mb: u64,
    pub rate_msats_per_sec: u64,
}

/// One workload port a template-spawned container exposes. Distinct from
/// `AccessDetailsContent.node_port`, which is the SSH forwarding port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateAccessPort {
    pub host_port: u16,
    pub container_port: u16,
    /// `tcp`, `http`, `ws`, `bitcoin-rpc`, ...
    pub protocol: String,
    /// Role label from the template definition (`relay-ws`, `rpc`, ...) so
    /// clients can route by role rather than guessing port-by-port.
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessDetailsContent {
    pub pod_npub: String,
    /// SSH port.
    pub node_port: u16,
    pub expires_at: String,
    pub cpu_millicores: u64,
    pub memory_mb: u64,
    pub pod_spec_name: String,
    pub pod_spec_description: String,
    pub instructions: Vec<String>,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host_address: String,

    /// Empty for non-template spawns; absent on the wire for old clients.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub template_ports: Vec<TemplateAccessPort>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponseContent {
    /// e.g. `insufficient_payment`, `invalid_spec`, `image_not_found`.
    pub error_type: String,
    pub message: String,
    pub details: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopUpResponseContent {
    pub success: bool,
    pub pod_npub: String,
    pub extended_duration_seconds: u64,
    pub new_expires_at: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedSpawnPodRequest {
    pub cashu_token: String,
    /// Defaults to the provider's first spec when absent.
    pub pod_spec_id: Option<String>,
    pub pod_image: String,
    pub ssh_username: String,
    pub ssh_password: String,

    /// When set, the provider materializes image / ports / env from its own
    /// local template registry rather than trusting consumer-supplied bytes,
    /// so a consumer cannot smuggle an arbitrary image past the vetted list.
    /// `pod_image` is ignored when this resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_slug: Option<String>,

    /// For `WarmStandby`, the consumer sends the *same* request to every
    /// provider in the set; each self-determines its role from
    /// `primary_npub` / `standby_providers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication: Option<crate::durable_workload::ReplicationMode>,

    /// Required when `replication` is `WarmStandby`; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_npub: Option<String>,

    /// Consumer-assigned identifier shared by the primary and all standbys, so
    /// a `LeaseRevocation` names one workload across providers. Unset for
    /// single-provider spawns, where the provider derives a vmid-based id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_id: Option<String>,

    /// LUKS-encrypts `template.data_path` and destroys the header at tenancy
    /// end. Protects against cold-disk forensics only — a live host can still
    /// read process memory and the kernel keyring. The key rides inside the
    /// encrypted DM, so relays never see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_encryption: Option<VolumeEncryption>,
}

/// Request to encrypt the workload's data volume. `algorithm` is a
/// forward-compat tag: v1 accepts `luks2-aes-xts` only, and providers reject
/// unknown algorithms loudly rather than falling back to a plain volume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeEncryption {
    #[serde(default = "volume_encryption_default_version")]
    pub version: u8,

    pub algorithm: String,

    /// 32-byte key, base64 URL-safe unpadded, fed to `cryptsetup luksFormat`
    /// as a raw passphrase.
    pub key_b64: String,
}

fn volume_encryption_default_version() -> u8 {
    1
}

impl VolumeEncryption {
    pub const ALGORITHM_V1: &'static str = "luks2-aes-xts";
    pub const VERSION_V1: u8 = 1;

    pub fn v1(key: [u8; 32]) -> Self {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        Self {
            version: Self::VERSION_V1,
            algorithm: Self::ALGORITHM_V1.to_string(),
            key_b64: URL_SAFE_NO_PAD.encode(key),
        }
    }

    pub fn decoded_key(&self) -> Result<[u8; 32], anyhow::Error> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let bytes = URL_SAFE_NO_PAD
            .decode(self.key_b64.as_bytes())
            .map_err(|e| anyhow::anyhow!("volume_encryption.key_b64 invalid base64: {}", e))?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "volume_encryption.key_b64 decoded to {} bytes, expected 32",
                bytes.len()
            );
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedTopUpPodRequest {
    pub pod_npub: String,
    pub cashu_token: String,
}

/// Unified request type for private messages. `Spawn` is boxed because it
/// dwarfs the other variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrivateRequest {
    Spawn(Box<EncryptedSpawnPodRequest>),
    TopUp(EncryptedTopUpPodRequest),
    Status(StatusRequestContent),
}

pub fn parse_private_message_content(content: &str) -> Result<PrivateRequest> {
    serde_json::from_str::<PrivateRequest>(content).map_err(|e| {
        // Truncate by chars, not bytes: `content` is attacker-supplied and
        // slicing it at byte 100 would panic mid-codepoint.
        let truncated = if content.chars().count() > 100 {
            format!("{}...", content.chars().take(100).collect::<String>())
        } else {
            content.to_string()
        };
        anyhow::anyhow!("JSON parsing failed: {}. Content: '{}'", e, truncated)
    })
}

/// `None` when the event is not a revocation, so callers can fall through to
/// other dispatch arms.
pub fn parse_revocation_event(event: &NostrEvent) -> Option<LeaseRevocationContent> {
    if event.kind != KIND_LEASE_REVOCATION as u32 {
        return None;
    }
    serde_json::from_str::<LeaseRevocationContent>(&event.content).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityInfo {
    /// Millicores.
    pub cpu_available: u64,
    pub memory_mb_available: u64,
    pub storage_gb_available: u64,
}

/// Isolation level a provider promises. `#[serde(default)]` on the fields that
/// carry it so v0 offers parse cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationLevel {
    /// Default LXC / shared-kernel container.
    #[default]
    SharedKernel,
    /// Whole host dedicated to a single workload (no co-tenants).
    DedicatedHost,
    /// Attested AMD SEV-SNP / Intel TDX.
    AttestedResearchTier,
}

impl IsolationLevel {
    /// Strength ordering for "minimum acceptable tier" comparisons; higher is
    /// more isolated. Not part of the wire format, which is the slug.
    pub fn rank(self) -> u8 {
        match self {
            Self::SharedKernel => 0,
            Self::DedicatedHost => 1,
            Self::AttestedResearchTier => 2,
        }
    }

    pub fn meets(self, min: IsolationLevel) -> bool {
        self.rank() >= min.rank()
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "shared-kernel" => Some(Self::SharedKernel),
            "dedicated-host" => Some(Self::DedicatedHost),
            "attested-research-tier" => Some(Self::AttestedResearchTier),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::SharedKernel => "shared-kernel",
            Self::DedicatedHost => "dedicated-host",
            Self::AttestedResearchTier => "attested-research-tier",
        }
    }
}

/// Body of a `KIND_PROVIDER_OFFER` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderOfferContent {
    pub provider_npub: String,
    pub hostname: String,
    pub location: Option<String>,
    /// e.g. `["lxc", "vm"]`.
    pub capabilities: Vec<String>,
    pub specs: Vec<PodSpec>,
    pub whitelisted_mints: Vec<String>,
    pub uptime_percent: f32,
    pub total_jobs_completed: u64,
    pub api_endpoint: Option<String>,

    /// v0 offers (no field on the wire) deserialize to `1`.
    #[serde(default = "default_schema_version")]
    pub version: u8,

    #[serde(default)]
    pub isolation_level: IsolationLevel,

    /// Fidelity-bond stake. Offers with a verifiable proof are eligible for
    /// the `staked` discovery tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stake_proof: Option<crate::stake::StakeProof>,
}

/// Body of both heartbeat kinds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatContent {
    pub provider_npub: String,
    pub timestamp: u64,
    pub active_workloads: u32,
    pub available_capacity: CapacityInfo,

    #[serde(default = "default_schema_version")]
    pub version: u8,
}

/// Body of a `KIND_LEASE_REVOCATION` event. A standby can promote on seeing one
/// without risking two writers, because the primary has already stood down.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRevocationContent {
    /// The consumer-assigned `workload_id` from the spawn request; standbys key
    /// their slot table by it.
    pub workload_id: String,
    pub primary_provider_npub: String,
    pub standby_providers: Vec<String>,
    pub reason: String,
    pub revoked_at: u64,

    /// Blossom URI of the latest checkpoint. When set, the standby restores
    /// from it rather than spawning a fresh container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_uri: Option<String>,

    #[serde(default = "default_schema_version")]
    pub version: u8,
}

/// Body of a `KIND_STANDBY_PROMOTION_ANNOUNCEMENT` event. Unlike a heartbeat
/// ("this provider is online"), it asserts "this provider has claimed this
/// workload's primary role"; peers query for it before claiming their own slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandbyPromotionAnnouncementContent {
    pub workload_id: String,
    pub new_primary_npub: String,
    pub promoted_at: u64,
    #[serde(default = "default_schema_version")]
    pub version: u8,
}

/// Provider info as seen by discovery clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub npub: String,
    pub hostname: String,
    pub location: Option<String>,
    pub capabilities: Vec<String>,
    pub specs: Vec<PodSpec>,
    pub whitelisted_mints: Vec<String>,
    pub uptime_percent: f32,
    pub total_jobs_completed: u64,
    pub last_seen: u64,
    pub is_online: bool,
    pub isolation_level: IsolationLevel,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderFilter {
    pub capability: Option<String>,
    pub min_uptime: Option<f32>,
    pub min_memory_mb: Option<u64>,
    pub min_cpu: Option<u64>,
    /// Minimum acceptable tier; stricter tiers also match.
    pub isolation_level: Option<IsolationLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRequestContent {
    /// NPUB or container id.
    pub pod_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponseContent {
    pub pod_id: String,
    pub status: String,
    pub expires_at: String,
    pub time_remaining_seconds: u64,
    pub cpu_millicores: u64,
    pub memory_mb: u64,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub ssh_username: String,
}

#[cfg(test)]
mod isolation_level_tests {
    use super::IsolationLevel;

    #[test]
    fn rank_orders_isolation_strength() {
        assert!(IsolationLevel::SharedKernel.rank() < IsolationLevel::DedicatedHost.rank());
        assert!(IsolationLevel::DedicatedHost.rank() < IsolationLevel::AttestedResearchTier.rank());
    }

    #[test]
    fn meets_accepts_equal_or_stricter_tiers() {
        assert!(IsolationLevel::SharedKernel.meets(IsolationLevel::SharedKernel));
        assert!(IsolationLevel::DedicatedHost.meets(IsolationLevel::SharedKernel));
        assert!(IsolationLevel::AttestedResearchTier.meets(IsolationLevel::SharedKernel));
        assert!(!IsolationLevel::SharedKernel.meets(IsolationLevel::DedicatedHost));
        assert!(IsolationLevel::DedicatedHost.meets(IsolationLevel::DedicatedHost));
        assert!(IsolationLevel::AttestedResearchTier.meets(IsolationLevel::DedicatedHost));
        assert!(!IsolationLevel::SharedKernel.meets(IsolationLevel::AttestedResearchTier));
        assert!(!IsolationLevel::DedicatedHost.meets(IsolationLevel::AttestedResearchTier));
        assert!(IsolationLevel::AttestedResearchTier.meets(IsolationLevel::AttestedResearchTier));
    }

    #[test]
    fn slug_round_trips() {
        for level in [
            IsolationLevel::SharedKernel,
            IsolationLevel::DedicatedHost,
            IsolationLevel::AttestedResearchTier,
        ] {
            assert_eq!(IsolationLevel::from_slug(level.slug()), Some(level));
        }
    }

    #[test]
    fn from_slug_rejects_unknown() {
        assert!(IsolationLevel::from_slug("paranoid-mode").is_none());
        assert!(IsolationLevel::from_slug("").is_none());
        assert!(IsolationLevel::from_slug("dedicated_host").is_none());
    }
}
