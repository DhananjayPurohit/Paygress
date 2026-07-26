// `paygress-cli ci` — run CI jobs on rented Paygress compute.
//
// Why this exists
// ---------------
// GitHub-hosted runners meter (and, for heavy pipelines, throttle or
// flag) the compute behind a repo's CI. A project whose test suite
// legitimately needs a dozen containers for twenty minutes per push
// is indistinguishable, to an automated abuse heuristic, from a
// project mining coins. The remedy is to bring your own compute —
// and Paygress rents it by the second for ecash, with no account to
// suspend.
//
// What `ci runner` does today (v1)
// --------------------------------
//   1. Asks GitHub for a just-in-time (JIT) runner registration.
//   2. Spawns a Paygress workload and waits for SSH access details.
//   3. Provisions Docker + the Actions runner over SSH.
//   4. Launches the runner with the JIT config, detached.
//
// GitHub JIT runners are inherently **ephemeral**: the runner accepts
// exactly one job, then deregisters itself and exits. That maps
// cleanly onto a per-second lease — you rent a box, it does one job,
// the lease lapses. No autoscaler, no webhook endpoint, no daemon.
//
// Deliberate v1 scope cuts
// ------------------------
//   - **No webhook autoscaler.** Production CI wants a service that
//     watches `workflow_job queued` and spawns on demand. That needs
//     a publicly-reachable endpoint and a GitHub App; it is the
//     natural follow-up, but it is friction that a first run should
//     not have to pay. One command, one runner, one job.
//   - **No job-completion teardown.** The provider's lifecycle is
//     purely time-based (`expires_at`), and nothing today observes
//     container exit. The lease therefore runs to expiry even if the
//     job finishes early. Closing that gap needs exit-code
//     propagation on the provider side, not here.
//   - **No matrix fan-out.** `paygress-cli batch` already fans out N
//     workloads; wiring one JIT runner per shard is a thin follow-up
//     once single-runner provisioning is proven.
//
// Requires `sshpass` on the caller's machine, matching the
// convention `bootstrap` already established (the provider
// auto-generates the workload's SSH password, so key auth isn't
// available on a fresh spawn).

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde::Deserialize;

use super::identity::{get_or_create_identity, parse_relays};
use super::spawn::{nostr_spawn_round_trip, NostrSpawnOutcome};

/// Labels every Paygress-provisioned runner carries. `self-hosted`
/// is implicit on GitHub's side but we register it explicitly so a
/// workflow's `runs-on:` can name it without surprises.
const DEFAULT_LABELS: &str = "self-hosted,paygress";

/// Fallback Actions runner version, used only when the GitHub API
/// lookup for the latest release fails (rate limit, offline, etc.).
/// Resolving dynamically is preferred so this never goes stale.
const FALLBACK_RUNNER_VERSION: &str = "2.330.0";

#[derive(Args)]
pub struct CiArgs {
    #[command(subcommand)]
    pub action: CiAction,
}

#[derive(Subcommand)]
pub enum CiAction {
    /// Rent compute and attach it to a GitHub repo as an ephemeral
    /// self-hosted Actions runner. Takes exactly one job.
    Runner(RunnerArgs),
}

#[derive(Args)]
pub struct RunnerArgs {
    /// Target repository as `owner/name`
    /// (e.g. `ngx-l402/ngx-l402`).
    #[arg(long)]
    pub repo: String,

    /// Provider ID — friendly 3-word name, full hex, `npub1…`, or an
    /// unambiguous 8+ character prefix.
    #[arg(long)]
    pub provider: String,

    /// Cashu token paying for the lease. The runner is billed per
    /// second for as long as the lease lasts, so size the token to
    /// the job: a 20-minute suite on a 400 msat/s tier costs ~480
    /// sats.
    #[arg(long)]
    pub token: String,

    /// Provider tier to rent. CI workloads that build containers
    /// want cores and RAM — `basic` (1 CPU / 1 GB) is rarely enough.
    #[arg(long, default_value = "premium")]
    pub tier: String,

    /// GitHub personal access token with `administration:write` on
    /// the repo (needed to mint a JIT runner registration). Falls
    /// back to `$GITHUB_TOKEN`, then `$GH_TOKEN`.
    #[arg(long)]
    pub github_token: Option<String>,

    /// Comma-separated runner labels. A workflow selects this runner
    /// with a matching `runs-on:`.
    #[arg(long, default_value = DEFAULT_LABELS)]
    pub labels: String,

    /// Base image for the rented workload. Must be a system image
    /// the provider's backend understands — on the LXD backend that
    /// means an LXD image alias, not a Docker image reference.
    #[arg(long, default_value = "ubuntu:24.04")]
    pub image: String,

    /// Runner name registered with GitHub. Defaults to a name
    /// derived from the workload once it is provisioned.
    #[arg(long)]
    pub name: Option<String>,

    /// Pin the Actions runner version instead of resolving the
    /// latest release at run time.
    #[arg(long)]
    pub runner_version: Option<String>,

    /// Skip installing Docker. Use when the base image already
    /// carries a Docker daemon — saves ~60s of provisioning on every
    /// run, which is the single biggest win from a prebaked image.
    #[arg(long)]
    pub skip_docker: bool,

    /// Nostr relays (comma-separated). Defaults to the CLI's
    /// standard relay set.
    #[arg(long)]
    pub relays: Option<String>,

    /// Nostr private key (nsec/hex). Defaults to the auto-generated
    /// identity at `~/.paygress/identity`.
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// How long to wait for the provider's spawn response.
    #[arg(long, default_value_t = 180)]
    pub timeout_secs: u64,

    /// Print the provisioning script instead of running it. Nothing
    /// is spawned and no token is spent — useful for auditing what
    /// lands on rented compute before paying for it.
    #[arg(long)]
    pub dry_run: bool,

    /// Keep the box running after the job instead of powering off.
    /// Default behavior releases compute the moment the runner's one
    /// job ends; keep it alive to SSH in and inspect logs.
    #[arg(long)]
    pub keep_alive: bool,
}

pub async fn execute(args: CiArgs, verbose: bool) -> Result<()> {
    match args.action {
        CiAction::Runner(a) => run_runner(a, verbose).await,
    }
}

/// `owner/name`, validated. GitHub rejects malformed slugs anyway,
/// but failing here keeps us from spending a Cashu token on a spawn
/// whose registration was never going to work.
fn parse_repo(repo: &str) -> Result<(String, String)> {
    let mut parts = repo.splitn(2, '/');
    let owner = parts.next().unwrap_or("").trim();
    let name = parts.next().unwrap_or("").trim();
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        bail!("--repo must be `owner/name` (got `{}`)", repo);
    }
    Ok((owner.to_string(), name.to_string()))
}

/// CLI flag wins, then `$GITHUB_TOKEN`, then `$GH_TOKEN` (what `gh`
/// exports). Resolved before any spend so a missing token fails
/// free.
fn resolve_github_token(explicit: Option<String>) -> Result<String> {
    if let Some(t) = explicit {
        if !t.trim().is_empty() {
            return Ok(t);
        }
    }
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            if !t.trim().is_empty() {
                return Ok(t);
            }
        }
    }
    bail!(
        "no GitHub token — pass --github-token or set $GITHUB_TOKEN. \
         It needs `administration:write` on the repo to mint a \
         just-in-time runner registration."
    )
}

fn labels_vec(labels: &str) -> Vec<String> {
    labels
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[derive(Deserialize)]
struct JitConfigResponse {
    encoded_jit_config: String,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// Ask GitHub to mint a just-in-time runner registration. The
/// returned blob encodes the repo, labels, and a single-use
/// credential; a runner started with it accepts one job and then
/// deregisters itself.
async fn generate_jit_config(
    owner: &str,
    repo: &str,
    name: &str,
    labels: &[String],
    github_token: &str,
) -> Result<String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/actions/runners/generate-jitconfig",
        owner, repo
    );
    let body = serde_json::json!({
        "name": name,
        "runner_group_id": 1,
        "labels": labels,
        "work_folder": "_work",
    });

    let resp = reqwest::Client::new()
        .post(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "paygress-cli")
        .bearer_auth(github_token)
        .json(&body)
        .send()
        .await
        .context("failed to reach the GitHub API")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        // 403 here is nearly always a scope problem, and the raw
        // GitHub message ("Resource not accessible by personal
        // access token") does not say so. Name the likely cause.
        let hint = match status.as_u16() {
            401 => "\n  hint: the token is invalid or expired.",
            403 => {
                "\n  hint: the token likely lacks `administration:write` on this repo. \
                 A fine-grained PAT needs Repository permissions → Administration → \
                 Read and write."
            }
            404 => {
                "\n  hint: repo not found, or the token cannot see it. \
                 Check the owner/name spelling and the token's repo access."
            }
            _ => "",
        };
        bail!(
            "GitHub refused the runner registration ({}): {}{}",
            status,
            text.trim(),
            hint
        );
    }

    let parsed: JitConfigResponse =
        serde_json::from_str(&text).context("unexpected JSON from generate-jitconfig")?;
    Ok(parsed.encoded_jit_config)
}

/// Resolve the newest Actions runner release. Pinning a constant
/// would rot; a failed lookup falls back rather than aborting a run
/// the caller already intends to pay for.
async fn resolve_runner_version(explicit: Option<String>) -> String {
    if let Some(v) = explicit {
        return v.trim_start_matches('v').to_string();
    }
    let fetched = reqwest::Client::new()
        .get("https://api.github.com/repos/actions/runner/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "paygress-cli")
        .send()
        .await
        .ok();

    if let Some(resp) = fetched {
        if let Ok(rel) = resp.json::<LatestRelease>().await {
            let v = rel.tag_name.trim_start_matches('v').to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    FALLBACK_RUNNER_VERSION.to_string()
}

/// Build the script that turns a bare rented box into a live Actions
/// runner. Kept as a pure function so its shape is testable without
/// spawning anything.
///
/// `RUNNER_ALLOW_RUNASROOT` is required because Paygress hands out
/// root SSH and the runner otherwise refuses to start as root.
fn provisioning_script(
    jit_config: &str,
    runner_version: &str,
    install_docker: bool,
    keep_alive: bool,
) -> String {
    let docker_step = if install_docker {
        r#"
if ! command -v docker >/dev/null 2>&1; then
  echo "[paygress-ci] installing docker"
  curl -fsSL https://get.docker.com | sh >/var/log/paygress-docker-install.log 2>&1
fi
systemctl start docker 2>/dev/null || true
docker version --format 'docker {{.Server.Version}}' || echo "[paygress-ci] WARNING: docker daemon not responding"
"#
    } else {
        "\necho \"[paygress-ci] skipping docker install (--skip-docker)\"\n"
    };

    // A JIT runner exits after its single job, so the box itself is
    // the completion signal: power off, and the lease's CPU/RAM are
    // released immediately instead of idling until `expires_at`. The
    // provider's expiry sweep deletes the stopped container later.
    let after_job = if keep_alive {
        r#"echo "[paygress-ci] job done (--keep-alive: box stays up until lease expiry)""#
    } else {
        r#"echo "[paygress-ci] job done - powering off to release compute"
poweroff"#
    };

    format!(
        r#"set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

# A prebaked image (e.g. the `paygress-gha-runner` LXD alias) ships
# the runner preinstalled; skip ~2-3 min of apt + download on those.
if [ -x /opt/actions-runner/run.sh ]; then
  echo "[paygress-ci] prebaked runner image detected - skipping install"
else
  echo "[paygress-ci] installing base packages"
  apt-get update -qq >/dev/null 2>&1
  apt-get install -y -qq curl tar git ca-certificates jq >/dev/null 2>&1
  echo "[paygress-ci] installing actions runner v{runner_version}"
  mkdir -p /opt/actions-runner
  cd /opt/actions-runner
  curl -fsSL -o runner.tar.gz \
    "https://github.com/actions/runner/releases/download/v{runner_version}/actions-runner-linux-x64-{runner_version}.tar.gz"
  tar xzf runner.tar.gz
  rm -f runner.tar.gz

  # Runner's own dependency installer (libicu et al).
  ./bin/installdependencies.sh >/var/log/paygress-runner-deps.log 2>&1 || \
    echo "[paygress-ci] WARNING: installdependencies.sh reported errors; see /var/log/paygress-runner-deps.log"
fi
{docker_step}
echo "[paygress-ci] starting ephemeral runner"
cd /opt/actions-runner
cat > paygress-wrap.sh <<'WRAP'
#!/bin/bash
export RUNNER_ALLOW_RUNASROOT=1
cd /opt/actions-runner
./run.sh --jitconfig "$PAYGRESS_JIT_CONFIG" > /var/log/paygress-runner.log 2>&1
{after_job}
WRAP
chmod +x paygress-wrap.sh
# Detached so the provisioning SSH session can close while the runner
# waits for its one job. JIT config travels via env, not argv, so it
# never shows in `ps` on the box.
PAYGRESS_JIT_CONFIG='{jit_config}' nohup ./paygress-wrap.sh > /var/log/paygress-wrap.log 2>&1 &

sleep 5
if pgrep -f Runner.Listener >/dev/null 2>&1; then
  echo "[paygress-ci] runner is live and listening for one job"
else
  echo "[paygress-ci] ERROR: runner failed to start; log follows"
  tail -30 /var/log/paygress-runner.log || true
  exit 1
fi
"#
    )
}

/// Pipe a script to the rented box over SSH. Mirrors `bootstrap`'s
/// shell-out approach (`sshpass` + `ssh`) rather than introducing a
/// Rust SSH client — one convention per repo.
fn ssh_run_script(host: &str, port: u16, user: &str, password: &str, script: &str) -> Result<bool> {
    let target = format!("{}@{}", user, host);
    let args = vec![
        "-p".to_string(),
        password.to_string(),
        "ssh".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
        "-p".to_string(),
        port.to_string(),
        target,
        "bash -s".to_string(),
    ];

    let mut child = Command::new("sshpass")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context(
            "failed to run sshpass. Is it installed? \
             (apt-get install sshpass / brew install hudochenkov/sshpass/sshpass)",
        )?;

    {
        use std::io::Write;
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open stdin on the ssh process")?;
        stdin.write_all(script.as_bytes())?;
    }

    Ok(child.wait()?.success())
}

/// Poll SSH until the freshly-spawned box accepts a connection. The
/// provider returns access details as soon as the container is
/// created, which is a little ahead of sshd actually listening.
fn wait_for_ssh(host: &str, port: u16, user: &str, password: &str, attempts: u32) -> bool {
    for i in 0..attempts {
        let ok = Command::new("sshpass")
            .args([
                "-p",
                password,
                "ssh",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=10",
                "-p",
                &port.to_string(),
                &format!("{}@{}", user, host),
                "true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        if i + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
    false
}

async fn run_runner(args: RunnerArgs, verbose: bool) -> Result<()> {
    let (owner, repo) = parse_repo(&args.repo)?;
    let labels = labels_vec(&args.labels);
    if labels.is_empty() {
        bail!("--labels resolved to an empty set");
    }

    // Resolve everything that can fail for free *before* spending
    // the Cashu token. A spawn is irreversible; a bad PAT is not
    // worth 480 sats.
    let github_token = resolve_github_token(args.github_token.clone())?;
    let runner_version = resolve_runner_version(args.runner_version.clone()).await;

    if args.dry_run {
        println!("{}", "Dry run — nothing spawned, no token spent".yellow());
        println!("  repo      : {}/{}", owner, repo);
        println!("  labels    : {}", labels.join(", "));
        println!("  runner    : v{}", runner_version);
        println!("  tier      : {}", args.tier);
        println!("\n{}", "--- provisioning script ---".dimmed());
        println!(
            "{}",
            provisioning_script(
                "<JIT_CONFIG_REDACTED>",
                &runner_version,
                !args.skip_docker,
                args.keep_alive
            )
        );
        return Ok(());
    }

    let runner_name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("paygress-{}", &uuid_suffix()));

    println!(
        "{} minting JIT runner registration for {}/{}",
        "→".blue(),
        owner,
        repo
    );
    let jit_config =
        generate_jit_config(&owner, &repo, &runner_name, &labels, &github_token).await?;

    println!(
        "{} renting compute from provider {} (tier {})",
        "→".blue(),
        args.provider,
        args.tier
    );

    let identity = get_or_create_identity(args.nostr_key.clone())?;
    let relays = parse_relays(args.relays.clone());
    let ssh_user = "root".to_string();
    let ssh_pass = generate_password();

    let outcome = nostr_spawn_round_trip(
        &args.provider,
        &args.tier,
        &args.token,
        args.image.clone(),
        ssh_user.clone(),
        ssh_pass.clone(),
        None, // no template — a bare system container is the runner
        None,
        None,
        None,
        None,
        None,
        relays,
        identity,
        args.timeout_secs,
    )
    .await?;

    let access = match outcome {
        NostrSpawnOutcome::Success(a) => a,
        NostrSpawnOutcome::ProviderOffline => {
            bail!(
                "provider is offline — no token was spent. Try `paygress-cli list --online-only`."
            )
        }
        NostrSpawnOutcome::ProviderError(e) => {
            bail!(
                "provider rejected the spawn: {} ({})",
                e.message,
                e.error_type
            )
        }
        NostrSpawnOutcome::Timeout => bail!(
            "provider did not respond within {}s. The token MAY have been spent — \
             check with `paygress-cli status --provider {}`.",
            args.timeout_secs,
            args.provider
        ),
        NostrSpawnOutcome::UnknownResponse(s) => {
            bail!("unrecognized provider response (newer provider?): {}", s)
        }
    };

    let host = if access.host_address.is_empty() {
        bail!("provider returned no host address; cannot reach the workload over SSH")
    } else {
        access.host_address.clone()
    };

    println!(
        "{} workload {} up at {}:{} (expires {})",
        "✓".green(),
        access.pod_npub,
        host,
        access.node_port,
        access.expires_at
    );

    // The provider overrides our requested password (see
    // `extract_ssh_password`), so the response is authoritative.
    let ssh_pass = extract_ssh_password(&access.instructions).unwrap_or(ssh_pass);

    println!("{} waiting for sshd", "→".blue());
    if !wait_for_ssh(&host, access.node_port, &ssh_user, &ssh_pass, 24) {
        // The lease is paid and running; surface the credentials so
        // the caller can salvage it rather than losing the spend.
        bail!(
            "workload never accepted SSH on {}:{}. The lease is live and paid — \
             connect manually to debug:\n  ssh -p {} {}@{}\n  password: {}",
            host,
            access.node_port,
            access.node_port,
            ssh_user,
            host,
            ssh_pass
        );
    }

    println!("{} provisioning runner", "→".blue());
    let script = provisioning_script(
        &jit_config,
        &runner_version,
        !args.skip_docker,
        args.keep_alive,
    );
    let ok = ssh_run_script(&host, access.node_port, &ssh_user, &ssh_pass, &script)?;
    if !ok {
        bail!(
            "provisioning failed. The lease is still live — inspect with: \
             ssh -p {} {}@{} 'tail -50 /var/log/paygress-runner.log'",
            access.node_port,
            ssh_user,
            host
        );
    }

    println!();
    println!("{}", "Runner is live".green().bold());
    println!("  repo    : {}/{}", owner, repo);
    println!("  name    : {}", runner_name);
    println!("  labels  : {}", labels.join(", "));
    println!("  expires : {}", access.expires_at);
    println!();
    println!("Target it from a workflow with:");
    println!("  {}", format!("runs-on: [{}]", labels.join(", ")).cyan());
    println!();
    println!("It accepts exactly one job, then deregisters.");
    println!(
        "  logs   : ssh -p {} {}@{} 'tail -f /var/log/paygress-runner.log'",
        access.node_port, ssh_user, host
    );
    println!(
        "  extend : paygress-cli topup --provider {} --pod-id {} --token <cashu>",
        args.provider, access.pod_npub
    );

    if verbose {
        println!();
        println!("{}", "SSH password (lease-scoped):".dimmed());
        println!("  {}", ssh_pass.dimmed());
    }

    Ok(())
}

/// Short random suffix for a runner name. GitHub requires runner
/// names to be unique within a repo, so a collision would fail the
/// registration.
fn uuid_suffix() -> String {
    use rand::Rng;
    const HEX: &[u8] = b"0123456789abcdef";
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| HEX[rng.gen_range(0..HEX.len())] as char)
        .collect()
}

/// Pull the SSH password out of the provider's connection
/// instructions.
///
/// The provider does **not** honor the consumer's requested
/// `ssh_pass`: `handle_spawn_request` unconditionally calls
/// `generate_password()` (`src/provider.rs:1276`) and reports the
/// result back in `AccessDetailsContent.instructions` as a
/// `Password: <value>` line. Trusting our locally-generated password
/// therefore fails authentication every time. Parse the authoritative
/// one instead, and treat our own as a fallback in case a future
/// provider starts honoring the request.
fn extract_ssh_password(instructions: &[String]) -> Option<String> {
    for line in instructions {
        // Match on the label rather than the emoji so a provider that
        // drops the decoration still parses.
        if let Some(idx) = line.find("Password:") {
            let value = line[idx + "Password:".len()..].trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Lease-scoped SSH password. Never reused, never persisted — the
/// lease outlives it by nothing.
fn generate_password() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..24)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_owner_and_name() {
        let (o, n) = parse_repo("ngx-l402/ngx-l402").unwrap();
        assert_eq!(o, "ngx-l402");
        assert_eq!(n, "ngx-l402");
    }

    #[test]
    fn rejects_malformed_repo_slugs() {
        for bad in ["", "ngx-l402", "/name", "owner/", "a/b/c"] {
            assert!(
                parse_repo(bad).is_err(),
                "expected `{}` to be rejected",
                bad
            );
        }
    }

    #[test]
    fn labels_split_and_trim() {
        assert_eq!(
            labels_vec(" self-hosted , paygress ,, "),
            vec!["self-hosted".to_string(), "paygress".to_string()]
        );
    }

    #[test]
    fn default_labels_include_self_hosted() {
        // A workflow's `runs-on: self-hosted` must match, or the job
        // queues forever against a runner that never claims it.
        assert!(labels_vec(DEFAULT_LABELS).contains(&"self-hosted".to_string()));
    }

    #[test]
    fn script_runs_runner_as_root_and_detaches() {
        let s = provisioning_script("JITBLOB", "2.330.0", true, false);
        // Paygress hands out root SSH; the runner refuses root
        // without this, which is the most likely silent failure.
        assert!(s.contains("RUNNER_ALLOW_RUNASROOT=1"));
        // Must survive the provisioning SSH session closing, with the
        // JIT config delivered via env (not argv).
        assert!(s.contains("PAYGRESS_JIT_CONFIG='JITBLOB' nohup ./paygress-wrap.sh"));
        assert!(s.contains("actions-runner-linux-x64-2.330.0.tar.gz"));
    }

    #[test]
    fn script_honors_skip_docker() {
        let with = provisioning_script("J", "2.330.0", true, false);
        let without = provisioning_script("J", "2.330.0", false, false);
        assert!(with.contains("get.docker.com"));
        assert!(!without.contains("get.docker.com"));
    }

    #[test]
    fn script_powers_off_after_job_unless_kept_alive() {
        // Poweroff is the lease-release mechanism: a stopped container
        // frees its compute instead of idling until `expires_at`.
        let default = provisioning_script("J", "2.330.0", false, false);
        let kept = provisioning_script("J", "2.330.0", false, true);
        assert!(default.contains("poweroff"));
        assert!(!kept.contains("poweroff"));
    }

    #[test]
    fn script_skips_install_on_prebaked_images() {
        let s = provisioning_script("J", "2.330.0", false, false);
        // The guard that makes prebaked images (runner preinstalled)
        // skip the ~2-3 min apt + download path.
        assert!(s.contains("if [ -x /opt/actions-runner/run.sh ]"));
    }

    #[test]
    fn extracts_password_from_provider_instructions() {
        // Verbatim shape emitted by `handle_spawn_request`
        // (src/provider.rs:1660-1665), emoji and all.
        let instructions = vec![
            "👤 Username: root".to_string(),
            "🔑 Password: Xk29fLpQ7zRm".to_string(),
            "⌛ Expires: 2026-07-19 17:40:09 UTC".to_string(),
            "  ssh -p 2000 root@72.61.173.244".to_string(),
        ];
        assert_eq!(
            extract_ssh_password(&instructions),
            Some("Xk29fLpQ7zRm".to_string())
        );
    }

    #[test]
    fn password_extraction_survives_missing_emoji() {
        let instructions = vec!["Password: plain123".to_string()];
        assert_eq!(
            extract_ssh_password(&instructions),
            Some("plain123".to_string())
        );
    }

    #[test]
    fn password_extraction_returns_none_when_absent_or_empty() {
        assert_eq!(extract_ssh_password(&[]), None);
        assert_eq!(
            extract_ssh_password(&["👤 Username: root".to_string()]),
            None
        );
        // An empty value must not be mistaken for a real password.
        assert_eq!(extract_ssh_password(&["Password:   ".to_string()]), None);
    }

    #[test]
    fn generated_secrets_are_not_trivially_short() {
        assert_eq!(generate_password().len(), 24);
        assert_eq!(uuid_suffix().len(), 8);
    }
}
