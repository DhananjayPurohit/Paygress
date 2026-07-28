// Publishing side of the relay client: offers, heartbeats, lease revocations
// and standby promotion announcements.

use anyhow::Result;
use nostr_sdk::{EventBuilder, Kind, Tag};
use tracing::{debug, error, info, warn};

use super::NostrRelaySubscriber;
use crate::nostr::kinds::*;
use crate::nostr::wire::*;

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

    /// Publish an arbitrary signed event. The kind and tags are the caller's
    /// schema, not ours — used for CI job results, whose shape belongs to the
    /// ngit-ci NIP rather than to paygress.
    pub async fn publish_foreign_event(
        &self,
        kind: u16,
        content: String,
        tags: Vec<Vec<String>>,
    ) -> Result<String> {
        let parsed: Result<Vec<Tag>, _> = tags.iter().map(Tag::parse).collect();
        let event = EventBuilder::new(Kind::Custom(kind), content)
            .tags(parsed?)
            .sign_with_keys(&self.keys)?;
        let event_id = event.id.to_hex();

        match self.client.send_event(&event).await {
            Ok(out) if out.success.is_empty() => {
                // `send_event` succeeds as long as the send itself did not
                // error, so an event every relay refused looks identical to a
                // published one unless the acceptances are counted.
                Err(anyhow::anyhow!(
                    "kind-{} event {} was accepted by no relay ({} refused)",
                    kind,
                    event_id,
                    out.failed.len()
                ))
            }
            Ok(out) => {
                info!(
                    "Published kind-{} event {}: accepted by {} relay(s)",
                    kind,
                    event_id,
                    out.success.len()
                );
                Ok(event_id)
            }
            Err(e) => {
                error!("Failed to publish kind-{} event: {}", kind, e);
                Err(e.into())
            }
        }
    }
}
