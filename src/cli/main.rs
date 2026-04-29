// Paygress CLI - Command Line Interface

use clap::{Parser, Subcommand};
use colored::Colorize;

mod api;
mod commands;

use commands::{batch, bootstrap, deploy, list, provider, spawn, status, system, topup};

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
    // ============ Consumer Commands ============
    /// Discover providers and their offers
    List(list::ListArgs),

    /// Spawn a new workload with Cashu payment
    Spawn(spawn::SpawnArgs),

    /// Deploy an opinionated template (Unit 9)
    Deploy(deploy::DeployArgs),

    /// Top up an existing workload with additional payment
    Topup(topup::TopupArgs),

    /// Get status of a workload
    Status(status::StatusArgs),

    /// Fan-out N workloads in parallel for map-reduce shards, CI
    /// matrices, and embarrassingly-parallel batch jobs.
    Batch(batch::BatchArgs),

    // ============ Provider Commands ============
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    if cli.verbose {
        print_banner();
    }

    let result = match cli.command {
        // Consumer
        Commands::List(args) => list::execute(args, cli.verbose).await,
        Commands::Spawn(args) => spawn::execute(args, cli.verbose).await,
        Commands::Deploy(args) => deploy::execute(args, cli.verbose).await,
        Commands::Topup(args) => topup::execute(args, cli.verbose).await,
        Commands::Status(args) => status::execute(args, cli.verbose).await,
        Commands::Batch(args) => batch::execute(args, cli.verbose).await,

        // Provider
        Commands::Provider(args) => provider::execute(args, cli.verbose).await,
        Commands::Bootstrap(args) => bootstrap::execute(args, cli.verbose).await,
        Commands::System(args) => system::execute(args, cli.verbose).await,
    };

    if let Err(e) = result {
        eprintln!("{} {}", "Error:".red().bold(), e);
        std::process::exit(1);
    }
}
