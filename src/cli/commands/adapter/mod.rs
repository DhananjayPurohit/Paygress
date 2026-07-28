// `paygress-cli adapter` — an execution adapter that buys each job a fresh
// paid sandbox.
//
// It speaks the Loom execution-adapter contract (line-delimited JSON over a
// Unix socket) that ngit-ci vendors as its `socket-adapter` backend, so an
// unmodified `ngit-ci --runner socket-adapter --adapter-socket <path>` runs its
// CI jobs on rented compute. The caller is the smart half: it owns queueing,
// retries and timeouts. This side is deliberately dumb — no queue (a busy
// adapter rejects immediately), no timeouts (the caller drops the connection
// and we kill the job), no state between connections.
//
// The sandbox must contain whatever the job script needs. ngit-ci's rendered
// script needs `bash`, `git`, `act` and a container daemon — build one with
// `images/ci-sandbox/`. `agent-sandbox` is enough for jobs that never shell out
// to `act`.
//
// A caller that hangs up while we are still buying a sandbox costs a lease with
// nothing to show for it: provisioning takes a minute or so and is not
// cancellable. Callers enforcing timeouts shorter than that will pay for boxes
// they never use.

mod ci_result;
mod job;
mod protocol;
mod script;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::util::{get_or_create_identity, parse_relays};
use job::SandboxConfig;
use protocol::{parse_request, Response};

#[derive(Args)]
pub struct AdapterArgs {
    /// Unix socket to listen on (ngit-ci's `--adapter-socket`)
    #[arg(long)]
    pub socket: PathBuf,

    /// Provider to buy each job's sandbox from (name, npub, or id prefix)
    #[arg(long)]
    pub provider: String,

    /// Shell command that prints one Cashu token, run once per job
    #[arg(long)]
    pub token_command: String,

    /// Template slug the job runs inside; omit for the provider's own image
    #[arg(long)]
    pub template: Option<String>,

    /// Image the sandbox runs, when no template is set (LXD alias, Docker ref)
    #[arg(long, default_value = "ubuntu:22.04")]
    pub image: String,

    /// Tier on the provider's offer
    #[arg(short, long, default_value = "basic")]
    pub tier: String,

    /// Jobs to run at once; the rest are rejected, never queued
    #[arg(long, default_value_t = 1)]
    pub max_concurrent_jobs: usize,

    /// SSH user to request on the sandbox
    // root by default: CI job scripts install packages.
    #[arg(long, default_value = "root")]
    pub ssh_user: String,

    /// Seconds to wait for the provider's spawn reply
    #[arg(long, default_value_t = 120)]
    pub spawn_timeout_secs: u64,

    /// Seconds to wait for the sandbox to accept SSH
    #[arg(long, default_value_t = 300)]
    pub ssh_ready_timeout_secs: u64,

    /// Your Nostr private key (nsec) - uses ~/.paygress/identity if not provided
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated)
    #[arg(long)]
    pub relays: Option<String>,
}

pub async fn execute(args: AdapterArgs, _verbose: bool) -> Result<()> {
    if args.max_concurrent_jobs == 0 {
        anyhow::bail!("--max-concurrent-jobs must be at least 1");
    }

    let config = Arc::new(SandboxConfig {
        provider: args.provider,
        tier: args.tier,
        template: args.template,
        image: args.image,
        token_command: args.token_command,
        ssh_user: args.ssh_user,
        relays: parse_relays(args.relays),
        nostr_key: get_or_create_identity(args.nostr_key)?,
        spawn_timeout_secs: args.spawn_timeout_secs,
        ssh_ready_timeout_secs: args.ssh_ready_timeout_secs,
    });

    let listener = bind(&args.socket).await?;
    let capacity = Arc::new(Semaphore::new(args.max_concurrent_jobs));

    println!("{}", "Paygress execution adapter".blue().bold());
    println!("  {}    {}", "Socket:".bold(), args.socket.display());
    println!("  {}  {}", "Provider:".bold(), config.provider.cyan());
    match config.template.as_deref() {
        Some(template) => println!("  {}  {}", "Template:".bold(), template),
        None => println!("  {}     {}", "Image:".bold(), config.image),
    }
    println!(
        "  {}  {} job(s)\n",
        "Capacity:".bold(),
        args.max_concurrent_jobs
    );

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!("accept failed: {}", e);
                continue;
            }
        };
        let config = Arc::clone(&config);
        let capacity = Arc::clone(&capacity);
        tokio::spawn(async move { serve(stream, config, capacity).await });
    }
}

/// A leftover socket file from a crash would make `bind` fail forever, but
/// removing one a live adapter is listening on would silently steal its jobs.
/// Connecting tells the two apart.
async fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            anyhow::bail!("{} is already served by another adapter", path.display());
        }
        std::fs::remove_file(path)
            .with_context(|| format!("could not remove stale socket {}", path.display()))?;
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("could not listen on {}", path.display()))?;

    // The socket spends money and runs code; other local users have no
    // business reaching it.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not restrict {}", path.display()))?;

    Ok(listener)
}

async fn send(writer: &mut OwnedWriteHalf, response: Response) -> bool {
    writer
        .write_all(response.to_line().as_bytes())
        .await
        .is_ok()
}

async fn reject(writer: &mut OwnedWriteHalf, error: String) {
    warn!("rejecting job: {}", error);
    send(writer, Response::Error { error }).await;
}

/// One connection, one job. Every failure before `started` is a rejection the
/// caller may retry elsewhere; after it, the connection carries the job's
/// output and one terminal message.
async fn serve(stream: UnixStream, config: Arc<SandboxConfig>, capacity: Arc<Semaphore>) {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => return,
        Ok(_) => {}
        Err(e) => {
            debug!("caller vanished before sending a request: {}", e);
            return;
        }
    }

    let request = match parse_request(&line) {
        Ok(request) => request,
        Err(e) => return reject(&mut writer, e).await,
    };

    let permit = match Arc::clone(&capacity).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return reject(
                &mut writer,
                "adapter at capacity - all job slots busy. Retry later.".to_string(),
            )
            .await
        }
    };

    let script = match script::build(&request.cmd, &request.args, &request.env, &request.stdin) {
        Ok(script) => script,
        Err(e) => return reject(&mut writer, e).await,
    };

    let identifier = request.identifier.clone();
    info!("job {}: buying a sandbox", identifier);
    let sandbox = match job::provision(&config).await {
        Ok(sandbox) => sandbox,
        Err(e) => {
            return reject(
                &mut writer,
                format!("could not provision a sandbox: {:#}", e),
            )
            .await
        }
    };
    info!(
        "job {}: sandbox {} at {}:{}, leased until {}",
        identifier, sandbox.pod_id, sandbox.host, sandbox.port, sandbox.expires_at
    );

    let ready_timeout = Duration::from_secs(config.ssh_ready_timeout_secs);
    if let Err(e) = job::wait_until_reachable(&sandbox, ready_timeout).await {
        return reject(&mut writer, format!("{:#}", e)).await;
    }

    if !send(&mut writer, Response::Started).await {
        return;
    }

    let outcome = run(&mut reader, &mut writer, &sandbox, &script, &identifier).await;
    drop(permit);

    // After the terminal message: the caller is already unblocked, and a relay
    // being slow must not hold its job slot open.
    if let Some((outcome, log)) = outcome {
        publish_job_result(&config, &sandbox, &request.env, outcome, log, &identifier).await;
    }
}

/// `None` when the job produced no verdict to attest to — the caller hung up,
/// or ssh itself failed.
async fn run(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
    sandbox: &job::Sandbox,
    script: &str,
    identifier: &str,
) -> Option<(ci_result::Outcome, ci_result::LogTail)> {
    let started_at = Instant::now();
    let started_unix = unix_now();
    let mut child = match job::start(sandbox, script).await {
        Ok(child) => child,
        Err(e) => {
            // Past `started`, so this is an infrastructure failure rather than
            // a rejection.
            send(
                writer,
                Response::Error {
                    error: format!("{:#}", e),
                },
            )
            .await;
            return None;
        }
    };

    let (mut stdout, mut stderr) = match (child.stdout.take(), child.stderr.take()) {
        (Some(out), Some(err)) => (out, err),
        _ => {
            send(
                writer,
                Response::Error {
                    error: "ssh output pipes were not captured".to_string(),
                },
            )
            .await;
            return None;
        }
    };

    let mut log = ci_result::LogTail::default();
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let mut discard = [0u8; 256];
    let mut stdout_open = true;
    let mut stderr_open = true;

    while stdout_open || stderr_open {
        // Every branch reads, which `AsyncReadExt::read` makes cancel-safe;
        // `read_line` is not, so the disconnect watch reads bytes it discards.
        tokio::select! {
            result = stdout.read(&mut out_buf), if stdout_open => match result {
                Ok(0) => stdout_open = false,
                Ok(n) => {
                    let data = String::from_utf8_lossy(&out_buf[..n]).into_owned();
                    log.push(&data);
                    if !send(writer, Response::Stdout { data }).await {
                        return None;
                    }
                }
                Err(e) => {
                    debug!("job {}: stdout read failed: {}", identifier, e);
                    stdout_open = false;
                }
            },
            result = stderr.read(&mut err_buf), if stderr_open => match result {
                Ok(0) => stderr_open = false,
                Ok(n) => {
                    let data = String::from_utf8_lossy(&err_buf[..n]).into_owned();
                    log.push(&data);
                    if !send(writer, Response::Stderr { data }).await {
                        return None;
                    }
                }
                Err(e) => {
                    debug!("job {}: stderr read failed: {}", identifier, e);
                    stderr_open = false;
                }
            },
            // The caller enforces its timeout by hanging up. Returning here
            // drops the child, and `kill_on_drop` kills ssh with it.
            result = reader.read(&mut discard) => {
                if !matches!(result, Ok(n) if n > 0) {
                    info!("job {}: caller disconnected, killing the job", identifier);
                    return None;
                }
            }
        }
    }

    let duration = started_at.elapsed().as_secs();
    match child.wait().await {
        Ok(status) => match status.code() {
            // 255 is also ssh's own transport failure; the contract already
            // says a nonzero exit is not distinguishable from a failed job.
            Some(exit_code) => {
                info!("job {}: exit {} after {}s", identifier, exit_code, duration);
                send(
                    writer,
                    Response::Completed {
                        exit_code,
                        duration,
                    },
                )
                .await;
                return Some((
                    ci_result::Outcome {
                        exit_code,
                        started_at: started_unix,
                    },
                    log,
                ));
            }
            None => {
                send(
                    writer,
                    Response::Error {
                        error: "ssh was killed by a signal".to_string(),
                    },
                )
                .await;
            }
        },
        Err(e) => {
            send(
                writer,
                Response::Error {
                    error: format!("could not reap ssh: {}", e),
                },
            )
            .await;
        }
    }
    None
}

/// Signs and publishes a kind-9841 attestation that this adapter ran the job on
/// the named lease. Silently skipped when the coordinator predates the
/// `job_env` patch and gives us too little to build a compliant event.
/// Wall-clock seconds, for the `started_at` tag. `Instant` measures elapsed
/// time but carries no epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

async fn publish_job_result(
    config: &SandboxConfig,
    sandbox: &job::Sandbox,
    env: &std::collections::BTreeMap<String, String>,
    outcome: ci_result::Outcome,
    log: ci_result::LogTail,
    identifier: &str,
) {
    let Some(ctx) = ci_result::CiContext::from_env(env) else {
        debug!(
            "job {}: no CI context in the execute env, publishing no attestation",
            identifier
        );
        return;
    };

    let tags = ci_result::build_tags(
        &ctx,
        &ci_result::Lease {
            provider: &config.provider,
            pod_id: &sandbox.pod_id,
        },
        &outcome,
    );

    let client = match paygress::discovery::DiscoveryClient::new_with_key(
        config.relays.clone(),
        config.nostr_key.clone(),
    )
    .await
    {
        Ok(client) => client,
        Err(e) => {
            warn!(
                "job {}: could not open a relay connection: {}",
                identifier, e
            );
            return;
        }
    };

    match client
        .nostr()
        .publish_foreign_event(ci_result::KIND_CI_JOB_RESULT, log.into_content(), tags)
        .await
    {
        Ok(id) => info!("job {}: published job result {}", identifier, id),
        Err(e) => warn!(
            "job {}: publishing the job result failed: {}",
            identifier, e
        ),
    }
}
