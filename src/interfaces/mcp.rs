// MCP Interface for Paygress
//
// Handles Model Context Protocol (MCP) requests for Context VM integration.
// This version calls HTTP endpoints (with L402 paywall support) instead of
// directly calling the PodProvisioningService.

use anyhow::Result;
use tracing::{error, info};

use crate::mcp::MCPServer;

/// Run the MCP interface (calling HTTP endpoints)
pub async fn run_mcp_interface() -> Result<()> {
    info!("🤖 Starting MCP interface (HTTP client mode)");

    // Get HTTP endpoint configuration
    let http_base_url =
        std::env::var("HTTP_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());

    // Optional L402 token for authentication
    let l402_token = std::env::var("HTTP_L402_TOKEN").ok();

    if l402_token.is_some() {
        info!("✅ L402 token configured for paywalled endpoints");
    } else {
        info!("⚠️  No L402 token configured - endpoints may require payment");
    }

    info!("🌐 HTTP base URL: {}", http_base_url);

    // Create and run the MCP server
    let mcp_server = MCPServer::new(http_base_url, l402_token);

    // MCP server ready - all logs go to stderr, stdio is clean for JSON-RPC
    info!("✅ MCP interface ready - listening on stdio transport");
    info!("   All tool calls will be proxied to HTTP endpoints");

    // Run the MCP server (this blocks forever and takes over stdio)
    if let Err(e) = mcp_server.run().await {
        error!("❌ MCP interface error: {}", e);
        return Err(e);
    }

    Ok(())
}
