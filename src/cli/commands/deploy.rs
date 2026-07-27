// Wrapper around `spawn` that hides reliability, persistence and replication
// behind per-template defaults, each overridable by an explicit flag.

use anyhow::Result;
use clap::{Args, ValueEnum};
use colored::Colorize;
use std::str::FromStr;

use super::spawn::{self, SpawnArgs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReplicationMode {
    /// One container, no checkpoint, no failover. Cheapest.
    None,
    /// Periodic Blossom checkpoints; restart on the same provider.
    Checkpointed,
    /// Checkpoints plus a hot standby on a second provider.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Template {
    /// Nostr relay (strfry / nostr-rs-relay)
    NostrRelay,
    /// Inference endpoint (vLLM / Ollama / TGI)
    InferenceEndpoint,
    /// Headless browser (Playwright / Puppeteer)
    HeadlessBrowser,
    /// Bitcoin full node
    BitcoinNode,
    /// Generic compute sandbox: Python + Node + git in /workspace
    AgentSandbox,
    /// OpenClaw personal AI assistant gateway (openclaw.ai)
    #[value(name = "openclaw")]
    OpenClaw,
}

impl Template {
    /// Slug the provider resolves image/ports/env from, instead of
    /// trusting consumer-supplied bytes.
    pub const fn slug(self) -> &'static str {
        match self {
            Template::NostrRelay => "nostr-relay",
            Template::InferenceEndpoint => "inference-endpoint",
            Template::HeadlessBrowser => "headless-browser",
            Template::BitcoinNode => "bitcoin-node",
            Template::AgentSandbox => "agent-sandbox",
            Template::OpenClaw => "openclaw",
        }
    }
}

pub struct TemplateDefaults {
    pub tier: &'static str,
    /// Fallback only; the provider normally resolves the real image from its
    /// registry via the template slug.
    pub image: &'static str,
    pub replication: ReplicationMode,
    pub summary: &'static str,
}

pub const fn template_defaults(t: Template) -> TemplateDefaults {
    match t {
        Template::NostrRelay => TemplateDefaults {
            tier: "basic",
            image: "ubuntu:22.04",
            replication: ReplicationMode::WarmStandby,
            summary: "Censorship-resistant Nostr relay; warm-standby across two providers.",
        },
        Template::InferenceEndpoint => TemplateDefaults {
            tier: "basic",
            image: "ubuntu:22.04",
            replication: ReplicationMode::Checkpointed,
            summary: "OpenAI-compatible inference endpoint; checkpointed.",
        },
        Template::HeadlessBrowser => TemplateDefaults {
            tier: "basic",
            image: "ubuntu:22.04",
            replication: ReplicationMode::None,
            summary: "Disposable headless browser; agent-driven scraping.",
        },
        Template::BitcoinNode => TemplateDefaults {
            tier: "basic",
            image: "ubuntu:22.04",
            replication: ReplicationMode::Checkpointed,
            summary: "Bitcoin full node; checkpointed (long sync).",
        },
        Template::AgentSandbox => TemplateDefaults {
            tier: "basic",
            image: "nikolaik/python-nodejs:python3.12-nodejs20",
            replication: ReplicationMode::None,
            summary: "Python + Node + git sandbox for agents, CI, and map-reduce shards.",
        },
        Template::OpenClaw => TemplateDefaults {
            tier: "standard",
            image: "ghcr.io/openclaw/openclaw:latest",
            replication: ReplicationMode::Checkpointed,
            summary: "OpenClaw personal AI assistant Gateway; checkpointed.",
        },
    }
}

/// Rejects malformed tokens before any network work.
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

    /// Provider ID. Required until auto-selection lands.
    #[arg(long)]
    pub provider: Option<String>,

    /// Override the template's default tier.
    #[arg(short, long)]
    pub tier: Option<String>,

    /// Override the template's default replication mode.
    #[arg(long, value_enum)]
    pub replication: Option<ReplicationMode>,

    /// Override the template's default container image.
    #[arg(long)]
    pub image: Option<String>,

    /// Your Nostr private key (nsec) — uses ~/.paygress/identity if unset
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
        println!(
            "{}",
            "  Note: warm-standby failover is honored (a standby is reserved and \
             promoted on lease revocation). Automatic respawn and restore from a \
             checkpoint are not implemented."
                .yellow()
        );
        println!();
    }

    if args.provider.is_none() {
        anyhow::bail!(
            "auto-selection of providers lands with the observatory. \
             Pass --provider <npub> for now."
        );
    }

    // Warm-standby degrades to `none` on the wire: deploy collects no
    // standby topology. That flow is `spawn --primary-id --workload-id`,
    // once per provider.
    let replication_str = match replication {
        ReplicationMode::WarmStandby => ReplicationMode::None,
        other => other,
    }
    .as_str()
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
        template_slug: Some(args.template.slug().to_string()),
        replication: replication_str,
        standby: None,
        primary_id: None,
        workload_id: None,
        // Both false: `spawn` applies the template's own volume policy.
        encrypt_volume: false,
        no_encrypt_volume: false,
        isolation_level: None,
    };
    spawn::execute(spawn_args, verbose).await
}
