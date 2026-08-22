// `paygress-cli ci deploy` — put the coordinator itself on rented compute.
//
// `ci up` still needs a machine you control. This does not: the coordinator
// becomes a paygress workload like any other, so the only thing a repo owner
// has to own is a Nostr key and some ecash.
//
// The two halves are bought separately and on purpose. A coordinator wants to
// be cheap, small and still running next week; a job wants Docker, eight
// gigabytes, and to be destroyed an hour from now. Buying both from the same
// provider works but concentrates the failure -- and a coordinator hosted on
// the machine it also buys jobs from is not a market, it is a single server
// with extra steps.

use anyhow::{anyhow, Result};
use clap::Args;
use colored::Colorize;

use crate::commands::spawn::{nostr_spawn_round_trip, NostrSpawnOutcome, NostrSpawnParams};
use crate::util::{generate_password, get_or_create_identity, parse_relays};

const TEMPLATE_SLUG: &str = "ci-coordinator";

#[derive(Args)]
pub struct DeployArgs {
    /// Repo to watch. A bare npub watches every repo that key publishes, so a
    /// new repo under the same key needs no redeploy.
    #[arg(long)]
    pub repo: String,

    /// Provider to host the coordinator. Wants uptime, not Docker.
    #[arg(long)]
    pub provider: String,

    /// Provider each job's sandbox is bought from. Must advertise `docker`.
    /// Defaults to the coordinator's host, which is convenient and not what
    /// you want in production.
    #[arg(long)]
    pub job_provider: Option<String>,

    /// Mint the coordinator's wallet draws on
    #[arg(long)]
    pub mint: String,

    /// Sats staged in the coordinator's wallet. A dishonest host can take this
    /// -- it reaches them through the spawn request, which they decrypt -- so
    /// stage a few runs' worth and top up rather than a month's.
    #[arg(long, default_value_t = 5000)]
    pub fund: u64,

    /// Cashu token paying for the coordinator's own lease
    #[arg(long)]
    pub token: String,

    /// Sats per job sandbox
    #[arg(long, default_value_t = 800)]
    pub job_sats: u64,

    /// Jobs the coordinator runs at once. Each is a separate lease.
    #[arg(long, default_value_t = 1)]
    pub max_concurrent_jobs: usize,

    /// Tier for the coordinator itself
    #[arg(long, default_value = "basic")]
    pub tier: String,

    /// Your Nostr private key (nsec) - uses ~/.paygress/identity if not provided
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

/// Template env the coordinator reads on boot.
///
/// Returned sorted so the same arguments produce the same request, which is
/// what makes the tests below mean anything.
fn template_env(args: &DeployArgs) -> Vec<(String, String)> {
    let job_provider = args
        .job_provider
        .clone()
        .unwrap_or_else(|| args.provider.clone());
    let mut env = vec![
        ("NGIT_CI_REPOS".to_string(), args.repo.clone()),
        ("PAYGRESS_JOB_PROVIDER".to_string(), job_provider),
        ("PAYGRESS_JOB_SATS".to_string(), args.job_sats.to_string()),
        ("PAYGRESS_MINT".to_string(), args.mint.clone()),
        ("PAYGRESS_FUND_SATS".to_string(), args.fund.to_string()),
        (
            "NGIT_CI_MAX_CONCURRENT_JOBS".to_string(),
            args.max_concurrent_jobs.to_string(),
        ),
    ];
    if let Some(relays) = &args.relays {
        env.push(("NOSTR_RELAYS".to_string(), relays.clone()));
    }
    env.sort();
    env
}

/// A coordinator buying jobs from the machine it runs on is a single server
/// with extra steps: one operator can see the coordinator's wallet, the job
/// sandboxes, and everything the suite touches. It is the convenient default
/// and worth saying out loud.
fn same_host_warning(args: &DeployArgs) -> Option<String> {
    let job_provider = args.job_provider.as_ref().unwrap_or(&args.provider);
    (job_provider == &args.provider).then(|| {
        format!(
            "coordinator and jobs are both on `{}`; pass --job-provider to spread them",
            args.provider
        )
    })
}

pub async fn execute(args: DeployArgs, _verbose: bool) -> Result<()> {
    if args.fund == 0 {
        return Err(anyhow!(
            "--fund 0 leaves the coordinator unable to buy a single job sandbox"
        ));
    }

    let env = template_env(&args);
    let password = generate_password(24);

    println!("{}", "Deploying a CI coordinator".blue().bold());
    println!("  {}       {}", "Repo:".bold(), args.repo.cyan());
    println!("  {}   {}", "Hosted by:".bold(), args.provider.cyan());
    println!(
        "  {}  {}",
        "Jobs from:".bold(),
        args.job_provider
            .as_deref()
            .unwrap_or(&args.provider)
            .cyan()
    );
    println!(
        "  {}    {} sats staged, {} per job",
        "Wallet:".bold(),
        args.fund,
        args.job_sats
    );
    if let Some(warning) = same_host_warning(&args) {
        println!("  {} {}", "Note:".yellow().bold(), warning.yellow());
    }
    println!();

    let outcome = nostr_spawn_round_trip(
        &args.provider,
        NostrSpawnParams {
            tier: args.tier.clone(),
            token: args.token.clone(),
            image: String::new(),
            ssh_user: "root".to_string(),
            ssh_pass: password,
            template_slug: Some(TEMPLATE_SLUG.to_string()),
            // The coordinator runs no containers, so it needs none of what a
            // job sandbox does. Requiring `docker` here would rule out exactly
            // the cheap, boring hosts that suit it best.
            required_capabilities: Vec::new(),
            template_env: env.into_iter().collect(),
            ..Default::default()
        },
        parse_relays(args.relays.clone()),
        get_or_create_identity(args.nostr_key.clone())?,
        180,
    )
    .await?;

    match outcome {
        NostrSpawnOutcome::Success(access) => {
            println!("{}", "Coordinator running.".green().bold());
            println!("  lease expires  {}", access.expires_at);
            println!();
            println!(
                "{}",
                "Open a proposal on the repo and it will be built. Nothing else to run.".dimmed()
            );
            println!(
                "{}",
                "Top the lease up with `paygress-cli topup` before it expires.".dimmed()
            );
            Ok(())
        }
        NostrSpawnOutcome::ProviderOffline => {
            Err(anyhow!("provider `{}` is offline", args.provider))
        }
        NostrSpawnOutcome::ProviderError(e) => Err(anyhow!(
            "provider refused the deploy: {} ({})",
            e.message,
            e.error_type
        )),
        NostrSpawnOutcome::Timeout => Err(anyhow!(
            "provider did not answer in 180s; the payment may have been taken"
        )),
        NostrSpawnOutcome::UnknownResponse(c) => {
            Err(anyhow!("provider sent an unrecognised response: {}", c))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> DeployArgs {
        DeployArgs {
            repo: "npub1repo".into(),
            provider: "CheapHost".into(),
            job_provider: None,
            mint: "https://mint.example".into(),
            fund: 5000,
            token: "cashuAtoken".into(),
            job_sats: 800,
            max_concurrent_jobs: 1,
            tier: "basic".into(),
            nostr_key: None,
            relays: None,
        }
    }

    fn env_of(a: &DeployArgs) -> std::collections::HashMap<String, String> {
        template_env(a).into_iter().collect()
    }

    #[test]
    fn the_coordinator_is_told_what_to_watch_and_what_to_buy() {
        let e = env_of(&args());
        assert_eq!(e["NGIT_CI_REPOS"], "npub1repo");
        assert_eq!(e["PAYGRESS_MINT"], "https://mint.example");
        assert_eq!(e["PAYGRESS_JOB_SATS"], "800");
    }

    // Convenient, and a single point of failure: one operator would hold the
    // coordinator's wallet and every job sandbox.
    #[test]
    fn jobs_default_to_the_coordinators_own_host_but_say_so() {
        let a = args();
        assert_eq!(env_of(&a)["PAYGRESS_JOB_PROVIDER"], "CheapHost");
        assert!(same_host_warning(&a).is_some());
    }

    #[test]
    fn a_separate_job_provider_is_carried_through_and_not_warned_about() {
        let mut a = args();
        a.job_provider = Some("DockerHost".into());
        assert_eq!(env_of(&a)["PAYGRESS_JOB_PROVIDER"], "DockerHost");
        assert!(same_host_warning(&a).is_none());
    }

    #[test]
    fn the_same_arguments_always_build_the_same_request() {
        assert_eq!(template_env(&args()), template_env(&args()));
    }

    #[tokio::test]
    async fn a_coordinator_that_cannot_buy_a_job_is_refused_before_paying() {
        let mut a = args();
        a.fund = 0;
        let err = execute(a, false).await.unwrap_err().to_string();
        assert!(err.contains("--fund 0"), "{err}");
    }
}
