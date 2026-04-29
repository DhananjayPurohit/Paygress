// Nostr client for receiving pod provisioning events with private messaging
use anyhow::{Context, Result};
use nostr_sdk::nips::nip04;
use nostr_sdk::nips::nip59::UnwrappedGift;
use nostr_sdk::{
    Client, EventBuilder, Filter, Keys, Kind, RelayPoolNotification, Tag, Timestamp, ToBech32, Url,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

// Custom event kinds for Paygress provider discovery.
//
// `KIND_PROVIDER_OFFER` (38383) is a NIP-33 parameterized-replaceable
// event keyed by `(pubkey, kind, d-tag)`. We use a versioned `d` tag
// (`paygress:offer:v1:<npub>`) so future schema bumps can coexist on
// the same relay without overwriting v1 events.
//
// Heartbeats use TWO kinds (Unit 4 of the 12-month plan):
// - `KIND_PROVIDER_HEARTBEAT` (38384): NIP-33 addressable. We publish
//   with a *bucketed* `d` tag
//   (`paygress:heartbeat:v1:<npub>:<minute-bucket>`) so distinct
//   minutes coexist on the relay and stored history is queryable for
//   uptime aggregation. This fixes the original "addressable replaces
//   each heartbeat" bug where `calculate_uptime` saw only one event.
// - `KIND_PROVIDER_HEARTBEAT_EPHEMERAL` (20384): NIP-16 ephemeral.
//   Relays do not store these, so they're cheap for live-presence
//   subscribers but useless for uptime history. We publish on both.
pub const KIND_PROVIDER_OFFER: u16 = 38383;
pub const KIND_PROVIDER_HEARTBEAT: u16 = 38384;
pub const KIND_PROVIDER_HEARTBEAT_EPHEMERAL: u16 = 20384;

/// Schema version for offer + heartbeat payloads. Old payloads
/// without this field deserialize to `1` via `#[serde(default)]`.
pub const SCHEMA_VERSION: u8 = 1;

/// Live-presence query window. Ephemeral heartbeats are not stored
/// at relays, so any "is this provider alive right now?" query is
/// implicitly bounded to whatever subscribers were live recently.
/// Stored heartbeats can be queried over arbitrary windows; this
/// constant only governs the ephemeral / fast-path lookups.
pub const LIVE_HEARTBEAT_WINDOW_SECS: u64 = 300;

/// Heartbeat bucket size for the addressable (stored) kind. One
/// bucket per minute matches the 60s heartbeat cadence: every
/// heartbeat lands in its own `(npub, kind, d-tag)` slot, so relays
/// preserve history for uptime aggregation.
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
    pub message_type: String, // "nip04" or "nip17" to track which method was used
}

#[derive(Clone)]
pub struct NostrRelaySubscriber {
    client: Client,
    keys: Keys,
    // config field removed - not used in current implementation
}

impl NostrRelaySubscriber {
    pub async fn new(config: RelayConfig) -> Result<Self> {
        let keys = match &config.private_key {
            Some(private_key_hex) if !private_key_hex.is_empty() => {
                // Parse as nsec format (nostr private key)
                if private_key_hex.starts_with("nsec1") {
                    Keys::parse(private_key_hex).context("Invalid nsec private key format")?
                } else {
                    // Assume hex format for backward compatibility
                    Keys::parse(private_key_hex).context("Invalid private key format")?
                }
            }
            _ => {
                // Generate a new key if none provided
                Keys::generate()
            }
        };

        let client = Client::new(&keys);

        // Add relays
        for relay_url in &config.relays {
            info!("Adding relay: {}", relay_url);
            let url = Url::parse(relay_url)
                .with_context(|| format!("Invalid relay URL: {}", relay_url))?;
            client.add_relay(url).await?;
        }

        info!("Connecting to {} relays...", config.relays.len());
        client.connect().await;

        // Wait a moment for connections to establish
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
        // Subscribe to messages sent TO us (filter by p-tag)
        let nip04_filter = Filter::new()
            .kind(Kind::EncryptedDirectMessage)
            .pubkeys(vec![self.keys.public_key()]) // Sets #p tag
            .limit(0);

        let nip17_filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkeys(vec![self.keys.public_key()]) // Sets #p tag
            .limit(0);

        let _ = self
            .client
            .subscribe(vec![nip04_filter, nip17_filter], None)
            .await;
        info!("Subscribed to NIP-04 (Encrypted Direct Messages) and NIP-17 (Gift Wrap) messages for pod provisioning and top-up requests");

        // Handle incoming events
        self.client.handle_notifications(|notification| async {
            if let RelayPoolNotification::Event { relay_url: _, subscription_id: _, event } = notification {
                match event.kind {
                    Kind::GiftWrap => {
                        info!("Received NIP-17 Gift Wrap message: {}", event.id);

                        // Unwrap the Gift Wrap to get the inner message
                        match self.client.unwrap_gift_wrap(&event).await {
                            Ok(UnwrappedGift { rumor, sender }) => {
                                info!("Unwrapped Gift Wrap from sender: {}, rumor kind: {}", sender, rumor.kind);

                                // Check if the rumor is a private direct message
                                if rumor.kind == Kind::PrivateDirectMessage {
                                    debug!("NIP-17 rumor is PrivateDirectMessage. Content length: {}", rumor.content.len());

                                    // Create a NostrEvent from the unwrapped rumor with NIP-17 flag
                                    let nostr_event = NostrEvent {
                                        id: rumor.id.map(|id| id.to_hex()).unwrap_or_else(|| "unknown".to_string()),
                                        pubkey: rumor.pubkey.to_hex(),
                                        created_at: rumor.created_at.as_u64(),
                                        kind: rumor.kind.as_u32(),
                                        tags: rumor.tags.iter().map(|tag| {
                                            tag.as_vec().iter().map(|s| s.to_string()).collect()
                                        }).collect(),
                                        content: rumor.content,
                                        sig: "unsigned".to_string(), // UnsignedEvent doesn't have a signature
                                        message_type: "nip17".to_string(), // Flag to indicate NIP-17
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

                        // Decrypt the NIP-04 message using NIP-04 module
                        match self.keys.secret_key() {
                            Ok(secret_key) => {
                                match nip04::decrypt(&secret_key, &event.pubkey, &event.content) {
                                    Ok(decrypted_content) => {
                                        debug!("Decrypted NIP-04 message. Length: {}", decrypted_content.len());

                                        // Create a NostrEvent from the decrypted message with NIP-04 flag
                                        let nostr_event = NostrEvent {
                                            id: event.id.to_hex(),
                                            pubkey: event.pubkey.to_hex(),
                                            created_at: event.created_at.as_u64(),
                                            kind: event.kind.as_u32(),
                                            tags: event.tags.iter().map(|tag| {
                                                tag.as_vec().iter().map(|s| s.to_string()).collect()
                                            }).collect(),
                                            content: decrypted_content,
                                            sig: event.sig.to_string(),
                                            message_type: "nip04".to_string(), // Flag to indicate NIP-04
                                        };

                                        match handler(nostr_event).await {
                                            Ok(()) => {
                                                info!("Successfully processed NIP-04 private message: {}", event.id);
                                            }
                                            Err(e) => {
                                                error!("Failed to process NIP-04 private message {}: {}", event.id, e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to decrypt NIP-04 message {}: {}", event.id, e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to get secret key for NIP-04 decryption: {}", e);
                            }
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

    pub async fn publish_offer(&self, offer: OfferEventContent) -> Result<String> {
        let content = serde_json::to_string(&offer)?;
        info!("Publishing offer event with content: {}", content);

        let tags = vec![Tag::hashtag("paygress"), Tag::hashtag("offer")];

        info!("Creating event with kind 999 and {} tags", tags.len());
        let builder = EventBuilder::new(Kind::Custom(999), content, tags);
        let event = builder.to_event(&self.keys)?;
        let event_id = event.id.to_hex();

        info!("Event created with ID: {}", event_id);
        info!("Sending offer event to relays: {}", event_id);

        match self.client.send_event(event).await {
            Ok(res) => {
                info!(
                    "✅ Successfully published offer event: {} and {:?}",
                    event_id, res
                );
                Ok(event_id)
            }
            Err(e) => {
                error!("❌ Failed to send offer event: {}", e);
                Err(e.into())
            }
        }
    }

    // Generic method to send an encrypted private message (supports both NIP-04 and NIP-17)
    pub async fn send_encrypted_private_message(
        &self,
        receiver_pubkey: &str,
        content: String,
        message_type: &str,
    ) -> Result<String> {
        let receiver_pubkey_parsed = nostr_sdk::PublicKey::parse(receiver_pubkey)?;

        match message_type {
            "nip04" => match self.keys.secret_key() {
                Ok(secret_key) => {
                    let encrypted_content =
                        nip04::encrypt(&secret_key, &receiver_pubkey_parsed, &content)?;
                    let receiver_tag = Tag::public_key(receiver_pubkey_parsed);
                    let alt_tag = Tag::parse(&["alt", "Private Message"])?;

                    let event_builder = EventBuilder::new(
                        Kind::EncryptedDirectMessage,
                        encrypted_content,
                        [receiver_tag, alt_tag],
                    );
                    let event = event_builder.to_event(&self.keys)?;
                    let event_id = self.client.send_event(event).await?;
                    info!("Sent NIP-04 message to {}: {:?}", receiver_pubkey, event_id);
                    Ok(event_id.to_hex())
                }
                Err(e) => {
                    error!("Failed to get secret key for NIP-04 encryption: {}", e);
                    Err(e.into())
                }
            },
            "nip17" | _ => {
                // Default to NIP-17 if not specified or nip17
                let event_id = self
                    .client
                    .send_private_msg(receiver_pubkey_parsed, content, None)
                    .await?;
                info!("Sent NIP-17 message to {}: {:?}", receiver_pubkey, event_id);
                Ok(event_id.to_hex())
            }
        }
    }

    // Send access details via private encrypted message
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

    // Send status response via private encrypted message
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

    // Convenience helper to send error response with individual fields
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

    // Send error response via private encrypted message
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

    // Send top-up response via private encrypted message
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

    // Get the underlying Nostr client
    pub fn client(&self) -> &Client {
        &self.client
    }

    // NEW: Get service public key for users
    pub fn get_service_public_key(&self) -> String {
        self.keys.public_key().to_hex()
    }

    fn convert_event(&self, event: &nostr_sdk::Event) -> NostrEvent {
        NostrEvent {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_u64(),
            kind: event.kind.as_u32(),
            tags: event
                .tags
                .iter()
                .map(|tag| tag.as_vec().iter().map(|s| s.to_string()).collect())
                .collect(),
            content: event.content.clone(),
            sig: event.sig.to_string(),
            message_type: "unknown".to_string(), // Default value since this function doesn't know the context
        }
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

        // Subscribe to messages sent TO us
        let filter = Filter::new()
            .pubkeys(vec![receiver_pk])
            .kinds(vec![Kind::EncryptedDirectMessage, Kind::GiftWrap]);

        let _ = client.subscribe(vec![filter], None).await;

        // Use tokio::select to handle timeout and notification processing
        let result = tokio::select! {
            notification_res = client.handle_notifications(|notification| {
                let tx = tx.clone();
                let receiver_keys = receiver_keys.clone();
                let sender_pk = sender_pk.clone();
                let client = client.clone();

                async move {
                    if let RelayPoolNotification::Event { event, .. } = notification {
                        let mut event_to_send = None;

                        match event.kind {
                            Kind::GiftWrap => {
                                // GiftWrap might be NIP-17
                                if let Ok(UnwrappedGift { rumor, sender }) = client.unwrap_gift_wrap(&event).await {
                                    if sender == sender_pk && rumor.kind == Kind::PrivateDirectMessage {
                                        event_to_send = Some(NostrEvent {
                                            id: rumor.id.map(|id| id.to_hex()).unwrap_or_default(),
                                            pubkey: sender.to_hex(),
                                            created_at: rumor.created_at.as_u64(),
                                            kind: rumor.kind.as_u32(),
                                            tags: rumor.tags.iter().map(|tag| tag.as_vec().iter().map(|s| s.to_string()).collect()).collect(),
                                            content: rumor.content,
                                            sig: String::new(),
                                            message_type: "nip17".to_string(),
                                        });
                                    }
                                }
                            }
                            Kind::EncryptedDirectMessage => {
                                if event.pubkey == sender_pk {
                                    if let Ok(secret_key) = receiver_keys.secret_key() {
                                        if let Ok(content) = nip04::decrypt(&secret_key, &event.pubkey, &event.content) {
                                            event_to_send = Some(NostrEvent {
                                                id: event.id.to_hex(),
                                                pubkey: event.pubkey.to_hex(),
                                                created_at: event.created_at.as_u64(),
                                                kind: event.kind.as_u32(),
                                                tags: event.tags.iter().map(|tag| tag.as_vec().iter().map(|s| s.to_string()).collect()).collect(),
                                                content,
                                                sig: event.sig.to_string(),
                                                message_type: "nip04".to_string(),
                                            });
                                        }
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
        };

        result
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
    pub id: String, // Unique identifier for this spec (e.g., "basic", "standard", "premium")
    pub name: String, // Human-readable name (e.g., "Basic", "Standard", "Premium")
    pub description: String, // Description of the spec
    pub cpu_millicores: u64, // CPU in millicores (1000 millicores = 1 CPU core)
    pub memory_mb: u64, // Memory in MB
    pub rate_msats_per_sec: u64, // Payment rate for this spec
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferEventContent {
    pub minimum_duration_seconds: u64,
    pub whitelisted_mints: Vec<String>,
    pub pod_specs: Vec<PodSpec>, // Multiple pod specifications offered
}

/// One workload-port that a template-spawned container exposes to the
/// consumer. Distinct from `AccessDetailsContent.node_port` (the SSH
/// forwarding port). Empty for non-template spawns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateAccessPort {
    /// Host port the consumer connects to.
    pub host_port: u16,
    /// Container-internal port (informational; for the consumer to
    /// understand what's running).
    pub container_port: u16,
    /// Wire protocol (`tcp`, `http`, `ws`, `bitcoin-rpc`, ...).
    pub protocol: String,
    /// Human-readable label from the template definition
    /// (e.g. `relay-ws`, `ollama-http`, `rpc`). Lets clients route
    /// traffic by role rather than guessing port-by-port.
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessDetailsContent {
    pub pod_npub: String,             // Pod's NPUB identifier
    pub node_port: u16,               // SSH port for direct access
    pub expires_at: String,           // Pod expiration time
    pub cpu_millicores: u64,          // CPU allocation in millicores
    pub memory_mb: u64,               // Memory allocation in MB
    pub pod_spec_name: String,        // Human-readable spec name
    pub pod_spec_description: String, // Spec description
    pub instructions: Vec<String>,    // SSH connection instructions

    /// Host address the consumer connects to. Same string that
    /// appears in the SSH instruction; promoted to a structured
    /// field so programmatic clients don't have to scrape the
    /// instruction strings.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host_address: String,

    /// Workload-specific ports published by a template spawn.
    /// Empty for non-template (legacy) spawns. Old clients without
    /// this field continue to deserialize cleanly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub template_ports: Vec<TemplateAccessPort>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponseContent {
    pub error_type: String, // Type of error (e.g., "insufficient_payment", "invalid_spec", "image_not_found")
    pub message: String,    // Human-readable error message
    pub details: Option<String>, // Additional error details
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopUpResponseContent {
    pub success: bool,
    pub pod_npub: String,
    pub extended_duration_seconds: u64,
    pub new_expires_at: String,
    pub message: String,
}

// NEW: Encrypted request structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedSpawnPodRequest {
    pub cashu_token: String,
    pub pod_spec_id: Option<String>, // Optional: Which pod spec to use (defaults to first available)
    pub pod_image: String,           // Required: Container image to use for the pod
    pub ssh_username: String,
    pub ssh_password: String,

    /// Optional template slug. When set, the provider materializes
    /// the workload's image / ports / env from its OWN local
    /// template registry (`paygress::templates`) rather than
    /// trusting consumer-supplied bytes — so a consumer cannot
    /// smuggle an arbitrary image past the provider's vetted
    /// list. `pod_image` is ignored when `template_slug` resolves.
    /// Old clients that don't set this field continue to work
    /// (`#[serde(default)]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_slug: Option<String>,
}

// NEW: Encrypted top-up request structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedTopUpPodRequest {
    pub pod_npub: String, // Pod's NPUB identifier
    pub cashu_token: String,
}

// NEW: Helper function to send private message provisioning request
pub async fn send_provisioning_request_private_message(
    client: &Client,
    service_pubkey: &str,
    request: EncryptedSpawnPodRequest,
) -> Result<String> {
    let request_json = serde_json::to_string(&request)?;

    // Send as private message
    let service_pubkey_parsed = nostr_sdk::PublicKey::parse(service_pubkey)?;
    let event_id = client
        .send_private_msg(service_pubkey_parsed, request_json, None)
        .await?;

    Ok(event_id.to_hex())
}

// NEW: Helper function to parse private message content
/// Unified request type for private messages
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrivateRequest {
    Spawn(EncryptedSpawnPodRequest),
    TopUp(EncryptedTopUpPodRequest),
    Status(StatusRequestContent),
}

pub fn parse_private_message_content(content: &str) -> Result<PrivateRequest> {
    match serde_json::from_str::<PrivateRequest>(content) {
        Ok(request) => Ok(request),
        Err(e) => {
            // Provide detailed error information, but truncate content to avoid huge log strings
            let truncated_content = if content.len() > 100 {
                format!("{}...", &content[..100])
            } else {
                content.to_string()
            };
            Err(anyhow::anyhow!(
                "JSON parsing failed: {}. Content: '{}'",
                e,
                truncated_content
            ))
        }
    }
}

// ==================== Provider Discovery Structures ====================

/// Capacity information for a provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityInfo {
    pub cpu_available: u64,        // Available CPU in millicores
    pub memory_mb_available: u64,  // Available memory in MB
    pub storage_gb_available: u64, // Available storage in GB
}

/// Provider isolation level (Unit 4 surfaces this on offers from
/// Q1; Unit 22 will populate it with the real research-tier
/// implementation). `#[serde(default)]` so v0 offers parse cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationLevel {
    /// Default LXC / shared-kernel container.
    #[default]
    SharedKernel,
    /// Whole-host dedicated to a single workload (no co-tenants).
    DedicatedHost,
    /// Attested AMD SEV-SNP / Intel TDX research tier (year-2 R9).
    AttestedResearchTier,
}

fn default_schema_version() -> u8 {
    SCHEMA_VERSION
}

/// Provider offer content published to Nostr (Kind 38383).
///
/// Parameterized-replaceable event addressed by
/// `(pubkey, 38383, d="paygress:offer:v1:<npub>")`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderOfferContent {
    pub provider_npub: String,
    pub hostname: String,
    pub location: Option<String>,
    pub capabilities: Vec<String>, // ["lxc", "vm"]
    pub specs: Vec<PodSpec>,
    pub whitelisted_mints: Vec<String>,
    pub uptime_percent: f32,
    pub total_jobs_completed: u64,
    pub api_endpoint: Option<String>,

    /// Schema version. v0 offers (no field on the wire) deserialize
    /// to `1` via the default. Bump on any breaking change.
    #[serde(default = "default_schema_version")]
    pub version: u8,

    /// Isolation level the provider promises (Unit 4 / Unit 22).
    #[serde(default)]
    pub isolation_level: IsolationLevel,

    /// Optional fidelity-bond stake. Offers carrying a verifiable
    /// stake proof are eligible for the `staked` discovery tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stake_proof: Option<crate::stake::StakeProof>,
}

/// Heartbeat content published to Nostr.
///
/// Dual-published on two kinds (see Unit 4):
/// - `KIND_PROVIDER_HEARTBEAT` (38384, addressable, with bucketed
///   `d` tag) for stored uptime history.
/// - `KIND_PROVIDER_HEARTBEAT_EPHEMERAL` (20384) for live presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatContent {
    pub provider_npub: String,
    pub timestamp: u64,
    pub active_workloads: u32,
    pub available_capacity: CapacityInfo,

    /// Schema version. See `ProviderOfferContent::version`.
    #[serde(default = "default_schema_version")]
    pub version: u8,
}

/// Provider info as seen by discovery clients
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
    pub last_seen: u64, // Timestamp of last heartbeat
    pub is_online: bool,
}

/// Filter for querying providers
#[derive(Debug, Clone, Default)]
pub struct ProviderFilter {
    pub capability: Option<String>,
    pub min_uptime: Option<f32>,
    pub min_memory_mb: Option<u64>,
    pub min_cpu: Option<u64>,
}

impl NostrRelaySubscriber {
    /// Publish a provider offer event (Kind 38383 — parameterized
    /// replaceable). The `d` tag is versioned
    /// (`paygress:offer:v1:<npub>`) so future schema bumps coexist
    /// with v1 events on the same relay.
    pub async fn publish_provider_offer(&self, offer: ProviderOfferContent) -> Result<String> {
        let content = serde_json::to_string(&offer)?;
        info!("Publishing provider offer for {}", offer.hostname);

        let d_tag = format!("paygress:offer:v{}:{}", offer.version, offer.provider_npub);
        let tags = vec![
            Tag::hashtag("paygress"),
            Tag::hashtag("compute"),
            Tag::parse(&["d", &d_tag])?,
            Tag::parse(&["v", &offer.version.to_string()])?,
        ];

        let builder = EventBuilder::new(Kind::Custom(KIND_PROVIDER_OFFER), content, tags);
        let event = builder.to_event(&self.keys)?;
        let event_id = event.id.to_hex();

        match self.client.send_event(event).await {
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

    /// Publish a heartbeat event on BOTH the addressable (stored,
    /// kind 38384) and ephemeral (kind 20384) kinds. Returns the
    /// addressable event id (the one consumers care about for uptime
    /// history).
    ///
    /// The addressable form uses a per-minute bucketed `d` tag
    /// (`paygress:heartbeat:v1:<npub>:<bucket>`) so each minute's
    /// heartbeat lands in its own `(pubkey, kind, d-tag)` slot.
    /// Without bucketing, every heartbeat replaces the previous one
    /// at the relay and `calculate_uptime` sees zero history.
    pub async fn publish_heartbeat(&self, heartbeat: HeartbeatContent) -> Result<String> {
        let content = serde_json::to_string(&heartbeat)?;
        let bucket = heartbeat.timestamp / HEARTBEAT_BUCKET_SECS;
        let d_tag = format!(
            "paygress:heartbeat:v{}:{}:{}",
            heartbeat.version, heartbeat.provider_npub, bucket
        );

        let provider_pk = nostr_sdk::PublicKey::parse(&heartbeat.provider_npub)?;
        let v_tag = heartbeat.version.to_string();

        // 1. Stored, addressable: relays keep this for history-based
        //    queries (calculate_uptime).
        let stored_tags = vec![
            Tag::hashtag("paygress-heartbeat"),
            Tag::public_key(provider_pk),
            Tag::parse(&["d", &d_tag])?,
            Tag::parse(&["v", &v_tag])?,
        ];
        let stored = EventBuilder::new(
            Kind::Custom(KIND_PROVIDER_HEARTBEAT),
            content.clone(),
            stored_tags,
        );
        let stored_event = stored.to_event(&self.keys)?;
        let stored_id = stored_event.id.to_hex();

        // 2. Ephemeral: relays don't store, but live subscribers see
        //    it immediately. Cheap and good for dashboards.
        let ephemeral_tags = vec![
            Tag::hashtag("paygress-heartbeat"),
            Tag::public_key(provider_pk),
            Tag::parse(&["v", &v_tag])?,
        ];
        let ephemeral = EventBuilder::new(
            Kind::Custom(KIND_PROVIDER_HEARTBEAT_EPHEMERAL),
            content,
            ephemeral_tags,
        );
        let ephemeral_event = ephemeral.to_event(&self.keys)?;

        match self.client.send_event(stored_event).await {
            Ok(_) => debug!("📦 Stored heartbeat published: {}", stored_id),
            Err(e) => warn!("Failed to publish stored heartbeat: {}", e),
        }
        match self.client.send_event(ephemeral_event).await {
            Ok(_) => debug!("⚡ Ephemeral heartbeat published"),
            Err(e) => warn!("Failed to publish ephemeral heartbeat: {}", e),
        }

        info!("💓 Heartbeat published (stored + ephemeral): {}", stored_id);
        Ok(stored_id)
    }

    /// Query all provider offers from relays
    pub async fn query_providers(&self) -> Result<Vec<ProviderOfferContent>> {
        let filter = Filter::new()
            .kind(Kind::Custom(KIND_PROVIDER_OFFER))
            .hashtag("paygress");

        let events = self
            .client
            .get_events_of(vec![filter], Some(std::time::Duration::from_secs(5)))
            .await?;

        let mut providers = Vec::new();
        for event in events {
            match serde_json::from_str::<ProviderOfferContent>(&event.content) {
                Ok(offer) => providers.push(offer),
                Err(e) => {
                    warn!("Failed to parse provider offer {}: {}", event.id, e);
                }
            }
        }

        info!("Found {} providers", providers.len());
        Ok(providers)
    }

    /// Query heartbeats for a specific provider since a given time
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
            .get_events_of(vec![filter], Some(std::time::Duration::from_secs(5)))
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

    /// Get the latest heartbeat for a provider (to check if online).
    /// Queries the stored kind 38384 (which now retains per-minute
    /// bucketed history thanks to the `d`-tag fix in Unit 4) within
    /// the live window. Ephemeral kind 20384 is not queried here
    /// because relays do not store it; it would only be visible to
    /// live subscribers.
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
            .get_events_of(vec![filter], Some(std::time::Duration::from_secs(3)))
            .await?;

        if let Some(event) = events.first() {
            match serde_json::from_str::<HeartbeatContent>(&event.content) {
                Ok(hb) => return Ok(Some(hb)),
                Err(e) => warn!("Failed to parse heartbeat: {}", e),
            }
        }

        Ok(None)
    }

    /// Get the latest heartbeats for multiple providers in a single batch query
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

        // Query the stored kind 38384 (now retains bucketed history
        // per Unit 4) for "have any of these providers heartbeat'd
        // recently?". Ephemeral 20384 isn't queried because relays
        // do not store it.
        let filter = Filter::new()
            .kind(Kind::Custom(KIND_PROVIDER_HEARTBEAT))
            .authors(pubkeys)
            .since(Timestamp::from(live_since));

        // Use a short timeout of 3 seconds for fast feedback
        let events = self
            .client
            .get_events_of(vec![filter], Some(std::time::Duration::from_secs(3)))
            .await?;

        let mut heartbeats = std::collections::HashMap::new();

        // Process events, keeping only the latest for each provider
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

    /// Calculate uptime percentage for a provider over the last N
    /// days, against the stored kind 38384 (which now retains
    /// per-minute bucketed history thanks to Unit 4's `d`-tag fix —
    /// previously every new heartbeat replaced the prior one and
    /// uptime always returned ~0).
    pub async fn calculate_uptime(&self, provider_npub: &str, days: u32) -> Result<f32> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let since = now - (days as u64 * 24 * 60 * 60);

        let heartbeats = self.query_heartbeats(provider_npub, since).await?;

        if heartbeats.is_empty() {
            return Ok(0.0);
        }

        // Expected heartbeats: one per HEARTBEAT_BUCKET_SECS over
        // the window. Distinct heartbeats coexist on the relay
        // because each lands in its own bucketed `d`-tag slot.
        let expected = (days as f32) * 24.0 * 3600.0 / HEARTBEAT_BUCKET_SECS as f32;
        let actual = heartbeats.len() as f32;

        Ok((actual / expected * 100.0).min(100.0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRequestContent {
    pub pod_id: String, // Can be NPUB or container ID
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
