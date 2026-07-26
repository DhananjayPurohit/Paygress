// Nostr client for receiving pod provisioning events with private messaging
use anyhow::{Context, Result};
use nostr_sdk::nips::nip04;
use nostr_sdk::nips::nip59::UnwrappedGift;
use nostr_sdk::{
    Client, EventBuilder, Filter, Keys, Kind, RelayPoolNotification, Tag, Timestamp, ToBech32,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// NIP-33 parameterized-replaceable, `d = paygress:offer:v1:<npub>`.
pub const KIND_PROVIDER_OFFER: u16 = 38383;
/// NIP-33 addressable heartbeat. The `d` tag is bucketed per minute
/// (`paygress:heartbeat:v1:<npub>:<bucket>`) so heartbeats accumulate as
/// queryable history instead of each one replacing the last.
pub const KIND_PROVIDER_HEARTBEAT: u16 = 38384;
/// NIP-16 ephemeral heartbeat. Relays do not store these, so they serve live
/// presence only; both kinds are published on every beat.
pub const KIND_PROVIDER_HEARTBEAT_EPHEMERAL: u16 = 20384;
/// Lease revocation, published by a primary whose `WarmStandby` workload has
/// left `Live`. Addressable (`d = paygress:revocation:v1:<primary>:<workload>`)
/// so a standby that comes online later still sees it; each standby is added as
/// a `#p` tag for filterable subscriptions.
pub const KIND_LEASE_REVOCATION: u16 = 38385;
/// Published by a standby right after it promotes itself to primary. Peers
/// check for one of these before claiming their own slot, so only the winner of
/// the promotion race spawns a container. Addressable on
/// `d = paygress:promoted:v1:<workload_id>`.
pub const KIND_STANDBY_PROMOTION_ANNOUNCEMENT: u16 = 38386;

/// Schema version for offer + heartbeat payloads. Old payloads without this
/// field deserialize to `1` via `#[serde(default)]`.
pub const SCHEMA_VERSION: u8 = 1;

/// Window for "is this provider alive right now?" lookups.
pub const LIVE_HEARTBEAT_WINDOW_SECS: u64 = 300;

/// Heartbeat `d`-tag bucket size, matching the 60s heartbeat cadence so every
/// beat lands in its own `(npub, kind, d-tag)` slot.
pub const HEARTBEAT_BUCKET_SECS: u64 = 60;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub relays: Vec<String>,
    pub private_key: Option<String>,
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

#[derive(Clone)]
pub struct NostrRelaySubscriber {
    client: Client,
    keys: Keys,
}

impl NostrRelaySubscriber {
    pub async fn new(config: RelayConfig) -> Result<Self> {
        let keys = match &config.private_key {
            // `Keys::parse` accepts both nsec and raw hex.
            Some(private_key) if !private_key.is_empty() => {
                Keys::parse(private_key).context("Invalid private key format")?
            }
            _ => Keys::generate(),
        };

        let client = Client::new(keys.clone());

        for relay_url in &config.relays {
            info!("Adding relay: {}", relay_url);
            client
                .add_relay(relay_url)
                .await
                .with_context(|| format!("Invalid relay URL: {}", relay_url))?;
        }

        info!("Connecting to {} relays...", config.relays.len());
        client.connect().await;

        // `connect()` returns before the sockets are up.
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        info!("Connected to {} relays", config.relays.len());
        info!(
            "Service public key (npub): {}",
            keys.public_key().to_bech32().unwrap()
        );

        Ok(Self { client, keys })
    }

    pub fn public_key(&self) -> nostr_sdk::PublicKey {
        self.keys.public_key()
    }

    pub async fn subscribe_to_pod_events<F>(&self, handler: F) -> Result<()>
    where
        F: Fn(NostrEvent) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        // `pubkeys` sets the #p tag, i.e. messages addressed to us.
        let nip04_filter = Filter::new()
            .kind(Kind::EncryptedDirectMessage)
            .pubkeys(vec![self.keys.public_key()])
            .limit(0);

        let nip17_filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkeys(vec![self.keys.public_key()])
            .limit(0);

        let revocation_filter = Filter::new()
            .kind(Kind::Custom(KIND_LEASE_REVOCATION))
            .pubkeys(vec![self.keys.public_key()])
            .limit(0);

        let _ = self.client.subscribe(nip04_filter, None).await;
        let _ = self.client.subscribe(nip17_filter, None).await;
        let _ = self.client.subscribe(revocation_filter, None).await;
        info!("Subscribed to NIP-04 / NIP-17 messages and KIND_LEASE_REVOCATION events addressed to this provider");

        self.client.handle_notifications(|notification| async {
            if let RelayPoolNotification::Event { relay_url: _, subscription_id: _, event } = notification {
                match event.kind {
                    Kind::GiftWrap => {
                        info!("Received NIP-17 Gift Wrap message: {}", event.id);

                        // Unwrap the Gift Wrap to get the inner message
                        match self.client.unwrap_gift_wrap(&event).await {
                            Ok(UnwrappedGift { rumor, sender }) => {
                                info!("Unwrapped Gift Wrap from sender: {}, rumor kind: {}", sender, rumor.kind);

                                if rumor.kind == Kind::PrivateDirectMessage {
                                    debug!("NIP-17 rumor is PrivateDirectMessage. Content length: {}", rumor.content.len());

                                    let nostr_event = NostrEvent {
                                        id: rumor.id.map(|id| id.to_hex()).unwrap_or_else(|| "unknown".to_string()),
                                        pubkey: rumor.pubkey.to_hex(),
                                        created_at: rumor.created_at.as_u64(),
                                        kind: rumor.kind.as_u16() as u32,
                                        tags: rumor.tags.iter().map(|tag| {
                                            tag.as_slice().iter().map(|s| s.to_string()).collect()
                                        }).collect(),
                                        content: rumor.content,
                                        sig: "unsigned".to_string(), // rumors are unsigned by construction
                                        message_type: "nip17".to_string(),
                                    };

                                    match handler(nostr_event).await {
                                        Ok(()) => {
                                            info!("Successfully processed NIP-17 private message: {}", event.id);
                                        }
                                        Err(e) => {
                                            error!("Failed to process NIP-17 private message {}: {}", event.id, e);
                                        }
                                    }
                                } else {
                                    info!("Rumor is not a private direct message, kind: {}", rumor.kind);
                                }
                            }
                            Err(e) => {
                                error!("Failed to unwrap Gift Wrap {}: {}", event.id, e);
                            }
                        }
                    }
                    Kind::EncryptedDirectMessage => {
                        info!("Received NIP-04 Encrypted Direct Message: {}", event.id);

                        let secret_key = self.keys.secret_key();
                        match nip04::decrypt(secret_key, &event.pubkey, &event.content) {
                            Ok(decrypted_content) => {
                                debug!(
                                    "Decrypted NIP-04 message. Length: {}",
                                    decrypted_content.len()
                                );

                                let nostr_event = NostrEvent {
                                    id: event.id.to_hex(),
                                    pubkey: event.pubkey.to_hex(),
                                    created_at: event.created_at.as_u64(),
                                    kind: event.kind.as_u16() as u32,
                                    tags: event
                                        .tags
                                        .iter()
                                        .map(|tag| {
                                            tag.as_slice()
                                                .iter()
                                                .map(|s| s.to_string())
                                                .collect()
                                        })
                                        .collect(),
                                    content: decrypted_content,
                                    sig: event.sig.to_string(),
                                    message_type: "nip04".to_string(),
                                };

                                match handler(nostr_event).await {
                                    Ok(()) => info!(
                                        "Successfully processed NIP-04 private message: {}",
                                        event.id
                                    ),
                                    Err(e) => error!(
                                        "Failed to process NIP-04 private message {}: {}",
                                        event.id, e
                                    ),
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Failed to decrypt NIP-04 message {}: {}",
                                    event.id, e
                                );
                            }
                        }
                    }
                    Kind::Custom(k) if k == KIND_LEASE_REVOCATION => {
                        // Public events — no decryption; the handler dispatches
                        // by kind and parses with parse_revocation_event.
                        info!("Received lease revocation event: {}", event.id);
                        let nostr_event = NostrEvent {
                            id: event.id.to_hex(),
                            pubkey: event.pubkey.to_hex(),
                            created_at: event.created_at.as_u64(),
                            kind: event.kind.as_u16() as u32,
                            tags: event
                                .tags
                                .iter()
                                .map(|tag| {
                                    tag.as_slice().iter().map(|s| s.to_string()).collect()
                                })
                                .collect(),
                            content: event.content.clone(),
                            sig: event.sig.to_string(),
                            message_type: "lease_revocation".to_string(),
                        };
                        if let Err(e) = handler(nostr_event).await {
                            error!("Failed to process lease revocation {}: {}", event.id, e);
                        }
                    }
                    _ => {
                        info!("Received unsupported event kind: {}", event.kind);
                    }
                }
            }
            Ok(false) // Continue listening
        }).await?;

        Ok(())
    }

    /// Send an encrypted DM. `message_type` selects NIP-04; anything else
    /// (including `"nip17"`) uses NIP-17.
    pub async fn send_encrypted_private_message(
        &self,
        receiver_pubkey: &str,
        content: String,
        message_type: &str,
    ) -> Result<String> {
        let receiver_pubkey_parsed = nostr_sdk::PublicKey::parse(receiver_pubkey)?;

        match message_type {
            "nip04" => {
                let secret_key = self.keys.secret_key();
                let encrypted_content =
                    nip04::encrypt(secret_key, &receiver_pubkey_parsed, &content)?;
                let receiver_tag = Tag::public_key(receiver_pubkey_parsed);
                let alt_tag = Tag::parse(["alt", "Private Message"])?;

                let event = EventBuilder::new(Kind::EncryptedDirectMessage, encrypted_content)
                    .tags([receiver_tag, alt_tag])
                    .sign_with_keys(&self.keys)?;
                let event_id = self.client.send_event(&event).await?;
                info!("Sent NIP-04 message to {}: {:?}", receiver_pubkey, event_id);
                Ok(event_id.val.to_hex())
            }
            _ => {
                let event_id = self
                    .client
                    .send_private_msg(receiver_pubkey_parsed, content, [])
                    .await?;
                info!("Sent NIP-17 message to {}: {:?}", receiver_pubkey, event_id);
                Ok(event_id.val.to_hex())
            }
        }
    }

    pub async fn send_access_details_private_message(
        &self,
        request_pubkey: &str,
        details: AccessDetailsContent,
        message_type: &str,
    ) -> Result<String> {
        let details_json = serde_json::to_string(&details)?;
        self.send_encrypted_private_message(request_pubkey, details_json, message_type)
            .await
    }

    pub async fn send_status_response(
        &self,
        request_pubkey: &str,
        response: StatusResponseContent,
        message_type: &str,
    ) -> Result<String> {
        let response_json = serde_json::to_string(&response)?;
        self.send_encrypted_private_message(request_pubkey, response_json, message_type)
            .await
    }

    pub async fn send_error_response(
        &self,
        request_pubkey: &str,
        error_type: &str,
        message: &str,
        details: Option<&str>,
        message_type: &str,
    ) -> Result<String> {
        let error = ErrorResponseContent {
            error_type: error_type.to_string(),
            message: message.to_string(),
            details: details.map(|s| s.to_string()),
        };
        self.send_error_response_private_message(request_pubkey, error, message_type)
            .await
    }

    pub async fn send_error_response_private_message(
        &self,
        request_pubkey: &str,
        error: ErrorResponseContent,
        message_type: &str,
    ) -> Result<String> {
        let error_json = serde_json::to_string(&error)?;
        self.send_encrypted_private_message(request_pubkey, error_json, message_type)
            .await
    }

    pub async fn send_topup_response_private_message(
        &self,
        request_pubkey: &str,
        response: TopUpResponseContent,
        message_type: &str,
    ) -> Result<String> {
        let response_json = serde_json::to_string(&response)?;
        self.send_encrypted_private_message(request_pubkey, response_json, message_type)
            .await
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn get_service_public_key(&self) -> String {
        self.keys.public_key().to_hex()
    }

    /// Wait for a private decrypted message from a specific sender
    pub async fn wait_for_decrypted_message(
        &self,
        sender_pubkey: &str,
        timeout_secs: u64,
    ) -> Result<NostrEvent> {
        let sender_pk = nostr_sdk::PublicKey::parse(sender_pubkey)?;
        let receiver_pk = self.keys.public_key();

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let tx = Arc::new(Mutex::new(Some(tx)));
        let client = self.client.clone();
        let receiver_keys = self.keys.clone();
        let timeout = tokio::time::Duration::from_secs(timeout_secs);

        // `since` is required: relays replay historical events matching a
        // filter, so without it we'd match a stale DM from a previous session
        // instead of the response to the request we just sent. The 60s
        // lookback absorbs consumer/relay clock skew and covers a provider
        // that already replied before we subscribed.
        let subscribe_since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(60);
        let filter = Filter::new()
            .pubkeys(vec![receiver_pk])
            .kinds(vec![Kind::EncryptedDirectMessage, Kind::GiftWrap])
            .since(nostr_sdk::Timestamp::from_secs(subscribe_since));

        let _ = client.subscribe(filter, None).await;

        tokio::select! {
            notification_res = client.handle_notifications(|notification| {
                let tx = tx.clone();
                let receiver_keys = receiver_keys.clone();
                let client = client.clone();

                async move {
                    if let RelayPoolNotification::Event { event, .. } = notification {
                        let mut event_to_send = None;

                        match event.kind {
                            Kind::GiftWrap => {
                                if let Ok(UnwrappedGift { rumor, sender }) = client.unwrap_gift_wrap(&event).await {
                                    if sender == sender_pk && rumor.kind == Kind::PrivateDirectMessage {
                                        event_to_send = Some(NostrEvent {
                                            id: rumor.id.map(|id| id.to_hex()).unwrap_or_default(),
                                            pubkey: sender.to_hex(),
                                            created_at: rumor.created_at.as_u64(),
                                            kind: rumor.kind.as_u16() as u32,
                                            tags: rumor.tags.iter().map(|tag| tag.as_slice().iter().map(|s| s.to_string()).collect()).collect(),
                                            content: rumor.content,
                                            sig: String::new(),
                                            message_type: "nip17".to_string(),
                                        });
                                    }
                                }
                            }
                            Kind::EncryptedDirectMessage => {
                                if event.pubkey == sender_pk {
                                    let secret_key = receiver_keys.secret_key();
                                    if let Ok(content) = nip04::decrypt(secret_key, &event.pubkey, &event.content) {
                                        event_to_send = Some(NostrEvent {
                                            id: event.id.to_hex(),
                                            pubkey: event.pubkey.to_hex(),
                                            created_at: event.created_at.as_u64(),
                                            kind: event.kind.as_u16() as u32,
                                            tags: event.tags.iter().map(|tag| tag.as_slice().iter().map(|s| s.to_string()).collect()).collect(),
                                            content,
                                            sig: event.sig.to_string(),
                                            message_type: "nip04".to_string(),
                                        });
                                    }
                                }
                            }
                            _ => {}
                        }

                        if let Some(ev) = event_to_send {
                            let mut lock = tx.lock().await;
                            if let Some(sender) = lock.take() {
                                let _ = sender.send(ev).await;
                                return Ok(true); // Stop handling notifications
                            }
                        }
                    }
                    Ok(false)
                }
            }) => {
                match notification_res {
                    Ok(_) => rx.recv().await.ok_or_else(|| anyhow::anyhow!("Channel closed")),
                    Err(e) => Err(anyhow::anyhow!("Notification handler error: {}", e)),
                }
            }
            _ = tokio::time::sleep(timeout) => {
                Err(anyhow::anyhow!("Timeout waiting for response from {}", sender_pubkey))
            }
        }
    }
}

pub fn default_relay_config() -> RelayConfig {
    RelayConfig {
        relays: vec![
            "wss://relay.damus.io".to_string(),
            "wss://nos.lol".to_string(),
            "wss://relay.nostr.band".to_string(),
        ],
        private_key: None,
    }
}

pub fn custom_relay_config(relays: Vec<String>, private_key: Option<String>) -> RelayConfig {
    RelayConfig {
        relays,
        private_key,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodSpec {
    /// Spec identifier, e.g. `basic`, `standard`, `premium`.
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
    /// Wire protocol (`tcp`, `http`, `ws`, `bitcoin-rpc`, ...).
    pub protocol: String,
    /// Role label from the template definition (`relay-ws`, `rpc`, ...) so
    /// clients can route by role rather than guessing port-by-port.
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessDetailsContent {
    pub pod_npub: String,
    /// SSH port for direct access.
    pub node_port: u16,
    pub expires_at: String,
    pub cpu_millicores: u64,
    pub memory_mb: u64,
    pub pod_spec_name: String,
    pub pod_spec_description: String,
    pub instructions: Vec<String>,

    /// Host address the consumer connects to, so programmatic clients don't
    /// have to scrape `instructions`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host_address: String,

    /// Empty for non-template spawns; old clients without the field still
    /// deserialize cleanly.
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

    /// When set, the provider gives `template.data_path` a LUKS-encrypted
    /// volume and destroys the header at tenancy end, so post-eviction disk
    /// forensics reveal only ciphertext. This protects against cold-disk
    /// access, not against a live host reading process memory or the kernel
    /// keyring — that needs a confidential VM (`attested-research-tier`).
    ///
    /// The key rides inside the NIP-04/NIP-17-encrypted DM, so it is never
    /// visible on relays, and the provider holds it only in memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_encryption: Option<VolumeEncryption>,
}

/// Wire-format request to encrypt the workload's data volume.
///
/// `algorithm` is a forward-compat tag: v1 accepts `luks2-aes-xts` only, and
/// providers reject unknown algorithms loudly rather than silently falling back
/// to a plain volume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeEncryption {
    /// v1 = LUKS2 + AES-XTS-Plain64 with the key supplied directly. Bump for
    /// new key-derivation flows (e.g. attested key release from a TPM/TEE).
    #[serde(default = "volume_encryption_default_version")]
    pub version: u8,

    pub algorithm: String,

    /// 32-byte key, base64 URL-safe unpadded, fed to `cryptsetup luksFormat`
    /// as a raw passphrase. The consumer derives it from a stable secret plus
    /// the workload id so the same key recurs on respawn or standby promotion.
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

/// Role this provider takes on a `WarmStandby` spawn request:
/// `Primary` if it is the named primary, `Standby` if it appears in
/// `standby_providers`, `NotAddressed` (reject the request) otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmStandbyRole {
    Primary,
    Standby { index: usize, count: usize },
    NotAddressed,
}

pub fn warm_standby_role(
    self_npub: &str,
    primary_npub: &str,
    standby_providers: &[String],
) -> WarmStandbyRole {
    if npubs_equal(self_npub, primary_npub) {
        return WarmStandbyRole::Primary;
    }
    for (idx, p) in standby_providers.iter().enumerate() {
        if npubs_equal(self_npub, p) {
            return WarmStandbyRole::Standby {
                index: idx,
                count: standby_providers.len(),
            };
        }
    }
    WarmStandbyRole::NotAddressed
}

/// True iff two npub strings name the same key. Providers store their own npub
/// as hex while the consumer CLI ships bech32, so both sides must be
/// canonicalized before comparison — direct string comparison silently broke
/// warm-standby for every bech32 consumer.
///
/// Falls back to string equality only when *neither* side parses, which keeps
/// placeholder npubs in unit tests working without risking false positives on
/// real keys.
pub fn npubs_equal(a: &str, b: &str) -> bool {
    match (
        nostr_sdk::PublicKey::parse(a),
        nostr_sdk::PublicKey::parse(b),
    ) {
        (Ok(ka), Ok(kb)) => ka == kb,
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
        (Err(_), Err(_)) => a == b,
    }
}

/// True iff an offer's claimed `provider_npub` matches the key that signed the
/// event (`signer_hex` is `event.pubkey.to_hex()`).
///
/// nostr-sdk verifies the event signature on fetch, but `provider_npub` is a
/// free-form body field. Without this binding any key could publish an offer
/// impersonating any provider and last-write-wins over the real listing.
pub fn offer_authored_by_claimed_provider(signer_hex: &str, offer: &ProviderOfferContent) -> bool {
    npubs_equal(signer_hex, &offer.provider_npub)
}

#[cfg(test)]
mod offer_authenticity_tests {
    use super::*;

    fn offer_claiming(npub: &str) -> ProviderOfferContent {
        ProviderOfferContent {
            provider_npub: npub.to_string(),
            hostname: "attacker-chosen.example".to_string(),
            location: None,
            capabilities: vec![],
            specs: vec![],
            whitelisted_mints: vec![],
            uptime_percent: 100.0,
            total_jobs_completed: 0,
            api_endpoint: None,
            version: SCHEMA_VERSION,
            isolation_level: IsolationLevel::SharedKernel,
            stake_proof: None,
        }
    }

    #[test]
    fn genuine_offer_from_its_signer_is_accepted() {
        let k = Keys::generate();
        let offer = offer_claiming(&k.public_key().to_hex());
        assert!(offer_authored_by_claimed_provider(
            &k.public_key().to_hex(),
            &offer
        ));
    }

    #[test]
    fn offer_claiming_foreign_npub_is_rejected() {
        let victim = Keys::generate();
        let attacker = Keys::generate();
        let forged = offer_claiming(&victim.public_key().to_hex());
        assert!(!offer_authored_by_claimed_provider(
            &attacker.public_key().to_hex(),
            &forged
        ));
    }

    #[test]
    fn signer_binding_canonicalizes_hex_and_bech32() {
        let k = Keys::generate();
        let offer = offer_claiming(&k.public_key().to_bech32().unwrap());
        assert!(offer_authored_by_claimed_provider(
            &k.public_key().to_hex(),
            &offer
        ));
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

/// Parse a `NostrEvent` as a `LeaseRevocationContent`, or `None` if it is not a
/// revocation, so callers can fall through to other dispatch arms.
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

    /// Parse the kebab-case slug used on the CLI and the wire.
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

fn default_schema_version() -> u8 {
    SCHEMA_VERSION
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

/// Body of a `KIND_LEASE_REVOCATION` event, emitted by a primary whose workload
/// has left `Live` (typically split-brain self-eviction after its heartbeats
/// stopped reaching quorum). A standby can promote on seeing this without
/// risking two writers, because the primary has already stood down.
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
    /// Timestamp of the last heartbeat.
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
    /// Minimum acceptable tier; stricter tiers also match. `None` = no filter.
    pub isolation_level: Option<IsolationLevel>,
}

impl NostrRelaySubscriber {
    pub async fn publish_provider_offer(&self, offer: ProviderOfferContent) -> Result<String> {
        let content = serde_json::to_string(&offer)?;
        info!("Publishing provider offer for {}", offer.hostname);

        let d_tag = format!("paygress:offer:v{}:{}", offer.version, offer.provider_npub);
        let tags = vec![
            Tag::hashtag("paygress"),
            Tag::hashtag("compute"),
            Tag::parse(["d", d_tag.as_str()])?,
            Tag::parse(["v", offer.version.to_string().as_str()])?,
        ];

        let event = EventBuilder::new(Kind::Custom(KIND_PROVIDER_OFFER), content)
            .tags(tags)
            .sign_with_keys(&self.keys)?;
        let event_id = event.id.to_hex();

        match self.client.send_event(&event).await {
            Ok(res) => {
                info!("✅ Published provider offer: {} ({:?})", event_id, res);
                Ok(event_id)
            }
            Err(e) => {
                error!("❌ Failed to publish provider offer: {}", e);
                Err(e.into())
            }
        }
    }

    /// Publish a heartbeat on both the stored and ephemeral kinds. Returns the
    /// stored event's id and the relays that accepted it; the orchestrator loop
    /// turns those into `HeartbeatObservation`s to drive the state machine.
    pub async fn publish_heartbeat(
        &self,
        heartbeat: HeartbeatContent,
    ) -> Result<(String, Vec<String>)> {
        let content = serde_json::to_string(&heartbeat)?;
        let bucket = heartbeat.timestamp / HEARTBEAT_BUCKET_SECS;
        let d_tag = format!(
            "paygress:heartbeat:v{}:{}:{}",
            heartbeat.version, heartbeat.provider_npub, bucket
        );

        let provider_pk = nostr_sdk::PublicKey::parse(&heartbeat.provider_npub)?;
        let v_tag = heartbeat.version.to_string();

        let stored_tags = vec![
            Tag::hashtag("paygress-heartbeat"),
            Tag::public_key(provider_pk),
            Tag::parse(["d", d_tag.as_str()])?,
            Tag::parse(["v", v_tag.as_str()])?,
        ];
        let stored_event =
            EventBuilder::new(Kind::Custom(KIND_PROVIDER_HEARTBEAT), content.clone())
                .tags(stored_tags)
                .sign_with_keys(&self.keys)?;
        let stored_id = stored_event.id.to_hex();

        let ephemeral_tags = vec![
            Tag::hashtag("paygress-heartbeat"),
            Tag::public_key(provider_pk),
            Tag::parse(["v", v_tag.as_str()])?,
        ];
        let ephemeral_event =
            EventBuilder::new(Kind::Custom(KIND_PROVIDER_HEARTBEAT_EPHEMERAL), content)
                .tags(ephemeral_tags)
                .sign_with_keys(&self.keys)?;

        let mut accepting_relays: Vec<String> = Vec::new();
        match self.client.send_event(&stored_event).await {
            Ok(out) => {
                debug!("📦 Stored heartbeat published: {}", stored_id);
                accepting_relays = out.success.iter().map(|u| u.to_string()).collect();
            }
            Err(e) => warn!("Failed to publish stored heartbeat: {}", e),
        }
        match self.client.send_event(&ephemeral_event).await {
            Ok(_) => debug!("⚡ Ephemeral heartbeat published"),
            Err(e) => warn!("Failed to publish ephemeral heartbeat: {}", e),
        }

        info!(
            "💓 Heartbeat published (stored + ephemeral): {} accepted by {} relay(s)",
            stored_id,
            accepting_relays.len()
        );
        Ok((stored_id, accepting_relays))
    }

    pub async fn publish_lease_revocation(
        &self,
        revocation: LeaseRevocationContent,
    ) -> Result<String> {
        let content = serde_json::to_string(&revocation)?;
        let d_tag = format!(
            "paygress:revocation:v{}:{}:{}",
            revocation.version, revocation.primary_provider_npub, revocation.workload_id
        );
        let v_tag = revocation.version.to_string();

        let mut tags = vec![
            Tag::hashtag("paygress"),
            Tag::hashtag("paygress-revocation"),
            Tag::parse(["d", d_tag.as_str()])?,
            Tag::parse(["v", v_tag.as_str()])?,
            Tag::parse(["workload", revocation.workload_id.as_str()])?,
        ];
        for standby_npub in &revocation.standby_providers {
            if let Ok(pk) = nostr_sdk::PublicKey::parse(standby_npub) {
                tags.push(Tag::public_key(pk));
            } else {
                warn!(
                    "Skipping unparseable standby npub in revocation: {}",
                    standby_npub
                );
            }
        }

        let event = EventBuilder::new(Kind::Custom(KIND_LEASE_REVOCATION), content)
            .tags(tags)
            .sign_with_keys(&self.keys)?;
        let event_id = event.id.to_hex();

        match self.client.send_event(&event).await {
            Ok(out) => {
                info!(
                    "📜 Lease revocation published for workload {}: {} accepted by {} relay(s)",
                    revocation.workload_id,
                    event_id,
                    out.success.len()
                );
                Ok(event_id)
            }
            Err(e) => {
                error!("Failed to publish lease revocation: {}", e);
                Err(e.into())
            }
        }
    }

    pub async fn publish_standby_promotion_announcement(
        &self,
        announcement: StandbyPromotionAnnouncementContent,
    ) -> Result<String> {
        let content = serde_json::to_string(&announcement)?;
        let d_tag = format!(
            "paygress:promoted:v{}:{}",
            announcement.version, announcement.workload_id
        );
        let v_tag = announcement.version.to_string();
        let tags = vec![
            Tag::hashtag("paygress"),
            Tag::hashtag("paygress-promoted"),
            Tag::parse(["d", d_tag.as_str()])?,
            Tag::parse(["v", v_tag.as_str()])?,
            Tag::parse(["workload", announcement.workload_id.as_str()])?,
        ];

        let event = EventBuilder::new(Kind::Custom(KIND_STANDBY_PROMOTION_ANNOUNCEMENT), content)
            .tags(tags)
            .sign_with_keys(&self.keys)?;
        let event_id = event.id.to_hex();

        match self.client.send_event(&event).await {
            Ok(out) => {
                info!(
                    "📢 Standby promotion announcement published for workload {}: {} accepted by {} relay(s)",
                    announcement.workload_id,
                    event_id,
                    out.success.len()
                );
                Ok(event_id)
            }
            Err(e) => {
                error!("Failed to publish standby promotion announcement: {}", e);
                Err(e.into())
            }
        }
    }

    /// Any `StandbyPromotionAnnouncement` for `workload_id` authored by one of
    /// `peer_npubs`. The promotion path calls this *before* spawning: if a peer
    /// already promoted, the local standby drops its slot instead of producing
    /// a duplicate primary.
    pub async fn query_standby_promotion_announcements(
        &self,
        workload_id: &str,
        peer_npubs: &[String],
    ) -> Result<Option<StandbyPromotionAnnouncementContent>> {
        if peer_npubs.is_empty() {
            return Ok(None);
        }
        let mut authors = Vec::new();
        for npub in peer_npubs {
            if let Ok(pk) = nostr_sdk::PublicKey::parse(npub) {
                authors.push(pk);
            }
        }
        if authors.is_empty() {
            return Ok(None);
        }

        let filter = Filter::new()
            .kind(Kind::Custom(KIND_STANDBY_PROMOTION_ANNOUNCEMENT))
            .authors(authors)
            .custom_tag(
                nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::D),
                format!("paygress:promoted:v{}:{}", SCHEMA_VERSION, workload_id),
            );

        let events = self
            .client
            .fetch_events(filter, std::time::Duration::from_secs(5))
            .await?;

        for event in events.iter() {
            if let Ok(content) =
                serde_json::from_str::<StandbyPromotionAnnouncementContent>(&event.content)
            {
                if content.workload_id == workload_id {
                    return Ok(Some(content));
                }
            }
        }
        Ok(None)
    }

    /// Query all provider offers from relays, dropping any whose body claims a
    /// `provider_npub` other than the event's (signature-verified) signer.
    pub async fn query_providers(&self) -> Result<Vec<ProviderOfferContent>> {
        let filter = Filter::new()
            .kind(Kind::Custom(KIND_PROVIDER_OFFER))
            .hashtag("paygress");

        let events = self
            .client
            .fetch_events(filter, std::time::Duration::from_secs(5))
            .await?;

        let mut providers = Vec::new();
        for event in events {
            let signer_hex = event.pubkey.to_hex();
            match serde_json::from_str::<ProviderOfferContent>(&event.content) {
                Ok(offer) => {
                    if !offer_authored_by_claimed_provider(&signer_hex, &offer) {
                        warn!(
                            "Dropping spoofed provider offer {}: body provider_npub={} does not match event signer {}",
                            event.id, offer.provider_npub, signer_hex
                        );
                        continue;
                    }
                    providers.push(offer);
                }
                Err(e) => {
                    warn!("Failed to parse provider offer {}: {}", event.id, e);
                }
            }
        }

        info!("Found {} providers", providers.len());
        Ok(providers)
    }

    pub async fn query_heartbeats(
        &self,
        provider_npub: &str,
        since_secs: u64,
    ) -> Result<Vec<HeartbeatContent>> {
        let provider_pubkey = nostr_sdk::PublicKey::parse(provider_npub)?;

        let filter = Filter::new()
            .kind(Kind::Custom(KIND_PROVIDER_HEARTBEAT))
            .author(provider_pubkey)
            .since(Timestamp::from(since_secs));

        let events = self
            .client
            .fetch_events(filter, std::time::Duration::from_secs(5))
            .await?;

        let mut heartbeats = Vec::new();
        for event in events {
            match serde_json::from_str::<HeartbeatContent>(&event.content) {
                Ok(hb) => heartbeats.push(hb),
                Err(e) => {
                    warn!("Failed to parse heartbeat {}: {}", event.id, e);
                }
            }
        }

        Ok(heartbeats)
    }

    /// Latest heartbeat for a provider within the live window. Only the stored
    /// kind is queried; relays do not retain the ephemeral one.
    pub async fn get_latest_heartbeat(
        &self,
        provider_npub: &str,
    ) -> Result<Option<HeartbeatContent>> {
        let provider_pubkey = nostr_sdk::PublicKey::parse(provider_npub)?;

        let live_since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            - LIVE_HEARTBEAT_WINDOW_SECS;

        let filter = Filter::new()
            .kind(Kind::Custom(KIND_PROVIDER_HEARTBEAT))
            .author(provider_pubkey)
            .since(Timestamp::from(live_since))
            .limit(1);

        let events = self
            .client
            .fetch_events(filter, std::time::Duration::from_secs(3))
            .await?;

        if let Some(event) = events.first() {
            match serde_json::from_str::<HeartbeatContent>(&event.content) {
                Ok(hb) => return Ok(Some(hb)),
                Err(e) => warn!("Failed to parse heartbeat: {}", e),
            }
        }

        Ok(None)
    }

    /// Batched [`Self::get_latest_heartbeat`] over many providers.
    pub async fn get_latest_heartbeats_multi(
        &self,
        provider_npubs: Vec<String>,
    ) -> Result<std::collections::HashMap<String, HeartbeatContent>> {
        if provider_npubs.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let mut pubkeys = Vec::new();
        for npub in provider_npubs {
            if let Ok(pk) = nostr_sdk::PublicKey::parse(&npub) {
                pubkeys.push(pk);
            }
        }

        let live_since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            - LIVE_HEARTBEAT_WINDOW_SECS;

        let filter = Filter::new()
            .kind(Kind::Custom(KIND_PROVIDER_HEARTBEAT))
            .authors(pubkeys)
            .since(Timestamp::from(live_since));

        let events = self
            .client
            .fetch_events(filter, std::time::Duration::from_secs(3))
            .await?;

        let mut heartbeats = std::collections::HashMap::new();

        // Keep only the latest heartbeat per provider.
        for event in events {
            if let Ok(hb) = serde_json::from_str::<HeartbeatContent>(&event.content) {
                match heartbeats.entry(hb.provider_npub.clone()) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        let existing: &HeartbeatContent = entry.get();
                        if hb.timestamp > existing.timestamp {
                            entry.insert(hb);
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(hb);
                    }
                }
            }
        }

        Ok(heartbeats)
    }

    /// Uptime percentage over the last `days`, as the ratio of stored
    /// heartbeats found to the number expected at one per bucket.
    pub async fn calculate_uptime(&self, provider_npub: &str, days: u32) -> Result<f32> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let since = now - (days as u64 * 24 * 60 * 60);

        let heartbeats = self.query_heartbeats(provider_npub, since).await?;

        if heartbeats.is_empty() {
            return Ok(0.0);
        }

        let expected = (days as f32) * 24.0 * 3600.0 / HEARTBEAT_BUCKET_SECS as f32;
        let actual = heartbeats.len() as f32;

        Ok((actual / expected * 100.0).min(100.0))
    }
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

#[cfg(test)]
mod npubs_equal_tests {
    use super::*;

    // Two encodings of one frozen public key.
    const PUBKEY_BECH32: &str = "npub1ae40uj62de87f8tvx56e6ytp5m7jd7l96mh0ew43e8q5wucm7z9q2uqvuc";
    const PUBKEY_HEX: &str = "ee6afe4b4a6e4fe49d6c35359d1161a6fd26fbe5d6eefcbab1c9c147731bf08a";

    #[test]
    fn bech32_matches_itself() {
        assert!(npubs_equal(PUBKEY_BECH32, PUBKEY_BECH32));
    }

    #[test]
    fn hex_matches_itself() {
        assert!(npubs_equal(PUBKEY_HEX, PUBKEY_HEX));
    }

    /// Regression: the provider stores hex, the consumer ships bech32, and
    /// without normalization the role check always returned `NotAddressed`.
    #[test]
    fn bech32_matches_hex_for_same_key() {
        assert!(npubs_equal(PUBKEY_BECH32, PUBKEY_HEX));
        assert!(npubs_equal(PUBKEY_HEX, PUBKEY_BECH32));
    }

    #[test]
    fn different_keys_in_different_encodings_do_not_match() {
        let other_bech32 = "npub1hyr9m7zeegr98w4e07gvdpqrk25jfp3vku8029u8pcxsc48dq6nqxtwztv";
        assert!(!npubs_equal(PUBKEY_HEX, other_bech32));
    }

    #[test]
    fn unparseable_strings_fall_back_to_string_equality() {
        assert!(npubs_equal("npub1primary", "npub1primary"));
        assert!(!npubs_equal("npub1primary", "npub1secondary"));
    }

    #[test]
    fn one_real_one_typoed_returns_false() {
        assert!(!npubs_equal(PUBKEY_BECH32, "npub1primary"));
        assert!(!npubs_equal("npub1primary", PUBKEY_HEX));
    }
}
