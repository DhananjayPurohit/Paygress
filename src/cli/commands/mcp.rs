// MCP (Model Context Protocol) server.
//
// Exposes the Paygress CLI's consumer commands (list, spawn, batch,
// status, topup) as tools that Claude Desktop / Claude Code / Cline /
// Cursor / any MCP client can call directly. Stdio transport so the
// host plugs us in via:
//
//     {
//       "mcpServers": {
//         "paygress": {
//           "command": "paygress-cli",
//           "args": ["mcp"]
//         }
//       }
//     }
//
// The server is intentionally a thin wrapper: each tool calls into
// the same helpers the regular CLI subcommands use, so behavior stays
// identical regardless of how the user invokes it.
//
// Tracing is routed to stderr by `cli/main.rs` (line ~68) so MCP's
// stdio transport on stdout stays uncluttered.

use std::future::Future;

use anyhow::Result;
use clap::Args;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, Error as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::batch;
use super::identity::{get_or_create_identity, parse_relays};
use super::spawn::{nostr_spawn_round_trip, NostrSpawnOutcome};
use super::status::{nostr_status_round_trip, NostrStatusOutcome};
use super::topup::{nostr_topup_round_trip, NostrTopupOutcome};
use paygress::discovery::DiscoveryClient;
use paygress::nostr::ProviderFilter;

#[derive(Args, Default, Debug, Clone)]
pub struct McpArgs {
    /// Override the Nostr private key used when calling Paygress
    /// providers. Falls back to ~/.paygress/identity if unset.
    /// Most users won't set this — Claude Desktop / Cursor pass it
    /// through environment variables if they need to override.
    #[arg(long)]
    pub nostr_key: Option<String>,

    /// Custom Nostr relays (comma-separated). Falls back to the
    /// CLI's default relay list.
    #[arg(long)]
    pub relays: Option<String>,
}

pub async fn execute(args: McpArgs, _verbose: bool) -> Result<()> {
    let server = PaygressMcpServer::new(args);
    let (stdin, stdout) = rmcp::transport::stdio();
    let running = server.serve((stdin, stdout)).await?;
    running.waiting().await?;
    Ok(())
}

/// The MCP server. Holds the rmcp tool router plus the per-process
/// defaults (nostr key + relays) so tools don't have to plumb them
/// from every call.
#[derive(Clone)]
pub struct PaygressMcpServer {
    nostr_key: Option<String>,
    relays_override: Option<String>,
    tool_router: ToolRouter<Self>,
}

// ---- Tool parameter types ----
//
// Each tool's parameters are a JsonSchema-derivable struct so rmcp
// can publish a schema to the client. Keep these stable — once a
// client has cached our tool schema, breaking changes here mean
// silent tool-call failures from older harnesses.

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListProvidersParams {
    /// Optional capability filter (e.g. "lxc", "vm"). When unset,
    /// returns every advertised provider.
    #[serde(default)]
    pub capability: Option<String>,
    /// Optional minimum advertised uptime percentage (0.0-100.0).
    #[serde(default)]
    pub min_uptime: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SpawnParams {
    /// Provider npub to spawn against.
    pub provider: String,
    /// Cashu token paying for this spawn.
    pub token: String,
    /// Tier id from the provider's offer (e.g. "basic").
    #[serde(default = "default_tier")]
    pub tier: String,
    /// Template slug (e.g. "agent-sandbox", "inference-endpoint").
    /// Empty string means a generic spawn governed by `image`.
    #[serde(default)]
    pub template: Option<String>,
    /// Container image used only when `template` is unset.
    #[serde(default = "default_image")]
    pub image: String,
    /// SSH username (default "user"). Some templates ignore this.
    #[serde(default = "default_ssh_user")]
    pub ssh_user: String,
    /// SSH password. Auto-generated if omitted.
    #[serde(default)]
    pub ssh_pass: Option<String>,
    /// Per-spawn timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_tier() -> String {
    "basic".to_string()
}
fn default_image() -> String {
    "ubuntu:22.04".to_string()
}
fn default_ssh_user() -> String {
    "user".to_string()
}
fn default_timeout_secs() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BatchParams {
    /// Provider npub to spawn against.
    pub provider: String,
    /// Either a list of N pre-minted Cashu tokens, OR a single token
    /// to split into `shards` shards. Mutually exclusive: set
    /// exactly one.
    #[serde(default)]
    pub tokens: Option<Vec<String>>,
    /// One large Cashu token to split into `shards` shards.
    #[serde(default)]
    pub split_token: Option<String>,
    /// Number of shards (required when `split_token` is set).
    #[serde(default)]
    pub shards: Option<usize>,
    /// Tier id from the provider's offer (default "basic").
    #[serde(default = "default_tier")]
    pub tier: String,
    /// Template slug (default "agent-sandbox").
    #[serde(default = "default_batch_template")]
    pub template: String,
    /// Per-shard timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_batch_template() -> String {
    "agent-sandbox".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatusParams {
    /// Provider npub that owns the workload.
    pub provider: String,
    /// Pod identifier from the spawn response (e.g.
    /// "container-1042").
    pub pod_id: String,
    /// Per-call timeout in seconds.
    #[serde(default = "default_status_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_status_timeout_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TopupParams {
    /// Provider npub that owns the workload.
    pub provider: String,
    /// Pod identifier from the spawn response.
    pub pod_id: String,
    /// Cashu token paying for the lease extension. The provider
    /// applies `redeemed_amount / rate_msats_per_sec` seconds of
    /// extension on success.
    pub token: String,
    /// Per-call timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

#[tool_router]
impl PaygressMcpServer {
    pub fn new(args: McpArgs) -> Self {
        Self {
            nostr_key: args.nostr_key,
            relays_override: args.relays,
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve the effective Nostr key. Falls back to
    /// `~/.paygress/identity`. Surfaced as a Result so we can map
    /// identity-load errors into McpError cleanly.
    fn resolve_nostr_key(&self) -> Result<String, McpError> {
        get_or_create_identity(self.nostr_key.clone()).map_err(|e| {
            McpError::internal_error(format!("failed to load Nostr identity: {}", e), None)
        })
    }

    fn resolve_relays(&self) -> Vec<String> {
        parse_relays(self.relays_override.clone())
    }

    #[tool(
        description = "List Paygress providers currently advertising on the Nostr network. Returns a JSON array of providers with their capabilities, advertised tiers, prices in msats/sec, and last heartbeat timestamps."
    )]
    async fn list_providers(
        &self,
        Parameters(params): Parameters<ListProvidersParams>,
    ) -> Result<CallToolResult, McpError> {
        let relays = self.resolve_relays();
        let nostr_key = self.resolve_nostr_key()?;

        let client = DiscoveryClient::new_with_key(relays, nostr_key)
            .await
            .map_err(|e| McpError::internal_error(format!("nostr connect: {}", e), None))?;

        // ProviderFilter applies server-side via DiscoveryClient. We
        // don't filter on `is_online` here so the model can decide
        // what to do with offline providers (e.g. wait + retry).
        let filter = if params.capability.is_some() || params.min_uptime.is_some() {
            Some(ProviderFilter {
                capability: params.capability.clone(),
                min_uptime: params.min_uptime,
                min_memory_mb: None,
                min_cpu: None,
            })
        } else {
            None
        };
        let providers = client
            .list_providers(filter)
            .await
            .map_err(|e| McpError::internal_error(format!("discovery: {}", e), None))?;

        let json = serde_json::to_string_pretty(&providers)
            .map_err(|e| McpError::internal_error(format!("serialize: {}", e), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Spawn a single Paygress workload. Pays the provider with the supplied Cashu token, optionally materializes from a template (e.g. `agent-sandbox`, `inference-endpoint`), and returns the workload's access details (host, ssh credentials, expiry, template ports). The agent then connects to the host:port itself to do the actual work."
    )]
    async fn spawn_workload(
        &self,
        Parameters(params): Parameters<SpawnParams>,
    ) -> Result<CallToolResult, McpError> {
        let relays = self.resolve_relays();
        let nostr_key = self.resolve_nostr_key()?;
        let ssh_pass = params
            .ssh_pass
            .clone()
            .unwrap_or_else(|| generate_password(16));

        let outcome = nostr_spawn_round_trip(
            &params.provider,
            &params.tier,
            &params.token,
            params.image.clone(),
            params.ssh_user.clone(),
            ssh_pass.clone(),
            params.template.clone(),
            relays,
            nostr_key,
            params.timeout_secs,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("spawn: {}", e), None))?;

        // Always reply with structured JSON so the caller can act on
        // either branch (success vs error) without parsing prose.
        let body = match outcome {
            NostrSpawnOutcome::Success(access) => serde_json::json!({
                "status": "spawned",
                "ssh_user": params.ssh_user,
                "ssh_pass": ssh_pass,
                "access": access,
            }),
            NostrSpawnOutcome::ProviderOffline => serde_json::json!({
                "status": "offline",
                "message": "provider's heartbeat did not appear within the live window",
            }),
            NostrSpawnOutcome::ProviderError(err) => serde_json::json!({
                "status": "provider_error",
                "error_type": err.error_type,
                "message": err.message,
                "details": err.details,
            }),
            NostrSpawnOutcome::UnknownResponse(content) => serde_json::json!({
                "status": "unknown_response",
                "content": content,
            }),
            NostrSpawnOutcome::Timeout => serde_json::json!({
                "status": "timeout",
                "message": "no response within timeout; token may have been spent",
            }),
        };
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()),
        )]))
    }

    #[tool(
        description = "Fan out N Paygress workloads in parallel for map-reduce shards, CI matrices, or batch jobs. Either pass `tokens` (a list of N pre-minted Cashu tokens) or `split_token` + `shards` (one big token split into N shards before fan-out). Returns the same shard manifest as `paygress-cli batch` — index, status, host, ssh credentials, expiry, error per shard."
    )]
    async fn batch_spawn(
        &self,
        Parameters(params): Parameters<BatchParams>,
    ) -> Result<CallToolResult, McpError> {
        // Use the same materialize_tokens logic the CLI uses, so the
        // split path behaves identically. We synthesize a BatchArgs
        // because that's what materialize_tokens consumes.
        let cli_args = batch::BatchArgs {
            provider: params.provider.clone(),
            tokens: params.tokens.as_ref().map(|v| v.join(",")),
            tokens_file: None,
            split_token: params.split_token.clone(),
            shards: params.shards,
            tier: params.tier.clone(),
            template: params.template.clone(),
            output: std::path::PathBuf::from("/tmp/paygress-mcp-batch-unused"),
            timeout_secs: params.timeout_secs,
            image: default_image(),
            nostr_key: self.nostr_key.clone(),
            relays: self.relays_override.clone(),
        };
        let tokens = batch::materialize_tokens(&cli_args)
            .await
            .map_err(|e| McpError::invalid_params(format!("token resolution: {}", e), None))?;
        let n = tokens.len();

        let relays = self.resolve_relays();
        let nostr_key = self.resolve_nostr_key()?;

        // Concurrent spawn fan-out (same shape as batch::execute, but
        // collected into a JSON manifest rather than written to disk).
        let mut handles = Vec::with_capacity(n);
        for (i, token) in tokens.into_iter().enumerate() {
            let provider = params.provider.clone();
            let tier = params.tier.clone();
            let image = default_image();
            let template = Some(params.template.clone());
            let relays = relays.clone();
            let nostr_key = nostr_key.clone();
            let timeout = params.timeout_secs;
            let ssh_user = "user".to_string();
            let ssh_pass = generate_password(16);

            handles.push(tokio::spawn(async move {
                let outcome = nostr_spawn_round_trip(
                    &provider,
                    &tier,
                    &token,
                    image,
                    ssh_user.clone(),
                    ssh_pass.clone(),
                    template,
                    relays,
                    nostr_key,
                    timeout,
                )
                .await;
                (i, ssh_user, ssh_pass, outcome)
            }));
        }

        let mut shards: Vec<serde_json::Value> = Vec::with_capacity(n);
        for h in handles {
            let (i, ssh_user, ssh_pass, outcome) = match h.await {
                Ok(v) => v,
                Err(e) => {
                    shards.push(serde_json::json!({
                        "index": 0,
                        "status": "join_error",
                        "error": e.to_string(),
                    }));
                    continue;
                }
            };
            let entry = match outcome {
                Ok(NostrSpawnOutcome::Success(access)) => serde_json::json!({
                    "index": i,
                    "status": "spawned",
                    "ssh_user": ssh_user,
                    "ssh_pass": ssh_pass,
                    "access": access,
                }),
                Ok(NostrSpawnOutcome::ProviderOffline) => serde_json::json!({
                    "index": i,
                    "status": "offline",
                }),
                Ok(NostrSpawnOutcome::ProviderError(err)) => serde_json::json!({
                    "index": i,
                    "status": "provider_error",
                    "error_type": err.error_type,
                    "message": err.message,
                }),
                Ok(NostrSpawnOutcome::UnknownResponse(content)) => serde_json::json!({
                    "index": i,
                    "status": "unknown_response",
                    "content": content,
                }),
                Ok(NostrSpawnOutcome::Timeout) => serde_json::json!({
                    "index": i,
                    "status": "timeout",
                }),
                Err(e) => serde_json::json!({
                    "index": i,
                    "status": "transport_error",
                    "error": e.to_string(),
                }),
            };
            shards.push(entry);
        }
        // Sort by index so the JSON ordering matches shard ordering.
        shards.sort_by_key(|v| v["index"].as_u64().unwrap_or(0));

        let spawned_count = shards
            .iter()
            .filter(|s| s["status"].as_str() == Some("spawned"))
            .count();
        let manifest = serde_json::json!({
            "provider": params.provider,
            "template": params.template,
            "tier": params.tier,
            "shard_count": n,
            "spawned_count": spawned_count,
            "shards": shards,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".to_string()),
        )]))
    }

    #[tool(
        description = "Get the current status of an existing Paygress workload by pod_id. Returns expiry, time-remaining, ssh host/port, and resource allocation. Use this to monitor a lease before it expires so you can call `topup_workload` proactively."
    )]
    async fn workload_status(
        &self,
        Parameters(params): Parameters<StatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let relays = self.resolve_relays();
        let nostr_key = self.resolve_nostr_key()?;

        let outcome = nostr_status_round_trip(
            &params.pod_id,
            &params.provider,
            relays,
            nostr_key,
            params.timeout_secs,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("status: {}", e), None))?;

        let body = match outcome {
            NostrStatusOutcome::Success(s) => serde_json::json!({
                "status": "ok",
                "pod_id": s.pod_id,
                "lease_status": s.status,
                "expires_at": s.expires_at,
                "time_remaining_seconds": s.time_remaining_seconds,
                "cpu_millicores": s.cpu_millicores,
                "memory_mb": s.memory_mb,
                "ssh_host": s.ssh_host,
                "ssh_port": s.ssh_port,
                "ssh_username": s.ssh_username,
            }),
            NostrStatusOutcome::UnparseableResponse(content) => serde_json::json!({
                "status": "unknown_response",
                "content": content,
            }),
            NostrStatusOutcome::Timeout => serde_json::json!({
                "status": "timeout",
                "message": "provider did not respond within the timeout window",
            }),
        };
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()),
        )]))
    }

    #[tool(
        description = "Extend an existing Paygress workload's lease by paying the provider with another Cashu token. The provider redeems the token at the mint and adds `redeemed_amount / rate_msats_per_sec` seconds to the lease. Returns the new expiry on success or a structured provider error (lease_expired, not_owner, insufficient_payment, race_lost, etc.) on failure."
    )]
    async fn topup_workload(
        &self,
        Parameters(params): Parameters<TopupParams>,
    ) -> Result<CallToolResult, McpError> {
        let relays = self.resolve_relays();
        let nostr_key = self.resolve_nostr_key()?;

        let outcome = nostr_topup_round_trip(
            &params.pod_id,
            &params.token,
            &params.provider,
            relays,
            nostr_key,
            params.timeout_secs,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("topup: {}", e), None))?;

        let body = match outcome {
            NostrTopupOutcome::Success(r) => serde_json::json!({
                "status": "ok",
                "pod_id": r.pod_npub,
                "extended_duration_seconds": r.extended_duration_seconds,
                "new_expires_at": r.new_expires_at,
                "message": r.message,
            }),
            NostrTopupOutcome::ProviderError(err) => serde_json::json!({
                "status": "provider_error",
                "error_type": err.error_type,
                "message": err.message,
                "details": err.details,
            }),
            NostrTopupOutcome::UnknownResponse(content) => serde_json::json!({
                "status": "unknown_response",
                "content": content,
            }),
            NostrTopupOutcome::Timeout => serde_json::json!({
                "status": "timeout",
                "message": "provider did not respond within the timeout window — token MAY have been spent; call workload_status to verify before retrying"
            }),
        };
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for PaygressMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Paygress: pay-per-use compute marketplace using Cashu ecash and Nostr. \
                 Lifecycle: `list_providers` (discover) → `spawn_workload` (single) or \
                 `batch_spawn` (N-shard fan-out) → `workload_status` (monitor) → \
                 `topup_workload` (extend before expiry). The agent connects to the \
                 returned host:ssh_port itself to actually run code — Paygress is the \
                 spawn/billing/lifecycle fabric, not the exec channel."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

fn generate_password(len: usize) -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_advertises_tool_capability() {
        // Pin the get_info contract: clients use this to discover
        // that we expose tools (vs prompts/resources).
        let server = PaygressMcpServer::new(McpArgs::default());
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        let inst = info.instructions.as_deref().unwrap_or("");
        assert!(inst.contains("Paygress"));
        // Pin the lifecycle hint so future edits don't drop the
        // workload_status / topup_workload mentions — agents rely on
        // these to know when to call them.
        assert!(inst.contains("workload_status"));
        assert!(inst.contains("topup_workload"));
    }

    #[test]
    fn spawn_params_default_tier_is_basic() {
        // Round-trip through serde so any rename / default rot
        // surfaces here, not at runtime.
        let v = serde_json::from_value::<SpawnParams>(serde_json::json!({
            "provider": "npub1abc",
            "token": "tok",
        }))
        .unwrap();
        assert_eq!(v.tier, "basic");
        assert_eq!(v.image, "ubuntu:22.04");
        assert_eq!(v.ssh_user, "user");
        assert_eq!(v.timeout_secs, 120);
        assert!(v.template.is_none());
    }

    #[test]
    fn batch_params_default_template_is_agent_sandbox() {
        let v = serde_json::from_value::<BatchParams>(serde_json::json!({
            "provider": "npub1abc",
            "tokens": ["t1", "t2"],
        }))
        .unwrap();
        assert_eq!(v.template, "agent-sandbox");
        assert_eq!(v.tier, "basic");
        assert_eq!(v.tokens.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn status_params_defaults_round_trip() {
        let v = serde_json::from_value::<StatusParams>(serde_json::json!({
            "provider": "npub1abc",
            "pod_id": "container-7",
        }))
        .unwrap();
        assert_eq!(v.timeout_secs, 30);
        assert_eq!(v.pod_id, "container-7");
    }

    #[test]
    fn topup_params_defaults_round_trip() {
        let v = serde_json::from_value::<TopupParams>(serde_json::json!({
            "provider": "npub1abc",
            "pod_id": "container-7",
            "token": "tok",
        }))
        .unwrap();
        assert_eq!(v.timeout_secs, 120);
        assert_eq!(v.token, "tok");
    }

    #[test]
    fn batch_params_split_mode_carries_through() {
        // The model is going to send {split_token, shards} as the
        // common pattern; pin that the schema accepts it.
        let v = serde_json::from_value::<BatchParams>(serde_json::json!({
            "provider": "npub1abc",
            "split_token": "big-token",
            "shards": 5,
        }))
        .unwrap();
        assert!(v.tokens.is_none());
        assert_eq!(v.split_token.as_deref(), Some("big-token"));
        assert_eq!(v.shards, Some(5));
    }
}
