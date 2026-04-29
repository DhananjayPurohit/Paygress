//! Tests for the `paygress deploy` CLI (Unit 9 of the 12-month
//! plan, docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//!
//! These tests live as integration tests because `deploy` is a CLI
//! binary feature; they exercise the public surface of the binary
//! crate by invoking the built `paygress-cli` with arguments.
//!
//! Scope: argument validation and help output. End-to-end tests
//! that actually spawn against a provider are out of scope here
//! (they require a real provider + mint and belong in the manual
//! Success Criterion 1 demo).

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
    // Templates render in kebab-case via the ValueEnum derive.
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

    // clap's value_parser fails the parse, so exit is non-zero and
    // we never get to the Nostr send step.
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
    // Use a known-valid synthetic Cashu token (V3 wire format) so
    // we get past `value_parser` and into the auto-selection check.
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
