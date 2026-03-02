//! # claude-dind
//!
//! A CLI tool that extracts Claude Code OAuth credentials from the macOS Keychain
//! and runs Claude Code inside a Docker container with the host's Docker socket
//! mounted (Docker-out-of-Docker).
//!
//! ## Modes
//!
//! ### Prompt mode (`claude-dind prompt "..."`)
//!
//! The original mode: runs a single Claude Code prompt in an ephemeral container.
//!
//! 1. Extracts the OAuth credential JSON from macOS Keychain using the `security` CLI.
//! 2. Validates the JSON structure (checks for `claudeAiOauth.accessToken`).
//! 3. Spawns a `docker run -i` process with the host Docker socket mounted.
//! 4. Pipes the credential JSON into the container's stdin, then closes the pipe (EOF).
//! 5. The container's entrypoint reads the credentials, writes them to
//!    `~/.claude/.credentials.json`, matches the socket GID, and runs
//!    `claude -p "<prompt>" --dangerously-skip-permissions`.
//! 6. Claude's output streams directly to the user's terminal.
//! 7. On exit, credentials are deleted inside the container, and `--rm` destroys it.
//!
//! ### Interactive mode (`claude-dind interactive`)
//!
//! A tmux-style terminal multiplexer for managing multiple Claude Code sessions
//! running inside a single long-lived container.
//!
//! 1. Starts a long-lived container with `CLAUDE_MODE=interactive` and the Docker
//!    socket mounted.
//! 2. Injects credentials via `docker exec`.
//! 3. Launches a ratatui TUI with shadow-terminal for virtual terminal emulation.
//! 4. Users create/switch/kill Claude sessions with tmux-style keybindings (Ctrl-b prefix).
//! 5. Detaching (`Ctrl-b d`) exits the TUI but keeps the container alive.
//!    Re-attach with `claude-dind interactive --attach <container-id>`.
//!
//! ## Security
//!
//! - Credentials are held in Rust process memory only — never written to a file on the host.
//! - The stdin pipe is a kernel-level construct; data flows directly between processes.
//! - Inside the container, credentials exist on disk only during the `claude` process,
//!   then are explicitly deleted. The `--rm` flag destroys the container filesystem.
//! - The prompt is passed via environment variable (`CLAUDE_PROMPT`), not stdin,
//!   so it does not interfere with the credential pipe.
//! - `--security-opt no-new-privileges` prevents privilege escalation inside the container.
//! - No `--privileged` flag — the container runs with default capabilities only.

mod container;
mod credentials;
mod multiplexer;
mod render;
mod session;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

/// Default path to the Docker socket on macOS/Linux.
const DEFAULT_DOCKER_SOCKET: &str = "/var/run/docker.sock";

#[derive(Parser, Debug)]
#[command(
    name = "claude-dind",
    version,
    about = "Run Claude Code in a Docker container with host Docker socket access",
    long_about = "Extracts Claude Code OAuth tokens from the macOS Keychain and runs \
                  Claude Code inside a Docker container with the host's Docker socket \
                  mounted (Docker-out-of-Docker). Supports both single-prompt and \
                  interactive multiplexer modes."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a single prompt in an ephemeral container (original behavior).
    #[command(
        after_help = "EXAMPLES:\n  \
                      # First run (builds the Docker image, then runs a prompt):\n  \
                      claude-dind prompt --build \"Write a Python hello world script\"\n\n  \
                      # Subsequent runs (image is cached):\n  \
                      claude-dind prompt \"List all files in /workspace\"\n\n  \
                      # Mount a host directory as the workspace:\n  \
                      claude-dind prompt -w ./my-project \"Describe the project structure\"\n\n  \
                      # Debug credential extraction:\n  \
                      claude-dind prompt --dump-creds \"ignored\"\n\n  \
                      # Keep container alive after exit for inspection:\n  \
                      claude-dind prompt --keep \"Describe the Docker environment\""
    )]
    Prompt {
        /// The prompt or command to pass to Claude Code.
        prompt: String,

        /// Build the Docker image before running the container.
        #[arg(long)]
        build: bool,

        /// Docker image tag to use.
        #[arg(long, default_value = "claude-dind:latest")]
        image: String,

        /// Path to the docker/ context directory containing the Dockerfile.
        #[arg(long)]
        docker_context: Option<PathBuf>,

        /// Host directory to mount as /workspace in the container.
        #[arg(long, short)]
        workspace: Option<PathBuf>,

        /// Path to the Docker socket to mount into the container.
        #[arg(long, default_value = DEFAULT_DOCKER_SOCKET)]
        docker_socket: PathBuf,

        /// Additional flags to pass to `claude` inside the container.
        #[arg(long)]
        claude_flags: Option<String>,

        /// Keep the container after exit instead of removing it.
        #[arg(long)]
        keep: bool,

        /// Print the extracted credential JSON to stdout and exit.
        #[arg(long)]
        dump_creds: bool,

        /// Enable verbose output.
        #[arg(long, short)]
        verbose: bool,
    },

    /// Launch an interactive terminal multiplexer with multiple Claude sessions.
    #[command(
        after_help = "EXAMPLES:\n  \
                      # Start interactive mode (builds image if needed):\n  \
                      claude-dind interactive --build\n\n  \
                      # Mount a host directory as the workspace:\n  \
                      claude-dind interactive --build -w ./my-project\n\n  \
                      # Re-attach to a running container:\n  \
                      claude-dind interactive --attach abc123def456\n\n  \
                      # Keybindings (tmux-style, prefix: Ctrl-b):\n  \
                      #   c     Create new session\n  \
                      #   n/p   Next/previous session\n  \
                      #   0-9   Jump to session\n  \
                      #   x     Kill current session\n  \
                      #   d     Detach (container keeps running)\n  \
                      #   ?     Show help"
    )]
    Interactive {
        /// Build the Docker image before starting.
        #[arg(long)]
        build: bool,

        /// Docker image tag to use.
        #[arg(long, default_value = "claude-dind:latest")]
        image: String,

        /// Path to the docker/ context directory containing the Dockerfile.
        #[arg(long)]
        docker_context: Option<PathBuf>,

        /// Host directory to mount as /workspace in the container.
        #[arg(long, short)]
        workspace: Option<PathBuf>,

        /// Path to the Docker socket to mount into the container.
        #[arg(long, default_value = DEFAULT_DOCKER_SOCKET)]
        docker_socket: PathBuf,

        /// Attach to an existing running container instead of creating a new one.
        #[arg(long)]
        attach: Option<String>,

        /// Enable verbose output.
        #[arg(long, short)]
        verbose: bool,
    },
}

/// Entry point. Parses CLI args, runs the main logic, and exits with the appropriate code.
fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Prompt { .. } => run_prompt(&cli.command),
        Commands::Interactive { .. } => run_interactive(&cli.command),
    };

    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("[claude-dind] Error: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Run in prompt mode (original ephemeral container behavior).
fn run_prompt(cmd: &Commands) -> Result<i32> {
    let Commands::Prompt {
        prompt,
        build,
        image,
        docker_context,
        workspace,
        docker_socket,
        claude_flags,
        keep,
        dump_creds,
        verbose,
    } = cmd
    else {
        unreachable!()
    };

    // Step 1: Build image if requested
    if *build {
        let context = resolve_docker_context(docker_context.as_ref())?;
        build_image(&context, image, *verbose)?;
    }

    // Step 2: Extract credentials from macOS Keychain
    eprintln!("[claude-dind] Extracting credentials from macOS Keychain...");
    let creds = credentials::extract_credentials()?;
    eprintln!("[claude-dind] Credentials extracted successfully.");

    // Step 3: Debug mode — print credentials and exit
    if *dump_creds {
        println!("{creds}");
        return Ok(0);
    }

    // Step 4: Run the container with credentials piped via stdin
    eprintln!("[claude-dind] Starting container (image: {image})...");
    let socket_str = docker_socket.to_string_lossy();
    let workspace_str = workspace.as_ref().map(|w| {
        std::fs::canonicalize(w)
            .unwrap_or_else(|_| w.clone())
            .to_string_lossy()
            .to_string()
    });
    let exit_code = run_container(
        image,
        prompt,
        &creds,
        *keep,
        claude_flags.as_deref(),
        workspace_str.as_deref(),
        &socket_str,
        *verbose,
    )?;

    Ok(exit_code)
}

/// Run in interactive multiplexer mode.
fn run_interactive(cmd: &Commands) -> Result<i32> {
    let Commands::Interactive {
        build,
        image,
        docker_context,
        workspace,
        docker_socket,
        attach,
        verbose,
    } = cmd
    else {
        unreachable!()
    };

    // Step 1: Build image if requested
    if *build {
        let context = resolve_docker_context(docker_context.as_ref())?;
        build_image(&context, image, *verbose)?;
    }

    // Step 2: Start or attach to container
    let container = if let Some(container_id) = attach {
        eprintln!("[claude-dind] Attaching to container {container_id}...");
        container::ContainerManager::attach(container_id)?
    } else {
        eprintln!("[claude-dind] Extracting credentials from macOS Keychain...");
        let creds = credentials::extract_credentials()?;
        eprintln!("[claude-dind] Credentials extracted successfully.");

        let socket_str = docker_socket.to_string_lossy();
        let workspace_str = workspace.as_ref().map(|w| {
            std::fs::canonicalize(w)
                .unwrap_or_else(|_| w.clone())
                .to_string_lossy()
                .to_string()
        });

        eprintln!("[claude-dind] Starting interactive container (image: {image})...");
        let container = container::ContainerManager::start(
            image,
            *verbose,
            workspace_str.as_deref(),
            &socket_str,
        )?;

        // Wait for container to be ready (Docker socket accessible)
        container.wait_for_ready(10)?;

        // Inject credentials
        eprintln!("[claude-dind] Injecting credentials into container...");
        container.inject_credentials(&creds)?;
        eprintln!("[claude-dind] Credentials injected.");

        container
    };

    // Step 3: Run the multiplexer TUI
    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
    let detached = rt.block_on(multiplexer::run(&container, false))?;

    if detached {
        eprintln!(
            "[claude-dind] Detached. Container {} is still running.",
            container.short_id()
        );
        eprintln!(
            "[claude-dind] Re-attach with: claude-dind interactive --attach {}",
            container.short_id()
        );
    } else {
        // All sessions ended, stop the container
        container.stop()?;
    }

    Ok(0)
}

// ── Helper functions extracted from the original main.rs ──────────────────

/// Resolves the path to the `docker/` context directory containing the Dockerfile.
fn resolve_docker_context(explicit: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.clone());
    }

    // Try relative to the binary location (handles: target/release/claude-dind)
    if let Ok(exe) = env::current_exe() {
        let candidate = exe
            .parent()
            .unwrap_or(exe.as_ref())
            .join("../../docker");
        if candidate.join("Dockerfile").exists() {
            return Ok(candidate);
        }
    }

    // Try relative to cwd (handles: running from project root)
    let cwd_candidate = PathBuf::from("docker");
    if cwd_candidate.join("Dockerfile").exists() {
        return Ok(cwd_candidate);
    }

    bail!(
        "Cannot find docker/ context directory.\n\
         Use --docker-context to specify the path, or run from the project root."
    );
}

/// Builds the Docker image from the Dockerfile in the given context directory.
fn build_image(context_dir: &PathBuf, image_tag: &str, verbose: bool) -> Result<()> {
    eprintln!(
        "[claude-dind] Building image {image_tag} from {}",
        context_dir.display()
    );

    let mut cmd = Command::new("docker");
    cmd.args(["build", "-t", image_tag, "."]);
    cmd.current_dir(context_dir);

    if !verbose {
        cmd.arg("--quiet");
    }

    let status = cmd
        .status()
        .context("Failed to run `docker build`. Is Docker running?")?;

    if !status.success() {
        bail!("Docker build failed (exit code: {})", status);
    }

    eprintln!("[claude-dind] Image built successfully.");
    Ok(())
}

/// Spawns the Docker container, pipes credentials via stdin, and streams output.
fn run_container(
    image: &str,
    prompt: &str,
    creds_json: &str,
    keep: bool,
    claude_flags: Option<&str>,
    workspace: Option<&str>,
    docker_socket: &str,
    verbose: bool,
) -> Result<i32> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-i".into(),
        // Mount host Docker socket for DooD
        "-v".into(),
        format!("{docker_socket}:/var/run/docker.sock"),
        // Prevent privilege escalation
        "--security-opt".into(),
        "no-new-privileges".into(),
    ];

    if !keep {
        args.push("--rm".into());
    }

    // Optional workspace mount
    if let Some(ws) = workspace {
        args.push("-v".into());
        args.push(format!("{ws}:/workspace"));
    }

    args.extend(["--env".into(), format!("CLAUDE_PROMPT={prompt}")]);

    if let Some(flags) = claude_flags {
        args.extend(["--env".into(), format!("CLAUDE_FLAGS={flags}")]);
    }

    args.push(image.into());

    if verbose {
        eprintln!("[claude-dind] docker {}", args.join(" "));
    }

    let mut child = Command::new("docker")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to start Docker container. Is Docker running?")?;

    {
        let stdin = child.stdin.as_mut().context("Failed to open stdin pipe")?;
        stdin
            .write_all(creds_json.as_bytes())
            .context("Failed to write credentials to container stdin")?;
    }

    let status = child.wait().context("Failed to wait for container")?;
    Ok(status.code().unwrap_or(1))
}
