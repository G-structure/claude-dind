//! Remote loom orchestration — runs CRIU-backed agents on GitHub Actions runners.
//!
//! This module implements the `claude-dind remote` subcommand. It starts a gwp
//! tunnel (TLS + yamux over a Cloudflare Worker relay), dispatches a GitHub
//! Actions workflow on a runner where CRIU works, and connects the local TUI
//! to the remote container.
//!
//! ## Data flow
//!
//! ```text
//! LOCAL (macOS)                           GITHUB RUNNER (Ubuntu)
//! ┌──────────────────────┐                ┌──────────────────────────┐
//! │ gwp serve (in-proc)  │◄══ relay ═══►  │ gwp agent               │
//! │ TLS + yamux + auth   │                │ SOCKS5 :1080             │
//! │ DNS resolves here    │                │ HTTP CONNECT :1081       │
//! └──────┬───────────────┘                └──────┬───────────────────┘
//! │ reverse fwd      │                │        │                    │
//! │ localhost:XXXXX ──┼────yamux──────►│ socat :2375 → docker.sock │
//! └──────┬───────────────┘                └──────────────────────────┘
//! │ DOCKER_HOST=tcp://..  │
//! │ Multiplexer TUI       │
//! └───────────────────────┘
//! ```
//!
//! The key insight is that `docker -H tcp://... exec -it <container>` works
//! transparently over the yamux tunnel — the PTY I/O flows through the
//! reverse-forwarded port. Checkpoint/restore commands also go through the
//! remote Docker API. DNS resolution and TCP dials happen locally (gwp serve
//! side), so Claude Code's API calls appear from the local machine's IP.

use anyhow::{bail, Context, Result};
use gh_worker_proxy::reverse::ReverseForwardRequest;
use gh_worker_proxy::socks5::Address;
use gh_worker_proxy::tls;
use std::path::Path;
use std::process::Command;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsAcceptor;

use crate::container::ContainerManager;
use crate::multiplexer;

/// Run the remote loom session.
///
/// Orchestration flow:
/// 1. Extract credentials from macOS Keychain
/// 2. Generate gwp TLS identity (fingerprint + token)
/// 3. Start gwp serve in-process as a tokio task (via relay)
/// 4. Dispatch GitHub Actions workflow via `gh workflow run`
/// 5. Wait for agent connection
/// 6. Start local TCP forwarder for Docker API
/// 7. Poll remote Docker until reachable
/// 8. Find or wait for container
/// 9. Inject credentials via remote docker exec
/// 10. Run multiplexer TUI
/// 11. On exit: stop container, cancel workflow
pub async fn run_remote(
    repo: &str,
    workflow: &str,
    relay: &str,
    relay_token: &str,
    image: &str,
    loom: bool,
    loom_path: Option<&Path>,
    verbose: bool,
) -> Result<i32> {
    // Step 1: Extract credentials
    eprintln!("[claude-dind] Extracting credentials from macOS Keychain...");
    let creds = crate::credentials::extract_credentials()?;
    eprintln!("[claude-dind] Credentials extracted successfully.");

    // Step 2: Generate TLS identity and auth token
    let identity = tls::generate_identity()?;
    let fingerprint = identity.fingerprint.clone();
    let token: String = {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let bytes: [u8; 16] = Rng::r#gen(&mut rng);
        hex::encode(bytes)
    };

    eprintln!("[claude-dind] TLS fingerprint: {fingerprint}");
    if verbose {
        eprintln!("[claude-dind] Auth token: {token}");
    }

    // Step 3: Start gwp serve in-process via WebSocket relay
    let acceptor = TlsAcceptor::from(identity.tls_config);
    let (reverse_tx, reverse_rx) = mpsc::channel::<ReverseForwardRequest>(32);

    let ws_url = format!("{relay}/connect?token={relay_token}&role=server");
    eprintln!("[claude-dind] Connecting to relay: {relay}");

    let serve_token = token.clone();
    let serve_handle = tokio::spawn(async move {
        let stream = gh_worker_proxy::ws::connect(&ws_url).await?;
        eprintln!("[claude-dind] Relay connected, waiting for agent TLS handshake...");
        gh_worker_proxy::serve::handle_connection_io_with_reverse(
            acceptor,
            &serve_token,
            stream,
            Some(reverse_rx),
        )
        .await
    });

    // Step 4: Dispatch GitHub Actions workflow
    eprintln!("[claude-dind] Dispatching workflow {workflow} on {repo}...");
    let gh_status = Command::new("gh")
        .args([
            "workflow", "run", workflow,
            "--repo", repo,
            "-f", &format!("fingerprint={fingerprint}"),
            "-f", &format!("token={token}"),
            "-f", &format!("relay_addr={relay}"),
            "-f", &format!("relay_token={relay_token}"),
            "-f", &format!("image={image}"),
            "-f", &format!("loom={loom}"),
        ])
        .status()
        .context("Failed to run `gh workflow run`. Is the GitHub CLI installed?")?;

    if !gh_status.success() {
        bail!("gh workflow run failed. Check that the repo and workflow exist.");
    }
    eprintln!("[claude-dind] Workflow dispatched. Waiting for runner to connect...");

    // Step 5: Wait for agent connection (the serve task blocks until paired + TLS + auth)
    // Meanwhile, start the local TCP forwarder

    // Step 6: Start local TCP forwarder
    // Binds localhost:0, for each connection opens a reverse-forwarded yamux stream
    // to the runner's 127.0.0.1:2375 (socat-exposed Docker socket)
    let forwarder_listener = TcpListener::bind("127.0.0.1:0").await?;
    let docker_port = forwarder_listener.local_addr()?.port();
    let docker_host = format!("tcp://127.0.0.1:{docker_port}");
    eprintln!("[claude-dind] Docker API forwarder on {docker_host}");

    let fwd_tx = reverse_tx.clone();
    tokio::spawn(async move {
        loop {
            match forwarder_listener.accept().await {
                Ok((mut tcp, _peer)) => {
                    let tx = fwd_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = forward_to_remote(&mut tcp, tx).await {
                            eprintln!("[claude-dind] forwarder error: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("[claude-dind] forwarder accept error: {e}");
                    break;
                }
            }
        }
    });

    // Step 7: Poll remote Docker until reachable
    eprintln!("[claude-dind] Waiting for remote Docker daemon...");
    wait_for_remote_docker(&docker_host, 180)?;

    // Step 8: If loom, verify Docker experimental on the remote
    if loom {
        eprintln!("[claude-dind] Verifying Docker experimental on runner...");
        ContainerManager::ensure_experimental_with_host(Some(&docker_host))?;
        eprintln!("[claude-dind] Docker experimental confirmed on runner.");
    }

    // Step 9: Find container on the remote
    eprintln!("[claude-dind] Looking for container on runner...");
    let container_id = wait_for_container(&docker_host, "claude-remote", 60)?;
    eprintln!("[claude-dind] Found container: {}", &container_id[..12]);

    let container = ContainerManager::attach_with_host(&container_id, Some(&docker_host))?;

    // Wait for container to be ready
    container.wait_for_ready(30)?;

    // Inject credentials
    eprintln!("[claude-dind] Injecting credentials into remote container...");
    container.inject_credentials(&creds)?;
    eprintln!("[claude-dind] Credentials injected.");

    if loom {
        eprintln!("[claude-dind] Remote loom mode active. Ctrl-b s to snapshot, Ctrl-b t for tree.");
    }

    // Step 10: Run the multiplexer TUI
    let detached = multiplexer::run_with_host(
        &container,
        false,
        Some(&creds),
        loom_path,
        verbose,
        Some(&docker_host),
    )
    .await?;

    // Step 11: Cleanup
    if !detached {
        container.stop()?;
    }

    // Cancel the workflow run
    eprintln!("[claude-dind] Cancelling GitHub Actions workflow...");
    let _ = cancel_latest_run(repo, workflow);

    // Drop the serve handle — it will exit when the tunnel closes
    serve_handle.abort();

    if detached {
        eprintln!(
            "[claude-dind] Detached. Remote container {} is still running on the runner.",
            container.short_id()
        );
    }

    Ok(0)
}

/// Forward a local TCP connection to the remote Docker daemon via reverse port forwarding.
///
/// Opens a reverse-forwarded yamux stream targeting 127.0.0.1:2375 on the agent,
/// then copies bytes bidirectionally.
async fn forward_to_remote(
    tcp: &mut tokio::net::TcpStream,
    tx: mpsc::Sender<ReverseForwardRequest>,
) -> Result<()> {
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(ReverseForwardRequest {
        target: Address::Ipv4([127, 0, 0, 1], 2375),
        respond: resp_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("reverse forward channel closed"))?;

    let yamux_stream = resp_rx
        .await
        .map_err(|_| anyhow::anyhow!("reverse forward request dropped"))?
        .map_err(|e| anyhow::anyhow!("reverse forward open failed: {e:#}"))?;

    // Write the reverse prefix + address header so the agent knows where to connect
    use futures::AsyncWriteExt as _;
    let mut ys = yamux_stream;
    let mut header = vec![gh_worker_proxy::reverse::REVERSE_PREFIX];
    header.extend(Address::Ipv4([127, 0, 0, 1], 2375).encode());
    ys.write_all(&header).await?;
    ys.flush().await?;

    let mut compat = tokio_util::compat::FuturesAsyncReadCompatExt::compat(ys);
    tokio::io::copy_bidirectional(tcp, &mut compat).await?;

    Ok(())
}

/// Poll `docker -H <host> info` until the remote Docker daemon is reachable.
fn wait_for_remote_docker(docker_host: &str, timeout_secs: u32) -> Result<()> {
    for elapsed in 0..timeout_secs {
        let output = Command::new("docker")
            .args(["-H", docker_host, "info"])
            .output();

        if let Ok(output) = output {
            if output.status.success() {
                eprintln!(
                    "[claude-dind] Remote Docker reachable (took {elapsed}s)."
                );
                return Ok(());
            }
        }

        if elapsed > 0 && elapsed % 15 == 0 {
            eprintln!("[claude-dind] Still waiting for remote Docker... ({elapsed}s)");
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    bail!(
        "Remote Docker daemon not reachable after {timeout_secs}s. \
         Check that the GitHub Actions workflow started correctly."
    );
}

/// Find a container by name filter on the remote Docker host.
fn wait_for_container(docker_host: &str, name_filter: &str, timeout_secs: u32) -> Result<String> {
    for elapsed in 0..timeout_secs {
        let output = Command::new("docker")
            .args([
                "-H", docker_host,
                "ps", "-q", "--filter", &format!("name={name_filter}"),
            ])
            .output();

        if let Ok(output) = output {
            if output.status.success() {
                let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !id.is_empty() {
                    return Ok(id);
                }
            }
        }

        if elapsed > 0 && elapsed % 10 == 0 {
            eprintln!("[claude-dind] Waiting for container '{name_filter}'... ({elapsed}s)");
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    bail!(
        "Container '{name_filter}' not found after {timeout_secs}s. \
         Check the GitHub Actions workflow logs."
    );
}

/// Cancel the most recent workflow run for cleanup.
fn cancel_latest_run(repo: &str, workflow: &str) -> Result<()> {
    // Get the latest run ID
    let output = Command::new("gh")
        .args([
            "run", "list",
            "--repo", repo,
            "--workflow", workflow,
            "--status", "in_progress",
            "--limit", "1",
            "--json", "databaseId",
            "-q", ".[0].databaseId",
        ])
        .output()
        .context("Failed to list workflow runs")?;

    if output.status.success() {
        let run_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !run_id.is_empty() {
            let _ = Command::new("gh")
                .args(["run", "cancel", &run_id, "--repo", repo])
                .status();
            eprintln!("[claude-dind] Cancelled workflow run {run_id}.");
        }
    }

    Ok(())
}
