use clap::{Parser, Subcommand};
use colored::Colorize;

mod api;
mod commands;
mod exec_client;
mod util;

use commands::{
    batch, bootstrap, deploy, exec, list, mcp, provider, spawn, status, system, topup, wallet,
};

/// Paygress CLI - Pay-per-Use Compute with Lightning + Nostr
#[derive(Parser)]
#[command(name = "paygress-cli")]
#[command(author = "Dhananjay Purohit")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "CLI tool for Paygress - spawn compute with Lightning + Nostr", long_about = None)]
struct Cli {
    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Discover providers and their offers
    List(list::ListArgs),

    /// Spawn a new workload with Cashu payment
    Spawn(spawn::SpawnArgs),

    /// Deploy an opinionated template
    Deploy(deploy::DeployArgs),

    /// Top up an existing workload with additional payment
    Topup(topup::TopupArgs),

    /// Get status of a workload
    Status(status::StatusArgs),

    /// Fan out N workloads in parallel
    Batch(batch::BatchArgs),

    /// Run a Model Context Protocol (MCP) server over stdio
    Mcp(mcp::McpArgs),

    /// Run a shell command inside a spawned agent-sandbox workload
    Exec(exec::ExecArgs),

    /// Local token utilities
    Wallet(wallet::WalletArgs),

    /// Provider management - setup, start, stop, status
    Provider(provider::ProviderArgs),

    /// One-click bootstrap - set up a server as a Paygress provider
    Bootstrap(bootstrap::BootstrapArgs),

    /// System management - reset, clean up
    System(system::SystemArgs),
}

fn print_banner() {
    println!("{}", "PAYGRESS CLI".blue().bold());
    println!("{}", "Pay-per-Use Compute with Lightning + Nostr".blue());
    println!();
}

#[tokio::main]
async fn main() {
    // Default `error` hides relay-pool noise; RUST_LOG=info shows it.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    if cli.verbose {
        print_banner();
    }

    let result = match cli.command {
        Commands::List(args) => list::execute(args, cli.verbose).await,
        Commands::Spawn(args) => spawn::execute(args, cli.verbose).await,
        Commands::Deploy(args) => deploy::execute(args, cli.verbose).await,
        Commands::Topup(args) => topup::execute(args, cli.verbose).await,
        Commands::Status(args) => status::execute(args, cli.verbose).await,
        Commands::Batch(args) => batch::execute(args, cli.verbose).await,
        Commands::Mcp(args) => mcp::execute(args, cli.verbose).await,
        Commands::Exec(args) => exec::execute(args, cli.verbose).await,
        Commands::Wallet(args) => wallet::execute(args).await,
        Commands::Provider(args) => provider::execute(args, cli.verbose).await,
        Commands::Bootstrap(args) => bootstrap::execute(args, cli.verbose).await,
        Commands::System(args) => system::execute(args, cli.verbose).await,
    };

    if let Err(e) = result {
        eprintln!("{} {}", "Error:".red().bold(), e);
        std::process::exit(1);
    }
}
