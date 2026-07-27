// Read side of the relay client: offer, heartbeat and promotion lookups.

use std::collections::HashMap;

use anyhow::Result;
use nostr_sdk::{Filter, Kind, Timestamp};
use tracing::{info, warn};

use super::{unix_now, NostrRelaySubscriber};
use crate::nostr::identity::offer_authored_by_claimed_provider;
use crate::nostr::kinds::*;
use crate::nostr::wire::*;

impl NostrRelaySubscriber {
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
    ) -> Result<HashMap<String, HeartbeatContent>> {
        if provider_npubs.is_empty() {
            return Ok(HashMap::new());
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

        let mut heartbeats: HashMap<String, HeartbeatContent> = HashMap::new();

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
