// Template definitions consumers spawn via `paygress deploy <slug>`.

use std::collections::HashMap;

/// Template-default replication mode. Distinct from
/// `durable_workload::ReplicationMode` (which carries runtime data)
/// and `cli::commands::deploy::ReplicationMode` (the CLI flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationMode {
    /// Crash → consumer retries on a fresh provider.
    None,
    /// Periodic Blossom checkpoints; restart from the latest one.
    Checkpointed,
    /// Checkpoints plus a hot standby on a second provider.
    /// Single-writer always.
    WarmStandby,
}

/// Templates the marketplace knows about. Adding one is
/// compatibility-bearing: consumers may pin by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemplateName {
    NostrRelay,
    InferenceEndpoint,
    HeadlessBrowser,
    BitcoinNode,
    /// Python + Node + git in a writable `/workspace`. No browser —
    /// `HeadlessBrowser` covers that case.
    AgentSandbox,
    /// openclaw.ai personal AI assistant Gateway. Holds persistent
    /// memory + chat-app credentials in `~/.openclaw`.
    OpenClaw,
    /// One-shot ngit CI/CD runner: clones a repo, checks out a
    /// commit, runs the steps in `.ngit/ci.yml`.
    NgitRunner,
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
            Self::OpenClaw => "openclaw",
            Self::NgitRunner => "ngit-runner",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "nostr-relay" => Some(Self::NostrRelay),
            "inference-endpoint" => Some(Self::InferenceEndpoint),
            "headless-browser" => Some(Self::HeadlessBrowser),
            "bitcoin-node" => Some(Self::BitcoinNode),
            "agent-sandbox" => Some(Self::AgentSandbox),
            "openclaw" => Some(Self::OpenClaw),
            "ngit-runner" => Some(Self::NgitRunner),
            _ => None,
        }
    }

    pub fn all() -> [Self; 7] {
        [
            Self::NostrRelay,
            Self::InferenceEndpoint,
            Self::HeadlessBrowser,
            Self::BitcoinNode,
            Self::AgentSandbox,
            Self::OpenClaw,
            Self::NgitRunner,
        ]
    }
}

/// One port the consumer needs to reach to use the workload.
#[derive(Debug, Clone)]
pub struct Port {
    pub container_port: u16,
    /// Wire-protocol hint for tooling / docs (`tcp`, `http`, `ws`…).
    pub protocol: &'static str,
    /// Human label (`relay-ws`, `bitcoind-rpc`…).
    pub label: &'static str,
}

#[derive(Debug, Clone)]
pub struct TemplateDefinition {
    pub name: TemplateName,
    pub summary: &'static str,

    pub image: &'static str,
    pub ports: Vec<Port>,
    /// Default environment; consumers can override per-deploy.
    pub env: HashMap<&'static str, &'static str>,
    /// Repo-relative path to a `docker-compose.yml` that reproduces
    /// the workload locally with no Paygress involved.
    pub compose_path: &'static str,

    /// Extra `docker run` flags passed verbatim before the image
    /// positional. Every flag here is cross-template attack surface,
    /// so keep the list minimal and justified.
    pub extra_docker_args: &'static [&'static str],

    /// Container path holding persistent state; DockerBackend mounts
    /// a vmid-scoped volume there. `None` = stateless.
    pub data_path: Option<&'static str>,

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
            TemplateName::OpenClaw => openclaw(),
            TemplateName::NgitRunner => ngit_runner(),
        }
    }

    pub fn all() -> Vec<Self> {
        TemplateName::all()
            .iter()
            .map(|n| Self::lookup(*n))
            .collect()
    }
}

/// Whether `--encrypt-volume` defaults on for this template.
///
/// Rule: yes for anything with persistent state. Replication mode is
/// a recovery knob, not a confidentiality one — every stateful
/// template leaks the same class of data to a curious operator, and
/// stateless ones have nothing to encrypt.
pub fn template_default_encrypts_volume(name: TemplateName) -> bool {
    TemplateDefinition::lookup(name).data_path.is_some()
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
        // strfry raises its nofile rlimit to 1M at startup; without
        // this the container exits with "Unable to set NOFILES limit
        // to 1000000, exceeds max of 524288".
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
    // Credentials for the baked-in HTTP exec server. The provider
    // overwrites these with the consumer's ssh_username/ssh_password
    // at spawn; the server returns 503 while they are empty.
    env.insert("EXEC_USER", "");
    env.insert("EXEC_PASS", "");
    TemplateDefinition {
        name: TemplateName::AgentSandbox,
        summary: "Generic compute sandbox: Python 3.12 + Node 20 + git in a writable /workspace volume. Bundled HTTP exec server on port 8080 lets agents run shell commands directly via the `paygress-cli exec` / MCP `run_command` path — no SSH needed. Stateless by default — retry-on-fresh-provider is the recovery model. Browser-using agents should compose with the `headless-browser` template.",
        // Pinned so a registry-side rebuild can't silently change
        // spawn behavior. Published by
        // .github/workflows/agent-sandbox-image.yml on `agent-sandbox-v*`.
        image: "ghcr.io/dhananjaypurohit/paygress-agent-sandbox:0.1.0",
        ports: vec![Port {
            container_port: 8080,
            protocol: "http",
            label: "sandbox-exec",
        }],
        env,
        compose_path: "templates/agent-sandbox/docker-compose.yml",
        extra_docker_args: &[],
        data_path: Some("/workspace"),
        tier: "basic",
        min_cpu_millicores: 500,
        min_memory_mb: 1024,
        min_storage_gb: 5,
        replication: ReplicationMode::None,
    }
}

fn openclaw() -> TemplateDefinition {
    let mut env = HashMap::new();
    env.insert("OPENCLAW_GATEWAY_PORT", "18789");
    env.insert("OPENCLAW_GATEWAY_HOST", "0.0.0.0");
    // Config + memory + sessions + per-skill credentials, so a
    // checkpoint round-trip preserves them.
    env.insert("OPENCLAW_CONFIG_DIR", "/data/.openclaw");
    TemplateDefinition {
        name: TemplateName::OpenClaw,
        summary: "OpenClaw — open-source personal AI assistant Gateway (openclaw.ai). Connects outbound to chat apps (WhatsApp/Telegram/Discord/Slack/Signal/iMessage), keeps persistent memory + tool credentials in /data/.openclaw, exposes the Gateway control plane on 18789. Checkpointed because the memory + credentials are personal and should survive provider restarts.",
        // TODO(openclaw-image): swap to a paygress-pinned image once
        // published; upstream's GHCR build is the only option today,
        // so deploys break if upstream stops publishing.
        image: "ghcr.io/openclaw/openclaw:latest",
        ports: vec![Port {
            container_port: 18789,
            protocol: "http",
            label: "openclaw-gateway",
        }],
        env,
        compose_path: "templates/openclaw/docker-compose.yml",
        extra_docker_args: &[],
        data_path: Some("/data/.openclaw"),
        tier: "standard",
        replication: ReplicationMode::Checkpointed,
        min_cpu_millicores: 1000,
        min_memory_mb: 2048,
        min_storage_gb: 5,
    }
}

fn ngit_runner() -> TemplateDefinition {
    let mut env = HashMap::new();
    // Empty defaults mean "refuse to start with a clear error"
    // rather than running against an unintended target.
    env.insert("NGIT_REPO", "");
    env.insert("NGIT_COMMIT", "");
    env.insert("NGIT_PIPELINE_PATH", ".ngit/ci.yml");
    env.insert("NGIT_STATUS_PORT", "8080");
    TemplateDefinition {
        name: TemplateName::NgitRunner,
        summary: "ngit CI/CD runner — one-shot pipeline executor for Nostr-based git repos. Clones the repo at the requested commit, parses .ngit/ci.yml, runs each step. Result reporting today is exit code + /status HTTP; the follow-up step ships the kind-38401 Nostr-event publish once the ngit-ci bridge daemon and event schema are agreed upon.",
        // TODO(ngit-runner-image): publish this image from
        // `images/ngit-runner/`. Until then deploys fail at pull —
        // the config is staged ahead of the image so the CLI
        // surface, schema and tests can land first.
        image: "ghcr.io/dhananjaypurohit/paygress-ngit-runner:0.1.0",
        ports: vec![Port {
            container_port: 8080,
            protocol: "http",
            label: "ngit-runner-status",
        }],
        env,
        compose_path: "templates/ngit-runner/docker-compose.yml",
        extra_docker_args: &[],
        data_path: None,
        tier: "basic",
        replication: ReplicationMode::None,
        min_cpu_millicores: 1000,
        min_memory_mb: 2048,
        min_storage_gb: 10,
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
        // Changing any of these is compatibility-bearing.
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
        assert_eq!(
            TemplateDefinition::lookup(TemplateName::AgentSandbox).replication,
            ReplicationMode::None
        );
        assert_eq!(
            TemplateDefinition::lookup(TemplateName::NgitRunner).replication,
            ReplicationMode::None
        );
    }

    #[test]
    fn agent_sandbox_has_workspace_data_path() {
        // /workspace is the contract for callers retrieving
        // artifacts over SSH; the compose file and docs track it.
        let def = TemplateDefinition::lookup(TemplateName::AgentSandbox);
        assert_eq!(def.data_path, Some("/workspace"));
        assert_eq!(def.env.get("WORKSPACE"), Some(&"/workspace"));
    }

    #[test]
    fn ngit_runner_is_stateless_and_requires_repo_and_commit() {
        let def = TemplateDefinition::lookup(TemplateName::NgitRunner);
        assert_eq!(def.data_path, None, "CI runner must be stateless");
        assert_eq!(def.env.get("NGIT_REPO"), Some(&""));
        assert_eq!(def.env.get("NGIT_COMMIT"), Some(&""));
        assert_eq!(def.env.get("NGIT_PIPELINE_PATH"), Some(&".ngit/ci.yml"));
    }
}

#[cfg(test)]
mod default_policy_tests {
    use super::*;

    #[test]
    fn templates_with_persistent_state_default_to_encrypted() {
        for name in TemplateName::all() {
            let def = TemplateDefinition::lookup(name);
            let expected = def.data_path.is_some();
            assert_eq!(
                template_default_encrypts_volume(name),
                expected,
                "template {:?} default-encrypt mismatch (data_path={:?})",
                name,
                def.data_path,
            );
        }
    }

    #[test]
    fn nostr_relay_encrypts_by_default() {
        // strfry's LMDB carries subscribers' message graph, so
        // at-rest encryption is justified even under warm-standby.
        assert!(template_default_encrypts_volume(TemplateName::NostrRelay));
    }

    #[test]
    fn headless_browser_does_not_encrypt_by_default() {
        assert!(!template_default_encrypts_volume(
            TemplateName::HeadlessBrowser
        ));
    }

    #[test]
    fn openclaw_encrypts_by_default() {
        // /data/.openclaw holds chat-app OAuth tokens.
        assert!(template_default_encrypts_volume(TemplateName::OpenClaw));
    }
}
