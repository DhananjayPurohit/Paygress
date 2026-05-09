// `paygress-cli exec` — run a shell command inside a spawned
// agent-sandbox workload via its baked-in HTTP exec server.
//
// Used both as a standalone CLI convenience (interactive shell loop,
// CI scripts, batch fan-out) and as the underlying transport for the
// MCP `run_command` tool. Both call into `cli::exec_client::call_exec`
// so behavior stays identical.
//
// The host / port / user / pass values come from a prior
// `paygress-cli spawn` (or `batch`) — see the `--from-manifest` shape
// in the batch coordinator's docs for the natural pipeline.

use std::time::Duration;

use anyhow::Result;
use clap::Args;
use colored::Colorize;

use crate::exec_client;

#[derive(Args)]
pub struct ExecArgs {
    /// Host the agent-sandbox is published on. Either a bare host
    /// (1.2.3.4 / example.com) or a full URL (http://...). Comes from
    /// AccessDetails.host_address on the spawn response.
    #[arg(long)]
    pub host: String,

    /// Port the exec server is reachable on. For agent-sandbox
    /// templates this is the host port mapped to container 8080
    /// (printed as `sandbox-exec` in the spawn response's template
    /// ports list).
    #[arg(long)]
    pub port: u16,

    /// HTTP Basic auth username. Defaults to `root` (matching what
    /// the provider sets via the EXEC_USER env var on the container).
    #[arg(long, default_value = "root")]
    pub user: String,

    /// HTTP Basic auth password. Use the password the spawn response
    /// printed in the connection instructions.
    #[arg(long)]
    pub pass: String,

    /// Shell command to run. Interpreted by `bash -lc` inside the
    /// container, so pipes / redirects / `cd ... && ...` all work.
    #[arg(short, long)]
    pub command: String,

    /// Server-side command timeout (seconds). Server caps this at
    /// 1800s. Client transport timeout is set 5s above this so the
    /// server can return a structured `timed_out: true` response
    /// before our HTTP call gives up.
    #[arg(long, default_value_t = 60)]
    pub timeout_secs: u64,

    /// Override the working directory. Defaults server-side to
    /// `/workspace`.
    #[arg(long)]
    pub working_dir: Option<String>,

    /// Print structured JSON instead of human-readable
    /// stdout/stderr/exit. Useful for scripts.
    #[arg(long)]
    pub json: bool,
}

pub async fn execute(args: ExecArgs, _verbose: bool) -> Result<()> {
    let total_timeout = Duration::from_secs(args.timeout_secs.saturating_add(5));
    let resp = exec_client::call_exec(
        &args.host,
        args.port,
        &args.user,
        &args.pass,
        &args.command,
        Some(args.timeout_secs),
        args.working_dir.as_deref(),
        total_timeout,
    )
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        if !resp.stdout.is_empty() {
            print!("{}", resp.stdout);
        }
        if !resp.stderr.is_empty() {
            eprint!("{}", resp.stderr);
        }
        if resp.timed_out {
            eprintln!(
                "{} command timed out after {}s",
                "[timeout]".yellow(),
                args.timeout_secs
            );
        }
        eprintln!(
            "{} exit={} duration={}ms",
            "[done]".dimmed(),
            resp.exit_code,
            resp.duration_ms
        );
    }

    if resp.exit_code != 0 || resp.timed_out {
        std::process::exit(if resp.timed_out { 124 } else { resp.exit_code });
    }
    Ok(())
}
