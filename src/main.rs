// Unified Paygress Service
//
// Single binary that runs MCP and HTTP interfaces concurrently.
// MCP interface calls HTTP endpoints (with L402 paywall support).
// HTTP interface provides the actual paywalled endpoints.

use anyhow::Result;
use std::sync::Arc;
use tracing_subscriber::{self, EnvFilter};

mod cashu;
mod interfaces;
mod mcp;
mod nostr; // Still used for PodSpec type
mod pod_provisioning;
mod sidecar_service;
mod stake; // referenced by nostr::ProviderOfferContent

use crate::interfaces::run_all_interfaces;
use crate::pod_provisioning::PodProvisioningService;

/// Main entry point for the unified Paygress service
#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables from .env file
    dotenv::dotenv().ok();

    // Initialize tracing (all logs go to stderr automatically)
    init_tracing()?;

    // Startup logs (emojis are fine - they go to stderr, not stdout)
    tracing::info!("🚀 Starting Paygress Service");
    tracing::info!("   Architecture: MCP → HTTP (L402 Paywall)");

    // Load configuration
    let config = get_sidecar_config();

    // Validate configuration
    if config.pod_specs.is_empty() {
        tracing::error!("❌ Error: No pod specifications configured");
        tracing::error!("   Please ensure POD_SPECS_FILE points to a valid JSON file");
        std::process::exit(1);
    }

    tracing::info!("✅ Loaded {} pod specification(s)", config.pod_specs.len());
    for spec in &config.pod_specs {
        tracing::info!(
            "  - {}: {} msats/sec ({} CPU, {} MB)",
            spec.name,
            spec.rate_msats_per_sec,
            spec.cpu_millicores,
            spec.memory_mb
        );
    }

    // Create the shared pod provisioning service
    let service = Arc::new(PodProvisioningService::new(config).await?);

    tracing::info!("✅ Pod provisioning service initialized");

    // Run all enabled interfaces concurrently
    run_all_interfaces(service).await?;

    tracing::info!("🛑 Paygress service shutdown");
    Ok(())
}

/// Initialize tracing with separate log streams for each interface
fn init_tracing() -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_writer(std::io::stderr) // Force all logs to stderr
        .init();

    Ok(())
}

/// Get sidecar configuration from environment variables
fn get_sidecar_config() -> crate::sidecar_service::SidecarConfig {
    use crate::sidecar_service::SidecarConfig;

    let cashu_db_path = std::env::var("CASHU_DB_PATH").unwrap_or_else(|_| "./cashu.db".to_string());

    SidecarConfig {
        cashu_db_path,
        pod_namespace: std::env::var("POD_NAMESPACE")
            .unwrap_or_else(|_| "user-workloads".to_string()),
        minimum_pod_duration_seconds: std::env::var("MINIMUM_POD_DURATION_SECONDS")
            .unwrap_or_else(|_| "60".to_string())
            .parse()
            .unwrap_or(60),
        base_image: std::env::var("BASE_IMAGE")
            .unwrap_or_else(|_| "linuxserver/openssh-server:latest".to_string()),
        ssh_host: std::env::var("SSH_HOST").unwrap_or_else(|_| "localhost".to_string()),
        ssh_port_range_start: std::env::var("SSH_PORT_RANGE_START")
            .unwrap_or_else(|_| "30000".to_string())
            .parse()
            .unwrap_or(30000),
        ssh_port_range_end: std::env::var("SSH_PORT_RANGE_END")
            .unwrap_or_else(|_| "31000".to_string())
            .parse()
            .unwrap_or(31000),
        enable_cleanup_task: std::env::var("ENABLE_CLEANUP_TASK")
            .unwrap_or_else(|_| "true".to_string())
            .parse()
            .unwrap_or(true),
        whitelisted_mints: {
            let mints_str = match std::env::var("WHITELISTED_MINTS") {
                Ok(mints) => mints,
                Err(_) => {
                    tracing::error!("❌ Error: WHITELISTED_MINTS environment variable is required");
                    tracing::error!(
                        "   Please set WHITELISTED_MINTS with comma-separated mint URLs"
                    );
                    std::process::exit(1);
                }
            };

            mints_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        },
        pod_specs: get_pod_specs_from_env(),
    }
}

/// Get pod specifications from JSON file
fn get_pod_specs_from_env() -> Vec<crate::nostr::PodSpec> {
    use std::env;

    // Get the pod specs file path from environment variable
    let specs_file =
        env::var("POD_SPECS_FILE").unwrap_or_else(|_| "/app/pod-specs.json".to_string());

    // Read the JSON file
    match std::fs::read_to_string(&specs_file) {
        Ok(specs_json) => match serde_json::from_str::<Vec<crate::nostr::PodSpec>>(&specs_json) {
            Ok(specs) => {
                if !specs.is_empty() {
                    tracing::info!(
                        "✅ Loaded {} pod specification(s) from {}",
                        specs.len(),
                        specs_file
                    );
                    return specs;
                } else {
                    tracing::error!(
                        "❌ Error: Pod specifications file '{}' contains empty array",
                        specs_file
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    "❌ Error: Failed to parse pod specifications from '{}': {}",
                    specs_file,
                    e
                );
                tracing::error!("   Please ensure the JSON file contains valid pod specifications");
            }
        },
        Err(e) => {
            tracing::error!(
                "❌ Error: Failed to read pod specifications file '{}': {}",
                specs_file,
                e
            );
            tracing::error!("   Please ensure the file exists and is readable");
            tracing::error!("   You can set POD_SPECS_FILE environment variable to specify a different file path");
        }
    }

    tracing::error!("❌ Error: No valid pod specifications found");
    tracing::error!("   Expected file: {}", specs_file);
    tracing::error!("   Example pod-specs.json content:");
    tracing::error!(
        r#"   [
     {{
       "id": "basic",
       "name": "Basic",
       "description": "Basic VPS - 1 CPU core, 1GB RAM",
       "cpu_millicores": 1000,
       "memory_mb": 1024,
       "rate_msats_per_sec": 100
     }}
   ]"#
    );
    std::process::exit(1);
}
