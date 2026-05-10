// Deploy command (Unit 9 of the 12-month plan).
//
// Opinionated wrapper around `spawn` that hides reliability,
// persistence, and replication choices behind sane per-template
// defaults. Freedom-tech operators and AI agents alike can run
//
//     paygress deploy nostr-relay --pay <token>
//
// without first learning the marketplace's full surface (specs,
// images, ports, replication modes). Every default is overridable
// by an explicit flag.
//
// Real template definitions (image, ports, sysctl tweaks) land in
// later units (Unit 8 = nostr-relay flagship, Unit 13 =
// inference-endpoint, Unit 19 = headless-browser, Unit 21 =
// bitcoin-node). Until those land, the defaults table here points
// at placeholder images so the dispatch path is testable end-to-end
// today.

use anyhow::Result;
use clap::{Args, ValueEnum};
use colored::Colorize;
use std::str::FromStr;

use super::spawn::{self, SpawnArgs};

/// Replication / availability override. Defaults vary per template;
/// see [`template_defaults`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReplicationMode {
    /// One container, no checkpoint, no failover. Cheapest.
    None,
    /// Periodic Blossom checkpoints (Unit 6). Restart on the same
    /// provider after crash.
    Checkpointed,
    /// Periodic checkpoints PLUS a hot standby on a second provider.
    /// Single-writer always (Unit 5). Most expensive.
    WarmStandby,
}

impl ReplicationMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReplicationMode::None => "none",
            ReplicationMode::Checkpointed => "checkpointed",
            ReplicationMode::WarmStandby => "warm-standby",
        }
    }
}

/// Templates the marketplace knows about. Each template is a
/// deliberate intersection of (use-case, image, port profile,
/// replication default). Adding one is a compatibility-bearing
/// decision, not a config tweak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Template {
    /// Nostr relay (strfry / nostr-rs-relay). Freedom-tech anchor.
    /// Defaults to warm-standby because relay outage = censorship
    /// surface for users who depend on it.
    NostrRelay,
    /// Inference endpoint (vLLM / Ollama / TGI). Agent-economy
    /// anchor. Defaults to checkpointed (resumable model state) but
    /// no warm standby — costs scale linearly with replication and
    /// most agents accept retry on a fresh provider.
    InferenceEndpoint,
    /// Headless browser (Playwright / Puppeteer). Stateless by
    /// design, so replication is `none` by default — a crash means
    /// "retry from scratch", which is what callers already do.
    HeadlessBrowser,
    /// Bitcoin full node. Long sync, large state — checkpointed
    /// makes sense; warm-standby is overkill for this Q4 demo.
    BitcoinNode,
    /// Generic compute sandbox: Python + Node + git in /workspace.
    /// For AI agents writing code, CI/test runners, and map-reduce
    /// shards. Stateless by default — retry on a fresh provider is
    /// the recovery model.
    AgentSandbox,
    /// OpenClaw — open-source personal AI assistant Gateway
    /// (openclaw.ai). Connects outbound to chat apps and tools;
    /// keeps memory + per-skill credentials in /data/.openclaw.
    /// Checkpointed so the user's assistant identity survives a
    /// provider restart.
    #[value(name = "openclaw")]
    OpenClaw,
}

/// Per-template "what should we do unless told otherwise" table.
pub struct TemplateDefaults {
    pub tier: &'static str,
    pub image: &'static str,
    pub replication: ReplicationMode,
    /// Human-readable summary used in `--help`-style output.
    pub summary: &'static str,
}

pub const fn template_defaults(t: Template) -> TemplateDefaults {
    match t {
        Template::NostrRelay => TemplateDefaults {
            tier: "basic",
            // TODO(Unit 8): swap to a strfry-bundled image.
            image: "ubuntu:22.04",
            replication: ReplicationMode::WarmStandby,
            summary: "Censorship-resistant Nostr relay; warm-standby across two providers.",
        },
        Template::InferenceEndpoint => TemplateDefaults {
            tier: "basic",
            // TODO(Unit 13): swap to a vLLM/Ollama image with the
            // chosen quantized model preloaded.
            image: "ubuntu:22.04",
            replication: ReplicationMode::Checkpointed,
            summary: "OpenAI-compatible inference endpoint; checkpointed.",
        },
        Template::HeadlessBrowser => TemplateDefaults {
            tier: "basic",
            // TODO(Unit 19): swap to a Playwright-prebuilt image.
            image: "ubuntu:22.04",
            replication: ReplicationMode::None,
            summary: "Disposable headless browser; agent-driven scraping.",
        },
        Template::BitcoinNode => TemplateDefaults {
            tier: "basic",
            // TODO(Unit 21): swap to a bitcoind image with sane defaults.
            image: "ubuntu:22.04",
            replication: ReplicationMode::Checkpointed,
            summary: "Bitcoin full node; checkpointed (long sync).",
        },
        Template::AgentSandbox => TemplateDefaults {
            tier: "basic",
            // The provider resolves the real image from its template
            // registry once `--template-slug` is forwarded; this fallback
            // only matters when --image is not overridden AND the provider
            // doesn't recognize the slug (which would be rejected upstream).
            image: "nikolaik/python-nodejs:python3.12-nodejs20",
            replication: ReplicationMode::None,
            summary: "Python + Node + git sandbox for agents, CI, and map-reduce shards.",
        },
        Template::OpenClaw => TemplateDefaults {
            tier: "standard",
            // The provider resolves the real image from its template
            // registry; this fallback is only consulted when the
            // provider doesn't recognize the slug.
            image: "ghcr.io/openclaw/openclaw:latest",
            replication: ReplicationMode::Checkpointed,
            summary: "OpenClaw personal AI assistant Gateway; checkpointed.",
        },
    }
}

/// Reject malformed Cashu tokens before the CLI does ANY network
/// work. Mirrors the research recommendation in the plan: clap
/// `value_parser` short-circuits on bad input so consumers get a
/// fast, clear error rather than a Nostr round-trip timeout.
fn parse_cashu_token(s: &str) -> Result<String, String> {
    cdk::nuts::Token::from_str(s)
        .map(|_| s.to_string())
        .map_err(|e| format!("not a valid Cashu token: {}", e))
}

#[derive(Args)]
pub struct DeployArgs {
    /// Template to deploy (e.g., `nostr-relay`).
    #[arg(value_enum)]
    pub template: Template,

    /// Cashu token paying for the deployment.
    #[arg(short = 'k', long, value_parser = parse_cashu_token)]
    pub token: String,

    /// Provider npub. If omitted, the CLI auto-selects the
    /// lowest-priced provider that advertises this template's
    /// capabilities (auto-selection lands with Unit 12's
    /// observatory; today this flag is required).
    #[arg(long)]
    pub provider: Option<String>,

    /// Override the template's default tier.
    #[arg(short, long)]
    pub tier: Option<String>,

    /// Override the template's default replication mode.
    #[arg(long, value_enum)]
    pub replication: Option<ReplicationMode>,

    /// Override the template's default container image. Useful for
    /// pinning to a specific tag during incident-response.
    #[arg(long)]
    pub image: Option<String>,

    /// Your Nostr private key (nsec) — uses ~/.paygress/identity if
    /// not provided.
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated).
    #[arg(long)]
    pub relays: Option<String>,
}

pub async fn execute(args: DeployArgs, verbose: bool) -> Result<()> {
    let defaults = template_defaults(args.template);
    let tier = args.tier.unwrap_or_else(|| defaults.tier.to_string());
    let image = args.image.unwrap_or_else(|| defaults.image.to_string());
    let replication = args.replication.unwrap_or(defaults.replication);

    println!("{}", "Deploying Template".blue().bold());
    println!("  Template:    {}", format!("{:?}", args.template).cyan());
    println!("  Summary:     {}", defaults.summary);
    println!("  Tier:        {}", tier);
    println!("  Image:       {}", image);
    println!("  Replication: {}", replication.as_str());
    println!();

    if replication != ReplicationMode::None {
        // Warm-standby and checkpointed both depend on Unit 5
        // (Durable Workload state machine) and Unit 6 (Blossom
        // checkpoints). Until those land, we honor the override
        // syntactically but the provider currently treats every
        // workload as `none`. Surface that explicitly so users
        // aren't surprised.
        println!(
            "{}",
            "  Note: replication != none is parsed but not yet enforced;".yellow()
        );
        println!(
            "{}",
            "  Units 5/6 wire it through to the provider.".yellow()
        );
        println!();
    }

    if args.provider.is_none() {
        anyhow::bail!(
            "auto-selection of providers lands with the observatory (Unit 12). \
             Pass --provider <npub> for now."
        );
    }

    // Delegate to the existing spawn flow. Deploy is a thin,
    // opinionated lens over spawn — not a parallel implementation.
    // The `template_slug` we pass here is what makes the provider
    // resolve image/ports/env from its OWN template registry
    // rather than trusting `--image` bytes.
    let template_slug = match args.template {
        Template::NostrRelay => "nostr-relay",
        Template::InferenceEndpoint => "inference-endpoint",
        Template::HeadlessBrowser => "headless-browser",
        Template::BitcoinNode => "bitcoin-node",
        Template::AgentSandbox => "agent-sandbox",
        Template::OpenClaw => "openclaw",
    };
    // Translate the deploy CLI's replication enum to the spawn CLI's
    // string form. Deploy doesn't yet collect --standby (each
    // template's standby topology is not first-class for now); when
    // the user picks `--replication warm-standby` via deploy, fall
    // back to `none` on the wire — the deploy command surfaces the
    // "not yet enforced" warning above. Once the consumer-side
    // standby coordination flow lands, this maps will route the list.
    let replication_str = match replication {
        ReplicationMode::None => "none",
        ReplicationMode::Checkpointed => "checkpointed",
        ReplicationMode::WarmStandby => "none", // see comment above
    }
    .to_string();
    let spawn_args = SpawnArgs {
        provider: args.provider,
        server: None,
        tier,
        token: args.token,
        image,
        ssh_user: None,
        ssh_pass: None,
        nostr_key: args.nostr_key,
        relays: args.relays,
        template_slug: Some(template_slug.to_string()),
        replication: replication_str,
        standby: None,
        // Deploy doesn't yet collect a primary/standby topology
        // (see the warning printed above when replication != none).
        // The full warm-standby flow is `paygress-cli spawn` with
        // explicit --primary-npub / --workload-id, called once per
        // provider in the set.
        primary_npub: None,
        workload_id: None,
        // Per-template encryption defaults land with Phase 2; for now
        // deploy does not flip --encrypt-volume on its own. Consumers
        // who want encryption use `paygress-cli spawn --encrypt-volume`
        // directly.
        encrypt_volume: false,
    };
    spawn::execute(spawn_args, verbose).await
}
