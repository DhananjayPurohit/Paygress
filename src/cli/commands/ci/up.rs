// `paygress-cli ci up` — one command from "a repo on Nostr" to "green CI".

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use colored::Colorize;
use tokio::process::Command;
use tracing::info;

use crate::commands::adapter::{self, AdapterArgs};

/// Where act finds the sandbox's own daemon.
///
/// ngit-ci defaults this to `-`, which means "mount nothing" -- the safe
/// default for a coordinator running jobs on hardware it owns, where the
/// socket is root on that hardware. Here the daemon belongs to a rented box
/// that is destroyed when the lease ends, and handing it over is what lets a
/// workflow use docker without carrying its own bootstrap.
const SANDBOX_DAEMON_SOCKET: &str = "unix:///var/run/docker.sock";

/// Capability a provider must advertise to run a CI job. See `crate::lxd`:
/// unprivileged LXD runs a daemon that cannot start a container, so a provider
/// that has not opted in cannot serve this however willing it is.
const REQUIRED_CAPABILITY: &str = "docker";

#[derive(Args)]
pub struct UpArgs {
    /// Repo to watch, as an naddr (ngit-ci's `--repos`)
    #[arg(long)]
    pub repo: String,

    /// Provider to buy each job's sandbox from (name, npub, or id prefix)
    #[arg(long)]
    pub provider: String,

    /// Mint to buy each job's sandbox with
    #[arg(long)]
    pub mint: String,

    /// Sats per job. A cold CI job compiles from scratch; too low and the
    /// lease expires mid-run, which reads as a hung job.
    #[arg(long, default_value_t = 800)]
    pub sats: u64,

    /// Image the sandbox runs (LXD alias). Build it with images/ci-sandbox/.
    #[arg(long, default_value = "paygress-ci")]
    pub image: String,

    /// Tier on the provider's offer
    #[arg(long, default_value = "basic")]
    pub tier: String,

    /// Unix socket the coordinator submits jobs on
    #[arg(long, default_value = "/tmp/paygress-adapter.sock")]
    pub socket: PathBuf,

    /// Jobs to run at once
    #[arg(long, default_value_t = 1)]
    pub max_concurrent_jobs: usize,

    /// The coordinator executable
    #[arg(long, default_value = "ngit-ci")]
    pub coordinator: String,

    /// Print the coordinator command and exit, for running it under systemd
    /// or on another host rather than as our child.
    #[arg(long)]
    pub print_only: bool,

    /// Your Nostr private key (nsec) - uses ~/.paygress/identity if not provided
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

/// The coordinator invocation, as argv rather than a string, so nothing here
/// has to be quoted correctly to be correct.
fn coordinator_argv(args: &UpArgs) -> Vec<String> {
    let mut argv = vec![
        "--repos".to_string(),
        args.repo.clone(),
        "--runner".to_string(),
        "socket-adapter".to_string(),
        "--adapter-socket".to_string(),
        args.socket.display().to_string(),
        "--act-container-daemon-socket".to_string(),
        SANDBOX_DAEMON_SOCKET.to_string(),
    ];
    if let Some(relays) = &args.relays {
        for relay in relays.split(',').map(str::trim).filter(|r| !r.is_empty()) {
            argv.push("--index-relays".to_string());
            argv.push(relay.to_string());
        }
    }
    argv
}

/// Shown to the operator, and pasteable. Quoting is deliberately naive: these
/// are naddrs, URLs and paths, and a value needing more than this is a value
/// worth noticing.
fn printable(program: &str, argv: &[String]) -> String {
    let mut out = program.to_string();
    for a in argv {
        out.push(' ');
        if a.contains(char::is_whitespace) {
            out.push_str(&format!("'{}'", a));
        } else {
            out.push_str(a);
        }
    }
    out
}

fn adapter_args(args: &UpArgs) -> AdapterArgs {
    AdapterArgs {
        socket: args.socket.clone(),
        provider: args.provider.clone(),
        token_command: format!(
            "paygress-cli wallet mint --mint {} --amount {}",
            args.mint, args.sats
        ),
        template: None,
        image: args.image.clone(),
        tier: args.tier.clone(),
        requires: REQUIRED_CAPABILITY.to_string(),
        max_concurrent_jobs: args.max_concurrent_jobs,
        ssh_user: "root".to_string(),
        spawn_timeout_secs: 120,
        ssh_ready_timeout_secs: 300,
        nostr_key: args.nostr_key.clone(),
        relays: args.relays.clone(),
    }
}

pub async fn execute(args: UpArgs, verbose: bool) -> Result<()> {
    let argv = coordinator_argv(&args);
    let command_line = printable(&args.coordinator, &argv);

    println!("{}", "Paygress CI".blue().bold());
    println!("  {}      {}", "Repo:".bold(), args.repo.cyan());
    println!("  {}  {}", "Provider:".bold(), args.provider.cyan());
    println!(
        "  {}      {} sats/job from {}",
        "Cost:".bold(),
        args.sats,
        args.mint
    );
    println!();
    println!("  {}", "coordinator:".bold());
    println!("    {}", command_line.dimmed());
    println!();

    if args.print_only {
        println!(
            "{}",
            "Start the adapter with `paygress-cli adapter` and run the above.".dimmed()
        );
        return Ok(());
    }

    // Resolved before the adapter binds its socket: a missing coordinator
    // should be a message, not an adapter left listening for a caller that
    // will never arrive.
    let coordinator = which(&args.coordinator).await.ok_or_else(|| {
        anyhow!(
            "`{}` is not on PATH. Install it from https://ngit.dev, or run \
             `paygress-cli ci up --print-only` and start it yourself.",
            args.coordinator
        )
    })?;

    let mut adapter = tokio::spawn(adapter::execute(adapter_args(&args), verbose));

    // The adapter has to be listening before the coordinator dials it. It
    // binds in milliseconds, but "milliseconds" is not "already", and the
    // failure is a coordinator that exits on a refused connection.
    wait_for_socket(&args.socket).await?;
    info!("adapter listening on {}", args.socket.display());

    let mut child = Command::new(&coordinator)
        .args(&argv)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("could not start `{}`", coordinator.display()))?;

    // Either half dying makes the other useless: a coordinator with no adapter
    // cannot run a job, and an adapter with no coordinator is a socket nobody
    // dials. So the first to exit ends the command.
    tokio::select! {
        status = child.wait() => {
            let status = status.context("coordinator failed")?;
            adapter.abort();
            if status.success() {
                Ok(())
            } else {
                Err(anyhow!("coordinator exited with {}", status))
            }
        }
        joined = &mut adapter => {
            let _ = child.kill().await;
            match joined {
                Ok(result) => result.context("adapter stopped"),
                Err(e) => Err(anyhow!("adapter panicked: {}", e)),
            }
        }
    }
}

async fn which(program: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(program);
    if candidate.is_absolute() {
        return candidate.exists().then_some(candidate);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

async fn wait_for_socket(path: &Path) -> Result<()> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(anyhow!(
        "the adapter did not bind {} within 5s",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> UpArgs {
        UpArgs {
            repo: "naddr1abc".into(),
            provider: "SwiftGoldenOwl".into(),
            mint: "https://testnut.cashu.space".into(),
            sats: 800,
            image: "paygress-ci".into(),
            tier: "basic".into(),
            socket: PathBuf::from("/tmp/a.sock"),
            max_concurrent_jobs: 1,
            coordinator: "ngit-ci".into(),
            print_only: false,
            nostr_key: None,
            relays: None,
        }
    }

    // The three flags this command exists to know about. Getting any of them
    // wrong is a silent failure -- jobs run on the coordinator's own host, or
    // the workflow finds no docker -- so they are pinned rather than reviewed.
    #[test]
    fn the_coordinator_is_pointed_at_our_socket_and_the_sandbox_daemon() {
        let argv = coordinator_argv(&args());
        let pair = |flag: &str| {
            argv.iter()
                .position(|a| a == flag)
                .map(|i| argv[i + 1].clone())
        };
        assert_eq!(pair("--runner").as_deref(), Some("socket-adapter"));
        assert_eq!(pair("--adapter-socket").as_deref(), Some("/tmp/a.sock"));
        assert_eq!(
            pair("--act-container-daemon-socket").as_deref(),
            Some(SANDBOX_DAEMON_SOCKET)
        );
        assert_eq!(pair("--repos").as_deref(), Some("naddr1abc"));
    }

    #[test]
    fn relays_become_one_flag_each() {
        let mut a = args();
        a.relays = Some("wss://one, wss://two ,".into());
        let argv = coordinator_argv(&a);
        let relays: Vec<_> = argv
            .iter()
            .enumerate()
            .filter(|(_, v)| *v == "--index-relays")
            .map(|(i, _)| argv[i + 1].clone())
            .collect();
        assert_eq!(relays, vec!["wss://one", "wss://two"]);
    }

    // A provider that has not advertised `docker` cannot run an act job, and
    // the check that catches it happens before the token is spent.
    #[test]
    fn the_adapter_refuses_a_provider_that_cannot_run_docker() {
        assert_eq!(adapter_args(&args()).requires, "docker");
    }

    #[test]
    fn the_token_command_buys_what_was_asked_for() {
        let mut a = args();
        a.sats = 1200;
        assert_eq!(
            adapter_args(&a).token_command,
            "paygress-cli wallet mint --mint https://testnut.cashu.space --amount 1200"
        );
    }

    #[test]
    fn the_printed_command_is_pasteable() {
        let a = args();
        let line = printable(&a.coordinator, &coordinator_argv(&a));
        assert!(line.starts_with("ngit-ci --repos naddr1abc"));
        assert!(line.contains("--act-container-daemon-socket unix:///var/run/docker.sock"));
    }
}
