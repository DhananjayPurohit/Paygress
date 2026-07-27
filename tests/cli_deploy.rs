//! Argument validation and help output for `paygress deploy`, driven
//! through the built `paygress-cli` binary. Spawning against a real
//! provider + mint is out of scope here.

use std::process::Command;

fn paygress_cli() -> Command {
    let exe = env!("CARGO_BIN_EXE_paygress-cli");
    Command::new(exe)
}

#[test]
fn deploy_help_lists_templates() {
    let out = paygress_cli()
        .args(["deploy", "--help"])
        .output()
        .expect("invoke paygress-cli deploy --help");
    assert!(out.status.success(), "deploy --help should exit 0");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("nostr-relay"),
        "deploy --help should mention nostr-relay; got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("inference-endpoint"),
        "deploy --help should mention inference-endpoint; got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("--replication"),
        "deploy --help should expose --replication; got:\n{}",
        stdout
    );
}

#[test]
fn deploy_rejects_malformed_cashu_token_before_network() {
    let out = paygress_cli()
        .args([
            "deploy",
            "nostr-relay",
            "--token",
            "not-a-cashu-token",
            "--provider",
            "npub1example",
        ])
        .output()
        .expect("invoke paygress-cli deploy with bad token");

    // clap's value_parser fails first, so we never reach the Nostr send.
    assert!(
        !out.status.success(),
        "malformed token must fail parsing; stdout: {:?} stderr: {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("cashu") || stderr.to_lowercase().contains("token"),
        "error must mention cashu/token, got: {}",
        stderr
    );
}

#[test]
fn deploy_requires_provider_until_observatory_lands() {
    // Valid synthetic V3 token, so we get past `value_parser` and into
    // the auto-selection check.
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let body = serde_json::json!({
        "token": [{
            "mint": "https://testnut.cashu.space",
            "proofs": [{
                "amount": 1,
                "secret": "deploy-test-secret",
                "C": "023be53e8c60530eea9b3943fda1a2ce71c7b3f0cf0dc6d846fa765aaf779fa81d",
                "id": "009a1f293253e41e",
            }],
        }],
        "unit": "sat",
    });
    let token = format!(
        "cashuA{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_string(&body).unwrap().as_bytes())
    );

    let out = paygress_cli()
        .args(["deploy", "nostr-relay", "--token", &token])
        .output()
        .expect("invoke paygress-cli deploy without --provider");

    assert!(
        !out.status.success(),
        "deploy without --provider must fail until observatory ships"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.contains("--provider") || combined.to_lowercase().contains("provider"),
        "error must mention --provider hint; got: {}",
        combined
    );
}
