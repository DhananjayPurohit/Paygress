//! Fixtures shared by the integration tests. Each test binary is its own crate,
//! so this is pulled in with `mod common;` rather than imported from the lib.
#![allow(dead_code)]

/// Synthetic Cashu V3 token. Proof signatures are dummy hex: nothing verifies
/// them locally before `Wallet::receive` would reach the mint, and no test that
/// uses this gets that far.
pub fn synthetic_cashu_token(mint_url: &str, amount_sat: u64, secret: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let body = serde_json::json!({
        "token": [{
            "mint": mint_url,
            "proofs": [{
                "amount": amount_sat,
                "secret": secret,
                "C": "023be53e8c60530eea9b3943fda1a2ce71c7b3f0cf0dc6d846fa765aaf779fa81d",
                "id": "009a1f293253e41e",
            }],
        }],
        "unit": "sat",
    });

    let json = serde_json::to_string(&body).expect("synthetic token body");
    format!("cashuA{}", URL_SAFE_NO_PAD.encode(json.as_bytes()))
}
