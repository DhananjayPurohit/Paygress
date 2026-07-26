// `paygress-cli exec` — run a shell command inside a spawned
// agent-sandbox workload via its baked-in HTTP exec server. Shares
// `cli::exec_client` with the MCP `run_command` tool so both behave
// identically.

use std::time::Duration;

use anyhow::Result;
use clap::Args;
use colored::Colorize;

use crate::exec_client::{self, ExecRequest, ExecTarget};

#[derive(Args)]
pub struct ExecArgs {
    /// Sandbox host: bare host or full URL (from the spawn response)
    #[arg(long)]
    pub host: String,

    /// Port the exec server is reachable on (`sandbox-exec` in the spawn response)
    #[arg(long)]
    pub port: u16,

    /// HTTP Basic auth username
    #[arg(long, default_value = "root")]
    pub user: String,

    /// HTTP Basic auth password (the spawn response's SSH password)
    #[arg(long)]
    pub pass: String,

    /// Shell command to run, interpreted by `bash -lc` inside the container
    #[arg(short, long)]
    pub command: String,

    /// Server-side command timeout in seconds (server caps at 1800)
    #[arg(long, default_value_t = 60)]
    pub timeout_secs: u64,

    /// Working directory (server default: /workspace)
    #[arg(long)]
    pub working_dir: Option<String>,

    /// Print structured JSON instead of human-readable output
    #[arg(long)]
    pub json: bool,
}

pub async fn execute(args: ExecArgs, _verbose: bool) -> Result<()> {
    // Give the server 5s of headroom so it can report `timed_out: true`
    // before our transport timeout fires.
    let total_timeout = Duration::from_secs(args.timeout_secs.saturating_add(5));
    let target = ExecTarget {
        host: &args.host,
        port: args.port,
        user: &args.user,
        pass: &args.pass,
    };
    let request = ExecRequest {
        command: args.command.clone(),
        timeout_secs: Some(args.timeout_secs),
        working_dir: args.working_dir.clone(),
    };
    let resp = exec_client::call_exec(target, &request, total_timeout).await?;

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
