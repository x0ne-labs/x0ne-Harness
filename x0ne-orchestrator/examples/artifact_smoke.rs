//! Live smoke test for artifact_local_proxy — registers a artifact that
//! lives inside a container (Incus, Podman or Docker) and keeps the
//! proxy up so you can curl `http://127.0.0.1:7800/c/<id>/` from another
//! terminal.
//!
//! Usage
//! ─────
//!   cargo run --release --example artifact_smoke -- \
//!        [incus|podman|docker] <tenant> <artifact_id> <root_path>
//!
//!   # Incus example
//!   cargo run --release --example artifact_smoke -- \
//!        incus prod-infra:x0ne-sandbox cv_01a78880 \
//!        /root/.x0ne/artifacts/ephemeral/orphan/cv_01a78880
//!
//!   # Podman example
//!   cargo run --release --example artifact_smoke -- \
//!        podman mycontainer cv_abc123 \
//!        /root/.x0ne/artifacts/ephemeral/orphan/cv_abc123
//!
//! Then from another shell: `curl -I http://127.0.0.1:7800/c/<id>/`.
//! Ctrl-C to stop.

use std::env;

use x0ne_orchestrator::artifact_local_proxy::{self, ArtifactEntry, ArtifactKind};
use x0ne_orchestrator::container_runtime::Backend;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!(
            "usage: artifact_smoke <backend> <tenant> <artifact_id> <root_path>\n\
             backend ∈ {{incus, podman, docker}}\n\
             example: artifact_smoke incus prod-infra:x0ne-sandbox cv_01a78880 \\\n\
                       /root/.x0ne/artifacts/ephemeral/orphan/cv_01a78880"
        );
        std::process::exit(2);
    }
    let backend = Backend::parse(&args[0]).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown backend '{}' — expected incus|podman|docker",
            args[0]
        )
    })?;
    let tenant = args[1].clone();
    let artifact_id = args[2].clone();
    let root_path = args[3].clone();

    let reg = artifact_local_proxy::ensure_started().await?;
    reg.register(ArtifactEntry {
        artifact_id: artifact_id.clone(),
        tenant: Some(tenant),
        backend: Some(backend),
        root_path,
        kind: ArtifactKind::Static,
        expires_at: None,
    })
    .await;

    let url = artifact_local_proxy::url_for(&artifact_id).unwrap();
    println!("✓ proxy up. try:");
    println!("  curl -I {}", url);
    println!("ctrl-c to stop.");

    tokio::signal::ctrl_c().await?;
    println!("bye.");
    Ok(())
}
