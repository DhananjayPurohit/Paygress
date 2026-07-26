// Consumer-side discovery of providers advertised on Nostr.

use anyhow::Result;
use tracing::info;

use crate::nostr::{NostrRelaySubscriber, ProviderFilter, ProviderInfo, RelayConfig};

pub struct DiscoveryClient {
    nostr: NostrRelaySubscriber,
}

/// A provider whose most recent heartbeat is older than this is
/// reported offline.
const ONLINE_HEARTBEAT_WINDOW_SECS: u64 = 120;

impl DiscoveryClient {
    /// Read-only client; no key needed for queries.
    pub async fn new(relays: Vec<String>) -> Result<Self> {
        let config = RelayConfig {
            relays,
            private_key: None,
        };

        let nostr = NostrRelaySubscriber::new(config).await?;

        Ok(Self { nostr })
    }

    /// Client that can also send DMs (spawn / topup / status).
    pub async fn new_with_key(relays: Vec<String>, private_key: String) -> Result<Self> {
        let config = RelayConfig {
            relays,
            private_key: Some(private_key),
        };

        let nostr = NostrRelaySubscriber::new(config).await?;

        Ok(Self { nostr })
    }

    pub fn get_npub(&self) -> String {
        self.nostr.get_service_public_key()
    }

    pub async fn list_providers(
        &self,
        filter: Option<ProviderFilter>,
    ) -> Result<Vec<ProviderInfo>> {
        let offers = self.nostr.query_providers().await?;

        let mut providers = Vec::new();

        // One batched query rather than a round-trip per provider.
        let provider_npubs: Vec<String> = offers.iter().map(|o| o.provider_npub.clone()).collect();
        let heartbeats = self
            .nostr
            .get_latest_heartbeats_multi(provider_npubs)
            .await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        for offer in offers {
            let (is_online, last_seen) = match heartbeats.get(&offer.provider_npub) {
                Some(hb) => (
                    now.saturating_sub(hb.timestamp) < ONLINE_HEARTBEAT_WINDOW_SECS,
                    hb.timestamp,
                ),
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

    /// Look up a provider by ID or friendly name, so every
    /// `--provider` flag accepts either form. `input` is tried as:
    ///
    /// 1. an exact ID — full hex pubkey or `npub1…` bech32;
    /// 2. an unambiguous ID prefix of ≥ 8 hex chars;
    /// 3. the provider's 3-word name, case-insensitively.
    pub async fn get_provider(&self, input: &str) -> Result<Option<ProviderInfo>> {
        let providers = self.list_providers(None).await?;

        // Normalize to hex; handles both raw hex and npub1… bech32.
        let lookup_hex = match nostr_sdk::PublicKey::parse(input) {
            Ok(pk) => pk.to_hex(),
            Err(_) => input.to_string(),
        };

        if let Some(p) = providers.iter().find(|p| p.npub == lookup_hex) {
            return Ok(Some(p.clone()));
        }

        if lookup_hex.len() >= 8 {
            let matches: Vec<&ProviderInfo> = providers
                .iter()
                .filter(|p| p.npub.starts_with(&lookup_hex))
                .collect();

            if matches.len() == 1 {
                return Ok(Some(matches[0].clone()));
            }
        }

        let input_lower = input.to_lowercase();
        let name_matches: Vec<&ProviderInfo> = providers
            .iter()
            .filter(|p| p.hostname.to_lowercase() == input_lower)
            .collect();

        match name_matches.len() {
            0 => Ok(None),
            1 => Ok(Some(name_matches[0].clone())),
            _ => {
                // Possible when the same Nostr key was bootstrapped
                // on two machines. Make the user disambiguate.
                let ids: Vec<String> = name_matches
                    .iter()
                    // npub comes off the wire; slice by chars so a short
                    // or non-ASCII value can't panic here.
                    .map(|p| format!("  {}", p.npub.chars().take(16).collect::<String>()))
                    .collect();
                anyhow::bail!(
                    "multiple providers share the name '{}'; use the provider ID instead:\n{}",
                    input,
                    ids.join("\n")
                )
            }
        }
    }

    pub async fn is_provider_online(&self, npub: &str) -> bool {
        match self.get_provider(npub).await {
            Ok(Some(p)) => p.is_online,
            _ => false,
        }
    }

    pub async fn get_uptime(&self, npub: &str, days: u32) -> Result<f32> {
        let full_npub = if let Ok(Some(p)) = self.get_provider(npub).await {
            p.npub
        } else {
            npub.to_string()
        };
        self.nostr.calculate_uptime(&full_npub, days).await
    }

    /// Underlying Nostr client, for sending messages.
    pub fn nostr(&self) -> &NostrRelaySubscriber {
        &self.nostr
    }

    /// Sort in place by `price`, `uptime`, `capacity` or `jobs`.
    /// Any other value leaves the order untouched.
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
            _ => {}
        }
    }

    pub fn format_provider_table(providers: &[ProviderInfo]) -> String {
        use std::fmt::Write;

        let mut output = String::new();

        // Column widths: ID(16) | PROVIDER(18) | LOCATION(10) | UPTIME(8) | CHEAPEST(8) | TIER(10) | MINTS(36) | ONLINE(6)
        // Inner = 112, separators = 9×3 = 27 → 139 + 2 borders = 141
        writeln!(&mut output, "┌─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐").unwrap();
        writeln!(
            &mut output,
            "│ {:^16} │ {:^18} │ {:^10} │ {:^8} │ {:^8} │ {:^10} │ {:^36} │ {:^6} │",
            "ID", "PROVIDER", "LOCATION", "UPTIME", "CHEAPEST", "TIER", "MINTS", "ONLINE"
        )
        .unwrap();
        writeln!(&mut output, "├─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤").unwrap();

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
            // Compact labels that fit the 10-char column.
            let tier = match p.isolation_level {
                crate::nostr::IsolationLevel::SharedKernel => "shared",
                crate::nostr::IsolationLevel::DedicatedHost => "dedicated",
                crate::nostr::IsolationLevel::AttestedResearchTier => "attested",
            };
            let mints = format_mints_column(&p.whitelisted_mints, 36);
            let online = if p.is_online { "✓" } else { "✗" };

            writeln!(
                &mut output,
                "│ {:16} │ {:18} │ {:^10} │ {:>6.1}% │ {:>8} │ {:^10} │ {:^36} │ {:^6} │",
                id,
                truncate_str(&p.hostname, 18),
                truncate_str(location, 10),
                p.uptime_percent,
                cheapest,
                tier,
                mints,
                online
            )
            .unwrap();
        }

        writeln!(&mut output, "└─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘").unwrap();

        output
    }

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

/// Truncate by characters, not bytes. These strings come off the wire
/// (provider names, mint hostnames) and may be non-ASCII, where a byte
/// slice can land mid-codepoint and panic.
fn truncate_str(s: &str, max_len: usize) -> &str {
    let keep = max_len.saturating_sub(2);
    match s.char_indices().nth(keep) {
        Some((byte_idx, _)) if s.chars().count() > max_len => &s[..byte_idx],
        _ => s,
    }
}

/// Strip the URL scheme and any path, keeping the full hostname
/// (`https://mint.minibits.cash/api` → `mint.minibits.cash`).
fn mint_label(url: &str) -> String {
    let stripped = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    stripped.split('/').next().unwrap_or(stripped).to_string()
}

/// Render whitelisted mints into a `col`-wide table column:
/// `-` when empty, one or two labels otherwise, with a ` +N`
/// overflow suffix and truncation only when the result won't fit.
fn format_mints_column(mints: &[String], col: usize) -> String {
    match mints.len() {
        0 => "-".to_string(),
        1 => truncate_owned(mint_label(&mints[0]), col),
        n => {
            let l0 = mint_label(&mints[0]);
            let l1 = mint_label(&mints[1]);
            let suffix = if n > 2 {
                format!(" +{}", n - 2)
            } else {
                String::new()
            };
            let combined = format!("{}, {}{}", l0, l1, suffix);
            if combined.len() <= col {
                return combined;
            }
            let two = format!("{}, {}", l0, l1);
            if two.len() <= col {
                return truncate_owned(two, col);
            }
            let sfx = format!(" +{}", if n > 2 { n - 1 } else { 1 });
            let room = col.saturating_sub(sfx.len());
            format!("{}{}", truncate_owned(l0, room), sfx)
        }
    }
}

fn truncate_owned(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let keep = max.saturating_sub(2);
    let cut = s
        .char_indices()
        .nth(keep)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}..", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nostr::PodSpec;

    // These strings arrive from Nostr offers, so a byte slice landing
    // mid-codepoint is a remote panic, not a display bug.
    #[test]
    fn truncation_does_not_split_multibyte_characters() {
        let s = "日本語のプロバイダー名です";
        assert!(truncate_str(s, 5).chars().count() <= 5);
        assert!(truncate_owned(s.to_string(), 5).chars().count() <= 5 + 2);
        // Emoji are 4 bytes each — the worst case for byte slicing.
        let emoji = "🚀🚀🚀🚀🚀🚀";
        assert!(truncate_str(emoji, 3).chars().count() <= 3);
        assert!(!truncate_owned(emoji.to_string(), 3).is_empty());
    }

    #[test]
    fn truncation_leaves_short_strings_alone() {
        assert_eq!(truncate_str("abc", 10), "abc");
        assert_eq!(truncate_owned("abc".to_string(), 10), "abc");
        // max < 2 must not underflow.
        assert!(truncate_str("abcdef", 1).chars().count() <= 1);
        assert!(!truncate_owned("abcdef".to_string(), 1).is_empty());
    }

    #[test]
    fn mint_label_keeps_mint_subdomain() {
        assert_eq!(
            mint_label("https://mint.minibits.cash"),
            "mint.minibits.cash"
        );
        assert_eq!(
            mint_label("https://testnut.cashu.space"),
            "testnut.cashu.space"
        );
        assert_eq!(mint_label("http://localhost:3338"), "localhost:3338");
        assert_eq!(
            mint_label("https://mint.example.com/api/v1"),
            "mint.example.com"
        );
    }

    #[test]
    fn format_mints_column_shows_two() {
        let mints = vec![
            "https://mint.minibits.cash".to_string(),
            "https://mint.nucash.com".to_string(),
        ];
        let result = format_mints_column(&mints, 36);
        assert!(result.contains("mint.minibits.cash"), "missing first mint");
        assert!(result.contains("mint.nucash.com"), "missing second mint");
        assert!(
            !result.contains("+"),
            "unexpected overflow suffix for 2 mints"
        );
    }

    #[test]
    fn format_mints_column_shows_two_plus_overflow() {
        let mints = vec![
            "https://mint.a.com".to_string(),
            "https://mint.b.com".to_string(),
            "https://mint.c.com".to_string(),
        ];
        let result = format_mints_column(&mints, 36);
        assert!(result.contains("mint.a.com"), "missing first mint");
        assert!(result.contains("mint.b.com"), "missing second mint");
        assert!(result.contains("+1"), "missing overflow suffix");
    }

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
