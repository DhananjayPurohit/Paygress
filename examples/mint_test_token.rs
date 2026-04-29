// Test-only helper: mint a Cashu token from a test mint and print
// it to stdout. Used for end-to-end testing on the canonical
// Nostr-DM provider path against `https://testnut.cashu.space`
// (Nutshell FakeWallet — auto-pays bolt11 invoices, so anyone can
// mint test sats for free).
//
// Usage:
//     cargo run --release --example mint_test_token -- \
//         <mint_url> <amount_sats> [<wallet_db_path>]
//
// The wallet DB persists between runs; reuse the same path to
// accumulate proofs across mints. The printed token is a
// serializable Cashu V4 token suitable for `paygress-cli spawn -k`.

use std::env;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cdk::amount::SplitTarget;
use cdk::nuts::{CurrencyUnit, MintQuoteState};
use cdk::wallet::{SendOptions, Wallet};
use cdk::Amount;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let mint_url = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("https://testnut.cashu.space");
    let amount: u64 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let db_path = args
        .get(3)
        .map(|s| s.as_str())
        .unwrap_or("./test-cashu-wallet.redb");

    eprintln!(
        "minting {} sat from {} (db: {})",
        amount, mint_url, db_path
    );

    let db = cdk_redb::wallet::WalletRedbDatabase::new(Path::new(db_path))
        .context("open redb wallet database")?;
    // Deterministic seed for the test wallet so it survives restarts.
    let seed = [0u8; 32];
    let wallet = Wallet::new(mint_url, CurrencyUnit::Sat, Arc::new(db), &seed, None)
        .context("construct cdk Wallet")?;

    // Populate the local keyset cache so mint() / send() can pick
    // an active keyset. Without this we'd hit "No active keyset".
    let _ = wallet
        .get_active_mint_keysets()
        .await
        .context("fetch mint keysets")?;

    // 1. Request a mint quote (Lightning bolt11 invoice).
    let quote = wallet
        .mint_quote(Amount::from(amount), None)
        .await
        .context("request mint quote")?;
    eprintln!("got quote {}", quote.id);
    eprintln!("invoice: {}", quote.request);

    // 2. Wait for the mint to mark the quote paid. Testnut's
    //    FakeWallet auto-pays after a brief delay.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let state = wallet
            .mint_quote_state(&quote.id)
            .await
            .context("poll mint quote state")?;
        if state.state == MintQuoteState::Paid {
            eprintln!("quote paid");
            break;
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("mint quote not paid within 30s; use a real lightning wallet against a non-test mint");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 3. Mint the proofs.
    let proofs = wallet
        .mint(&quote.id, SplitTarget::default(), None)
        .await
        .context("mint proofs")?;
    let total: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum();
    eprintln!("minted {} proofs totaling {} sat", proofs.len(), total);

    // 4. Send the full balance back as a serialized token.
    let prepared = wallet
        .prepare_send(Amount::from(amount), SendOptions::default())
        .await
        .context("prepare_send")?;
    let token = wallet
        .send(prepared, None)
        .await
        .context("send")?;
    let token_str = token.to_string();
    println!("{}", token_str);
    eprintln!("token length: {} chars", token_str.len());

    // Sanity check: parse our own output.
    let parsed = cdk::nuts::Token::from_str(&token_str).context("re-parse our own token")?;
    let parsed_total: u64 = parsed.proofs().iter().map(|p| u64::from(p.amount)).sum();
    assert_eq!(parsed_total, amount, "token amount mismatch");
    Ok(())
}
