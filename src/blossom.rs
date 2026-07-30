// Blossom client — the BUD-01/02/04 subset needed for warm-standby
// checkpoint storage: PUT /upload, GET /<sha256>, DELETE /<sha256>,
// plus the BUD-06 HEAD /upload pre-flight.
//
// Auth is a NIP-98-style kind-24242 Nostr event, base64-JSON in
// `Authorization: Nostr <b64>`, tagged with the operation (`t`), the
// post-encryption content hash (`x`) and an `expiration`.
//
// Callers encrypt/decrypt via `crate::blossom_crypto`, so the server
// only ever sees ciphertext.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr_sdk::{EventBuilder, Keys, Kind, Tag, Timestamp};
use reqwest::Client as HttpClient;
use serde::Deserialize;

const AUTH_KIND: u16 = 24242;
const DEFAULT_AUTH_TTL_SECS: u64 = 60;

/// Named in the auth event's `t` tag.
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

pub struct BlossomClient {
    http: HttpClient,
    server: String,
    keys: Keys,
    auth_ttl_secs: u64,
}

/// `PUT /upload` response (BUD-02 §3).
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

/// `HEAD /upload` outcome (BUD-06). A refusal is an answer, not an error, so
/// the status travels back intact instead of being flattened into `Err`.
#[derive(Debug, Clone)]
pub struct UploadCheck {
    pub status: u16,
    /// `X-Reason`, the server's human-readable explanation when it says no.
    pub reason: Option<String>,
}

impl UploadCheck {
    pub fn accepted(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Servers that never implemented BUD-06 answer the probe itself, not the
    /// question — treating that as a refusal would rule out a usable server.
    pub fn unsupported(&self) -> bool {
        matches!(self.status, 404 | 405 | 501)
    }
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

    pub fn with_auth_ttl(mut self, secs: u64) -> Self {
        self.auth_ttl_secs = secs;
        self
    }

    /// Upload already-encrypted ciphertext; the response's `sha256` is the
    /// checkpoint's content address.
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

    /// Ask whether the server would accept a blob of this size and hash before
    /// spending the bandwidth to find out (BUD-06). The authorization event is
    /// the same one `put` would send — an unsigned probe is rejected with 401
    /// before the size is ever looked at.
    pub async fn check_upload(
        &self,
        sha256: &str,
        size: u64,
        mime_type: Option<&str>,
    ) -> Result<UploadCheck> {
        let auth = self.build_auth_header(BlossomOp::Upload, sha256).await?;
        let url = format!("{}/upload", self.server);

        let mut req = self
            .http
            .head(&url)
            .header("Authorization", auth)
            .header("X-SHA-256", sha256)
            .header("X-Content-Length", size.to_string());
        if let Some(mime) = mime_type {
            req = req.header("X-Content-Type", mime);
        }

        let resp = req.send().await.with_context(|| format!("HEAD {}", url))?;
        let reason = resp
            .headers()
            .get("X-Reason")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        Ok(UploadCheck {
            status: resp.status().as_u16(),
            reason,
        })
    }

    /// Fetch by hash. Bytes are still encrypted.
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

    pub async fn build_auth_header(&self, op: BlossomOp, x_hash: &str) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let expiration = now + self.auth_ttl_secs;

        let exp_str = expiration.to_string();
        let tags = vec![
            Tag::parse(["t", op.tag_value()])?,
            Tag::parse(["x", x_hash])?,
            Tag::parse(["expiration", exp_str.as_str()])?,
        ];

        let event = EventBuilder::new(Kind::Custom(AUTH_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(now))
            .sign_with_keys(&self.keys)?;

        let json = serde_json::to_string(&event)?;
        Ok(format!("Nostr {}", BASE64.encode(json.as_bytes())))
    }
}
