// Blossom client (Unit 6 of the 12-month plan,
// docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//
// In-tree implementation of the BUD-01 / BUD-02 / BUD-04 subset
// Paygress needs for warm-standby checkpoint storage:
//   - PUT /upload  (BUD-02): upload a blob; server returns
//     `{ url, sha256, size, type, uploaded }`.
//   - GET /<sha256> (BUD-01): fetch by hash.
//   - DELETE /<sha256> (BUD-04): remove a blob (auth required).
//
// Auth: NIP-98-style Nostr event of kind 24242, base64-encoded
// JSON in `Authorization: Nostr <b64>`. Tags:
//   - ["t", "upload"|"delete"|"get"] — operation.
//   - ["x", "<sha256>"] — content hash (post-encryption).
//   - ["expiration", "<unix_ts>"] — short-lived (60s default).
//
// Encryption is client-side and orthogonal: callers encrypt before
// `put` and decrypt after `get`, using `crate::blossom_crypto`.
// The Blossom server only ever sees ciphertext.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr_sdk::{EventBuilder, Keys, Kind, Tag, Timestamp};
use reqwest::Client as HttpClient;
use serde::Deserialize;

const AUTH_KIND: u16 = 24242;
const DEFAULT_AUTH_TTL_SECS: u64 = 60;

/// Operation tagged in the auth event's `t` tag. Mirrors the
/// Blossom spec wording.
#[derive(Debug, Clone, Copy)]
pub enum BlossomOp {
    Upload,
    Get,
    Delete,
}

impl BlossomOp {
    fn tag_value(self) -> &'static str {
        match self {
            BlossomOp::Upload => "upload",
            BlossomOp::Get => "get",
            BlossomOp::Delete => "delete",
        }
    }
}

/// Minimal client. Holds a long-lived `reqwest::Client`, the target
/// Blossom server URL, and the Nostr `Keys` used to sign auth
/// events. One client per (server, identity) pair.
pub struct BlossomClient {
    http: HttpClient,
    server: String,
    keys: Keys,
    auth_ttl_secs: u64,
}

/// Response shape from `PUT /upload` (BUD-02 §3).
#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    #[serde(rename = "type", default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub uploaded: u64,
}

impl BlossomClient {
    pub fn new(server: impl Into<String>, keys: Keys) -> Self {
        Self {
            http: HttpClient::new(),
            server: server.into().trim_end_matches('/').to_string(),
            keys,
            auth_ttl_secs: DEFAULT_AUTH_TTL_SECS,
        }
    }

    /// Override the auth-event TTL. Most callers don't need this.
    pub fn with_auth_ttl(mut self, secs: u64) -> Self {
        self.auth_ttl_secs = secs;
        self
    }

    /// Upload `bytes` (already-encrypted ciphertext). Returns the
    /// server's response; the `sha256` field is what callers should
    /// persist as the checkpoint's content address.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<UploadResponse> {
        let hash = crate::blossom_crypto::sha256_hex(&bytes);
        let auth = self.build_auth_header(BlossomOp::Upload, &hash).await?;

        let url = format!("{}/upload", self.server);
        let resp = self
            .http
            .put(&url)
            .header("Authorization", auth)
            .body(bytes)
            .send()
            .await
            .with_context(|| format!("PUT {}", url))?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "Blossom upload returned {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }

        let parsed: UploadResponse = resp.json().await.context("parse Blossom upload response")?;
        Ok(parsed)
    }

    /// Fetch by hash. Returns the wire-format bytes (still
    /// encrypted — caller decrypts via `blossom_crypto`).
    pub async fn get(&self, sha256: &str) -> Result<Vec<u8>> {
        let url = format!("{}/{}", self.server, sha256);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {}", url))?;
        if !resp.status().is_success() {
            anyhow::bail!("Blossom fetch returned {}", resp.status());
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Delete by hash. Auth-required.
    pub async fn delete(&self, sha256: &str) -> Result<()> {
        let auth = self.build_auth_header(BlossomOp::Delete, sha256).await?;
        let url = format!("{}/{}", self.server, sha256);
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", auth)
            .send()
            .await
            .with_context(|| format!("DELETE {}", url))?;
        if !resp.status().is_success() {
            anyhow::bail!("Blossom delete returned {}", resp.status());
        }
        Ok(())
    }

    /// Build the `Authorization: Nostr <base64>` header value. Pure
    /// (no I/O), so unit-testable.
    pub async fn build_auth_header(&self, op: BlossomOp, x_hash: &str) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let expiration = now + self.auth_ttl_secs;

        let tags = vec![
            Tag::parse(&["t", op.tag_value()])?,
            Tag::parse(&["x", x_hash])?,
            Tag::parse(&["expiration", &expiration.to_string()])?,
        ];

        let event = EventBuilder::new(Kind::Custom(AUTH_KIND), "", tags)
            .custom_created_at(Timestamp::from(now))
            .to_event(&self.keys)?;

        let json = serde_json::to_string(&event)?;
        Ok(format!("Nostr {}", BASE64.encode(json.as_bytes())))
    }
}
