// Paid-sandbox mechanics: buy a workload for one job, reach it over SSH, and
// hand back a running child whose pipes are the job's streams.
//
// There is no consumer-side teardown in the protocol — a lease ends when the
// money runs out — so a sandbox is never reused and never explicitly
// destroyed. That is also why a hung job cannot leak a slot the way it does
// for pool-based adapters: the box dies on its own.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tracing::{debug, warn};

use crate::commands::spawn::{nostr_spawn_round_trip, NostrSpawnOutcome, NostrSpawnParams};
use crate::util::generate_password;

const SSH_PROBE_INTERVAL: Duration = Duration::from_secs(3);
const SSH_PASSWORD_LEN: usize = 24;

pub struct SandboxConfig {
    pub provider: String,
    pub tier: String,
    pub template: Option<String>,
    /// Only reaches the backend when no template is set. LXD reads it as an
    /// image alias (`paygress-ci`), Docker as an image reference.
    pub image: String,
    pub token_command: String,
    pub ssh_user: String,
    pub relays: Vec<String>,
    pub nostr_key: String,
    pub spawn_timeout_secs: u64,
    pub ssh_ready_timeout_secs: u64,
}

pub struct Sandbox {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub pod_id: String,
    pub expires_at: String,
}

/// Runs the operator's token command and returns what it printed. Anything it
/// writes to stderr is the operator's own wallet tooling talking, so it goes
/// to our log rather than to the caller.
async fn mint_token(command: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await
        .with_context(|| format!("could not run token command `{}`", command))?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        debug!("token command stderr: {}", stderr);
    }

    if !output.status.success() {
        return Err(anyhow!(
            "token command exited {}: {}",
            output.status.code().unwrap_or(-1),
            if stderr.is_empty() {
                "no output"
            } else {
                stderr
            }
        ));
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(anyhow!("token command printed nothing on stdout"));
    }
    Ok(token)
}

pub async fn provision(cfg: &SandboxConfig) -> Result<Sandbox> {
    let token = mint_token(&cfg.token_command).await?;
    let password = generate_password(SSH_PASSWORD_LEN);

    let params = NostrSpawnParams {
        tier: cfg.tier.clone(),
        token,
        image: cfg.image.clone(),
        ssh_user: cfg.ssh_user.clone(),
        ssh_pass: password.clone(),
        template_slug: cfg.template.clone(),
        ..Default::default()
    };

    let outcome = nostr_spawn_round_trip(
        &cfg.provider,
        params,
        cfg.relays.clone(),
        cfg.nostr_key.clone(),
        cfg.spawn_timeout_secs,
    )
    .await?;

    let access = match outcome {
        NostrSpawnOutcome::Success(access) => access,
        NostrSpawnOutcome::ProviderOffline => {
            return Err(anyhow!("provider `{}` is offline", cfg.provider))
        }
        NostrSpawnOutcome::ProviderError(e) => {
            return Err(anyhow!(
                "provider refused the spawn: {} ({})",
                e.message,
                e.error_type
            ))
        }
        // The token may well have been spent, so this is not a free retry.
        NostrSpawnOutcome::Timeout => {
            return Err(anyhow!(
                "provider did not answer within {}s; the payment may have been taken",
                cfg.spawn_timeout_secs
            ))
        }
        NostrSpawnOutcome::UnknownResponse(content) => {
            return Err(anyhow!(
                "provider sent an unrecognised response: {}",
                content
            ))
        }
    };

    if access.host_address.is_empty() {
        return Err(anyhow!(
            "provider returned no host address; it is too old to drive over SSH"
        ));
    }

    Ok(Sandbox {
        host: access.host_address,
        port: access.node_port,
        user: cfg.ssh_user.clone(),
        password,
        pod_id: access.pod_npub,
        expires_at: access.expires_at,
    })
}

/// `sshpass -e` takes the password from `SSHPASS` in the environment, so it
/// never reaches argv where any local user could read it.
///
/// Host keys are neither checked nor recorded: the box is new for this job and
/// its key is unknown by construction, and its operator could impersonate it
/// regardless — they own the machine.
fn ssh_argv(sandbox: &Sandbox) -> Vec<String> {
    [
        "-e",
        "ssh",
        "-T",
        "-p",
        &sandbox.port.to_string(),
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "PubkeyAuthentication=no",
        "-o",
        "NumberOfPasswordPrompts=1",
        "-o",
        "ConnectTimeout=10",
        // Otherwise ssh's own chatter is indistinguishable from the job's
        // stderr in the log the caller publishes.
        "-o",
        "LogLevel=ERROR",
        &format!("{}@{}", sandbox.user, sandbox.host),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn sshpass(sandbox: &Sandbox) -> Command {
    let mut command = Command::new("sshpass");
    command.args(ssh_argv(sandbox));
    command.env("SSHPASS", &sandbox.password);
    command
}

/// Polls until the box answers, because a provider reports the workload
/// created well before sshd is listening inside it.
pub async fn wait_until_reachable(sandbox: &Sandbox, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let output = sshpass(sandbox)
            .arg("true")
            .stdin(Stdio::null())
            .output()
            .await
            .context("could not run sshpass; is it installed?")?;

        if output.status.success() {
            return Ok(());
        }
        if tokio::time::Instant::now() + SSH_PROBE_INTERVAL >= deadline {
            let failure = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(anyhow!(
                "sandbox {} was not reachable over SSH within {}s: {}",
                sandbox.pod_id,
                timeout.as_secs(),
                if failure.is_empty() {
                    "no error output"
                } else {
                    &failure
                }
            ));
        }
        tokio::time::sleep(SSH_PROBE_INTERVAL).await;
    }
}

/// Feeds `script` to a remote `/bin/sh` and returns the child with its output
/// pipes open. `kill_on_drop` is what honours the contract's "kill the job as
/// soon as the caller disconnects".
pub async fn start(sandbox: &Sandbox, script: &str) -> Result<Child> {
    let mut child = sshpass(sandbox)
        .arg("/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("could not run sshpass; is it installed?")?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("ssh stdin was not piped"))?;

    if let Err(e) = stdin.write_all(script.as_bytes()).await {
        warn!("writing the job script to ssh failed: {}", e);
    }
    // Closing stdin is what lets the remote shell run the script rather than
    // wait for more of it.
    drop(stdin);

    Ok(child)
}
