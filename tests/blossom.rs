//! Exercises the Blossom client against a `wiremock` stub, including the auth
//! header real servers verify. Crypto round-trips live in `src/blossom_crypto.rs`.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr_sdk::Keys;
use wiremock::matchers::{header, header_exists, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use paygress::blossom::{BlossomClient, BlossomOp};
use paygress::blossom_crypto::{
    decrypt_after_download, encrypt_for_upload, sha256_hex, EncryptionKey,
};

fn key() -> EncryptionKey {
    [0xab; 32]
}

async fn stub_server() -> (MockServer, BlossomClient) {
    let server = MockServer::start().await;
    let client = BlossomClient::new(server.uri(), Keys::generate());
    (server, client)
}

#[tokio::test]
async fn auth_header_carries_required_tags_and_signature() {
    let (_server, client) = stub_server().await;

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
    let (server, client) = stub_server().await;

    // The server must never see plaintext.
    let plaintext = b"a checkpoint blob worth protecting".to_vec();
    let ciphertext = encrypt_for_upload(&plaintext, &key()).expect("encrypt");
    let expected_hash = sha256_hex(&ciphertext);

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
    let (server, client) = stub_server().await;

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
async fn check_upload_sends_signed_bud06_headers() {
    let (server, client) = stub_server().await;

    let hash = "a".repeat(64);
    Mock::given(method("HEAD"))
        .and(path("/upload"))
        .and(header_exists("authorization"))
        .and(header("x-sha-256", hash.as_str()))
        .and(header("x-content-length", "191889408"))
        .and(header("x-content-type", "application/octet-stream"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let check = client
        .check_upload(&hash, 191_889_408, Some("application/octet-stream"))
        .await
        .expect("probe completes");
    assert!(check.accepted());
    assert!(!check.unsupported());
}

#[tokio::test]
async fn check_upload_reports_refusal_with_reason_instead_of_erroring() {
    let (server, client) = stub_server().await;

    Mock::given(method("HEAD"))
        .and(path("/upload"))
        .respond_with(
            ResponseTemplate::new(413).insert_header("X-Reason", "File too large, max 100MB"),
        )
        .mount(&server)
        .await;

    let check = client
        .check_upload(&"b".repeat(64), 191_889_408, None)
        .await
        .expect("a refusal is an answer, not a transport error");
    assert_eq!(check.status, 413);
    assert!(!check.accepted());
    assert_eq!(check.reason.as_deref(), Some("File too large, max 100MB"));
}

#[tokio::test]
async fn check_upload_flags_servers_without_bud06_as_unsupported() {
    let (server, client) = stub_server().await;

    Mock::given(method("HEAD"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let check = client
        .check_upload(&"c".repeat(64), 1024, None)
        .await
        .expect("probe completes");
    assert!(check.unsupported(), "404 must not read as a size refusal");
    assert!(!check.accepted());
}

#[tokio::test]
async fn delete_uses_auth_and_targets_hash_path() {
    let (server, client) = stub_server().await;

    let hash = "0".repeat(64);

    Mock::given(method("DELETE"))
        .and(path(format!("/{}", hash)))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    client.delete(&hash).await.expect("delete succeeds");
}
