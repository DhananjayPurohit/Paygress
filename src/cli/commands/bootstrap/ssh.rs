// Remote command transport. Every step runs over a single ControlMaster
// connection so the operator authenticates once.

use anyhow::{Context, Result};
use colored::Colorize;
use std::io::Write;
use std::process::{Command, Stdio};

use super::{step_banner, BootstrapArgs};

pub(super) fn step_ssh_connection(args: &BootstrapArgs) -> Result<()> {
    step_banner("Step 1: Testing SSH Connection");

    if args.dry_run {
        println!("  Would connect to {}", args.host.cyan());
    } else {
        print!("  Connecting to {}... ", args.host);
        std::io::stdout().flush()?;

        open_ssh_master(args)?;

        if !run_ssh_command(args, "echo 'Connected'")? {
            println!("{}", "FAILED".red());
            close_ssh_master(args);
            return Err(anyhow::anyhow!("SSH connection failed"));
        }
        println!("{}", "OK".green());
    }
    println!();
    Ok(())
}

/// Grant passwordless sudo for the rest of the session, so the user is
/// prompted once here instead of on every subsequent SSH call. Removed
/// again at the end of `execute`.
pub(super) fn step_passwordless_sudo(args: &BootstrapArgs) -> Result<()> {
    if args.is_root() || args.dry_run {
        return Ok(());
    }

    println!(
        "{}",
        "Configuring passwordless sudo for bootstrap session...".yellow()
    );
    let grant_cmd = format!(
        "echo '{} ALL=(ALL) NOPASSWD: ALL' | sudo tee /etc/sudoers.d/paygress-bootstrap > /dev/null && echo 'GRANTED'",
        args.user
    );
    if !run_ssh_command(args, &grant_cmd)? {
        return Err(anyhow::anyhow!(
            "Failed to configure passwordless sudo. Check that your user has sudo privileges."
        ));
    }
    println!(
        "  {} sudo escalation configured (will be removed at end)",
        "✓".green()
    );
    println!();
    Ok(())
}

/// Write `content` to `path` on the remote: a quoted heredoc as root, or
/// `printf | sudo tee` otherwise. `printf '%s\n'` rather than `echo`
/// because a POSIX `echo` (dash) expands backslash escapes, which would
/// corrupt JSON containing escaped characters.
pub(super) fn write_remote_file(
    args: &BootstrapArgs,
    path: &str,
    content: &str,
    heredoc_tag: &str,
) -> Result<()> {
    let cmd = if args.is_root() {
        format!(
            "cat > {} << '{}'\n{}\n{}",
            path, heredoc_tag, content, heredoc_tag
        )
    } else {
        format!(
            r"printf '%s\n' '{}' | {}tee {} > /dev/null",
            content.replace('\'', "'\\''"),
            args.sudo(),
            path
        )
    };
    run_ssh_command(args, &cmd)?;
    Ok(())
}

pub(super) fn control_path(host: &str, port: u16) -> String {
    format!("/tmp/paygress-ssh-{}-{}", host, port)
}

fn base_ssh_args(args: &BootstrapArgs) -> Vec<String> {
    let mut v = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path(&args.host, args.port)),
        "-o".to_string(),
        "ControlPersist=10m".to_string(),
        "-p".to_string(),
        args.port.to_string(),
    ];
    if let Some(ref key) = args.key {
        v.push("-i".to_string());
        v.push(key.clone());
    }
    v
}

/// Route the ssh argv through `sshpass` when a password was supplied,
/// so no step re-prompts. Returns (program, argv).
fn ssh_invocation(args: &BootstrapArgs, ssh_args: Vec<String>) -> (String, Vec<String>) {
    match args.password {
        Some(ref password) => {
            let mut v = vec!["-p".to_string(), password.clone(), "ssh".to_string()];
            v.extend(ssh_args);
            ("sshpass".to_string(), v)
        }
        None => ("ssh".to_string(), ssh_args),
    }
}

fn missing_program_hint(program: &str) -> &'static str {
    if program == "sshpass" {
        "Is sshpass installed? (apt-get install sshpass / brew install sshpass)"
    } else {
        ""
    }
}

/// Open a persistent ControlMaster connection (authenticates once).
fn open_ssh_master(args: &BootstrapArgs) -> Result<()> {
    let cp = control_path(&args.host, args.port);
    if std::path::Path::new(&cp).exists() {
        return Ok(());
    }
    let mut ssh_args = base_ssh_args(args);
    ssh_args.extend([
        "-o".to_string(),
        "ControlMaster=yes".to_string(),
        "-N".to_string(), // no command — just keep the connection open
        "-f".to_string(), // background immediately after auth
        format!("{}@{}", args.user, args.host),
    ]);
    let (program, final_args) = ssh_invocation(args, ssh_args);

    let status = Command::new(&program)
        .args(&final_args)
        .status()
        .with_context(|| {
            format!(
                "Failed to open SSH master connection. {}",
                missing_program_hint(&program)
            )
        })?;
    if !status.success() {
        return Err(anyhow::anyhow!("SSH master connection failed"));
    }
    Ok(())
}

pub(super) fn close_ssh_master(args: &BootstrapArgs) {
    let cp = control_path(&args.host, args.port);
    let _ = Command::new("ssh")
        .args([
            "-o",
            &format!("ControlPath={}", cp),
            "-O",
            "exit",
            &format!("{}@{}", args.user, args.host),
        ])
        .output();
}

pub(super) fn run_ssh_command(args: &BootstrapArgs, cmd: &str) -> Result<bool> {
    let mut ssh_args = base_ssh_args(args);
    ssh_args.push("-t".to_string()); // allocate PTY for interactive commands
    ssh_args.push(format!("{}@{}", args.user, args.host));
    ssh_args.push(cmd.to_string());
    let (program, final_args) = ssh_invocation(args, ssh_args);

    let status = Command::new(&program)
        .args(&final_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| {
            format!(
                "Failed to execute {} command. {}",
                program,
                missing_program_hint(&program)
            )
        })?;

    Ok(status.success())
}

pub(super) fn run_ssh_command_output(args: &BootstrapArgs, cmd: &str) -> Result<String> {
    let mut ssh_args = base_ssh_args(args);
    ssh_args.push(format!("{}@{}", args.user, args.host));
    ssh_args.push(cmd.to_string());
    let (program, final_args) = ssh_invocation(args, ssh_args);

    let output = Command::new(&program)
        .args(&final_args)
        .output()
        .with_context(|| {
            format!(
                "Failed to execute {} command. {}",
                program,
                missing_program_hint(&program)
            )
        })?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
