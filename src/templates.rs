// Template definitions consumers spawn via `paygress deploy <slug>`.

use std::collections::HashMap;

/// Template default. Distinct from `durable_workload::ReplicationMode` (which
/// carries runtime data) and `cli::commands::deploy::ReplicationMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationMode {
    /// Crash → consumer retries on a fresh provider.
    None,
    /// Periodic Blossom checkpoints; restart from the latest one.
    Checkpointed,
    /// Checkpoints plus a hot standby on a second provider. Single-writer.
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
    AgentSandbox,
    OpenClaw,
    CiCoordinator,
}

impl TemplateName {
    /// Wire format: `paygress deploy <slug>` and `templates/<slug>/`.
    pub fn slug(self) -> &'static str {
        match self {
            Self::NostrRelay => "nostr-relay",
            Self::InferenceEndpoint => "inference-endpoint",
            Self::HeadlessBrowser => "headless-browser",
            Self::BitcoinNode => "bitcoin-node",
            Self::AgentSandbox => "agent-sandbox",
            Self::OpenClaw => "openclaw",
            Self::CiCoordinator => "ci-coordinator",
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
            "ci-coordinator" => Some(Self::CiCoordinator),
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
            Self::CiCoordinator,
        ]
    }
}

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
    /// Provider-side defaults. Not consumer-settable unless the key is also
    /// listed in `consumer_env`.
    pub env: HashMap<&'static str, &'static str>,

    /// The only env keys a consumer may set on a deploy.
    ///
    /// A template's whole point is that the provider builds the workload from
    /// its own registry rather than from consumer-supplied bytes, so a consumer
    /// cannot smuggle an image or a mount past the vetted list. But some
    /// templates are useless without being told *what* to work on -- a CI
    /// coordinator has to learn which repo to watch -- and that is
    /// configuration, not code.
    ///
    /// A whitelist keeps both: anything named here is data the template
    /// expects, and anything else a consumer sends is dropped. Adding a key is
    /// a decision about what a stranger may influence, so keep the list short
    /// and never put anything path- or image-shaped on it.
    pub consumer_env: &'static [&'static str],
    /// Repo-relative `docker-compose.yml` reproducing the workload locally.
    pub compose_path: &'static str,

    /// Passed verbatim before the image positional. Every flag here is
    /// cross-template attack surface, so keep the list minimal and justified.
    pub extra_docker_args: &'static [&'static str],

    /// Where DockerBackend mounts a vmid-scoped volume. `None` = stateless.
    pub data_path: Option<&'static str>,

    pub tier: &'static str,
    pub replication: ReplicationMode,

    /// Provisioning rejects tiers below these.
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
            TemplateName::CiCoordinator => ci_coordinator(),
        }
    }

    pub fn all() -> Vec<Self> {
        TemplateName::all().into_iter().map(Self::lookup).collect()
    }
}

/// Whether `--encrypt-volume` defaults on: yes for anything with persistent
/// state, since only that leaks data to a curious operator.
pub fn template_default_encrypts_volume(name: TemplateName) -> bool {
    TemplateDefinition::lookup(name).data_path.is_some()
}

/// The other half of Paygress CI: the thing that watches a repo, rather than
/// the sandbox a job runs in.
///
/// Deliberately the opposite shape to a job sandbox. A job wants Docker,
/// eight gigabytes and a short life; this runs no containers of its own, fits
/// in the basic tier, and has to still be there next week. So it takes none of
/// the `docker` capability, and warm-standby replication instead — a
/// coordinator that is down is a repo whose proposals silently go untested,
/// which looks like nothing rather than like a failure.
///
/// Two things a deployer has to weigh, both documented rather than designed
/// away:
///
/// - The Cashu wallet inside funds job sandboxes, and reaches the host through
///   the spawn request, which the provider decrypts. A dishonest host can take
///   whatever is staged there, so stage little and top up.
/// - A coordinator injects a repo's CI secrets into maintainer-triggered runs.
///   Hosting one on rented compute hands those to the host, which is fine for
///   a repo that has none -- most open-source ones -- and not fine otherwise.
fn ci_coordinator() -> TemplateDefinition {
    let mut env = HashMap::new();
    env.insert("PAYGRESS_JOB_TIER", "ci");
    env.insert("PAYGRESS_JOB_IMAGE", "paygress-ci");
    env.insert("PAYGRESS_JOB_SATS", "800");
    env.insert("NGIT_CI_MAX_CONCURRENT_JOBS", "1");
    env.insert("NOSTR_RELAYS", "wss://relay.ngit.dev,wss://gitnostr.com");
    TemplateDefinition {
        name: TemplateName::CiCoordinator,
        summary: "Watches a Nostr repo and buys a disposable sandbox for every CI job. Runs no containers itself, so it needs no `docker` capability and fits the basic tier; warm-standby because a coordinator that is down is a repo whose proposals silently go untested.",
        image: "ghcr.io/dhananjaypurohit/paygress-ci-coordinator:latest",
        // Outbound only: relays, the mint, and its job provider. Nothing dials
        // in, so there is nothing to publish.
        ports: vec![],
        env,
        // Configuration only: which repo, which mint, which provider to buy
        // jobs from, and how many at once. Nothing here can change what image
        // runs or what it can reach -- those stay the provider's to decide.
        consumer_env: &[
            "NGIT_CI_REPOS",
            "PAYGRESS_JOB_PROVIDER",
            "PAYGRESS_JOB_TIER",
            "PAYGRESS_JOB_SATS",
            "PAYGRESS_MINT",
            "PAYGRESS_FUND_SATS",
            "NGIT_CI_MAX_CONCURRENT_JOBS",
            "NOSTR_RELAYS",
        ],
        compose_path: "templates/ci-coordinator/docker-compose.yml",
        extra_docker_args: &[],
        // The Nostr identity that signs job results and the wallet that pays
        // for them. Losing it on a restart means a new key, which throws away
        // whatever reputation the old one had.
        data_path: Some("/var/lib/paygress"),
        tier: "basic",
        replication: ReplicationMode::WarmStandby,
        min_cpu_millicores: 500,
        min_memory_mb: 512,
        min_storage_gb: 2,
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
        consumer_env: &[],
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
        consumer_env: &[],
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
        consumer_env: &[],
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
        consumer_env: &[],
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
    // The provider overwrites these with the consumer's
    // ssh_username/ssh_password at spawn; the server 503s while empty.
    env.insert("EXEC_USER", "");
    env.insert("EXEC_PASS", "");
    TemplateDefinition {
        name: TemplateName::AgentSandbox,
        summary: "Generic compute sandbox: Python 3.12 + Node 20 + git in a writable /workspace volume. Bundled HTTP exec server on port 8080 lets agents run shell commands directly via the `paygress-cli exec` / MCP `run_command` path — no SSH needed. Stateless by default — retry-on-fresh-provider is the recovery model. Browser-using agents should compose with the `headless-browser` template.",
        // Pinned so a registry-side rebuild can't silently change spawn
        // behavior. Published by .github/workflows/agent-sandbox-image.yml.
        image: "ghcr.io/dhananjaypurohit/paygress-agent-sandbox:0.1.0",
        ports: vec![Port {
            container_port: 8080,
            protocol: "http",
            label: "sandbox-exec",
        }],
        env,
        consumer_env: &[],
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
    // Config + memory + credentials, so a checkpoint round-trip keeps them.
    env.insert("OPENCLAW_CONFIG_DIR", "/data/.openclaw");
    TemplateDefinition {
        name: TemplateName::OpenClaw,
        summary: "OpenClaw — open-source personal AI assistant Gateway (openclaw.ai). Connects outbound to chat apps (WhatsApp/Telegram/Discord/Slack/Signal/iMessage), keeps persistent memory + tool credentials in /data/.openclaw, exposes the Gateway control plane on 18789. Checkpointed because the memory + credentials are personal and should survive provider restarts.",
        // TODO(openclaw-image): swap to a paygress-pinned image; deploys
        // break today if upstream stops publishing.
        image: "ghcr.io/openclaw/openclaw:latest",
        ports: vec![Port {
            container_port: 18789,
            protocol: "http",
            label: "openclaw-gateway",
        }],
        env,
        consumer_env: &[],
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

    /// A template that publishes nothing is only sane if nobody is meant to
    /// reach it. The coordinator is the one such workload: it dials relays,
    /// a mint and a provider, and nothing ever dials back.
    fn serves_traffic(name: TemplateName) -> bool {
        name != TemplateName::CiCoordinator
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
            if serves_traffic(def.name) {
                assert!(
                    !def.ports.is_empty(),
                    "{:?} has no ports — workload would be unreachable",
                    def.name
                );
            } else {
                assert!(
                    def.ports.is_empty(),
                    "{:?} publishes a port but nothing should dial it",
                    def.name
                );
            }
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
    }

    #[test]
    fn agent_sandbox_has_workspace_data_path() {
        // /workspace is the contract for callers retrieving artifacts over SSH.
        let def = TemplateDefinition::lookup(TemplateName::AgentSandbox);
        assert_eq!(def.data_path, Some("/workspace"));
        assert_eq!(def.env.get("WORKSPACE"), Some(&"/workspace"));
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
        // strfry's LMDB carries subscribers' message graph.
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

#[cfg(test)]
mod consumer_env_tests {
    use super::*;

    /// The filter the provider applies. Kept next to the whitelist it reads so
    /// the rule and its meaning cannot drift apart.
    fn accepted(def: &TemplateDefinition, sent: &[(&str, &str)]) -> Vec<String> {
        sent.iter()
            .filter(|(k, _)| def.consumer_env.contains(k))
            .map(|(k, _)| k.to_string())
            .collect()
    }

    // A template exists so the provider builds the workload from its own
    // registry rather than from whatever a stranger sent. Anything not named
    // by the template is not configuration, whatever it is called.
    #[test]
    fn keys_a_template_did_not_ask_for_are_dropped() {
        let def = TemplateDefinition::lookup(TemplateName::CiCoordinator);
        let got = accepted(
            &def,
            &[
                ("NGIT_CI_REPOS", "npub1abc"),
                ("PAYGRESS_MINT", "https://mint.example"),
                // The shapes that would matter if this were not filtered.
                ("PATH", "/tmp/evil"),
                ("LD_PRELOAD", "/tmp/x.so"),
                ("EXEC_PASS", "hunter2"),
                ("PAYGRESS_JOB_IMAGE_OVERRIDE", "attacker/image"),
            ],
        );
        assert_eq!(got, vec!["NGIT_CI_REPOS", "PAYGRESS_MINT"]);
    }

    // Every other template takes no consumer configuration at all, so a deploy
    // cannot influence them even by naming a key they happen to define.
    #[test]
    fn templates_that_take_no_configuration_accept_nothing() {
        for def in TemplateDefinition::all() {
            if def.name == TemplateName::CiCoordinator {
                continue;
            }
            assert!(
                def.consumer_env.is_empty(),
                "{} unexpectedly accepts consumer env",
                def.name.slug()
            );
            let keys: Vec<&str> = def.env.keys().copied().collect();
            let sent: Vec<(&str, &str)> = keys.iter().map(|k| (*k, "x")).collect();
            assert!(accepted(&def, &sent).is_empty());
        }
    }

    // Nothing path- or image-shaped may be delegated to a consumer: those are
    // decisions about what runs, not about what it runs on.
    #[test]
    fn no_template_lets_a_consumer_choose_what_runs() {
        for def in TemplateDefinition::all() {
            for key in def.consumer_env {
                let k = key.to_ascii_uppercase();
                assert!(
                    !["IMAGE", "PATH", "LD_PRELOAD", "ENTRYPOINT", "COMMAND"]
                        .iter()
                        .any(|bad| k == **bad || k.ends_with("_IMAGE")),
                    "{} lets a consumer set {}",
                    def.name.slug(),
                    key
                );
            }
        }
    }

    #[test]
    fn the_coordinator_can_be_told_what_to_watch() {
        let def = TemplateDefinition::lookup(TemplateName::CiCoordinator);
        for required in ["NGIT_CI_REPOS", "PAYGRESS_MINT", "PAYGRESS_JOB_PROVIDER"] {
            assert!(def.consumer_env.contains(&required), "missing {required}");
        }
        // It runs no containers, so it must not be sold as needing to.
        assert!(def.ports.is_empty());
        assert_eq!(def.tier, "basic");
    }
}
