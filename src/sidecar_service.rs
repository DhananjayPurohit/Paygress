use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tracing::{info, warn};

// Using NIP-17 private direct messages - no manual encryption needed
use std::sync::Mutex;

use crate::cashu::initialize_cashu;
use crate::nostr;

// Configuration for the sidecar service
#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub cashu_db_path: String,
    pub pod_namespace: String,
    pub minimum_pod_duration_seconds: u64, // Minimum pod duration in seconds
    pub base_image: String,                // Base image for pods
    pub ssh_host: String,                  // SSH host for connections
    pub ssh_port_range_start: u16,         // Start of port range for pod allocation
    pub ssh_port_range_end: u16,           // End of port range for pod allocation
    pub enable_cleanup_task: bool,
    pub whitelisted_mints: Vec<String>, // Allowed Cashu mint URLs
    pub pod_specs: Vec<nostr::PodSpec>, // Available pod specifications
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            cashu_db_path: "./cashu.db".to_string(),
            pod_namespace: "user-workloads".to_string(),
            minimum_pod_duration_seconds: 60, // 1 minute minimum
            base_image: "linuxserver/openssh-server:latest".to_string(),
            ssh_host: "localhost".to_string(),
            ssh_port_range_start: 30000,
            ssh_port_range_end: 31000,
            enable_cleanup_task: true,
            whitelisted_mints: vec![], // Will be populated from environment variables
            pod_specs: vec![],         // Will be populated from environment variables
        }
    }
}

// Port pool for managing SSH port allocation
#[derive(Debug)]
pub struct PortPool {
    available_ports: HashSet<u16>,
    allocated_ports: HashSet<u16>,
    range_start: u16,
    range_end: u16,
}

impl PortPool {
    pub fn new(range_start: u16, range_end: u16) -> Self {
        let mut available_ports = HashSet::new();
        for port in range_start..=range_end {
            available_ports.insert(port);
        }

        Self {
            available_ports,
            allocated_ports: HashSet::new(),
            range_start,
            range_end,
        }
    }

    pub fn available_count(&self) -> usize {
        self.available_ports.len()
    }

    pub fn allocated_count(&self) -> usize {
        self.allocated_ports.len()
    }
}

// Sidecar service state
#[derive(Clone)]
pub struct SidecarState {
    pub config: SidecarConfig,
    pub k8s_client: Arc<PodManager>,
    pub active_pods: Arc<tokio::sync::RwLock<HashMap<String, PodInfo>>>,
    pub port_pool: Arc<Mutex<PortPool>>,
}

// Information about active pods
#[derive(Clone, Debug, Serialize)]
pub struct PodInfo {
    pub pod_npub: String, // Use NPUB instead of pod_name
    pub namespace: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub allocated_port: u16, // Port allocated from the port pool (this is the SSH port)
    pub ssh_username: String,
    pub ssh_password: String,
    pub payment_amount_msats: u64,
    pub duration_seconds: u64,
    pub node_port: Option<u16>,
    pub nostr_public_key: String,  // Pod's npub
    pub nostr_private_key: String, // Pod's nsec
}

// Pod management service
pub struct PodManager {
    pub client: kube::Client,
}

impl PodManager {
    pub async fn new() -> Result<Self, String> {
        let client = kube::Client::try_default()
            .await
            .map_err(|e| format!("Failed to create Kubernetes client: {}", e))?;
        Ok(Self { client })
    }

    pub async fn create_ssh_pod(
        &self,
        _config: &SidecarConfig,
        namespace: &str,
        pod_name: &str,
        pod_npub: &str, // Add NPUB parameter
        pod_nsec: &str, // Add NSEC parameter
        image: &str,
        ssh_port: u16,
        username: &str,
        password: &str,
        duration_seconds: u64,
        memory_mb: u64,
        cpu_millicores: u64,
        user_pubkey: &str, // User's public key for sending access events
    ) -> Result<u16, String> {
        // Return only node_port since we have NPUB
        use k8s_openapi::api::core::v1::{Container, ContainerPort, EnvVar, Pod, PodSpec, Volume};
        use kube::api::PostParams;
        use kube::Api;

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);

        // Create SSH environment variables
        let env_vars = vec![
            EnvVar {
                name: "PUID".to_string(),
                value: Some("1000".to_string()),
                value_from: None,
            },
            EnvVar {
                name: "PGID".to_string(),
                value: Some("1000".to_string()),
                value_from: None,
            },
            EnvVar {
                name: "TZ".to_string(),
                value: Some("Etc/UTC".to_string()),
                value_from: None,
            },
            EnvVar {
                name: "PUBLIC_KEY_FILE".to_string(),
                value: Some("/config/.ssh/authorized_keys".to_string()),
                value_from: None,
            },
            EnvVar {
                name: "USER_NAME".to_string(),
                value: Some(username.to_string()),
                value_from: None,
            },
            EnvVar {
                name: "USER_PASSWORD".to_string(),
                value: Some(password.to_string()),
                value_from: None,
            },
            EnvVar {
                name: "PASSWORD_ACCESS".to_string(),
                value: Some("true".to_string()),
                value_from: None,
            },
            // Nostr keys for the pod
            EnvVar {
                name: "POD_NPUB".to_string(),
                value: Some(pod_npub.to_string()),
                value_from: None,
            },
            EnvVar {
                name: "POD_NSEC".to_string(),
                value: Some(pod_nsec.to_string()),
                value_from: None,
            },
            EnvVar {
                name: "USER_PUBKEY".to_string(),
                value: Some(user_pubkey.to_string()),
                value_from: None,
            },
            EnvVar {
                name: "NOSTR_RELAYS".to_string(),
                value: Some(
                    "wss://relay.damus.io,wss://nos.lol,wss://relay.nostr.band".to_string(),
                ),
                value_from: None,
            },
            EnvVar {
                name: "SSH_PORT".to_string(),
                value: Some(ssh_port.to_string()), // SSH should listen on this port with hostNetwork
                value_from: None,
            },
        ];

        // Create pod labels and annotations
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), "paygress-ssh-pod".to_string());
        labels.insert("managed-by".to_string(), "paygress-sidecar".to_string());
        labels.insert("pod-type".to_string(), "ssh-access".to_string());
        labels.insert("pod-name".to_string(), pod_name.to_string());
        // Extract hex part from NPUB and truncate to fit Kubernetes label limit (63 chars)
        let npub_hex = if let Some(stripped) = pod_npub.strip_prefix("npub1") {
            stripped // Remove "npub1" prefix
        } else {
            pod_npub // Already hex or different format
        };
        // Truncate to 63 characters max for Kubernetes labels
        let truncated_hex = if npub_hex.len() > 63 {
            &npub_hex[..63]
        } else {
            npub_hex
        };
        labels.insert("pod-npub".to_string(), truncated_hex.to_string()); // Add NPUB hex as label

        let mut annotations = BTreeMap::new();
        annotations.insert(
            "paygress.io/created-at".to_string(),
            Utc::now().to_rfc3339(),
        );
        annotations.insert(
            "paygress.io/expires-at".to_string(),
            (Utc::now() + chrono::Duration::seconds(duration_seconds as i64)).to_rfc3339(),
        );
        annotations.insert(
            "paygress.io/duration-seconds".to_string(),
            duration_seconds.to_string(),
        );
        annotations.insert("paygress.io/ssh-username".to_string(), username.to_string());
        // Note: No TTL annotations needed - activeDeadlineSeconds handles pod termination

        // Create volumes
        let _volumes: Vec<Volume> = Vec::new();

        // Create containers with direct host port binding for SSH access
        let containers = vec![Container {
            name: "ssh-server".to_string(),
            image: Some(image.to_string()),
            ports: Some(vec![ContainerPort {
                container_port: ssh_port as i32, // With hostNetwork, container listens directly on host port
                host_port: None,                 // Not needed with hostNetwork: true
                name: Some("ssh".to_string()),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
            env: Some(env_vars),
            image_pull_policy: Some("IfNotPresent".to_string()),
            // Universal SSH setup that works with any image
            // Detects the base OS and installs/configures SSH accordingly
            command: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    r#"set -e
echo "Setting up SSH access on port {ssh_port}..."

# Detect package manager and install OpenSSH if not present
if command -v apt-get >/dev/null 2>&1; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq && apt-get install -y -qq openssh-server sudo 2>/dev/null || true
    mkdir -p /run/sshd
elif command -v apk >/dev/null 2>&1; then
    apk add --no-cache openssh sudo 2>/dev/null || true
    ssh-keygen -A 2>/dev/null || true
elif command -v yum >/dev/null 2>&1; then
    yum install -y openssh-server sudo 2>/dev/null || true
fi

# Detect available shell
if [ -f /bin/bash ]; then
    DEFAULT_SHELL="/bin/bash"
else
    DEFAULT_SHELL="/bin/sh"
fi

# Create user if it doesn't exist
if ! id "{username}" >/dev/null 2>&1; then
    useradd -m -s "$DEFAULT_SHELL" "{username}" 2>/dev/null || adduser -D -s "$DEFAULT_SHELL" "{username}" 2>/dev/null || true
fi

# Set password
echo "{username}:{password}" | chpasswd 2>/dev/null || true

# Add user to sudoers
echo "{username} ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/{username} 2>/dev/null || true
chmod 0440 /etc/sudoers.d/{username} 2>/dev/null || true

# Configure SSH
mkdir -p /etc/ssh
cat > /etc/ssh/sshd_config <<EOF
Port {ssh_port}
ListenAddress 0.0.0.0
PermitRootLogin yes
PasswordAuthentication yes
PubkeyAuthentication yes
UseDNS no
X11Forwarding yes
PrintMotd no
AcceptEnv LANG LC_*
Subsystem sftp internal-sftp
EOF

# Start SSH daemon
if command -v sshd >/dev/null 2>&1; then
    # Use absolute path to sshd if possible
    SSHD_BIN=$(command -v sshd)
    $SSHD_BIN -f /etc/ssh/sshd_config -D &
    echo "SSH server started on port {ssh_port}"
else
    echo "Warning: sshd not found"
fi

echo "Container ready. SSH on port {ssh_port}, user: {username} (shell: $DEFAULT_SHELL)"
tail -f /dev/null
"#,
                    ssh_port = ssh_port,
                    username = username,
                    password = password
                ),
            ]),
            resources: Some(k8s_openapi::api::core::v1::ResourceRequirements {
                limits: Some({
                    let mut limits = std::collections::BTreeMap::new();
                    limits.insert(
                        "memory".to_string(),
                        k8s_openapi::apimachinery::pkg::api::resource::Quantity(format!(
                            "{}Mi",
                            memory_mb
                        )),
                    );
                    limits.insert(
                        "cpu".to_string(),
                        k8s_openapi::apimachinery::pkg::api::resource::Quantity(format!(
                            "{}m",
                            cpu_millicores
                        )),
                    );
                    limits
                }),
                requests: Some({
                    let mut requests = std::collections::BTreeMap::new();
                    requests.insert(
                        "memory".to_string(),
                        k8s_openapi::apimachinery::pkg::api::resource::Quantity(format!(
                            "{}Mi",
                            memory_mb
                        )),
                    );
                    requests.insert(
                        "cpu".to_string(),
                        k8s_openapi::apimachinery::pkg::api::resource::Quantity(format!(
                            "{}m",
                            cpu_millicores
                        )),
                    );
                    requests
                }),
                claims: None,
            }),
            ..Default::default()
        }];

        // Create the pod
        let pod = Pod {
            metadata: kube::core::ObjectMeta {
                name: Some(pod_name.to_string()),
                labels: Some(labels.clone()),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers,
                volumes: None,
                restart_policy: Some("Never".to_string()),
                active_deadline_seconds: Some(duration_seconds as i64), // Kubernetes will auto-terminate after this time
                host_network: Some(true), // Use host networking - required for direct port access
                dns_policy: Some("ClusterFirstWithHostNet".to_string()), // Required when hostNetwork is true
                ..Default::default()
            }),
            ..Default::default()
        };

        // Create the pod
        let pp = PostParams::default();
        pods.create(&pp, &pod)
            .await
            .map_err(|e| format!("Failed to create pod: {}", e))?;

        // Use host networking with direct port binding for efficiency
        // This eliminates the need for separate services per pod
        // Each pod gets a unique port on the host directly

        // Wait for pod to be running and SSH to be ready
        info!("Waiting for pod {} to be ready...", pod_name);
        let mut attempts = 0;
        let max_attempts = 30; // 30 attempts * 2 seconds = 60 seconds max wait
        let mut pod_ready = false;

        while attempts < max_attempts {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            attempts += 1;

            match pods.get(pod_name).await {
                Ok(p) => {
                    if let Some(status) = &p.status {
                        if let Some(phase) = &status.phase {
                            if phase == "Running" {
                                // Check if container is ready
                                if let Some(container_statuses) = &status.container_statuses {
                                    if let Some(container_status) = container_statuses.first() {
                                        if container_status.ready {
                                            // Verify SSH is actually listening on the port
                                            use std::net::TcpStream;
                                            match TcpStream::connect_timeout(
                                                &std::net::SocketAddr::new(
                                                    std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                                                        127, 0, 0, 1,
                                                    )),
                                                    ssh_port,
                                                ),
                                                std::time::Duration::from_secs(1),
                                            ) {
                                                Ok(_) => {
                                                    pod_ready = true;
                                                    info!("Pod {} is ready and SSH is listening on port {}", pod_name, ssh_port);
                                                    break;
                                                }
                                                Err(_) => {
                                                    if attempts % 5 == 0 {
                                                        info!("Pod {} is running but SSH not yet listening on port {} (attempt {}/{})", pod_name, ssh_port, attempts, max_attempts);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else if phase == "Failed" || phase == "Succeeded" {
                                return Err(format!("Pod {} entered {} state", pod_name, phase));
                            }
                        }
                    }
                }
                Err(e) => {
                    if attempts % 5 == 0 {
                        warn!(
                            "Failed to get pod status: {} (attempt {}/{})",
                            e, attempts, max_attempts
                        );
                    }
                }
            }
        }

        if !pod_ready {
            warn!(
                "Pod {} may not be fully ready, but proceeding anyway",
                pod_name
            );
        }

        // SSH is directly accessible via hostPort binding
        let node_port = ssh_port; // The allocated port is directly accessible on the host

        info!(
            pod_name = %pod_name,
            namespace = %namespace,
            duration_seconds = %duration_seconds,
            ssh_port = %ssh_port,
            username = %username,
            node_port = %node_port,
            "SSH pod created with direct host port access (no service overhead)"
        );

        // Access details are now sent via NIP-17 Gift Wrap private messages from main.rs
        // No need to send public kind 15 events anymore

        Ok(node_port)
    }

    pub async fn extend_pod_deadline(
        &self,
        namespace: &str,
        pod_name: &str,
        additional_duration_seconds: u64,
    ) -> Result<(), String> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::api::{Patch, PatchParams};
        use kube::Api;
        use serde_json::json;

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);

        // Get current pod to read existing activeDeadlineSeconds
        let current_pod = pods
            .get(pod_name)
            .await
            .map_err(|e| format!("Failed to get pod: {}", e))?;

        // Calculate new deadline
        let current_deadline_seconds = current_pod
            .spec
            .as_ref()
            .and_then(|spec| spec.active_deadline_seconds)
            .unwrap_or(0);

        let new_deadline_seconds = current_deadline_seconds + additional_duration_seconds as i64;
        let new_expires_at =
            Utc::now() + chrono::Duration::seconds(additional_duration_seconds as i64);

        // Create patch to update activeDeadlineSeconds and annotations
        let patch = json!({
            "spec": {
                "activeDeadlineSeconds": new_deadline_seconds
            },
            "metadata": {
                "annotations": {
                    "paygress.io/expires-at": new_expires_at.to_rfc3339(),
                    "paygress.io/extended-at": Utc::now().to_rfc3339()
                }
            }
        });

        let pp = PatchParams::default();
        let _ = pods
            .patch(pod_name, &pp, &Patch::Merge(patch))
            .await
            .map_err(|e| format!("Failed to update pod deadline: {}", e))?;

        info!(
            pod_name = %pod_name,
            namespace = %namespace,
            additional_seconds = %additional_duration_seconds,
            new_deadline_seconds = %new_deadline_seconds,
            "Extended pod activeDeadlineSeconds"
        );

        Ok(())
    }
}

impl SidecarState {
    pub async fn new(config: SidecarConfig) -> Result<Self, String> {
        // Initialize Cashu
        initialize_cashu(&config.cashu_db_path)
            .await
            .map_err(|e| format!("Cashu init failed: {}", e))?;

        // Initialize Kubernetes client
        let k8s_client = Arc::new(PodManager::new().await?);

        let active_pods = Arc::new(tokio::sync::RwLock::new(HashMap::new()));

        // Initialize port pool
        let port_pool = Arc::new(Mutex::new(PortPool::new(
            config.ssh_port_range_start,
            config.ssh_port_range_end,
        )));

        Ok(Self {
            config,
            k8s_client,
            active_pods,
            port_pool,
        })
    }

    // Calculate duration based on payment amount (using first available spec as default)
    pub fn calculate_duration_from_payment(&self, payment_msats: u64) -> u64 {
        let msats_per_sec = self
            .config
            .pod_specs
            .first()
            .map(|spec| spec.rate_msats_per_sec)
            .unwrap_or(100) // Default rate if no specs available
            .max(1);

        // Calculate duration in seconds: payment_msats / msats_per_sec
        payment_msats / msats_per_sec
    }

    // Generate secure random password
    pub fn generate_password() -> String {
        use rand::Rng;
        const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut rng = rand::thread_rng();
        (0..16)
            .map(|_| {
                let idx = rng.gen_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect()
    }

    // Check if port is in use using multiple methods for reliability
    pub fn is_port_in_use(&self, port: u16) -> bool {
        // Method 1: Try to bind to the port (most reliable)
        use std::net::{SocketAddr, TcpListener};
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        match TcpListener::bind(addr) {
            Ok(_listener) => {
                // Port is available - the listener will be automatically dropped when it goes out of scope
                false
            }
            Err(_) => {
                // Port is definitely in use
                true
            }
        }
    }

    // Check what ports are actually in use by existing pods
    async fn get_ports_in_use_by_pods(&self) -> Result<HashSet<u16>, String> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let pods_api: Api<Pod> =
            Api::namespaced(self.k8s_client.client.clone(), &self.config.pod_namespace);
        let pods = pods_api
            .list(&kube::api::ListParams::default())
            .await
            .map_err(|e| format!("Failed to list pods: {}", e))?;

        let mut used_ports = HashSet::new();
        for pod in pods.items {
            if let Some(spec) = &pod.spec {
                for container in &spec.containers {
                    if let Some(ports) = &container.ports {
                        for port in ports {
                            if let Some(host_port) = port.host_port {
                                used_ports.insert(host_port as u16);
                            }
                        }
                    }
                }
            }
        }

        info!(
            "Found {} ports in use by existing pods: {:?}",
            used_ports.len(),
            used_ports
        );
        Ok(used_ports)
    }

    // Generate unique SSH port for each pod from the port pool with simple collision prevention
    pub async fn generate_ssh_port(&self) -> Result<u16, String> {
        // Get ports actually in use by existing pods first (before holding any locks)
        let pods_using_ports = self.get_ports_in_use_by_pods().await.unwrap_or_else(|e| {
            warn!("Failed to get ports in use by pods: {}", e);
            HashSet::new()
        });

        // Get a snapshot of allocated ports to check outside the lock
        let allocated_ports: Vec<u16> = {
            let port_pool = self
                .port_pool
                .lock()
                .map_err(|e| format!("Failed to lock port pool: {}", e))?;
            port_pool.allocated_ports.iter().cloned().collect()
        };

        // Clean up any ports that are marked as allocated but are actually free
        for port in allocated_ports {
            if !self.is_port_in_use(port) && !pods_using_ports.contains(&port) {
                // Port is actually free, move it back to available
                let mut port_pool = self
                    .port_pool
                    .lock()
                    .map_err(|e| format!("Failed to lock port pool: {}", e))?;
                port_pool.allocated_ports.remove(&port);
                port_pool.available_ports.insert(port);
                info!(
                    "Port {} was marked allocated but is actually free - moving back to available",
                    port
                );
            }
        }

        // Try to find an available port that's actually free
        let available_ports: Vec<u16> = {
            let port_pool = self
                .port_pool
                .lock()
                .map_err(|e| format!("Failed to lock port pool: {}", e))?;
            port_pool.available_ports.iter().cloned().collect()
        };

        for port in available_ports {
            // Double-check if port is actually available on the system and not used by pods
            if !self.is_port_in_use(port) && !pods_using_ports.contains(&port) {
                // Add small random delay to reduce race conditions (1-10ms)
                let delay_ms = (port % 10) + 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms as u64)).await;

                // Double-check again after delay
                if !self.is_port_in_use(port) && !pods_using_ports.contains(&port) {
                    // Port is free, allocate it
                    let mut port_pool = self
                        .port_pool
                        .lock()
                        .map_err(|e| format!("Failed to lock port pool: {}", e))?;
                    port_pool.available_ports.remove(&port);
                    port_pool.allocated_ports.insert(port);

                    info!(
                        "✅ Allocated unique SSH port {} from pool ({} available, {} allocated)",
                        port,
                        port_pool.available_count(),
                        port_pool.allocated_count()
                    );
                    return Ok(port);
                }
            }

            // Port is in use by system or pods, remove from available pool
            let mut port_pool = self
                .port_pool
                .lock()
                .map_err(|e| format!("Failed to lock port pool: {}", e))?;
            port_pool.available_ports.remove(&port);
            if pods_using_ports.contains(&port) {
                warn!(
                    "Port {} is in use by existing pod - removed from pool",
                    port
                );
            } else {
                warn!("Port {} is in use by system - removed from pool", port);
            }
        }

        // If no ports in pool, search entire range for any free port
        let start_port = self.config.ssh_port_range_start;
        let end_port = self.config.ssh_port_range_end;

        info!(
            "No ports available in pool, searching range {}-{}",
            start_port, end_port
        );

        for port in start_port..=end_port {
            // Skip if used by existing pods
            if pods_using_ports.contains(&port) {
                continue;
            }

            // Check if port is already allocated in our pool
            let is_allocated = {
                let port_pool = self
                    .port_pool
                    .lock()
                    .map_err(|e| format!("Failed to lock port pool: {}", e))?;
                port_pool.allocated_ports.contains(&port)
            };

            if is_allocated {
                continue;
            }

            // Check if port is actually available on system
            if !self.is_port_in_use(port) {
                // Found a free port outside our pool - allocate it
                let mut port_pool = self
                    .port_pool
                    .lock()
                    .map_err(|e| format!("Failed to lock port pool: {}", e))?;
                port_pool.allocated_ports.insert(port);

                info!(
                    "✅ Allocated unique SSH port {} from range ({} available, {} allocated)",
                    port,
                    port_pool.available_count(),
                    port_pool.allocated_count()
                );
                return Ok(port);
            }
        }

        Err("No available ports in the configured range".to_string())
    }
}

// Extract token value in sats from Cashu token
pub async fn extract_token_value(token: &str) -> Result<u64, String> {
    crate::cashu::extract_token_value(token)
        .await
        .map_err(|e| e.to_string())
}
