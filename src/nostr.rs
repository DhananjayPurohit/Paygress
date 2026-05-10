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
/// Lease revocation event (Unit 5 wiring). Published by a primary
/// provider when its workload state machine emits
/// `PublishLeaseRevocation` — i.e. the local state has left `Live`
/// for a `WarmStandby` workload. Addressable so a standby that came
/// online after the publish can still find it on cold start. The
/// `d` tag is `paygress:revocation:v1:<primary_npub>:<workload_id>`
/// and each standby is added as a `#p` tag for filterable subscriptions.
pub const KIND_LEASE_REVOCATION: u16 = 38385;

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

        let client = Client::new(keys.clone());

        // Add relays
        for relay_url in &config.relays {
            info!("Adding relay: {}", relay_url);
            client
                .add_relay(relay_url)
                .await
                .with_context(|| format!("Invalid relay URL: {}", relay_url))?;
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

        // Lease revocation events (Unit 5 — standby-side promotion).
        // Public events (no encryption); a primary publishes them
        // addressed to the standby_providers via #p tags. The standby's
        // listener filters by its own pubkey to receive only events
        // addressed to it.
        let revocation_filter = Filter::new()
            .kind(Kind::Custom(KIND_LEASE_REVOCATION))
            .pubkeys(vec![self.keys.public_key()])
            .limit(0);

        let _ = self.client.subscribe(nip04_filter, None).await;
        let _ = self.client.subscribe(nip17_filter, None).await;
        let _ = self.client.subscribe(revocation_filter, None).await;
        info!("Subscribed to NIP-04 / NIP-17 messages and KIND_LEASE_REVOCATION events addressed to this provider");

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
                                        kind: rumor.kind.as_u16() as u32,
                                        tags: rumor.tags.iter().map(|tag| {
                                            tag.as_slice().iter().map(|s| s.to_string()).collect()
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
                        // Lease revocation events are public — no
                        // decryption. Build a NostrEvent with the
                        // raw content; the handler dispatches by
                        // kind and parses with parse_revocation_event.
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

    pub async fn publish_offer(&self, offer: OfferEventContent) -> Result<String> {
        let content = serde_json::to_string(&offer)?;
        info!("Publishing offer event with content: {}", content);

        let tags = vec![Tag::hashtag("paygress"), Tag::hashtag("offer")];

        info!("Creating event with kind 999 and {} tags", tags.len());
        let event = EventBuilder::new(Kind::Custom(999), content)
            .tags(tags)
            .sign_with_keys(&self.keys)?;
        let event_id = event.id.to_hex();

        info!("Event created with ID: {}", event_id);
        info!("Sending offer event to relays: {}", event_id);

        match self.client.send_event(&event).await {
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
            "nip17" | _ => {
                // Default to NIP-17 if not specified or nip17
                let event_id = self
                    .client
                    .send_private_msg(receiver_pubkey_parsed, content, [])
                    .await?;
                info!("Sent NIP-17 message to {}: {:?}", receiver_pubkey, event_id);
                Ok(event_id.val.to_hex())
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

    #[allow(dead_code)]
    fn convert_event(&self, event: &nostr_sdk::Event) -> NostrEvent {
        NostrEvent {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_u64(),
            kind: event.kind.as_u16() as u32,
            tags: event
                .tags
                .iter()
                .map(|tag| tag.as_slice().iter().map(|s| s.to_string()).collect())
                .collect(),
            content: event.content.clone(),
            sig: event.sig.to_string(),
            message_type: "unknown".to_string(),
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

        let _ = client.subscribe(filter, None).await;

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

    /// Replication mode requested by the consumer (Unit 5 wiring
    /// completion). Old clients that don't set this field default to
    /// `ReplicationMode::None` — same shape as before, no behavior
    /// change for unspecified spawns.
    ///
    /// `WarmStandby { standby_providers }` is the load-bearing
    /// variant: the consumer sends the SAME spawn request to every
    /// provider in the standby set; each provider determines its own
    /// role (primary if it is not in the standby list, standby
    /// otherwise) and the orchestrator coordinates failover via the
    /// `LeaseRevocation` event published by #34's wiring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication: Option<crate::durable_workload::ReplicationMode>,

    /// Primary provider's npub. Required when `replication` is
    /// `WarmStandby`; ignored otherwise. Lets each receiving
    /// provider self-determine its role: if `self.npub == primary_npub`
    /// it acts as the primary (spawns + heartbeats); otherwise (and
    /// only if it is in `standby_providers`) it acts as a standby
    /// (reserves a slot, listens for revocations, promotes on signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_npub: Option<String>,

    /// Consumer-assigned workload identifier (UUID-shaped string).
    /// Required when `replication` is `WarmStandby` so the primary
    /// and N standbys share one stable id across providers — the
    /// `LeaseRevocation` event uses this id, and the standby looks
    /// up its reserved slot by it on receipt. Single-provider spawns
    /// can leave this unset; the provider derives a vmid-based id
    /// internally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_id: Option<String>,

    /// Optional encryption key for the workload's persistent data
    /// volume. When set, the provider creates a LUKS-encrypted
    /// volume (instead of a plain one) for `template.data_path` and
    /// destroys the volume header on tenancy end so post-eviction
    /// disk forensics reveal only ciphertext.
    ///
    /// Threat model: protects against post-eviction snooping, lazy
    /// host-operator backups, co-tenant attacks on shared storage,
    /// and cold-disk forensics if the host is seized. Does NOT
    /// protect against a live host with `CAP_SYS_PTRACE` reading
    /// /proc/<pid>/mem or extracting the LUKS key from the kernel
    /// keyring while the workload runs — that requires hardware
    /// confidential VMs (SEV-SNP / TDX), which the
    /// `attested-research-tier` `IsolationLevel` is reserved for.
    ///
    /// The key travels inside this Nostr DM, which is itself
    /// NIP-04 / NIP-17 encrypted to the provider's pubkey, so it is
    /// never visible on relays or in transit. The provider holds it
    /// only in memory while the workload runs.
    ///
    /// Old clients that don't set this field get plain volumes —
    /// same shape as before, no behavior change for unspecified
    /// spawns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_encryption: Option<VolumeEncryption>,
}

/// Wire-format request to encrypt the workload's data volume.
///
/// The `key_b64` is a 32-byte symmetric key, base64-encoded
/// (URL-safe, no padding). Provider feeds it to `cryptsetup
/// luksFormat` as a passphrase (raw bytes, no hashing on top).
///
/// `algorithm` is a forward-compat tag so a future schema bump can
/// introduce e.g. `xchacha20-poly1305` or hardware-attested keying
/// without breaking existing requests. v1 supports `luks2-aes-xts`
/// only; providers reject unknown algorithms with a structured
/// `UnsupportedVolumeEncryption` error so old providers seeing a
/// future-algorithm request fail loud rather than silently fall
/// back to plain volumes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeEncryption {
    /// Schema version. v1 = LUKS2 + AES-XTS-Plain64, key supplied
    /// directly. Bump for new key-derivation flows (e.g. attested
    /// key release from a TPM / TEE).
    #[serde(default = "volume_encryption_default_version")]
    pub version: u8,

    /// Algorithm tag. v1 only accepts `luks2-aes-xts`.
    pub algorithm: String,

    /// 32-byte key, base64 (URL-safe, unpadded). Consumer derives
    /// from a stable secret + workload_id so the same key recurs
    /// on respawn / standby promotion (the standby decrypts the
    /// checkpoint with it).
    pub key_b64: String,
}

fn volume_encryption_default_version() -> u8 {
    1
}

impl VolumeEncryption {
    /// Algorithm tag for the v1 wire format. Spelled out so callers
    /// don't need to know the LUKS internals.
    pub const ALGORITHM_V1: &'static str = "luks2-aes-xts";
    pub const VERSION_V1: u8 = 1;

    /// Build a v1 VolumeEncryption from a raw 32-byte key.
    pub fn v1(key: [u8; 32]) -> Self {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        Self {
            version: Self::VERSION_V1,
            algorithm: Self::ALGORITHM_V1.to_string(),
            key_b64: URL_SAFE_NO_PAD.encode(key),
        }
    }

    /// Decode the base64 key back to raw bytes. Errors if the
    /// payload is malformed or the wrong length for the declared
    /// algorithm.
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

/// Pure helper for self-role detection on a `WarmStandby` spawn
/// request. Returns the role this provider should take. Surfaced so
/// the role-routing logic in `provider::handle_spawn_request` is
/// unit-testable without spinning up a state machine.
///
/// Convention:
///   - if `self_npub == primary_npub` → Primary
///   - else if `self_npub` is in `standby_providers` → Standby (with index)
///   - else → NotAddressed (provider should reject the request)
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
    if self_npub == primary_npub {
        return WarmStandbyRole::Primary;
    }
    if let Some(idx) = standby_providers.iter().position(|p| p == self_npub) {
        return WarmStandbyRole::Standby {
            index: idx,
            count: standby_providers.len(),
        };
    }
    WarmStandbyRole::NotAddressed
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
        .send_private_msg(service_pubkey_parsed, request_json, [])
        .await?;

    Ok(event_id.val.to_hex())
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

/// Parse a `NostrEvent` as a `LeaseRevocationContent` if its `kind`
/// matches `KIND_LEASE_REVOCATION` and the body deserializes
/// cleanly. Returns `None` for any non-revocation event so the
/// caller can fall through to other dispatch arms without re-parsing.
///
/// Pure function — exposed so the standby-side dispatcher can be
/// unit-tested without spinning up the relay pool.
pub fn parse_revocation_event(event: &NostrEvent) -> Option<LeaseRevocationContent> {
    if event.kind != KIND_LEASE_REVOCATION as u32 {
        return None;
    }
    serde_json::from_str::<LeaseRevocationContent>(&event.content).ok()
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

/// Lease revocation content (Unit 5 wiring).
///
/// Emitted by a primary provider whose workload state machine has
/// transitioned the workload out of `Live` (typically because the
/// primary observed its own heartbeats failing to reach quorum at
/// relays — split-brain self-eviction). Standby providers listed in
/// `standby_providers` can promote on observing this event without
/// fear of two writers, because the primary has *already* left Live
/// before publishing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRevocationContent {
    /// Consumer-assigned workload identifier (the same UUID-shaped
    /// string the consumer sent in the spawn request as
    /// `workload_id`). Standbys key their slot table by this id and
    /// use it to look up the matching reservation when a revocation
    /// arrives. v0 events used a u32 (the primary's local vmid) —
    /// the change to String is a wire-format bump, but no v0
    /// revocations were ever published in production (#34/#41
    /// shipped the listener, not the publisher's own consumers).
    pub workload_id: String,
    pub primary_provider_npub: String,
    pub standby_providers: Vec<String>,
    pub reason: String,
    pub revoked_at: u64,

    /// Optional Blossom URI of the latest checkpoint (Unit 6). When
    /// set, the standby restores from this state rather than spawning
    /// a fresh container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_uri: Option<String>,

    /// Schema version. v0 (no field on the wire) deserializes to 1.
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

    /// Publish a heartbeat event on BOTH the addressable (stored,
    /// kind 38384) and ephemeral (kind 20384) kinds. Returns the
    /// addressable event id and the set of relay URLs that
    /// successfully accepted the *stored* heartbeat — the orchestrator
    /// loop (Unit 5 wiring) consumes these as `HeartbeatObservation`s
    /// to drive the workload state machine.
    ///
    /// The addressable form uses a per-minute bucketed `d` tag
    /// (`paygress:heartbeat:v1:<npub>:<bucket>`) so each minute's
    /// heartbeat lands in its own `(pubkey, kind, d-tag)` slot.
    /// Without bucketing, every heartbeat replaces the previous one
    /// at the relay and `calculate_uptime` sees zero history.
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

        // 1. Stored, addressable: relays keep this for history-based
        //    queries (calculate_uptime).
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

        // 2. Ephemeral: relays don't store, but live subscribers see
        //    it immediately. Cheap and good for dashboards.
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

    /// Publish a `LeaseRevocationContent` event (Unit 5 wiring).
    ///
    /// Addressable kind 38385, keyed by
    /// `(pubkey, kind, d="paygress:revocation:v1:<primary_npub>:<workload_id>")`
    /// so a standby coming online after the publish still observes
    /// the latest revocation for that workload. Each standby is
    /// added as a `#p` tag so subscribers filtering by their own
    /// pubkey see only revocations addressed to them.
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
        let workload_id_str = revocation.workload_id.to_string();

        let mut tags = vec![
            Tag::hashtag("paygress"),
            Tag::hashtag("paygress-revocation"),
            Tag::parse(["d", d_tag.as_str()])?,
            Tag::parse(["v", v_tag.as_str()])?,
            Tag::parse(["workload", workload_id_str.as_str()])?,
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

    /// Query all provider offers from relays
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
            .fetch_events(filter, std::time::Duration::from_secs(3))
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
