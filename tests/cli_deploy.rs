//! Argument validation and help output for `paygress deploy`, driven through the
//! built `paygress-cli` binary.

use std::process::Command;

mod common;

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
    // Valid token, so we get past `value_parser` and into the auto-selection
    // check.
    let token =
        common::synthetic_cashu_token("https://testnut.cashu.space", 1, "deploy-test-secret");

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
