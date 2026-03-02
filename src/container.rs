use anyhow::{bail, Context, Result};
use std::process::Command;

/// Manages a long-lived Docker-in-Docker container for interactive mode.
///
/// Instead of the ephemeral one-container-per-prompt model, interactive mode
/// starts a single privileged DinD container that stays alive. Individual
/// Claude sessions are spawned inside it via `docker exec`.
pub struct ContainerManager {
    pub container_id: String,
    #[allow(dead_code)]
    pub image: String,
}

impl ContainerManager {
    /// Start a long-lived DinD container in interactive mode.
    ///
    /// The container runs with `CLAUDE_MODE=interactive`, which tells the
    /// entrypoint to start dockerd and then `sleep infinity` instead of
    /// running a single Claude prompt.
    ///
    /// Credentials are injected after the container starts via `inject_credentials`.
    pub fn start(image: &str, verbose: bool) -> Result<Self> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),           // Detached mode
            "--privileged".into(), // Required for DinD
            "--env".into(),
            "CLAUDE_MODE=interactive".into(),
        ];

        args.push(image.into());

        if verbose {
            eprintln!("[claude-dind] docker {}", args.join(" "));
        }

        let output = Command::new("docker")
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
        })
    }

    /// Attach to an existing running container by ID or name.
    pub fn attach(container_id: &str) -> Result<Self> {
        let manager = Self {
            container_id: container_id.to_string(),
            image: String::new(),
        };

        if !manager.is_running()? {
            bail!("Container {container_id} is not running");
        }

        // Get the image name from the running container
        let output = Command::new("docker")
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
        })
    }

    /// Check if the container is still running.
    pub fn is_running(&self) -> Result<bool> {
        let output = Command::new("docker")
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
        let status = Command::new("docker")
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
        let mut child = Command::new("docker")
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

    /// Wait for the Docker daemon inside the container to be ready.
    pub fn wait_for_dockerd(&self, timeout_secs: u32) -> Result<()> {
        eprintln!("[claude-dind] Waiting for Docker daemon inside container...");

        for elapsed in 0..timeout_secs {
            let output = Command::new("docker")
                .args([
                    "exec",
                    &self.container_id,
                    "docker",
                    "info",
                ])
                .output()
                .context("Failed to check Docker daemon status")?;

            if output.status.success() {
                eprintln!("[claude-dind] Docker daemon ready (took {elapsed}s).");
                return Ok(());
            }

            std::thread::sleep(std::time::Duration::from_secs(1));
        }

        bail!("Docker daemon failed to start within {timeout_secs}s");
    }

    /// Stop and remove the container.
    pub fn stop(&self) -> Result<()> {
        eprintln!(
            "[claude-dind] Stopping container {}...",
            &self.container_id[..12.min(self.container_id.len())]
        );

        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container_id])
            .output();

        Ok(())
    }

    /// Get the short (12 char) container ID for display.
    pub fn short_id(&self) -> &str {
        &self.container_id[..12.min(self.container_id.len())]
    }
}
