// Unified Pod Provisioning Service
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::nostr::{EncryptedSpawnPodRequest, EncryptedTopUpPodRequest, PodSpec};
use crate::sidecar_service::{extract_token_value, PodInfo, SidecarConfig, SidecarState};

/// Request for spawning a new pod
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnPodTool {
    pub cashu_token: String,
    pub pod_spec_id: Option<String>,
    pub pod_image: String,
    pub ssh_username: String,
    pub ssh_password: String,
    pub user_pubkey: Option<String>,
}

/// Request for topping up an existing pod
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopUpPodTool {
    pub pod_npub: String,
    pub cashu_token: String,
}

/// Request for getting available pod specifications/offers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetOffersTool {}

/// Request for getting pod status/details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPodStatusTool {
    pub pod_npub: String,
}

/// Response for pod spawning
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnPodResponse {
    pub success: bool,
    pub message: String,
    pub pod_npub: Option<String>,
    pub ssh_host: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_username: Option<String>,
    pub ssh_password: Option<String>,
    pub expires_at: Option<String>,
    pub pod_spec_name: Option<String>,
    pub cpu_millicores: Option<u64>,
    pub memory_mb: Option<u64>,
    pub instructions: Vec<String>,
}

/// Response for pod top-up
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopUpPodResponse {
    pub success: bool,
    pub message: String,
    pub pod_npub: String,
    pub extended_duration_seconds: Option<u64>,
    pub new_expires_at: Option<String>,
}

/// Response for getting offers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetOffersResponse {
    pub minimum_duration_seconds: u64,
    pub whitelisted_mints: Vec<String>,
    pub pod_specs: Vec<PodSpec>,
}

/// Response for pod status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPodStatusResponse {
    pub success: bool,
    pub message: String,
    pub pod_npub: String,
    pub found: bool,
    pub created_at: Option<String>,
    pub expires_at: Option<String>,
    pub time_remaining_seconds: Option<u64>,
    pub pod_spec_name: Option<String>,
    pub cpu_millicores: Option<u64>,
    pub memory_mb: Option<u64>,
    pub status: Option<String>,
}

/// Unified service handler for pod provisioning
pub struct PodProvisioningService {
    state: SidecarState,
}

impl PodProvisioningService {
    pub async fn new(config: SidecarConfig) -> Result<Self> {
        let state = SidecarState::new(config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to initialize sidecar state: {}", e))?;

        Ok(Self { state })
    }

    /// Get the service configuration
    pub fn get_config(&self) -> &SidecarConfig {
        &self.state.config
    }

    /// Handle spawn pod request
    pub async fn spawn_pod(&self, request: SpawnPodTool) -> Result<SpawnPodResponse> {
        info!(
            "Pod spawn request received for image: {}",
            request.pod_image
        );

        // Convert request to internal format
        let spawn_request = EncryptedSpawnPodRequest {
            cashu_token: request.cashu_token,
            pod_spec_id: request.pod_spec_id,
            pod_image: request.pod_image,
            ssh_username: request.ssh_username,
            ssh_password: request.ssh_password,
            template_slug: None,
            replication: None,
        };

        // Use the existing logic from main.rs handle_spawn_pod_request
        match self
            .handle_spawn_pod_internal(
                spawn_request,
                &request
                    .user_pubkey
                    .unwrap_or_else(|| "mcp-client".to_string()),
            )
            .await
        {
            Ok(response) => Ok(response),
            Err(e) => {
                error!("Failed to spawn pod: {}", e);
                Ok(SpawnPodResponse {
                    success: false,
                    message: format!("Failed to spawn pod: {}", e),
                    pod_npub: None,
                    ssh_host: None,
                    ssh_port: None,
                    ssh_username: None,
                    ssh_password: None,
                    expires_at: None,
                    pod_spec_name: None,
                    cpu_millicores: None,
                    memory_mb: None,
                    instructions: vec![],
                })
            }
        }
    }

    /// Handle top-up pod request
    pub async fn topup_pod(&self, request: TopUpPodTool) -> Result<TopUpPodResponse> {
        info!("Pod top-up request received for NPUB: {}", request.pod_npub);

        // Convert request to internal format
        let topup_request = EncryptedTopUpPodRequest {
            pod_npub: request.pod_npub.clone(),
            cashu_token: request.cashu_token,
        };

        // Use the existing logic from main.rs handle_top_up_request
        match self.handle_topup_pod_internal(topup_request).await {
            Ok(response) => Ok(response),
            Err(e) => {
                error!("Failed to top-up pod: {}", e);
                Ok(TopUpPodResponse {
                    success: false,
                    message: format!("Failed to top-up pod: {}", e),
                    pod_npub: request.pod_npub,
                    extended_duration_seconds: None,
                    new_expires_at: None,
                })
            }
        }
    }

    /// Handle get offers request
    pub async fn get_offers(&self, _request: GetOffersTool) -> Result<GetOffersResponse> {
        info!("Get offers request received");

        Ok(GetOffersResponse {
            minimum_duration_seconds: self.state.config.minimum_pod_duration_seconds,
            whitelisted_mints: self.state.config.whitelisted_mints.clone(),
            pod_specs: self.state.config.pod_specs.clone(),
        })
    }

    /// Handle get pod status request
    pub async fn get_pod_status(&self, request: GetPodStatusTool) -> Result<GetPodStatusResponse> {
        info!(
            "Get pod status request received for NPUB: {}",
            request.pod_npub
        );

        use chrono::Utc;
        use k8s_openapi::api::core::v1::Pod;
        use kube::{api::ListParams, Api};

        // First check our internal tracking
        let active_pods = self.state.active_pods.read().await;
        let pod_info = active_pods.get(&request.pod_npub);

        if let Some(info) = pod_info {
            // Calculate time remaining
            let now = Utc::now();
            let time_remaining = if info.expires_at > now {
                Some((info.expires_at - now).num_seconds() as u64)
            } else {
                Some(0) // Expired
            };

            // Get pod spec details
            let pod_spec = self.state.config.pod_specs.first(); // Default spec

            Ok(GetPodStatusResponse {
                success: true,
                message: "Pod found and active".to_string(),
                pod_npub: request.pod_npub.clone(),
                found: true,
                created_at: Some(info.created_at.to_rfc3339()),
                expires_at: Some(info.expires_at.to_rfc3339()),
                time_remaining_seconds: time_remaining,
                pod_spec_name: pod_spec.map(|s| s.name.clone()),
                cpu_millicores: pod_spec.map(|s| s.cpu_millicores),
                memory_mb: pod_spec.map(|s| s.memory_mb),
                status: Some(if time_remaining.unwrap_or(0) > 0 {
                    "running".to_string()
                } else {
                    "expired".to_string()
                }),
            })
        } else {
            // Pod not found in our tracking, check if it exists in Kubernetes but expired
            let pods_api: Api<Pod> = Api::namespaced(
                self.state.k8s_client.client.clone(),
                &self.state.config.pod_namespace,
            );
            let pods = match pods_api.list(&ListParams::default()).await {
                Ok(pods) => pods,
                Err(_) => {
                    // If we can't list pods, assume pod doesn't exist
                    return Ok(GetPodStatusResponse {
                        success: true,
                        message: "Pod not found".to_string(),
                        pod_npub: request.pod_npub.clone(),
                        found: false,
                        created_at: None,
                        expires_at: None,
                        time_remaining_seconds: None,
                        pod_spec_name: None,
                        cpu_millicores: None,
                        memory_mb: None,
                        status: None,
                    });
                }
            };

            // Check if pod exists in Kubernetes
            let target_pod = pods.items.iter().find(|pod| {
                pod.metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("pod-npub"))
                    .map(|stored_hex| {
                        let requested_hex = if request.pod_npub.starts_with("npub1") {
                            &request.pod_npub[5..]
                        } else {
                            &request.pod_npub
                        };
                        let stored_truncated = if stored_hex.len() > 63 {
                            &stored_hex[..63]
                        } else {
                            stored_hex
                        };
                        let requested_truncated = if requested_hex.len() > 63 {
                            &requested_hex[..63]
                        } else {
                            requested_hex
                        };
                        stored_truncated == requested_truncated
                    })
                    .unwrap_or(false)
            });

            if let Some(pod) = target_pod {
                // Pod exists in Kubernetes but not in our tracking (likely expired)
                let status = pod
                    .status
                    .as_ref()
                    .and_then(|status| status.phase.as_ref())
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());

                Ok(GetPodStatusResponse {
                    success: true,
                    message: "Pod found but not tracked (likely expired)".to_string(),
                    pod_npub: request.pod_npub.clone(),
                    found: true,
                    created_at: pod
                        .metadata
                        .creation_timestamp
                        .as_ref()
                        .map(|ts| ts.0.to_rfc3339()),
                    expires_at: None,
                    time_remaining_seconds: Some(0), // Assume expired
                    pod_spec_name: None,
                    cpu_millicores: None,
                    memory_mb: None,
                    status: Some(status),
                })
            } else {
                // Pod not found anywhere
                Ok(GetPodStatusResponse {
                    success: true,
                    message: "Pod not found".to_string(),
                    pod_npub: request.pod_npub.clone(),
                    found: false,
                    created_at: None,
                    expires_at: None,
                    time_remaining_seconds: None,
                    pod_spec_name: None,
                    cpu_millicores: None,
                    memory_mb: None,
                    status: None,
                })
            }
        }
    }

    /// Internal handler for spawning pods (adapted from main.rs)
    async fn handle_spawn_pod_internal(
        &self,
        request: EncryptedSpawnPodRequest,
        user_pubkey: &str,
    ) -> Result<SpawnPodResponse> {
        use chrono::Utc;
        use nostr_sdk::{Keys, ToBech32};

        // Select pod specification
        let pod_spec = if let Some(spec_id) = &request.pod_spec_id {
            self.state
                .config
                .pod_specs
                .iter()
                .find(|s| s.id == *spec_id)
        } else {
            self.state.config.pod_specs.first()
        };

        let pod_spec = match pod_spec {
            Some(spec) => spec,
            None => {
                return Ok(SpawnPodResponse {
                    success: false,
                    message: format!(
                        "Pod specification '{}' not found",
                        request.pod_spec_id.as_deref().unwrap_or("default")
                    ),
                    pod_npub: None,
                    ssh_host: None,
                    ssh_port: None,
                    ssh_username: None,
                    ssh_password: None,
                    expires_at: None,
                    pod_spec_name: None,
                    cpu_millicores: None,
                    memory_mb: None,
                    instructions: vec![
                        "Please check available specifications in the offer".to_string()
                    ],
                });
            }
        };

        // Decode token to get amount and duration
        let payment_amount_msats = match extract_token_value(&request.cashu_token).await {
            Ok(msats) => msats,
            Err(e) => {
                return Ok(SpawnPodResponse {
                    success: false,
                    message: "Failed to decode Cashu token".to_string(),
                    pod_npub: None,
                    ssh_host: None,
                    ssh_port: None,
                    ssh_username: None,
                    ssh_password: None,
                    expires_at: None,
                    pod_spec_name: None,
                    cpu_millicores: None,
                    memory_mb: None,
                    instructions: vec![format!("Token decode error: {}", e)],
                });
            }
        };

        // Check if payment is sufficient for minimum duration with selected spec
        let minimum_payment =
            self.state.config.minimum_pod_duration_seconds * pod_spec.rate_msats_per_sec;
        if payment_amount_msats < minimum_payment {
            return Ok(SpawnPodResponse {
                success: false,
                message: format!("Insufficient payment: {} msats", payment_amount_msats),
                pod_npub: None,
                ssh_host: None,
                ssh_port: None,
                ssh_username: None,
                ssh_password: None,
                expires_at: None,
                pod_spec_name: Some(pod_spec.name.clone()),
                cpu_millicores: Some(pod_spec.cpu_millicores),
                memory_mb: Some(pod_spec.memory_mb),
                instructions: vec![format!(
                    "Minimum required: {} msats for {} seconds with {} spec (rate: {} msats/sec)",
                    minimum_payment,
                    self.state.config.minimum_pod_duration_seconds,
                    pod_spec.name,
                    pod_spec.rate_msats_per_sec
                )],
            });
        }

        // Calculate duration based on payment and selected spec rate
        let duration_seconds = payment_amount_msats / pod_spec.rate_msats_per_sec;

        // Token verification handled by ngx_l402 at nginx layer
        info!(
            "✅ Using payment: {} msats for {} seconds (verified by ngx_l402)",
            payment_amount_msats, duration_seconds
        );

        // Generate NPUB first and use it as pod name
        let pod_keys = Keys::generate();
        let pod_npub = pod_keys.public_key().to_bech32().unwrap();
        let pod_nsec = pod_keys.secret_key().to_secret_hex();

        // Create Kubernetes-safe pod name from NPUB (take first 8 chars after npub1 prefix)
        let pod_name = format!(
            "pod-{}",
            pod_npub
                .replace("npub1", "")
                .chars()
                .take(8)
                .collect::<String>()
        );
        let username = request.ssh_username;
        let password = request.ssh_password;
        let image = request.pod_image;
        let ssh_port = match self.state.generate_ssh_port().await {
            Ok(port) => port,
            Err(e) => {
                return Ok(SpawnPodResponse {
                    success: false,
                    message: "Failed to allocate SSH port".to_string(),
                    pod_npub: None,
                    ssh_host: None,
                    ssh_port: None,
                    ssh_username: None,
                    ssh_password: None,
                    expires_at: None,
                    pod_spec_name: Some(pod_spec.name.clone()),
                    cpu_millicores: Some(pod_spec.cpu_millicores),
                    memory_mb: Some(pod_spec.memory_mb),
                    instructions: vec![format!("Port allocation error: {}", e)],
                });
            }
        };

        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(duration_seconds as i64);

        match self
            .state
            .k8s_client
            .create_ssh_pod(
                &self.state.config,
                &self.state.config.pod_namespace,
                &pod_name,
                &pod_npub,
                &pod_nsec,
                &image,
                ssh_port,
                &username,
                &password,
                duration_seconds,
                pod_spec.memory_mb,
                pod_spec.cpu_millicores,
                user_pubkey,
            )
            .await
        {
            Ok(node_port) => {
                let pod_info = PodInfo {
                    pod_npub: pod_npub.clone(),
                    namespace: self.state.config.pod_namespace.clone(),
                    created_at: now,
                    expires_at,
                    allocated_port: ssh_port,
                    ssh_username: username.clone(),
                    ssh_password: password.clone(),
                    payment_amount_msats,
                    duration_seconds,
                    node_port: Some(node_port),
                    nostr_public_key: pod_npub.clone(),
                    nostr_private_key: pod_nsec,
                };
                self.state
                    .active_pods
                    .write()
                    .await
                    .insert(pod_npub.clone(), pod_info.clone());

                let instructions = vec![
                    "🚀 SSH access available:".to_string(),
                    "".to_string(),
                    "Direct access (no kubectl needed):".to_string(),
                    format!("   ssh -o PreferredAuthentications=password -o PubkeyAuthentication=no {}@{} -p {}", username, self.state.config.ssh_host, node_port),
                    "".to_string(),
                    "⚠️  Pod expires at:".to_string(),
                    format!("   {}", expires_at.format("%Y-%m-%d %H:%M:%S UTC")),
                    "".to_string(),
                    "📋 Pod Details:".to_string(),
                    format!("   Pod NPUB: {}", pod_npub),
                    format!("   Specification: {} ({})", pod_spec.name, pod_spec.description),
                    format!("   CPU: {} millicores", pod_spec.cpu_millicores),
                    format!("   Memory: {} MB", pod_spec.memory_mb),
                    format!("   Duration: {} seconds", duration_seconds),
                ];

                info!("Pod with NPUB {} created successfully", pod_npub);

                Ok(SpawnPodResponse {
                    success: true,
                    message: format!(
                        "Pod created successfully. SSH access available for {} seconds",
                        duration_seconds
                    ),
                    pod_npub: Some(pod_npub),
                    ssh_host: Some(self.state.config.ssh_host.clone()),
                    ssh_port: Some(node_port),
                    ssh_username: Some(username),
                    ssh_password: Some(password),
                    expires_at: Some(expires_at.to_rfc3339()),
                    pod_spec_name: Some(pod_spec.name.clone()),
                    cpu_millicores: Some(pod_spec.cpu_millicores),
                    memory_mb: Some(pod_spec.memory_mb),
                    instructions,
                })
            }
            Err(e) => Ok(SpawnPodResponse {
                success: false,
                message: "Failed to create pod".to_string(),
                pod_npub: None,
                ssh_host: None,
                ssh_port: None,
                ssh_username: None,
                ssh_password: None,
                expires_at: None,
                pod_spec_name: Some(pod_spec.name.clone()),
                cpu_millicores: Some(pod_spec.cpu_millicores),
                memory_mb: Some(pod_spec.memory_mb),
                instructions: vec![format!("Pod creation error: {}", e)],
            }),
        }
    }

    /// Internal handler for topping up pods (adapted from main.rs)
    async fn handle_topup_pod_internal(
        &self,
        request: EncryptedTopUpPodRequest,
    ) -> Result<TopUpPodResponse> {
        use chrono::Utc;
        use k8s_openapi::api::core::v1::Pod;
        use kube::{api::ListParams, Api};

        info!("Pod top-up request received for NPUB: {}", request.pod_npub);

        // Find pod by NPUB label in Kubernetes
        let pods_api: Api<Pod> = Api::namespaced(
            self.state.k8s_client.client.clone(),
            &self.state.config.pod_namespace,
        );
        let pods = match pods_api.list(&ListParams::default()).await {
            Ok(pods) => pods,
            Err(e) => {
                error!("Failed to list pods: {}", e);
                return Ok(TopUpPodResponse {
                    success: false,
                    message: format!("Failed to list pods: {}", e),
                    pod_npub: request.pod_npub,
                    extended_duration_seconds: None,
                    new_expires_at: None,
                });
            }
        };

        // Find pod by NPUB label (compare truncated hex parts)
        let target_pod = match pods.items.iter().find(|pod| {
            pod.metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("pod-npub"))
                .map(|stored_hex| {
                    // Extract hex from the requested NPUB
                    let requested_hex = if request.pod_npub.starts_with("npub1") {
                        &request.pod_npub[5..] // Remove "npub1" prefix
                    } else {
                        &request.pod_npub // Already hex or different format
                    };
                    // Truncate both to 63 chars for comparison
                    let stored_truncated = if stored_hex.len() > 63 {
                        &stored_hex[..63]
                    } else {
                        stored_hex
                    };
                    let requested_truncated = if requested_hex.len() > 63 {
                        &requested_hex[..63]
                    } else {
                        requested_hex
                    };
                    stored_truncated == requested_truncated
                })
                .unwrap_or(false)
        }) {
            Some(pod) => pod,
            None => {
                return Ok(TopUpPodResponse {
                    success: false,
                    message: format!(
                        "Pod with NPUB '{}' not found or already expired",
                        request.pod_npub
                    ),
                    pod_npub: request.pod_npub,
                    extended_duration_seconds: None,
                    new_expires_at: None,
                });
            }
        };

        // Get pod name from metadata
        let pod_name = match &target_pod.metadata.name {
            Some(name) => name.clone(),
            None => {
                return Ok(TopUpPodResponse {
                    success: false,
                    message: "Pod has no name in metadata".to_string(),
                    pod_npub: request.pod_npub,
                    extended_duration_seconds: None,
                    new_expires_at: None,
                });
            }
        };

        // Extract payment amount from token
        let payment_amount_msats = match extract_token_value(&request.cashu_token).await {
            Ok(msats) => msats,
            Err(e) => {
                return Ok(TopUpPodResponse {
                    success: false,
                    message: format!("Failed to decode Cashu token: {}", e),
                    pod_npub: request.pod_npub,
                    extended_duration_seconds: None,
                    new_expires_at: None,
                });
            }
        };

        // Calculate additional duration from payment
        let additional_duration_seconds = self
            .state
            .calculate_duration_from_payment(payment_amount_msats);

        if additional_duration_seconds == 0 {
            return Ok(TopUpPodResponse {
                success: false,
                message: format!("Insufficient payment: {} msats. Minimum required: {} msats for 1 second extension",
                    payment_amount_msats, self.state.config.pod_specs.first().map(|s| s.rate_msats_per_sec).unwrap_or(100)),
                pod_npub: request.pod_npub,
                extended_duration_seconds: None,
                new_expires_at: None,
            });
        }

        // Token verification handled by ngx_l402 at nginx layer
        info!(
            "✅ Top-up payment: {} msats for {} additional seconds (verified by ngx_l402)",
            payment_amount_msats, additional_duration_seconds
        );

        // Get current pod configuration before restarting

        let pods_api: Api<Pod> = Api::namespaced(
            self.state.k8s_client.client.clone(),
            &self.state.config.pod_namespace,
        );
        let current_pod = match pods_api.get(&pod_name).await {
            Ok(pod) => pod,
            Err(e) => {
                error!("Failed to get current pod configuration: {}", e);
                return Ok(TopUpPodResponse {
                    success: false,
                    message: format!("Failed to get pod configuration: {}", e),
                    pod_npub: request.pod_npub,
                    extended_duration_seconds: None,
                    new_expires_at: None,
                });
            }
        };

        // Use the proper deadline extension method instead of recreating the pod
        if let Err(e) = self
            .state
            .k8s_client
            .extend_pod_deadline(
                &self.state.config.pod_namespace,
                &pod_name,
                additional_duration_seconds,
            )
            .await
        {
            error!("Failed to extend pod deadline: {}", e);
            return Ok(TopUpPodResponse {
                success: false,
                message: format!("Failed to extend pod deadline: {}", e),
                pod_npub: request.pod_npub,
                extended_duration_seconds: None,
                new_expires_at: None,
            });
        }

        // Get current pod expiration time to calculate new expiration
        let current_deadline_seconds = current_pod
            .spec
            .as_ref()
            .and_then(|spec| spec.active_deadline_seconds)
            .unwrap_or(0);

        // Calculate new expiration time from pod creation time + new deadline
        let new_expires_at = match &current_pod.metadata.creation_timestamp {
            Some(creation_time) => {
                let creation_utc = creation_time.0;
                let new_deadline_seconds =
                    current_deadline_seconds + additional_duration_seconds as i64;
                creation_utc + chrono::Duration::seconds(new_deadline_seconds)
            }
            None => Utc::now() + chrono::Duration::seconds(additional_duration_seconds as i64), // Fallback
        };

        // Update the pod info in our tracking with the new deadline
        let mut active_pods = self.state.active_pods.write().await;
        if let Some(pod_info) = active_pods.get_mut(&request.pod_npub) {
            pod_info.expires_at = new_expires_at;
            pod_info.duration_seconds =
                current_deadline_seconds as u64 + additional_duration_seconds;
        }
        drop(active_pods);

        info!(
            "🔄 Pod '{}' (NPUB: {}) extended by {} seconds (new deadline: {} seconds)",
            pod_name,
            request.pod_npub,
            additional_duration_seconds,
            current_deadline_seconds + additional_duration_seconds as i64
        );

        Ok(TopUpPodResponse {
            success: true,
            message: format!(
                "Pod '{}' (NPUB: {}) successfully extended by {} seconds. New expiration: {}",
                pod_name,
                request.pod_npub,
                additional_duration_seconds,
                new_expires_at.format("%Y-%m-%d %H:%M:%S UTC")
            ),
            pod_npub: request.pod_npub,
            extended_duration_seconds: Some(additional_duration_seconds),
            new_expires_at: Some(new_expires_at.to_rfc3339()),
        })
    }
}
