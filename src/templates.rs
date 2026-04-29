// Killer templates — the real-workload definitions consumers spawn
// via `paygress deploy <template>`.
//
// Each template is a deliberate intersection of:
//   - a working public Docker image (no `ubuntu:22.04` placeholders),
//   - a port profile (host-port mapping the consumer needs to know),
//   - per-template environment defaults,
//   - a sensible replication mode (relay = warm-standby; browser =
//     none; etc.) tied to the workload's recovery semantics,
//   - a `compose_path` pointing at a checked-in `docker-compose.yml`
//     so anyone can reproduce the workload locally.
//
// The CLI's `deploy` command consumes these definitions; the
// provider's spawn handler currently spawns the image only (port and
// env wiring lands when the Durable Workload state machine wires the
// fourth concurrent loop in `ProviderService::run`).

use std::collections::HashMap;

/// Replication mode at the **template-default** level — "what does
/// the workload's recovery model look like, before consumer
/// overrides?". Distinct from `durable_workload::ReplicationMode`
/// (which carries runtime data like the standby provider list) and
/// `cli::commands::deploy::ReplicationMode` (the CLI flag enum).
/// This one is a const-friendly tag for template tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationMode {
    /// Crash → consumer retries on a fresh provider. Cheapest;
    /// suitable for stateless workloads.
    None,
    /// Periodic state checkpoints (Blossom). Restart from latest
    /// checkpoint on the same or a fresh provider.
    Checkpointed,
    /// Periodic checkpoints PLUS a hot standby on a second
    /// provider. Single-writer always.
    WarmStandby,
}

/// Templates the marketplace knows about. Adding one is a
/// compatibility-bearing decision: consumers may pin by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemplateName {
    NostrRelay,
    InferenceEndpoint,
    HeadlessBrowser,
    BitcoinNode,
}

impl TemplateName {
    /// Wire-format slug used by `paygress deploy <slug>` and the
    /// `templates/<slug>/` directory.
    pub fn slug(self) -> &'static str {
        match self {
            Self::NostrRelay => "nostr-relay",
            Self::InferenceEndpoint => "inference-endpoint",
            Self::HeadlessBrowser => "headless-browser",
            Self::BitcoinNode => "bitcoin-node",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "nostr-relay" => Some(Self::NostrRelay),
            "inference-endpoint" => Some(Self::InferenceEndpoint),
            "headless-browser" => Some(Self::HeadlessBrowser),
            "bitcoin-node" => Some(Self::BitcoinNode),
            _ => None,
        }
    }

    pub fn all() -> [Self; 4] {
        [
            Self::NostrRelay,
            Self::InferenceEndpoint,
            Self::HeadlessBrowser,
            Self::BitcoinNode,
        ]
    }
}

/// One port the consumer needs to reach to use the workload.
#[derive(Debug, Clone)]
pub struct Port {
    /// Container-internal port.
    pub container_port: u16,
    /// Wire-protocol hint for tooling / docs (`tcp`, `http`, `ws`,
    /// `https`, `bitcoin-rpc`, etc.).
    pub protocol: &'static str,
    /// Human label (`relay-ws`, `inference-http`, `bitcoind-rpc`).
    pub label: &'static str,
}

/// Full definition of a template. The consumer-visible defaults
/// (tier, replication) and the operator-visible facts (image,
/// ports, env, compose_path).
#[derive(Debug, Clone)]
pub struct TemplateDefinition {
    pub name: TemplateName,
    pub summary: &'static str,

    // ---- operator-side facts ----
    /// Real, public Docker image. No `ubuntu:22.04` placeholders.
    pub image: &'static str,
    pub ports: Vec<Port>,
    /// Environment variables the workload expects. Values are
    /// defaults; consumers can override per-deploy.
    pub env: HashMap<&'static str, &'static str>,
    /// Path (relative to the repo root) to a working
    /// `docker-compose.yml`. Operators run
    /// `docker compose -f <compose_path> up` to reproduce the
    /// workload locally with no Paygress involved.
    pub compose_path: &'static str,

    // ---- consumer-side defaults ----
    pub tier: &'static str,
    pub replication: ReplicationMode,

    /// Minimum sane resources. Provisioning rejects tiers below this.
    pub min_cpu_millicores: u64,
    pub min_memory_mb: u64,
    pub min_storage_gb: u64,
}

impl TemplateDefinition {
    pub fn lookup(name: TemplateName) -> Self {
        match name {
            TemplateName::NostrRelay => nostr_relay(),
            TemplateName::InferenceEndpoint => inference_endpoint(),
            TemplateName::HeadlessBrowser => headless_browser(),
            TemplateName::BitcoinNode => bitcoin_node(),
        }
    }

    pub fn all() -> Vec<Self> {
        TemplateName::all()
            .iter()
            .map(|n| Self::lookup(*n))
            .collect()
    }
}

fn nostr_relay() -> TemplateDefinition {
    let mut env = HashMap::new();
    env.insert("STRFRY_DB_PATH", "/app/strfry-db");
    env.insert("RELAY_NAME", "paygress-relay");
    TemplateDefinition {
        name: TemplateName::NostrRelay,
        summary: "Censorship-resistant Nostr relay (strfry). Freedom-tech anchor; warm-standby across two providers because relay outage = censorship surface for the users who depend on it.",
        image: "dockurr/strfry:latest",
        ports: vec![Port {
            container_port: 7777,
            protocol: "ws",
            label: "relay-ws",
        }],
        env,
        compose_path: "templates/nostr-relay/docker-compose.yml",
        tier: "basic",
        replication: ReplicationMode::WarmStandby,
        min_cpu_millicores: 500,
        min_memory_mb: 512,
        min_storage_gb: 5,
    }
}

fn inference_endpoint() -> TemplateDefinition {
    let mut env = HashMap::new();
    env.insert("OLLAMA_HOST", "0.0.0.0:11434");
    env.insert("OLLAMA_MODELS", "/root/.ollama/models");
    TemplateDefinition {
        name: TemplateName::InferenceEndpoint,
        summary: "OpenAI-compatible inference endpoint (Ollama). Agent-economy anchor; checkpointed (resumable model state) but no warm standby — costs scale linearly with replication and most agents accept retry on a fresh provider.",
        image: "ollama/ollama:latest",
        ports: vec![Port {
            container_port: 11434,
            protocol: "http",
            label: "ollama-http",
        }],
        env,
        compose_path: "templates/inference-endpoint/docker-compose.yml",
        tier: "standard",
        replication: ReplicationMode::Checkpointed,
        min_cpu_millicores: 2000,
        min_memory_mb: 4096,
        min_storage_gb: 20,
    }
}

fn headless_browser() -> TemplateDefinition {
    let mut env = HashMap::new();
    env.insert("CONNECTION_TIMEOUT", "300000");
    env.insert("MAX_CONCURRENT_SESSIONS", "10");
    TemplateDefinition {
        name: TemplateName::HeadlessBrowser,
        summary: "Disposable headless Chrome (browserless). Agent-driven scraping. Stateless by design, so replication is `none` by default — a crash means \"retry from scratch\", which is what callers already do.",
        image: "ghcr.io/browserless/chromium:latest",
        ports: vec![
            Port {
                container_port: 3000,
                protocol: "http",
                label: "browserless-http",
            },
            Port {
                container_port: 9222,
                protocol: "http",
                label: "cdp",
            },
        ],
        env,
        compose_path: "templates/headless-browser/docker-compose.yml",
        tier: "basic",
        replication: ReplicationMode::None,
        min_cpu_millicores: 1000,
        min_memory_mb: 1024,
        min_storage_gb: 5,
    }
}

fn bitcoin_node() -> TemplateDefinition {
    let mut env = HashMap::new();
    env.insert("BITCOIN_NETWORK", "regtest");
    env.insert("BITCOIN_RPC_USER", "paygress");
    TemplateDefinition {
        name: TemplateName::BitcoinNode,
        summary: "Bitcoin full node (bitcoind). Long sync, large state — checkpointed so a provider crash doesn't restart the chain download. Defaults to regtest for fast smoke testing; mainnet via env override.",
        image: "btcpayserver/bitcoin:28.1",
        ports: vec![
            Port {
                container_port: 8332,
                protocol: "bitcoin-rpc",
                label: "rpc",
            },
            Port {
                container_port: 8333,
                protocol: "tcp",
                label: "p2p",
            },
        ],
        env,
        compose_path: "templates/bitcoin-node/docker-compose.yml",
        tier: "standard",
        replication: ReplicationMode::Checkpointed,
        min_cpu_millicores: 1000,
        min_memory_mb: 2048,
        min_storage_gb: 50,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_round_trip() {
        for t in TemplateName::all() {
            assert_eq!(TemplateName::from_slug(t.slug()), Some(t));
        }
    }

    #[test]
    fn unknown_slug_is_none() {
        assert!(TemplateName::from_slug("not-a-template").is_none());
    }

    #[test]
    fn every_template_has_an_image_and_ports() {
        for def in TemplateDefinition::all() {
            assert!(
                !def.image.contains("ubuntu:22.04"),
                "{:?} still on placeholder image",
                def.name
            );
            assert!(!def.image.is_empty(), "{:?} has empty image", def.name);
            assert!(
                !def.ports.is_empty(),
                "{:?} has no ports — workload would be unreachable",
                def.name
            );
        }
    }

    #[test]
    fn min_resources_are_nonzero() {
        for def in TemplateDefinition::all() {
            assert!(def.min_cpu_millicores > 0);
            assert!(def.min_memory_mb > 0);
            assert!(def.min_storage_gb > 0);
        }
    }

    #[test]
    fn compose_paths_match_slug() {
        for def in TemplateDefinition::all() {
            let expected = format!("templates/{}/docker-compose.yml", def.name.slug());
            assert_eq!(def.compose_path, expected);
        }
    }

    #[test]
    fn replication_defaults_match_workload_semantics() {
        // Pin the design choices: relay needs availability, browser
        // is throwaway, others checkpoint. Changing these is a
        // compatibility-bearing decision.
        assert_eq!(
            TemplateDefinition::lookup(TemplateName::NostrRelay).replication,
            ReplicationMode::WarmStandby
        );
        assert_eq!(
            TemplateDefinition::lookup(TemplateName::HeadlessBrowser).replication,
            ReplicationMode::None
        );
        assert_eq!(
            TemplateDefinition::lookup(TemplateName::InferenceEndpoint).replication,
            ReplicationMode::Checkpointed
        );
        assert_eq!(
            TemplateDefinition::lookup(TemplateName::BitcoinNode).replication,
            ReplicationMode::Checkpointed
        );
    }
}
