//! Integration tests for the Blossom client (Unit 6 of the
//! 12-month plan,
//! docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//!
//! Crypto round-trip tests live as inline `#[cfg(test)] mod tests`
//! in `src/blossom_crypto.rs` (no I/O needed). This file uses
//! `wiremock` to stub the Blossom HTTP surface and exercise the
//! client end-to-end against a local server, including the auth
//! header generation that real Blossom servers verify.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr_sdk::Keys;
use wiremock::matchers::{header_exists, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use paygress::blossom::{BlossomClient, BlossomOp};
use paygress::blossom_crypto::{
    decrypt_after_download, encrypt_for_upload, sha256_hex, EncryptionKey,
};

fn key() -> EncryptionKey {
    [0xab; 32]
}

#[tokio::test]
async fn auth_header_carries_required_tags_and_signature() {
    let server = MockServer::start().await;
    let keys = Keys::generate();
    let client = BlossomClient::new(server.uri(), keys);

    let header = client
        .build_auth_header(BlossomOp::Upload, "abc123")
        .await
        .expect("auth header builds");

    let prefix = "Nostr ";
    assert!(header.starts_with(prefix));
    let json_bytes = BASE64
        .decode(&header[prefix.len()..])
        .expect("base64 decodes");
    let event: serde_json::Value = serde_json::from_slice(&json_bytes).expect("auth body is JSON");
    assert_eq!(event["kind"], 24242);

    let tags = event["tags"].as_array().unwrap();
    let mut saw_t = false;
    let mut saw_x = false;
    let mut saw_exp = false;
    for tag in tags {
        let arr = tag.as_array().unwrap();
        match arr[0].as_str() {
            Some("t") => {
                assert_eq!(arr[1], "upload");
                saw_t = true;
            }
            Some("x") => {
                assert_eq!(arr[1], "abc123");
                saw_x = true;
            }
            Some("expiration") => saw_exp = true,
            _ => {}
        }
    }
    assert!(
        saw_t && saw_x && saw_exp,
        "auth event missing required tags"
    );
    assert!(event["sig"].as_str().is_some(), "auth event must be signed");
}

#[tokio::test]
async fn put_then_get_round_trips_through_blossom_stub() {
    let server = MockServer::start().await;
    let keys = Keys::generate();
    let client = BlossomClient::new(server.uri(), keys);

    // Encrypt a payload before uploading; the server should never
    // see plaintext.
    let plaintext = b"a checkpoint blob worth protecting".to_vec();
    let ciphertext = encrypt_for_upload(&plaintext, &key()).expect("encrypt");
    let expected_hash = sha256_hex(&ciphertext);

    // Stub /upload: requires Authorization header, returns the
    // server's response shape.
    let upload_response = serde_json::json!({
        "url": format!("{}/{}", server.uri(), expected_hash),
        "sha256": expected_hash,
        "size": ciphertext.len(),
        "type": "application/octet-stream",
        "uploaded": 1700000000u64,
    });
    Mock::given(method("PUT"))
        .and(path("/upload"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upload_response.clone()))
        .mount(&server)
        .await;

    let resp = client
        .put(ciphertext.clone())
        .await
        .expect("upload succeeds");
    assert_eq!(resp.sha256, expected_hash);
    assert_eq!(resp.size, ciphertext.len() as u64);

    // Stub GET /<sha256>: returns the ciphertext bytes.
    let ciphertext_for_response = ciphertext.clone();
    Mock::given(method("GET"))
        .and(path_regex(r"^/[0-9a-f]{64}$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(ciphertext_for_response))
        .mount(&server)
        .await;

    let fetched = client.get(&expected_hash).await.expect("fetch succeeds");
    assert_eq!(fetched, ciphertext, "fetched bytes must equal upload");

    let decrypted = decrypt_after_download(&fetched, &key()).expect("decrypt");
    assert_eq!(decrypted, plaintext);
}

#[tokio::test]
async fn upload_5xx_is_surfaced_as_error() {
    let server = MockServer::start().await;
    let keys = Keys::generate();
    let client = BlossomClient::new(server.uri(), keys);

    Mock::given(method("PUT"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(503).set_body_string("backend down"))
        .mount(&server)
        .await;

    let err = client
        .put(b"some bytes".to_vec())
        .await
        .expect_err("503 must propagate");
    let msg = err.to_string();
    assert!(
        msg.contains("503") || msg.contains("backend down"),
        "error must surface server status, got: {}",
        msg
    );
}

#[tokio::test]
async fn delete_uses_auth_and_targets_hash_path() {
    let server = MockServer::start().await;
    let keys = Keys::generate();
    let client = BlossomClient::new(server.uri(), keys);

    let hash = "0".repeat(64);

    Mock::given(method("DELETE"))
        .and(path(format!("/{}", hash)))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    client.delete(&hash).await.expect("delete succeeds");
}
