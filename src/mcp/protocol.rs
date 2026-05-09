// MCP Protocol Implementation
//
// This module contains the data structures and protocol handlers
// for the Model Context Protocol (MCP) server implementation.

use crate::mcp::http_client::{PaywalledHttpClient, SpawnPodRequest};
use serde_json::Value;

/// Handle MCP initialization request
pub fn handle_initialize(request: &Value) -> Value {
    use serde_json::json;

    json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "Paygress",
                "version": "0.1.0"
            },
            "instructions": "Paygress - Lightning-powered VM provisioning service with complete pay-as-you-go model using Cashu payments",
            "tags": [
                ["name", "Paygress"], // Optional: Human-readable server name
                ["about", "Lightning-powered VM provisioning service with complete pay-as-you-go model using Cashu payments"], // Optional: Server description
            ]
        }
    })
}

/// Handle MCP tools/list request
pub fn handle_tools_list(request: &Value) -> Value {
    use serde_json::json;

    json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {
            "tools": [
                {
                    "name": "spawn_pod",
                    "description": "Spawn a new SSH-accessible pod with Cashu payment verification",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "cashu_token": {"type": "string", "description": "Valid Cashu token for payment verification"},
                            "pod_spec_id": {"type": "string", "description": "Optional pod specification ID"},
                            "pod_image": {"type": "string", "description": "Container image to use for the pod"},
                            "ssh_username": {"type": "string", "description": "SSH username for accessing the pod"},
                            "ssh_password": {"type": "string", "description": "SSH password for accessing the pod"},
                            "user_pubkey": {"type": "string", "description": "Optional user public key for identification"}
                        },
                        "required": ["cashu_token", "pod_image", "ssh_username", "ssh_password"]
                    }
                },
                {
                    "name": "topup_pod",
                    "description": "Extend the duration of an existing pod by providing additional Cashu payment",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pod_npub": {"type": "string", "description": "Pod's NPUB identifier"},
                            "cashu_token": {"type": "string", "description": "Valid Cashu token for additional payment"}
                        },
                        "required": ["pod_npub", "cashu_token"]
                    }
                },
                {
                    "name": "get_offers",
                    "description": "Get available pod specifications and pricing information",
                    "inputSchema": {"type": "object", "properties": {}}
                },
                {
                    "name": "get_pod_status",
                    "description": "Get pod status, time remaining, and specifications by NPUB",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "pod_npub": {"type": "string", "description": "Pod's NPUB identifier"}
                        },
                        "required": ["pod_npub"]
                    }
                }
            ]
        }
    })
}

/// Handle MCP tools/call request using HTTP client (with L402 paywall support)
pub async fn handle_tools_call_http(http_client: &PaywalledHttpClient, request: &Value) -> Value {
    use serde_json::json;
    use tracing::{error, info};

    let params = &request["params"];
    let tool_name = params["name"].as_str().unwrap_or("");
    let arguments = &params["arguments"];

    info!("🔧 MCP tool call (via HTTP): {}", tool_name);

    let result = match tool_name {
        "spawn_pod" => {
            let spawn_request = SpawnPodRequest {
                cashu_token: arguments["cashu_token"].as_str().unwrap_or("").to_string(),
                pod_spec_id: arguments["pod_spec_id"].as_str().map(|s| s.to_string()),
                pod_image: arguments["pod_image"].as_str().unwrap_or("").to_string(),
                ssh_username: arguments["ssh_username"].as_str().unwrap_or("").to_string(),
                ssh_password: arguments["ssh_password"].as_str().unwrap_or("").to_string(),
                user_pubkey: arguments["user_pubkey"].as_str().map(|s| s.to_string()),
            };

            match http_client.spawn_pod(spawn_request).await {
                Ok(response) => {
                    if response["success"].as_bool().unwrap_or(false) {
                        let pod_npub = response["pod_npub"].as_str().unwrap_or("N/A");
                        let ssh_host = response["ssh_host"].as_str().unwrap_or("N/A");
                        let ssh_port = response["ssh_port"].as_u64().unwrap_or(0);
                        let ssh_username = response["ssh_username"].as_str().unwrap_or("N/A");
                        let ssh_password = response["ssh_password"].as_str().unwrap_or("N/A");
                        let expires_at = response["expires_at"].as_str().unwrap_or("N/A");
                        let pod_spec_name = response["pod_spec_name"].as_str().unwrap_or("N/A");
                        let cpu = response["cpu_millicores"].as_u64().unwrap_or(0);
                        let memory = response["memory_mb"].as_u64().unwrap_or(0);

                        json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!(
                                        "🎉 VM Spawned Successfully!\n\n\
                                        Connect Command:\n\
                                        ssh {}@{} -p {}\n\
                                        Password: {}\n\n\
                                        VM Details:\n\
                                        • Pod ID: {}\n\
                                        • Spec: {} ({} CPU, {} MB RAM)\n\
                                        • Expires: {}\n\n\
                                        Quick Start: Copy the command above and use the password when prompted.",
                                        ssh_username, ssh_host, ssh_port,
                                        ssh_password,
                                        pod_npub,
                                        pod_spec_name, cpu, memory,
                                        expires_at
                                    )
                                }
                            ],
                            "isError": false
                        })
                    } else {
                        let message = response["message"].as_str().unwrap_or("Unknown error");
                        json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!("❌ **Failed to Spawn VM**\n\n{}", message)
                                }
                            ],
                            "isError": true
                        })
                    }
                }
                Err(e) => {
                    error!("Failed to spawn pod via HTTP: {}", e);
                    json!({
                        "content": [
                            {
                                "type": "text",
                                "text": format!(
                                    "❌ **Request Failed**\n\n{}\n\n\
                                    💡 This might be due to:\n\
                                    • Invalid or insufficient payment token\n\
                                    • Network connectivity issues\n\
                                    • Service temporarily unavailable",
                                    e
                                )
                            }
                        ],
                        "isError": true
                    })
                }
            }
        }
        "topup_pod" => {
            let pod_npub = arguments["pod_npub"].as_str().unwrap_or("").to_string();
            let cashu_token = arguments["cashu_token"].as_str().unwrap_or("").to_string();

            match http_client.topup_pod(pod_npub, cashu_token).await {
                Ok(response) => {
                    if response["success"].as_bool().unwrap_or(false) {
                        let extended_duration =
                            response["extended_duration_seconds"].as_u64().unwrap_or(0);
                        let new_expires_at = response["new_expires_at"].as_str().unwrap_or("N/A");

                        // Format duration
                        let hours = extended_duration / 3600;
                        let minutes = (extended_duration % 3600) / 60;
                        let seconds = extended_duration % 60;
                        let duration_str = if hours > 0 {
                            format!("{}h {}m {}s", hours, minutes, seconds)
                        } else if minutes > 0 {
                            format!("{}m {}s", minutes, seconds)
                        } else {
                            format!("{}s", seconds)
                        };

                        json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!(
                                        "✅ **VM Extended Successfully!**\n\n\
                                        ╔══════════════════════════════════════════╗\n\
                                        ║         ⏰ TIME ADDED TO YOUR VM         ║\n\
                                        ╚══════════════════════════════════════════╝\n\n\
                                        ⏱️  **Added:** {}\n\
                                        📅 **New Expiration:** {}\n\n\
                                        💡 Your VM will now remain active until the new expiration time.",
                                        duration_str, new_expires_at
                                    )
                                }
                            ],
                            "isError": false
                        })
                    } else {
                        let message = response["message"].as_str().unwrap_or("Unknown error");
                        json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!("❌ **Failed to Extend VM**\n\n{}", message)
                                }
                            ],
                            "isError": true
                        })
                    }
                }
                Err(e) => {
                    error!("Failed to topup pod via HTTP: {}", e);
                    json!({
                        "content": [
                            {
                                "type": "text",
                                "text": format!("❌ **Request Failed**\n\n{}", e)
                            }
                        ],
                        "isError": true
                    })
                }
            }
        }
        "get_offers" => {
            match http_client.get_offers().await {
                Ok(response) => {
                    let min_duration = response["minimum_duration_seconds"].as_u64().unwrap_or(0);
                    let whitelisted_mints = response["whitelisted_mints"]
                        .as_array()
                        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                        .unwrap_or_else(|| vec![]);

                    let empty_vec = vec![];
                    let pod_specs = response["pod_specs"].as_array().unwrap_or(&empty_vec);
                    let mut specs_text = String::new();

                    for (i, spec) in pod_specs.iter().enumerate() {
                        let id = spec["id"].as_str().unwrap_or("unknown");
                        let name = spec["name"].as_str().unwrap_or("Unknown");
                        let cpu = spec["cpu_millicores"].as_u64().unwrap_or(0);
                        let memory = spec["memory_mb"].as_u64().unwrap_or(0);
                        let rate = spec["rate_msats_per_sec"].as_u64().unwrap_or(0);
                        let description = spec["description"].as_str().unwrap_or("");

                        // Calculate example pricing
                        let cost_1_min = rate * 60;
                        let cost_1_hour = rate * 3600;

                        specs_text.push_str(&format!(
                            "\n{}. **{}** (`{}`)\n   {}\n   • {} CPU • {} MB RAM\n   • {} msats/sec ({}k msats/min, {}k msats/hour)\n",
                            i + 1, name, id, description, cpu, memory, rate, cost_1_min/1000, cost_1_hour/1000
                        ));
                    }

                    let mut mints_text = String::new();
                    for mint in whitelisted_mints {
                        mints_text.push_str(&format!("   • {}\n", mint));
                    }

                    json!({
                        "content": [
                            {
                                "type": "text",
                                "text": format!(
                                    "🛒 **Available VM Offerings**\n\n\
                                    ╔══════════════════════════════════════════╗\n\
                                    ║        💰 PAY-AS-YOU-GO PRICING          ║\n\
                                    ╚══════════════════════════════════════════╝\n\
                                    {}\n\
                                    ⏱️  **Minimum Duration:** {} seconds\n\n\
                                    ───────────────────────────────────────────\n\n\
                                    🏦 **Accepted Mints:**\n\
                                    {}\n\
                                    💡 Use these VM specs when spawning a pod for optimal performance!",
                                    specs_text, min_duration, mints_text
                                )
                            }
                        ],
                        "isError": false
                    })
                }
                Err(e) => {
                    error!("Failed to get offers via HTTP: {}", e);
                    json!({
                        "content": [
                            {
                                "type": "text",
                                "text": format!("❌ **Request Failed**\n\n{}", e)
                            }
                        ],
                        "isError": true
                    })
                }
            }
        }
        "get_pod_status" => {
            let pod_npub = arguments["pod_npub"].as_str().unwrap_or("").to_string();

            match http_client.get_pod_status(pod_npub).await {
                Ok(response) => {
                    if response["found"].as_bool().unwrap_or(false) {
                        let pod_npub = response["pod_npub"].as_str().unwrap_or("N/A");
                        let status = response["status"].as_str().unwrap_or("unknown");
                        let time_remaining =
                            response["time_remaining_seconds"].as_i64().unwrap_or(0);
                        let expires_at = response["expires_at"].as_str().unwrap_or("N/A");
                        let pod_spec_name = response["pod_spec_name"].as_str().unwrap_or("N/A");
                        let cpu = response["cpu_millicores"].as_u64().unwrap_or(0);
                        let memory = response["memory_mb"].as_u64().unwrap_or(0);

                        // Format time remaining
                        let time_str = if time_remaining > 0 {
                            let hours = time_remaining / 3600;
                            let minutes = (time_remaining % 3600) / 60;
                            let seconds = time_remaining % 60;
                            if hours > 0 {
                                format!("{}h {}m {}s", hours, minutes, seconds)
                            } else if minutes > 0 {
                                format!("{}m {}s", minutes, seconds)
                            } else {
                                format!("{}s", seconds)
                            }
                        } else {
                            "Expired ⏰".to_string()
                        };

                        let status_icon = if time_remaining > 0 { "✅" } else { "⏰" };

                        json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!(
                                        "📊 VM Status: {} {}\n\n\
                                        VM Details:\n\
                                        • Pod ID: {}\n\
                                        • Spec: {} ({} CPU, {} MB RAM)\n\n\
                                        Time Status:\n\
                                        • Remaining: {}\n\
                                        • Expires: {}\n\n\
                                        Use topup_pod to extend the VM lifetime before it expires.",
                                        status_icon,
                                        status.to_uppercase(),
                                        pod_npub,
                                        pod_spec_name, cpu, memory,
                                        time_str,
                                        expires_at
                                    )
                                }
                            ],
                            "isError": false
                        })
                    } else {
                        json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": "❌ **VM Not Found**\n\nThe VM with this ID was not found. It may have expired or been deleted."
                                }
                            ],
                            "isError": false
                        })
                    }
                }
                Err(e) => {
                    error!("Failed to get pod status via HTTP: {}", e);
                    json!({
                        "content": [
                            {
                                "type": "text",
                                "text": format!("❌ **Request Failed**\n\n{}", e)
                            }
                        ],
                        "isError": true
                    })
                }
            }
        }
        _ => {
            json!({
                "content": [
                    {
                        "type": "text",
                        "text": format!("❌ Unknown tool: {}", tool_name)
                    }
                ],
                "isError": true
            })
        }
    };

    json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": result
    })
}
