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
    /// Generic compute sandbox for AI agents, CI/test runners, and
    /// map-reduce / batch shards. Python + Node + git in a writable
    /// `/workspace` volume; no browser (the `HeadlessBrowser`
    /// template covers that case). Stateless by default — a crash
    /// means "retry from scratch", which is what the upstream caller
    /// already does.
    AgentSandbox,
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
            Self::AgentSandbox => "agent-sandbox",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "nostr-relay" => Some(Self::NostrRelay),
            "inference-endpoint" => Some(Self::InferenceEndpoint),
            "headless-browser" => Some(Self::HeadlessBrowser),
            "bitcoin-node" => Some(Self::BitcoinNode),
            "agent-sandbox" => Some(Self::AgentSandbox),
            _ => None,
        }
    }

    pub fn all() -> [Self; 5] {
        [
            Self::NostrRelay,
            Self::InferenceEndpoint,
            Self::HeadlessBrowser,
            Self::BitcoinNode,
            Self::AgentSandbox,
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

    /// Extra `docker run` flags this template needs (ulimits,
    /// sysctls, capabilities, etc.). Passed verbatim before the
    /// image positional. Keep these minimal and well-justified —
    /// every flag here is a cross-template attack surface.
    /// Example: `&["--ulimit", "nofile=1048576:1048576"]` for
    /// strfry, which tries to bump NOFILES to 1M and fails inside
    /// Docker's default 524288 cap.
    pub extra_docker_args: &'static [&'static str],

    /// Container-internal path that holds the workload's
    /// persistent state (LMDB for strfry, models for ollama,
    /// chain data for bitcoind). DockerBackend mounts a
    /// vmid-scoped volume here. None means stateless (browser).
    pub data_path: Option<&'static str>,

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
            TemplateName::AgentSandbox => agent_sandbox(),
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
        // strfry's startup tries to bump nofile rlimit to 1M; without
        // this flag the container immediately exits with "Unable to
        // set NOFILES limit to 1000000, exceeds max of 524288".
        extra_docker_args: &["--ulimit", "nofile=1048576:1048576"],
        data_path: Some("/app/strfry-db"),
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
        extra_docker_args: &[],
        data_path: Some("/root/.ollama"),
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
        extra_docker_args: &[],
        data_path: None,
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
        extra_docker_args: &[],
        data_path: Some("/data"),
        tier: "standard",
        replication: ReplicationMode::Checkpointed,
        min_cpu_millicores: 1000,
        min_memory_mb: 2048,
        min_storage_gb: 50,
    }
}

fn agent_sandbox() -> TemplateDefinition {
    let mut env = HashMap::new();
    env.insert("WORKSPACE", "/workspace");
    env.insert("PYTHONUNBUFFERED", "1");
    env.insert("NODE_ENV", "production");
    // EXEC_USER and EXEC_PASS are the auth credentials for the
    // baked-in HTTP exec server (images/agent-sandbox/server.py).
    // Provider's spawn handler overrides these with the consumer's
    // ssh_username / ssh_password at container-start time so the
    // caller can use ONE set of creds for both SSH (legacy) and the
    // exec endpoint. Default values here are placeholders — the
    // server returns 503 until they're set to non-empty values.
    env.insert("EXEC_USER", "");
    env.insert("EXEC_PASS", "");
    TemplateDefinition {
        name: TemplateName::AgentSandbox,
        summary: "Generic compute sandbox: Python 3.12 + Node 20 + git in a writable /workspace volume. Bundled HTTP exec server on port 8080 lets agents run shell commands directly via the `paygress-cli exec` / MCP `run_command` path — no SSH needed. Stateless by default — retry-on-fresh-provider is the recovery model. Browser-using agents should compose with the `headless-browser` template.",
        // Custom paygress image: nikolaik/python-nodejs +
        // /usr/local/bin/paygress-exec (the baked-in HTTP server).
        // Built and published by .github/workflows/agent-sandbox-image.yml
        // on tags `agent-sandbox-v*`. Pinned to 0.1.0 so a registry-
        // side rebuild can't silently change spawn behavior.
        image: "ghcr.io/dhananjaypurohit/paygress-agent-sandbox:0.1.0",
        ports: vec![Port {
            // The exec server listens here. AccessDetails surfaces
            // the host_port mapping so the caller can hit
            // http://<host>:<host_port>/exec with HTTP Basic auth
            // using the spawn-time ssh_user / ssh_pass.
            container_port: 8080,
            protocol: "http",
            label: "sandbox-exec",
        }],
        env,
        compose_path: "templates/agent-sandbox/docker-compose.yml",
        extra_docker_args: &[],
        // /workspace is the agent's writable scratch. Persistent
        // across restarts on the same provider so a long-running
        // agent task that gets restarted (e.g. backend restart)
        // doesn't lose its checkout.
        data_path: Some("/workspace"),
        tier: "basic",
        replication: ReplicationMode::None,
        // Sized for a typical CI step / agent run, not a heavyweight
        // ML job. Operators can offer larger tiers separately.
        min_cpu_millicores: 500,
        min_memory_mb: 1024,
        min_storage_gb: 5,
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
        // Agent sandbox: same recovery model as headless-browser
        // (retry-from-scratch on a fresh provider) — most CI / agent
        // runs are short-lived and naturally idempotent at the harness
        // level, so paying for warm-standby would be pure waste.
        assert_eq!(
            TemplateDefinition::lookup(TemplateName::AgentSandbox).replication,
            ReplicationMode::None
        );
    }

    #[test]
    fn agent_sandbox_has_workspace_data_path() {
        // The /workspace volume is the contract for callers that
        // want to leave artifacts for retrieval over SSH (e.g.
        // map-reduce shards writing partial results). If this
        // changes, the docker-compose.yml and the user-facing docs
        // need to follow.
        let def = TemplateDefinition::lookup(TemplateName::AgentSandbox);
        assert_eq!(def.data_path, Some("/workspace"));
        assert_eq!(def.env.get("WORKSPACE"), Some(&"/workspace"));
    }
}
