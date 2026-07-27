// Relay client: encrypted DM send/receive plus offer, heartbeat, revocation
// and promotion publishing/querying.

use anyhow::{Context, Result};
use nostr_sdk::nips::nip04;
use nostr_sdk::nips::nip59::UnwrappedGift;
use nostr_sdk::{
    Client, EventBuilder, Filter, Keys, Kind, RelayPoolNotification, Tag, Timestamp, ToBech32,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use super::identity::offer_authored_by_claimed_provider;
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

        self.client.handle_notifications(|notification| async {
            if let RelayPoolNotification::Event { relay_url: _, subscription_id: _, event } = notification {
                match event.kind {
                    Kind::GiftWrap => {
                        info!("Received NIP-17 Gift Wrap message: {}", event.id);

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
                                        tags: tags_to_strings(&rumor.tags),
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

                                let nostr_event = nostr_event_from(&event, decrypted_content, "nip04");

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
                        // Public event — no decryption; the handler dispatches
                        // by kind and parses with parse_revocation_event.
                        info!("Received lease revocation event: {}", event.id);
                        let nostr_event =
                            nostr_event_from(&event, event.content.clone(), "lease_revocation");
                        if let Err(e) = handler(nostr_event).await {
                            error!("Failed to process lease revocation {}: {}", event.id, e);
                        }
                    }
                    _ => {
                        info!("Received unsupported event kind: {}", event.kind);
                    }
                }
            }
            Ok(false) // keep listening
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
        let live_since = unix_now().saturating_sub(LIVE_HEARTBEAT_WINDOW_SECS);

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

        let live_since = unix_now().saturating_sub(LIVE_HEARTBEAT_WINDOW_SECS);

        let filter = Filter::new()
            .kind(Kind::Custom(KIND_PROVIDER_HEARTBEAT))
            .authors(pubkeys)
            .since(Timestamp::from(live_since));

        let events = self
            .client
            .fetch_events(filter, std::time::Duration::from_secs(3))
            .await?;

        let mut heartbeats: std::collections::HashMap<String, HeartbeatContent> =
            std::collections::HashMap::new();

        for event in events {
            if let Ok(hb) = serde_json::from_str::<HeartbeatContent>(&event.content) {
                match heartbeats.entry(hb.provider_npub.clone()) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        if hb.timestamp > entry.get().timestamp {
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
        let since = unix_now().saturating_sub(days as u64 * 24 * 60 * 60);
        let heartbeats = self.query_heartbeats(provider_npub, since).await?;

        if heartbeats.is_empty() {
            return Ok(0.0);
        }

        let expected = (days as f32) * 24.0 * 3600.0 / HEARTBEAT_BUCKET_SECS as f32;
        let actual = heartbeats.len() as f32;

        Ok((actual / expected * 100.0).min(100.0))
    }
}
