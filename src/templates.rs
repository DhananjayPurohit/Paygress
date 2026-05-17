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
    /// OpenClaw — open-source personal AI assistant Gateway
    /// (openclaw.ai). Connects to chat apps (WhatsApp/Telegram/
    /// Discord/Slack/Signal/iMessage) outbound, holds persistent
    /// memory + tool credentials in `~/.openclaw`, exposes a local
    /// HTTP control plane on 18789 for the user's companion app.
    /// Checkpointed because the memory + chat-app credentials are
    /// personal and should survive a provider restart.
    OpenClaw,
    /// ngit CI/CD runner — one-shot container that clones a repo
    /// (ngit-based or plain git), checks out a commit, parses
    /// `.ngit/ci.yml`, and runs each step. Result is reported back
    /// via stdout/exit code today; the follow-up event-publishing
    /// step (Nostr kind 38401, ngit-ci-status) lands once the
    /// bridge daemon and event schema are agreed upon.
    ///
    /// Stateless and replication=None — CI runs are naturally
    /// idempotent at the bridge level (re-spawn on a fresh provider
    /// is the recovery model). Warm-standby would burn money for
    /// no recovery benefit.
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

/// Default policy for `--encrypt-volume` on the spawn CLI: should
/// the consumer's data volume be LUKS-encrypted at rest *unless
/// they explicitly pass `--no-encrypt-volume`*?
///
/// Rule: yes for every template that holds persistent state
/// (`data_path: Some(_)`). Stateless templates have nothing to
/// encrypt and the default is a no-op for them.
///
/// Why this rule and not "Checkpointed only": every persistent-state
/// template leaks the *same* class of data to a curious operator —
/// strfry's LMDB has relay subscribers' message graph, ollama's
/// model dir carries any RAG context, openclaw's config dir holds
/// chat-app credentials, bitcoind's chaindata carries the wallet
/// pubkeys. The replication mode is a recovery-model knob, not a
/// confidentiality knob; encryption is justified in all of them.
/// Modest LUKS overhead beats a confused consumer-vs-template-
/// author trust split.
///
/// Pure function over the template name — testable without
/// touching the filesystem or the network.
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
        // ngit-runner: a CI run is naturally idempotent at the
        // bridge level (re-spawn on a fresh provider is the recovery
        // model). Warm-standby would burn money for no benefit on a
        // one-shot workload.
        assert_eq!(
            TemplateDefinition::lookup(TemplateName::NgitRunner).replication,
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

    #[test]
    fn ngit_runner_is_stateless_and_requires_repo_and_commit() {
        // CI runs are one-shot — no persistence between runs (failed
        // pipeline retries on a fresh provider). Empty defaults for
        // NGIT_REPO / NGIT_COMMIT mean "the entrypoint refuses to
        // start unless the consumer set them" rather than running
        // against an unintended target.
        let def = TemplateDefinition::lookup(TemplateName::NgitRunner);
        assert_eq!(def.data_path, None, "CI runner must be stateless");
        assert_eq!(def.env.get("NGIT_REPO"), Some(&""));
        assert_eq!(def.env.get("NGIT_COMMIT"), Some(&""));
        assert_eq!(def.env.get("NGIT_PIPELINE_PATH"), Some(&".ngit/ci.yml"));
    }
}

fn openclaw() -> TemplateDefinition {
    let mut env = HashMap::new();
    // Gateway HTTP control plane bind. The companion app + the
    // installed-skill webhooks need to hit this; consumers reach it
    // via the host-port mapping surfaced in AccessDetails.
    env.insert("OPENCLAW_GATEWAY_PORT", "18789");
    env.insert("OPENCLAW_GATEWAY_HOST", "0.0.0.0");
    // Persist config + memory + sessions + per-skill credentials
    // here so a checkpoint round-trip preserves them. Matches the
    // upstream default at ~/.openclaw.
    env.insert("OPENCLAW_CONFIG_DIR", "/data/.openclaw");
    TemplateDefinition {
        name: TemplateName::OpenClaw,
        summary: "OpenClaw — open-source personal AI assistant Gateway (openclaw.ai). Connects outbound to chat apps (WhatsApp/Telegram/Discord/Slack/Signal/iMessage), keeps persistent memory + tool credentials in /data/.openclaw, exposes the Gateway control plane on 18789. Checkpointed because the memory + credentials are personal and should survive provider restarts.",
        // TODO(openclaw-image): swap to a paygress-pinned image
        // (`ghcr.io/dhananjaypurohit/paygress-openclaw:<ver>`) once
        // we publish one — same pattern as agent-sandbox. Today's
        // best public image is the upstream openclaw repo's own
        // GHCR build; if upstream stops publishing, deploys break
        // until the paygress-pinned image lands.
        image: "ghcr.io/openclaw/openclaw:latest",
        ports: vec![Port {
            container_port: 18789,
            protocol: "http",
            label: "openclaw-gateway",
        }],
        env,
        compose_path: "templates/openclaw/docker-compose.yml",
        extra_docker_args: &[],
        // ~/.openclaw inside the container — config, memory store,
        // sessions, and per-skill OAuth tokens. The agent's whole
        // identity lives here; lose it and the user reauthenticates
        // every chat-app integration.
        data_path: Some("/data/.openclaw"),
        tier: "standard",
        // Personal assistant state is irreplaceable from the
        // consumer's POV — checkpointed gives them a Blossom-stored
        // restore point on provider crash. Warm-standby is overkill
        // (the assistant doesn't need sub-minute recovery).
        replication: ReplicationMode::Checkpointed,
        // Node 24 + memory store + concurrent chat-app integrations:
        // 1 vCPU is fine at idle, 2 GB lets the JS heap breathe under
        // bursty webhook activity (Telegram + Discord + Slack at once).
        min_cpu_millicores: 1000,
        min_memory_mb: 2048,
        min_storage_gb: 5,
    }
}

fn ngit_runner() -> TemplateDefinition {
    let mut env = HashMap::new();
    // Required per-spawn (consumer overrides via spawn env): the repo
    // to clone and the commit / ref to check out. Empty defaults
    // mean "the runner refuses to start and prints a clear error"
    // rather than running against an unintended target.
    env.insert("NGIT_REPO", "");
    env.insert("NGIT_COMMIT", "");
    // Pipeline file path inside the repo. `.ngit/ci.yml` mirrors the
    // `.github/workflows/`-style convention so a repo author can
    // grep for it. Override per-spawn if your repo uses a different
    // path (e.g. monorepos with multiple pipelines).
    env.insert("NGIT_PIPELINE_PATH", ".ngit/ci.yml");
    // Status HTTP server bind. Live log streaming + a final
    // /status JSON document so the bridge daemon (or a human via
    // ssh tunnel) can poll while the pipeline runs. The provider
    // surfaces the host-port mapping via AccessDetails just like
    // every other HTTP-serving template.
    env.insert("NGIT_STATUS_PORT", "8080");
    TemplateDefinition {
        name: TemplateName::NgitRunner,
        summary: "ngit CI/CD runner — one-shot pipeline executor for Nostr-based git repos. Clones the repo at the requested commit, parses .ngit/ci.yml, runs each step. Result reporting today is exit code + /status HTTP; the follow-up step ships the kind-38401 Nostr-event publish once the ngit-ci bridge daemon and event schema are agreed upon.",
        // TODO(ngit-runner-image): publish a paygress-pinned image
        // (`ghcr.io/dhananjaypurohit/paygress-ngit-runner:<ver>`)
        // built from `images/ngit-runner/`. Until that image exists,
        // deploys of this template will fail at docker pull — the
        // template config is staged ahead of the image so the CLI
        // surface, schema, and tests can land first.
        image: "ghcr.io/dhananjaypurohit/paygress-ngit-runner:0.1.0",
        ports: vec![Port {
            container_port: 8080,
            protocol: "http",
            label: "ngit-runner-status",
        }],
        env,
        compose_path: "templates/ngit-runner/docker-compose.yml",
        extra_docker_args: &[],
        // CI runs are one-shot — no persistence between runs. A
        // failed pipeline retries on a fresh provider with a clean
        // workspace, which is what the upstream bridge already
        // assumes (the whole spawn-per-run model is the recovery
        // story). Stateless ⇒ encryption defaults off ⇒ no LUKS
        // overhead on the hot path.
        data_path: None,
        tier: "basic",
        // Bridge respawns on a fresh provider per CI run; warm-standby
        // would burn money for no recovery benefit on a one-shot
        // workload.
        replication: ReplicationMode::None,
        // Sized for a typical compile + test cycle in a small repo
        // (Node/Python/Go usually fit). Heavyweight builds (large
        // Rust crates, Docker-in-Docker) should use a higher tier
        // — operators are free to offer larger SKUs.
        min_cpu_millicores: 1000,
        min_memory_mb: 2048,
        min_storage_gb: 10,
    }
}

#[cfg(test)]
mod default_policy_tests {
    use super::*;

    #[test]
    fn templates_with_persistent_state_default_to_encrypted() {
        // Anything with a `data_path` should get encrypted by default.
        // Stateful templates today: nostr-relay, inference-endpoint,
        // bitcoin-node, agent-sandbox, openclaw.
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
        // strfry's LMDB carries subscribers' message graph; encrypting
        // it at rest is justified even though replication is
        // warm-standby (recovery mode is orthogonal to confidentiality).
        assert!(template_default_encrypts_volume(TemplateName::NostrRelay));
    }

    #[test]
    fn headless_browser_does_not_encrypt_by_default() {
        // Stateless template — nothing to encrypt. The default is a
        // no-op for it.
        assert!(!template_default_encrypts_volume(
            TemplateName::HeadlessBrowser
        ));
    }

    #[test]
    fn openclaw_encrypts_by_default() {
        // OpenClaw's /data/.openclaw holds chat-app OAuth tokens —
        // arguably the load-bearing reason this default exists.
        assert!(template_default_encrypts_volume(TemplateName::OpenClaw));
    }
}
