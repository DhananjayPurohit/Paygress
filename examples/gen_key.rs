use nostr_sdk::{Keys, ToBech32};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let keys = Keys::generate();
    println!("NSEC: {}", keys.secret_key().to_bech32()?);
    println!("NPUB: {}", keys.public_key().to_bech32()?);
    Ok(())
}
