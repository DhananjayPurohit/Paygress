// MCP Server Implementation
//
// This module contains the main MCP server that handles JSON-RPC communication
// over stdio transport for the Model Context Protocol (MCP).
//
// This version calls HTTP endpoints (with L402 paywall support) instead of
// directly calling the PodProvisioningService.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info};

use crate::mcp::http_client::PaywalledHttpClient;
use crate::mcp::protocol::*;

/// MCP Server for handling Model Context Protocol requests
pub struct MCPServer {
    http_client: PaywalledHttpClient,
}

impl MCPServer {
    /// Create a new MCP server instance that calls HTTP endpoints
    pub fn new(base_url: String, l402_token: Option<String>) -> Self {
        let http_client = PaywalledHttpClient::new(base_url, l402_token);
        Self { http_client }
    }

    /// Run the MCP server with stdio transport
    pub async fn run(self) -> Result<()> {
        info!("Starting MCP server with stdio transport");

        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut writer = stdout;

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    if let Err(e) = self.handle_request(trimmed, &mut writer).await {
                        error!("Error handling request: {}", e);
                    }
                }
                Err(e) => {
                    error!("Error reading from stdin: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle a single MCP request
    async fn handle_request(&self, line: &str, writer: &mut tokio::io::Stdout) -> Result<()> {
        let request: Value = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(e) => {
                error!("Failed to parse JSON request: {}", e);
                return Ok(());
            }
        };

        let method = request["method"].as_str().unwrap_or("");
        let id = request["id"].clone();

        let response = match method {
            "initialize" => handle_initialize(&request),
            "tools/list" => handle_tools_list(&request),
            "tools/call" => handle_tools_call_http(&self.http_client, &request).await,
            "notifications/cancelled" => {
                // This is a notification, no response needed
                info!("Received cancellation notification");
                return Ok(());
            }
            _ => {
                // Check if this is a notification (no id field) or a request
                if id.is_null() {
                    info!("Received notification: {}", method);
                    return Ok(());
                } else {
                    error!("Unknown method: {}", method);
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": "Method not found"
                        }
                    })
                }
            }
        };

        let response_str = serde_json::to_string(&response)?;
        writer.write_all(response_str.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        // Send initialized notification after initialize response
        if method == "initialize" {
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            let notification_str = serde_json::to_string(&notification)?;
            writer.write_all(notification_str.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }

        Ok(())
    }
}
