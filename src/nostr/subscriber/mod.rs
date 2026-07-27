// Relay client: connection setup and encrypted DM send/receive. Event
// publishing lives in `publish`, relay queries in `query`.

mod publish;
mod query;

use anyhow::{Context, Result};
use nostr_sdk::nips::nip04;
use nostr_sdk::nips::nip59::UnwrappedGift;
use nostr_sdk::{Client, EventBuilder, Filter, Keys, Kind, RelayPoolNotification, Tag, ToBech32};
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use super::kinds::*;
use super::wire::*;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub relays: Vec<String>,
    pub private_key: Option<String>,
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

fn tags_to_strings(tags: &nostr_sdk::Tags) -> Vec<Vec<String>> {
    tags.iter().map(|tag| tag.as_slice().to_vec()).collect()
}

fn nostr_event_from(event: &nostr_sdk::Event, content: String, message_type: &str) -> NostrEvent {
    NostrEvent {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_u64(),
        kind: event.kind.as_u16() as u32,
        tags: tags_to_strings(&event.tags),
        content,
        sig: event.sig.to_string(),
        message_type: message_type.to_string(),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
            keys.public_key()
                .to_bech32()
                .unwrap_or_else(|_| keys.public_key().to_hex())
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

        self.client
            .handle_notifications(|notification| async {
                let RelayPoolNotification::Event { event, .. } = notification else {
                    return Ok(false);
                };
                if let Some(decoded) = self.decode_inbound_event(&event).await {
                    let message_type = decoded.message_type.clone();
                    match handler(decoded).await {
                        Ok(()) => info!("Processed {} event {}", message_type, event.id),
                        Err(e) => {
                            error!(
                                "Failed to process {} event {}: {}",
                                message_type, event.id, e
                            )
                        }
                    }
                }
                Ok(false) // keep listening
            })
            .await?;

        Ok(())
    }

    /// Decrypt/unwrap an inbound relay event into the form the caller's handler
    /// consumes. `None` for anything not routable: an unsupported kind, a
    /// gift wrap that will not unwrap, a rumor that is not a DM, or a NIP-04
    /// body we cannot decrypt.
    async fn decode_inbound_event(&self, event: &nostr_sdk::Event) -> Option<NostrEvent> {
        match event.kind {
            Kind::GiftWrap => {
                info!("Received NIP-17 Gift Wrap message: {}", event.id);
                let UnwrappedGift { rumor, sender } =
                    match self.client.unwrap_gift_wrap(event).await {
                        Ok(gift) => gift,
                        Err(e) => {
                            error!("Failed to unwrap Gift Wrap {}: {}", event.id, e);
                            return None;
                        }
                    };
                info!(
                    "Unwrapped Gift Wrap from sender: {}, rumor kind: {}",
                    sender, rumor.kind
                );
                if rumor.kind != Kind::PrivateDirectMessage {
                    info!(
                        "Rumor is not a private direct message, kind: {}",
                        rumor.kind
                    );
                    return None;
                }
                debug!(
                    "NIP-17 rumor is PrivateDirectMessage. Content length: {}",
                    rumor.content.len()
                );
                Some(NostrEvent {
                    id: rumor
                        .id
                        .map(|id| id.to_hex())
                        .unwrap_or_else(|| "unknown".to_string()),
                    pubkey: rumor.pubkey.to_hex(),
                    created_at: rumor.created_at.as_u64(),
                    kind: rumor.kind.as_u16() as u32,
                    tags: tags_to_strings(&rumor.tags),
                    content: rumor.content,
                    sig: "unsigned".to_string(), // rumors are unsigned by construction
                    message_type: "nip17".to_string(),
                })
            }
            Kind::EncryptedDirectMessage => {
                info!("Received NIP-04 Encrypted Direct Message: {}", event.id);
                match nip04::decrypt(self.keys.secret_key(), &event.pubkey, &event.content) {
                    Ok(content) => {
                        debug!("Decrypted NIP-04 message. Length: {}", content.len());
                        Some(nostr_event_from(event, content, "nip04"))
                    }
                    Err(e) => {
                        error!("Failed to decrypt NIP-04 message {}: {}", event.id, e);
                        None
                    }
                }
            }
            // Public event — no decryption; the handler dispatches by kind and
            // parses with parse_revocation_event.
            Kind::Custom(k) if k == KIND_LEASE_REVOCATION => {
                info!("Received lease revocation event: {}", event.id);
                Some(nostr_event_from(
                    event,
                    event.content.clone(),
                    "lease_revocation",
                ))
            }
            _ => {
                info!("Received unsupported event kind: {}", event.kind);
                None
            }
        }
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

    /// JSON-serialize `body` and send it as an encrypted DM.
    async fn send_json_private_message<T: Serialize>(
        &self,
        request_pubkey: &str,
        body: &T,
        message_type: &str,
    ) -> Result<String> {
        let json = serde_json::to_string(body)?;
        self.send_encrypted_private_message(request_pubkey, json, message_type)
            .await
    }

    pub async fn send_access_details_private_message(
        &self,
        request_pubkey: &str,
        details: AccessDetailsContent,
        message_type: &str,
    ) -> Result<String> {
        self.send_json_private_message(request_pubkey, &details, message_type)
            .await
    }

    pub async fn send_status_response(
        &self,
        request_pubkey: &str,
        response: StatusResponseContent,
        message_type: &str,
    ) -> Result<String> {
        self.send_json_private_message(request_pubkey, &response, message_type)
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
        self.send_json_private_message(request_pubkey, &error, message_type)
            .await
    }

    pub async fn send_topup_response_private_message(
        &self,
        request_pubkey: &str,
        response: TopUpResponseContent,
        message_type: &str,
    ) -> Result<String> {
        self.send_json_private_message(request_pubkey, &response, message_type)
            .await
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn get_service_public_key(&self) -> String {
        self.keys.public_key().to_hex()
    }

    /// Wait for a private decrypted message from a specific sender.
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
        // lookback absorbs clock skew and covers a provider that already
        // replied before we subscribed.
        let subscribe_since = unix_now().saturating_sub(60);
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
                                            tags: tags_to_strings(&rumor.tags),
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
                                        event_to_send = Some(nostr_event_from(&event, content, "nip04"));
                                    }
                                }
                            }
                            _ => {}
                        }

                        if let Some(ev) = event_to_send {
                            let mut lock = tx.lock().await;
                            if let Some(sender) = lock.take() {
                                let _ = sender.send(ev).await;
                                return Ok(true); // stop listening
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
