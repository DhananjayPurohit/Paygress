// `paygress-cli wallet` — local token utilities.
//
// `wallet mint` exists to fund unattended flows (CI runner spawns,
// scripted demos) against a testnut-style mint whose fake Lightning
// backend auto-pays quotes. It prints exactly one thing to stdout —
// the serialized token — so callers can compose it:
//
//   paygress-cli ci runner --token "$(paygress-cli wallet mint \
//     --mint https://testnut.cashu.space --amount 1000)" ...
//
// All progress/diagnostics go to stderr to keep stdout clean.

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
    /// Mint a fresh Cashu token from a mint. Only works unattended
    /// against testnut-style mints that auto-pay quotes; real mints
    /// need their bolt11 invoice paid out-of-band and will time out.
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
    // Same ephemeral-wallet convention as `batch --split-token`:
    // unique temp filename so concurrent invocations don't collide,
    // best-effort removal regardless of outcome.
    let mut db_path = std::env::temp_dir();
    db_path.push(format!(
        "paygress-wallet-mint-{}.redb",
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
    // The token is the entire stdout contract — nothing else.
    println!("{}", token);
    Ok(())
}
