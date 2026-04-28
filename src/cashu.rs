// Cashu Token Utilities
//
// Provides token amount extraction only
// Payment verification handled by ngx_l402 at nginx layer

use std::path::Path;
use std::sync::{Arc, OnceLock};

const MSAT_PER_SAT: u64 = 1000;

// Database singleton (kept for compatibility, may not be needed)
static CASHU_DB: OnceLock<Arc<cdk_redb::wallet::WalletRedbDatabase>> = OnceLock::new();

pub async fn initialize_cashu(db_path: &str) -> Result<(), String> {
    // Initialize database for compatibility
    match cdk_redb::wallet::WalletRedbDatabase::new(Path::new(db_path)) {
        Ok(db) => {
            tracing::debug!("Cashu database initialized at: {}", db_path);
            let _ = CASHU_DB.set(Arc::new(db));
            Ok(())
        }
        Err(e) => {
            let error = format!("Failed to create Cashu database: {:?}", e);
            tracing::error!("{}", error);
            Err(error)
        }
    }
}

// verify_cashu_token removed - ngx_l402 handles all payment verification
// Payment validation now happens at nginx layer before requests reach Paygress

/// Process a Cashu token and extract its total value in msats
pub async fn extract_token_value(token_str: &str) -> anyhow::Result<u64> {
    use std::str::FromStr;

    // Decode the token to get its value
    let token = cdk::nuts::Token::from_str(token_str)
        .map_err(|e| anyhow::anyhow!("Failed to decode Cashu token: {}", e))?;

    // Check if the token is valid
    if token.proofs().is_empty() {
        return Err(anyhow::anyhow!("Token has no proofs"));
    }

    // Calculate total token amount
    let total_amount: u64 = token
        .proofs()
        .iter()
        .map(|p| {
            let amt: u64 = p.amount.into();
            amt
        })
        .sum();

    // Unit handling
    let total_amount_msats: u64 = match token.unit().unwrap_or(cdk::nuts::CurrencyUnit::Sat) {
        cdk::nuts::CurrencyUnit::Sat => total_amount * MSAT_PER_SAT,
        cdk::nuts::CurrencyUnit::Msat => total_amount,
        unit => return Err(anyhow::anyhow!("Unsupported token unit: {:?}", unit)),
    };

    Ok(total_amount_msats)
}
