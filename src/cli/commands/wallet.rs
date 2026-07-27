// `wallet mint` prints the serialized token and nothing else to stdout, so
// callers can compose it (`--token "$(paygress-cli wallet mint ...)"`);
// progress goes to stderr.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct WalletArgs {
    #[command(subcommand)]
    pub action: WalletAction,
}

#[derive(Subcommand)]
pub enum WalletAction {
    /// Mint a fresh Cashu token (testnut-style auto-paying mints only)
    Mint(MintArgs),
}

#[derive(Args)]
pub struct MintArgs {
    /// Mint URL (e.g. https://testnut.cashu.space).
    #[arg(long)]
    pub mint: String,

    /// Token face value in sats.
    #[arg(long)]
    pub amount: u64,
}

pub async fn execute(args: WalletArgs) -> Result<()> {
    match args.action {
        WalletAction::Mint(a) => run_mint(a).await,
    }
}

async fn run_mint(args: MintArgs) -> Result<()> {
    // Unique temp wallet so concurrent invocations don't collide.
    let mut db_path = std::env::temp_dir();
    db_path.push(format!(
        "paygress-wallet-mint-{}.sqlite",
        uuid::Uuid::new_v4()
    ));

    eprintln!(
        "{} minting {} sats from {}",
        "→".blue(),
        args.amount,
        args.mint
    );

    let result = paygress::cashu::mint_fresh_token(&args.mint, args.amount, &db_path)
        .await
        .context("minting failed");
    let _ = std::fs::remove_file(&db_path);

    let token = result?;
    eprintln!("{} minted", "✓".green());
    // The token is the entire stdout contract.
    println!("{}", token);
    Ok(())
}
