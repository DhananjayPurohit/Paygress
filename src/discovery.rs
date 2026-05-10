// Discovery Client
//
// Used by end users to discover available providers on Nostr
// and interact with them for spawning workloads.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::nostr::{
    NostrRelaySubscriber, PodSpec, ProviderFilter, ProviderInfo, ProviderOfferContent, RelayConfig,
};

/// Discovery client for finding providers
pub struct DiscoveryClient {
    nostr: NostrRelaySubscriber,
}

impl DiscoveryClient {
    /// Create a new discovery client
    pub async fn new(relays: Vec<String>) -> Result<Self> {
        let config = RelayConfig {
            relays,
            private_key: None, // Read-only client doesn't need a key
        };

        let nostr = NostrRelaySubscriber::new(config).await?;

        Ok(Self { nostr })
    }

    /// Create with a private key (for sending spawn requests)
    pub async fn new_with_key(relays: Vec<String>, private_key: String) -> Result<Self> {
        let config = RelayConfig {
            relays,
            private_key: Some(private_key),
        };

        let nostr = NostrRelaySubscriber::new(config).await?;

        Ok(Self { nostr })
    }

    /// Get the client's public key (npub)
    pub fn get_npub(&self) -> String {
        self.nostr.get_service_public_key()
    }

    /// List all available providers
    pub async fn list_providers(
        &self,
        filter: Option<ProviderFilter>,
    ) -> Result<Vec<ProviderInfo>> {
        let offers = self.nostr.query_providers().await?;

        let mut providers = Vec::new();

        // Optimisation: Fetch all heartbeats in parallel (batch query)
        let provider_npubs: Vec<String> = offers.iter().map(|o| o.provider_npub.clone()).collect();
        let heartbeats = self
            .nostr
            .get_latest_heartbeats_multi(provider_npubs)
            .await?;

        for offer in offers {
            // Check if provider is online (has recent heartbeat)
            let (is_online, last_seen) = match heartbeats.get(&offer.provider_npub) {
                Some(hb) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    // Consider online if heartbeat within last 2 minutes
                    (now - hb.timestamp < 120, hb.timestamp)
                }
                None => (false, 0),
            };

            let provider = ProviderInfo {
                npub: offer.provider_npub.clone(),
                hostname: offer.hostname,
                location: offer.location,
                capabilities: offer.capabilities,
                specs: offer.specs,
                whitelisted_mints: offer.whitelisted_mints,
                uptime_percent: offer.uptime_percent,
                total_jobs_completed: offer.total_jobs_completed,
                last_seen,
                is_online,
                isolation_level: offer.isolation_level,
            };

            // Apply filters
            if let Some(ref f) = filter {
                if let Some(ref cap) = f.capability {
                    if !provider.capabilities.contains(cap) {
                        continue;
                    }
                }
                if let Some(min_uptime) = f.min_uptime {
                    if provider.uptime_percent < min_uptime {
                        continue;
                    }
                }
                if let Some(min_mem) = f.min_memory_mb {
                    if !provider.specs.iter().any(|s| s.memory_mb >= min_mem) {
                        continue;
                    }
                }
                if let Some(min_cpu) = f.min_cpu {
                    if !provider.specs.iter().any(|s| s.cpu_millicores >= min_cpu) {
                        continue;
                    }
                }
                if let Some(min_iso) = f.isolation_level {
                    if !provider.isolation_level.meets(min_iso) {
                        continue;
                    }
                }
            }

            providers.push(provider);
        }

        info!("Found {} providers matching filter", providers.len());
        Ok(providers)
    }

    /// Get details of a specific provider (supports exact match or prefix of at least 8 chars)
    /// Accepts both hex pubkeys and bech32 npub format.
    pub async fn get_provider(&self, npub: &str) -> Result<Option<ProviderInfo>> {
        let providers = self.list_providers(None).await?;

        // Normalize input to hex for comparison (provider npubs are stored as hex)
        let lookup_hex = match nostr_sdk::PublicKey::parse(npub) {
            Ok(pk) => pk.to_hex(),
            Err(_) => npub.to_string(),
        };

        // precise match first
        if let Some(p) = providers.iter().find(|p| p.npub == lookup_hex) {
            return Ok(Some(p.clone()));
        }

        // try prefix match if long enough
        if lookup_hex.len() >= 8 {
            let matches: Vec<&ProviderInfo> = providers
                .iter()
                .filter(|p| p.npub.starts_with(&lookup_hex))
                .collect();

            if matches.len() == 1 {
                return Ok(Some(matches[0].clone()));
            }
        }

        Ok(None)
    }

    /// Check if a provider is online
    pub async fn is_provider_online(&self, npub: &str) -> bool {
        match self.get_provider(npub).await {
            Ok(Some(p)) => p.is_online,
            _ => false,
        }
    }

    /// Get uptime percentage for a provider
    pub async fn get_uptime(&self, npub: &str, days: u32) -> Result<f32> {
        // Resolve full npub
        let full_npub = if let Ok(Some(p)) = self.get_provider(npub).await {
            p.npub
        } else {
            npub.to_string()
        };
        self.nostr.calculate_uptime(&full_npub, days).await
    }

    /// Get the underlying Nostr client (for sending messages)
    pub fn nostr(&self) -> &NostrRelaySubscriber {
        &self.nostr
    }

    /// Sort providers by various criteria
    pub fn sort_providers(providers: &mut [ProviderInfo], sort_by: &str) {
        match sort_by {
            "price" => {
                providers.sort_by(|a, b| {
                    let a_rate = a
                        .specs
                        .first()
                        .map(|s| s.rate_msats_per_sec)
                        .unwrap_or(u64::MAX);
                    let b_rate = b
                        .specs
                        .first()
                        .map(|s| s.rate_msats_per_sec)
                        .unwrap_or(u64::MAX);
                    a_rate.cmp(&b_rate)
                });
            }
            "uptime" => {
                providers.sort_by(|a, b| {
                    b.uptime_percent
                        .partial_cmp(&a.uptime_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            "capacity" => {
                providers.sort_by(|a, b| {
                    let a_mem = a.specs.iter().map(|s| s.memory_mb).max().unwrap_or(0);
                    let b_mem = b.specs.iter().map(|s| s.memory_mb).max().unwrap_or(0);
                    b_mem.cmp(&a_mem)
                });
            }
            "jobs" => {
                providers.sort_by(|a, b| b.total_jobs_completed.cmp(&a.total_jobs_completed));
            }
            _ => {} // No sorting
        }
    }

    /// Format provider list for display
    pub fn format_provider_table(providers: &[ProviderInfo]) -> String {
        use std::fmt::Write;

        let mut output = String::new();

        // Replaced the historically-uniform `LXC/VM` column with
        // `TIER` (the offer's isolation level) — every provider
        // today reports `lxc/vm`, so the old column was decorative.
        // `TIER` is the only column that distinguishes a Docker
        // provider from a per-VM KVM provider in the listing.
        writeln!(&mut output, "┌──────────────────────────────────────────────────────────────────────────────────────────────────────┐").unwrap();
        writeln!(
            &mut output,
            "│ {:^16} │ {:^18} │ {:^10} │ {:^8} │ {:^8} │ {:^10} │ {:^6} │",
            "ID", "PROVIDER", "LOCATION", "UPTIME", "CHEAPEST", "TIER", "ONLINE"
        )
        .unwrap();
        writeln!(&mut output, "├──────────────────────────────────────────────────────────────────────────────────────────────────────┤").unwrap();

        for p in providers {
            let id = truncate_str(&p.npub, 16);
            let location = p.location.as_deref().unwrap_or("Unknown");
            let cheapest = p
                .specs
                .iter()
                .map(|s| s.rate_msats_per_sec)
                .min()
                .map(|r| format!("{}m/s", r))
                .unwrap_or_else(|| "-".to_string());
            // Compact tier label that fits the 10-char column.
            // `attested-research-tier` is too long; abbreviate.
            let tier = match p.isolation_level {
                crate::nostr::IsolationLevel::SharedKernel => "shared",
                crate::nostr::IsolationLevel::DedicatedHost => "dedicated",
                crate::nostr::IsolationLevel::AttestedResearchTier => "attested",
            };
            let online = if p.is_online { "✓" } else { "✗" };

            writeln!(
                &mut output,
                "│ {:16} │ {:18} │ {:^10} │ {:>6.1}% │ {:>8} │ {:^10} │ {:^6} │",
                id,
                truncate_str(&p.hostname, 18),
                truncate_str(location, 10),
                p.uptime_percent,
                cheapest,
                tier,
                online
            )
            .unwrap();
        }

        writeln!(&mut output, "└──────────────────────────────────────────────────────────────────────────────────────────────────────┘").unwrap();

        output
    }

    /// Format single provider details
    pub fn format_provider_details(provider: &ProviderInfo) -> String {
        use std::fmt::Write;

        let mut output = String::new();

        writeln!(
            &mut output,
            "┌────────────────────────────────────────────────────────────┐"
        )
        .unwrap();
        writeln!(&mut output, "│ Provider: {}", provider.hostname).unwrap();
        writeln!(
            &mut output,
            "├────────────────────────────────────────────────────────────┤"
        )
        .unwrap();
        writeln!(
            &mut output,
            "│ NPUB:       {}",
            truncate_str(&provider.npub, 45)
        )
        .unwrap();
        writeln!(
            &mut output,
            "│ Location:   {}",
            provider.location.as_deref().unwrap_or("Unknown")
        )
        .unwrap();
        writeln!(&mut output, "│ Uptime:     {:.1}%", provider.uptime_percent).unwrap();
        writeln!(
            &mut output,
            "│ Jobs Done:  {}",
            provider.total_jobs_completed
        )
        .unwrap();
        writeln!(
            &mut output,
            "│ Status:     {}",
            if provider.is_online {
                "🟢 Online"
            } else {
                "🔴 Offline"
            }
        )
        .unwrap();
        writeln!(
            &mut output,
            "│ Supports:   {}",
            provider.capabilities.join(", ")
        )
        .unwrap();
        // Full slug here (vs the abbreviated form in the table).
        // Annotated so a reader who's only just discovering the
        // tier system understands what each label means without
        // bouncing to the docs.
        let iso_annotation = match provider.isolation_level {
            crate::nostr::IsolationLevel::SharedKernel => " (containers; co-tenant boundary only)",
            crate::nostr::IsolationLevel::DedicatedHost => {
                " (per-VM; no co-tenants, but operator can read guest)"
            }
            crate::nostr::IsolationLevel::AttestedResearchTier => {
                " (SEV-SNP / TDX; operator cannot read guest memory)"
            }
        };
        writeln!(
            &mut output,
            "│ Isolation:  {}{}",
            provider.isolation_level.slug(),
            iso_annotation
        )
        .unwrap();
        writeln!(
            &mut output,
            "├────────────────────────────────────────────────────────────┤"
        )
        .unwrap();
        writeln!(&mut output, "│ Available Tiers:").unwrap();

        for spec in &provider.specs {
            writeln!(
                &mut output,
                "│   • {} ({}) - {} msat/sec",
                spec.name, spec.id, spec.rate_msats_per_sec
            )
            .unwrap();
            writeln!(
                &mut output,
                "│     {} vCPU, {} MB RAM",
                spec.cpu_millicores / 1000,
                spec.memory_mb
            )
            .unwrap();
        }

        writeln!(
            &mut output,
            "├────────────────────────────────────────────────────────────┤"
        )
        .unwrap();
        writeln!(&mut output, "│ Accepted Mints:").unwrap();
        for mint in &provider.whitelisted_mints {
            writeln!(&mut output, "│   • {}", mint).unwrap();
        }
        writeln!(
            &mut output,
            "└────────────────────────────────────────────────────────────┘"
        )
        .unwrap();

        output
    }
}

/// Helper to truncate strings for display
fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len - 2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_provider_table() {
        let providers = vec![ProviderInfo {
            npub: "npub123".to_string(),
            hostname: "Test Provider".to_string(),
            location: Some("US-East".to_string()),
            capabilities: vec!["lxc".to_string()],
            specs: vec![PodSpec {
                id: "basic".to_string(),
                name: "Basic".to_string(),
                description: "Test".to_string(),
                cpu_millicores: 1000,
                memory_mb: 1024,
                rate_msats_per_sec: 50,
            }],
            whitelisted_mints: vec![],
            uptime_percent: 99.5,
            total_jobs_completed: 10,
            last_seen: 0,
            is_online: true,
            isolation_level: crate::nostr::IsolationLevel::SharedKernel,
        }];

        let table = DiscoveryClient::format_provider_table(&providers);
        assert!(table.contains("Test Provider"));
        assert!(table.contains("99.5%"));
    }
}
