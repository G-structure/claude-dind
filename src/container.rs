//! Long-lived container management for Docker-out-of-Docker (DooD) mode.
//!
//! In interactive mode, a single container stays alive while the multiplexer
//! creates/kills individual Claude Code sessions inside it via `docker exec`.
//! The host's Docker socket is bind-mounted so Claude can run Docker commands
//! as sibling containers on the host daemon. This module manages the container
//! lifecycle:
//!
//! - **Start**: `docker run -d` with socket mount, security opts, and
//!   `CLAUDE_MODE=interactive`. When `loom` is enabled, uses checkpoint-
//!   compatible flags instead of `no-new-privileges`.
//! - **Credential injection**: Writes the credential JSON into the container's
//!   filesystem via `docker exec -i` with stdin piping (avoids credentials
//!   appearing in process arguments visible to `ps`/`docker inspect`)
//! - **Health checks**: Verifies Docker socket access inside the container
//! - **Attach**: Reconnects to an already-running container by ID
//! - **Stop**: `docker rm -f` for cleanup
//! - **Checkpoint**: Creates a CRIU snapshot with `docker checkpoint create`
//! - **Restore**: Restores from a snapshot with `docker start --checkpoint`
//!
//! ## Remote Docker
//!
//! When `docker_host` is set (e.g., `tcp://localhost:12345`), all Docker CLI
//! commands are run with `-H <host>`, targeting a remote Docker daemon via the
//! gwp tunnel. This enables the `remote` subcommand to manage containers on
//! a GitHub Actions runner from the local machine.
//!
//! ## Security Flags
//!
//! The container starts with different security profiles depending on mode:
//!
//! - **Standard mode**: `--security-opt no-new-privileges` prevents privilege
//!   escalation inside the container.
//! - **Loom mode** (`--loom`): Uses `--net=host`, `--security-opt seccomp=unconfined`,
//!   and `--security-opt apparmor=unconfined`. CRIU requires ptrace and other
//!   syscalls blocked by the default seccomp profile, and AppArmor blocks CRIU's
//!   memory inspection operations. `--net=host` works around a containerd netns
//!   bind-mount failure (containerd#12141).
//!
//! ## Credential Injection
//!
//! Unlike prompt mode (where credentials are piped to the container's PID 1
//! via stdin), interactive mode injects credentials after the container is
//! already running:
//!
//! ```text
//! echo $JSON | docker exec -i <id> sh -c 'cat > ~/.claude/.credentials.json'
//! ```
//!
//! The JSON is piped through `docker exec`'s stdin rather than passed as a
//! command argument. This prevents the credential from appearing in the
//! process table, `docker inspect`, or Docker's event log.
//!
//! ## Checkpoint Operations (Loom)
//!
//! When loom mode is active, checkpoint operations follow a credential-safe
//! protocol:
//!
//! - **Before checkpoint**: Credentials are stripped from the container
//!   filesystem so they are not captured in the CRIU image.
//! - **After checkpoint/restore**: Fresh credentials are re-injected.
//! - **Containerd workaround**: Stale content blobs are purged via
//!   `ctr -n moby content rm` before each operation (moby#42900).

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Manages a long-lived Docker container for interactive mode.
///
/// Instead of the ephemeral one-container-per-prompt model, interactive mode
/// starts a single container with the host Docker socket mounted. Individual
/// Claude sessions are spawned inside it via `docker exec`.
///
/// When `docker_host` is set, all Docker CLI commands include `-H <host>`,
/// targeting a remote Docker daemon (e.g., on a GitHub Actions runner via
/// the gwp tunnel).
pub struct ContainerManager {
    pub container_id: String,
    #[allow(dead_code)]
    pub image: String,
    /// Optional remote Docker host (e.g., "tcp://localhost:12345").
    /// When set, all docker commands include `-H <host>`.
    pub docker_host: Option<String>,
}

impl ContainerManager {
    /// Build a `Command` for the docker CLI, prepending `-H <host>` when remote.
    fn docker_cmd(&self) -> Command {
        let mut cmd = Command::new("docker");
        if let Some(ref host) = self.docker_host {
            cmd.args(["-H", host]);
        }
        cmd
    }

    /// Build a `Command` for the docker CLI with a specific host override.
    fn docker_cmd_with_host(docker_host: Option<&str>) -> Command {
        let mut cmd = Command::new("docker");
        if let Some(host) = docker_host {
            cmd.args(["-H", host]);
        }
        cmd
    }

    /// Start a long-lived container in interactive mode.
    ///
    /// The container runs with `CLAUDE_MODE=interactive`, which tells the
    /// entrypoint to match the Docker socket GID and then `sleep infinity`
    /// instead of running a single Claude prompt.
    ///
    /// The host's Docker socket is bind-mounted so Claude can run Docker
    /// commands. Security flags depend on the `loom` parameter:
    ///
    /// - `loom: false` — Standard mode with `--security-opt no-new-privileges`
    /// - `loom: true` — Checkpoint-compatible with `--net=host`,
    ///   `seccomp=unconfined`, and `apparmor=unconfined` (required by CRIU)
    ///
    /// Credentials are injected after the container starts via `inject_credentials`.
    pub fn start(
        image: &str,
        verbose: bool,
        workspace: Option<&str>,
        docker_socket: &str,
        loom: bool,
    ) -> Result<Self> {
        Self::start_with_host(image, verbose, workspace, docker_socket, loom, None, &[])
    }

    /// Start a container, optionally targeting a remote Docker host.
    ///
    /// `extra_env` allows passing additional environment variables (e.g.,
    /// `HTTPS_PROXY` for remote mode).
    pub fn start_with_host(
        image: &str,
        verbose: bool,
        workspace: Option<&str>,
        docker_socket: &str,
        loom: bool,
        docker_host: Option<&str>,
        extra_env: &[(&str, &str)],
    ) -> Result<Self> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(), // Detached mode
            // Mount host Docker socket for DooD
            "-v".into(),
            format!("{docker_socket}:/var/run/docker.sock"),
        ];

        if loom {
            // Checkpoint-compatible flags (validated in DOOD test_docker_checkpoint.sh):
            // --net=host: containerd#12141 workaround (netns bind-mount failure)
            // seccomp=unconfined: CRIU uses syscalls blocked by default profile
            // apparmor=unconfined: AppArmor blocks CRIU operations
            args.extend([
                "--net=host".into(),
                "--security-opt".into(),
                "seccomp=unconfined".into(),
                "--security-opt".into(),
                "apparmor=unconfined".into(),
            ]);
        } else {
            // Standard security: prevent privilege escalation
            args.extend([
                "--security-opt".into(),
                "no-new-privileges".into(),
            ]);
        }

        args.extend([
            "--env".into(),
            "CLAUDE_MODE=interactive".into(),
        ]);

        for (key, val) in extra_env {
            args.extend(["--env".into(), format!("{key}={val}")]);
        }

        // Optional workspace mount
        if let Some(ws) = workspace {
            args.push("-v".into());
            args.push(format!("{ws}:/workspace"));
        }

        args.push(image.into());

        if verbose {
            eprintln!("[claude-dind] docker {}", args.join(" "));
        }

        let output = Self::docker_cmd_with_host(docker_host)
            .args(&args)
            .output()
            .context("Failed to start Docker container. Is Docker running?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to start container: {stderr}");
        }

        let container_id = String::from_utf8(output.stdout)
            .context("Container ID is not valid UTF-8")?
            .trim()
            .to_string();

        eprintln!("[claude-dind] Container started: {}", &container_id[..12]);

        Ok(Self {
            container_id,
            image: image.to_string(),
            docker_host: docker_host.map(|s| s.to_string()),
        })
    }

    /// Attach to an existing running container by ID or name.
    pub fn attach(container_id: &str) -> Result<Self> {
        Self::attach_with_host(container_id, None)
    }

    /// Attach to an existing running container, optionally via a remote Docker host.
    pub fn attach_with_host(container_id: &str, docker_host: Option<&str>) -> Result<Self> {
        let manager = Self {
            container_id: container_id.to_string(),
            image: String::new(),
            docker_host: docker_host.map(|s| s.to_string()),
        };

        if !manager.is_running()? {
            bail!("Container {container_id} is not running");
        }

        // Get the image name from the running container
        let output = manager
            .docker_cmd()
            .args(["inspect", "--format", "{{.Config.Image}}", container_id])
            .output()
            .context("Failed to inspect container")?;

        let image = String::from_utf8(output.stdout)
            .unwrap_or_default()
            .trim()
            .to_string();

        Ok(Self {
            container_id: container_id.to_string(),
            image,
            docker_host: docker_host.map(|s| s.to_string()),
        })
    }

    /// Check if the container is still running.
    pub fn is_running(&self) -> Result<bool> {
        let output = self
            .docker_cmd()
            .args([
                "inspect",
                "--format",
                "{{.State.Running}}",
                &self.container_id,
            ])
            .output()
            .context("Failed to inspect container")?;

        let running = String::from_utf8(output.stdout)
            .unwrap_or_default()
            .trim()
            .to_string();

        Ok(running == "true")
    }

    /// Inject credentials into the running container.
    ///
    /// Writes the credential JSON to the claude user's home directory
    /// inside the container, matching what the entrypoint does in prompt mode.
    pub fn inject_credentials(&self, creds_json: &str) -> Result<()> {
        // Create the directory
        let status = self
            .docker_cmd()
            .args([
                "exec",
                &self.container_id,
                "mkdir",
                "-p",
                "/home/claude/.claude",
            ])
            .status()
            .context("Failed to create credentials directory in container")?;

        if !status.success() {
            bail!("Failed to create credentials directory in container");
        }

        // Write credentials via stdin to avoid them appearing in process args
        let mut child = self
            .docker_cmd()
            .args([
                "exec",
                "-i",
                &self.container_id,
                "sh",
                "-c",
                "cat > /home/claude/.claude/.credentials.json && \
                 chmod 600 /home/claude/.claude/.credentials.json && \
                 chown -R claude:claude /home/claude/.claude",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("Failed to inject credentials into container")?;

        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            stdin
                .write_all(creds_json.as_bytes())
                .context("Failed to write credentials to container")?;
        }
        drop(child.stdin.take());

        let status = child.wait().context("Failed to wait for credential injection")?;
        if !status.success() {
            bail!("Credential injection failed");
        }

        Ok(())
    }

    /// Wait for the container to be ready (Docker socket accessible).
    ///
    /// Polls `docker info` inside the container to verify the mounted Docker
    /// socket is accessible. Warns instead of failing if Docker is not
    /// available — Claude can still function without Docker access.
    pub fn wait_for_ready(&self, timeout_secs: u32) -> Result<()> {
        eprintln!("[claude-dind] Waiting for container to be ready...");

        for elapsed in 0..timeout_secs {
            let output = self
                .docker_cmd()
                .args([
                    "exec",
                    &self.container_id,
                    "docker",
                    "info",
                ])
                .output()
                .context("Failed to check Docker status inside container")?;

            if output.status.success() {
                eprintln!("[claude-dind] Container ready, Docker accessible (took {elapsed}s).");
                return Ok(());
            }

            std::thread::sleep(std::time::Duration::from_secs(1));
        }

        eprintln!(
            "[claude-dind] WARNING: Docker not accessible inside container after {timeout_secs}s. \
             Docker commands may not work, but Claude sessions will still function."
        );
        Ok(())
    }

    /// Stop and remove the container.
    pub fn stop(&self) -> Result<()> {
        eprintln!(
            "[claude-dind] Stopping container {}...",
            &self.container_id[..12.min(self.container_id.len())]
        );

        let _ = self
            .docker_cmd()
            .args(["rm", "-f", &self.container_id])
            .output();

        Ok(())
    }

    /// Get the short (12 char) container ID for display.
    pub fn short_id(&self) -> &str {
        &self.container_id[..12.min(self.container_id.len())]
    }

    // ── Checkpoint (Loom) operations ───────────────────────────────────

    /// Create a CRIU checkpoint of the running container.
    ///
    /// 1. Strips credentials from the container filesystem
    /// 2. Purges stale containerd blobs (moby#42900 workaround)
    /// 3. Creates the checkpoint with `--leave-running`
    /// 4. Re-injects credentials
    pub fn checkpoint(&self, checkpoint_name: &str, creds_json: &str) -> Result<()> {
        // Strip credentials before snapshotting
        let _ = self
            .docker_cmd()
            .args([
                "exec",
                &self.container_id,
                "rm",
                "-f",
                "/home/claude/.claude/.credentials.json",
            ])
            .output();

        // Purge stale containerd content blobs
        self.purge_containerd_blobs()?;

        // Create the checkpoint (container keeps running)
        let output = self
            .docker_cmd()
            .args([
                "checkpoint",
                "create",
                "--leave-running",
                &self.container_id,
                checkpoint_name,
            ])
            .output()
            .context("Failed to run docker checkpoint create")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("docker checkpoint create failed: {stderr}");
        }

        // Re-inject credentials
        self.inject_credentials(creds_json)?;

        Ok(())
    }

    /// Restore the container from a named checkpoint.
    ///
    /// 1. Stops the container (kills all exec sessions)
    /// 2. Purges stale containerd blobs
    /// 3. Starts from the checkpoint
    /// 4. Waits for readiness
    /// 5. Re-injects fresh credentials
    pub fn restore_checkpoint(&self, checkpoint_name: &str, creds_json: &str) -> Result<()> {
        // Stop the container
        let output = self
            .docker_cmd()
            .args(["stop", &self.container_id])
            .output()
            .context("Failed to stop container for restore")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("docker stop failed: {stderr}");
        }

        // Purge stale containerd content blobs
        self.purge_containerd_blobs()?;

        // Start from checkpoint
        let output = self
            .docker_cmd()
            .args([
                "start",
                "--checkpoint",
                checkpoint_name,
                &self.container_id,
            ])
            .output()
            .context("Failed to restore from checkpoint")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("docker start --checkpoint failed: {stderr}");
        }

        // Wait for container to be ready
        self.wait_for_ready(10)?;

        // Inject fresh credentials
        self.inject_credentials(creds_json)?;

        Ok(())
    }

    /// Purge stale containerd content blobs (moby#42900 workaround).
    ///
    /// Without this, checkpoint create/restore can fail with "content
    /// already exists" errors.
    ///
    /// When `docker_host` is set (remote mode), runs the blob purge in a
    /// privileged sibling container through the remote Docker API instead
    /// of using local `sudo ctr`.
    fn purge_containerd_blobs(&self) -> Result<()> {
        if self.docker_host.is_some() {
            // Remote mode: run blob purge via a privileged sibling container
            let output = self
                .docker_cmd()
                .args([
                    "run", "--rm", "--privileged", "--pid=host",
                    "-v", "/run/containerd:/run/containerd",
                    "alpine:latest", "sh", "-c",
                    "apk add --no-cache containerd-ctr >/dev/null 2>&1; \
                     ctr -n moby content ls -q | xargs -r -n1 ctr -n moby content rm 2>/dev/null; true",
                ])
                .output();

            if let Ok(output) = output {
                if !output.status.success() {
                    eprintln!(
                        "[claude-dind] WARNING: Remote blob purge returned non-zero (non-fatal)"
                    );
                }
            }
        } else {
            // Local mode: use sudo ctr directly
            let output = Command::new("sudo")
                .args(["ctr", "-n", "moby", "content", "ls", "-q"])
                .output();

            if let Ok(output) = output {
                if output.status.success() {
                    let blobs = String::from_utf8_lossy(&output.stdout);
                    for blob in blobs.lines() {
                        let blob = blob.trim();
                        if !blob.is_empty() {
                            let _ = Command::new("sudo")
                                .args(["ctr", "-n", "moby", "content", "rm", blob])
                                .output();
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Check if Docker experimental mode is enabled (required for checkpoints).
    pub fn ensure_experimental() -> Result<()> {
        Self::ensure_experimental_with_host(None)
    }

    /// Check if Docker experimental mode is enabled, optionally on a remote host.
    pub fn ensure_experimental_with_host(docker_host: Option<&str>) -> Result<()> {
        let output = Self::docker_cmd_with_host(docker_host)
            .args(["info", "--format", "{{.ExperimentalBuild}}"])
            .output()
            .context("Failed to check Docker experimental mode")?;

        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if value != "true" {
            bail!(
                "Docker experimental mode is not enabled.\n\
                 Loom mode requires CRIU checkpointing which needs Docker experimental.\n\
                 Enable it by adding {{\"experimental\": true}} to /etc/docker/daemon.json\n\
                 and restarting Docker: sudo systemctl restart docker"
            );
        }

        Ok(())
    }

    /// List existing checkpoints for this container.
    #[allow(dead_code)]
    pub fn list_checkpoints(&self) -> Result<Vec<String>> {
        let output = self
            .docker_cmd()
            .args([
                "checkpoint",
                "ls",
                &self.container_id,
                "--format",
                "{{.Name}}",
            ])
            .output()
            .context("Failed to list checkpoints")?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let names: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        Ok(names)
    }

    /// Remove a Docker checkpoint by name.
    pub fn remove_checkpoint(&self, checkpoint_name: &str) -> Result<()> {
        let output = self
            .docker_cmd()
            .args([
                "checkpoint",
                "rm",
                &self.container_id,
                checkpoint_name,
            ])
            .output()
            .context("Failed to remove checkpoint")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("docker checkpoint rm failed: {stderr}");
        }

        Ok(())
    }
}
