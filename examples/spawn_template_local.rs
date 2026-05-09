// Local end-to-end test for the template-spawn integration
// (docker side only — bypasses Cashu redemption and Nostr DM round-
// trip, which require either a working real-mint flow or a full
// provider stack).
//
// What it does: construct a `ContainerConfig` exactly as the real
// `handle_spawn_request` would (image / ports / env from the
// template registry, host port from a synthetic SSH base), then
// run `DockerBackend::create_container`. If the container comes
// up healthy, the template-spawn integration is wired correctly.
//
// Usage:
//   cargo run --release --example spawn_template_local -- \
//     <slug> [<host-port-base>]
// Example:
//   cargo run --release --example spawn_template_local -- nostr-relay 35000

use std::collections::HashMap;
use std::env;

use paygress::compute::{ComputeBackend, ContainerConfig, PortMapping};
use paygress::docker::DockerBackend;
use paygress::templates::{TemplateDefinition, TemplateName};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let slug = args.get(1).map(|s| s.as_str()).unwrap_or("nostr-relay");
    let host_port_base: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(35000);

    let name = TemplateName::from_slug(slug)
        .ok_or_else(|| anyhow::anyhow!("unknown template slug `{}`", slug))?;
    let def = TemplateDefinition::lookup(name);
    eprintln!("template:    {:?}", def.name);
    eprintln!("image:       {}", def.image);
    eprintln!("ports:       {:?}", def.ports);
    eprintln!("env:         {:?}", def.env);

    let backend = DockerBackend::new();
    let id = backend
        .find_available_id(host_port_base as u32, host_port_base as u32 + 100)
        .await?;
    eprintln!("id:          {}", id);

    let template_ports: Vec<PortMapping> = def
        .ports
        .iter()
        .enumerate()
        .map(|(i, p)| PortMapping {
            host_port: host_port_base.saturating_add(1 + i as u16),
            container_port: p.container_port,
            protocol: "tcp",
        })
        .collect();
    let template_env: HashMap<String, String> = def
        .env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    eprintln!("host ports:  {:?}", template_ports);

    let extra_runtime_args: Vec<String> = def
        .extra_docker_args
        .iter()
        .map(|s| s.to_string())
        .collect();
    eprintln!("extra args:  {:?}", extra_runtime_args);

    let cfg = ContainerConfig {
        id,
        name: format!("paygress-{}", id),
        image: def.image.to_string(),
        cpu_cores: (def.min_cpu_millicores / 1000).max(1) as u32,
        memory_mb: def.min_memory_mb as u32,
        storage_gb: def.min_storage_gb as u32,
        password: "test".to_string(),
        ssh_key: None,
        host_port: Some(host_port_base),
        template_ports,
        template_env,
        extra_runtime_args,
        data_path: def.data_path.map(|p| p.to_string()),
    };

    let docker_id = backend.create_container(&cfg).await?;
    eprintln!("container:   {}", docker_id);
    eprintln!("\nverify with:");
    eprintln!("  docker ps --filter name=paygress-{}", id);
    eprintln!("  curl http://localhost:{}/", host_port_base + 1);
    eprintln!("\ncleanup with:");
    eprintln!("  docker rm -f paygress-{}", id);
    Ok(())
}
