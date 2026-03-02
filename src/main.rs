//! # claude-dind
//!
//! A CLI tool that extracts Claude Code OAuth credentials from the macOS Keychain
//! and runs Claude Code inside a Docker-in-Docker (DinD) container.
//!
//! ## Problem
//!
//! Claude Code authenticates Max/Pro subscribers via OAuth 2.0, storing tokens in
//! the macOS Keychain. There is no documented way to use these credentials inside
//! a Docker container. This tool bridges that gap by extracting the tokens and
//! injecting them into a containerized Claude Code instance via stdin — without
//! ever writing credentials to disk on the host.
//!
//! ## How It Works
//!
//! 1. Extracts the OAuth credential JSON from macOS Keychain using the `security` CLI.
//! 2. Validates the JSON structure (checks for `claudeAiOauth.accessToken`).
//! 3. Spawns a `docker run --privileged -i` process for the DinD container.
//! 4. Pipes the credential JSON into the container's stdin, then closes the pipe (EOF).
//! 5. The container's entrypoint reads the credentials, writes them to
//!    `~/.claude/.credentials.json` (the Linux file-based credential path), starts
//!    the Docker daemon, and runs `claude -p "<prompt>" --dangerously-skip-permissions`.
//! 6. Claude's output streams directly to the user's terminal.
//! 7. On exit, credentials are deleted inside the container, and `--rm` destroys it.
//!
//! ## Security
//!
//! - Credentials are held in Rust process memory only — never written to a file on the host.
//! - The stdin pipe is a kernel-level construct; data flows directly between processes.
//! - Inside the container, credentials exist on disk only during the `claude` process,
//!   then are explicitly deleted. The `--rm` flag destroys the container filesystem.
//! - The prompt is passed via environment variable (`CLAUDE_PROMPT`), not stdin,
//!   so it does not interfere with the credential pipe.

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

/// Command-line arguments for claude-dind.
///
/// The only required argument is `prompt` — the task or question to send to Claude Code.
/// All other arguments control Docker image management, container lifecycle, and debugging.
#[derive(Parser, Debug)]
#[command(
    name = "claude-dind",
    version,
    about = "Run Claude Code in a Docker-in-Docker container with host credentials",
    long_about = "Extracts Claude Code OAuth tokens from the macOS Keychain and runs \
                  Claude Code inside a privileged Docker-in-Docker container. The \
                  container has a working Docker daemon, so Claude can build and run \
                  containers as part of its work. Credentials are piped via stdin and \
                  never touch disk on the host.",
    after_help = "EXAMPLES:\n  \
                  # First run (builds the Docker image, then runs a prompt):\n  \
                  claude-dind --build \"Write a Python hello world script\"\n\n  \
                  # Subsequent runs (image is cached):\n  \
                  claude-dind \"List all files in /workspace\"\n\n  \
                  # Debug credential extraction:\n  \
                  claude-dind --dump-creds \"ignored\"\n\n  \
                  # Keep container alive after exit for inspection:\n  \
                  claude-dind --keep \"Describe the Docker environment\"\n\n  \
                  # Pass extra flags to claude inside the container:\n  \
                  claude-dind --claude-flags \"--output-format stream-json\" \"Hello\""
)]
struct Cli {
    /// The prompt or command to pass to Claude Code.
    ///
    /// This is sent to `claude -p "<prompt>"` inside the container.
    /// Wrap in quotes if it contains spaces or special characters.
    prompt: String,

    /// Build the Docker image before running the container.
    ///
    /// Required on first use or after modifying the Dockerfile/entrypoint.
    /// The image is cached after the first build, so subsequent runs without
    /// `--build` will reuse it.
    #[arg(long)]
    build: bool,

    /// Docker image tag to use for the container.
    ///
    /// Defaults to `claude-dind:latest`. Use this to manage multiple image
    /// versions or to point at a custom image.
    #[arg(long, default_value = "claude-dind:latest")]
    image: String,

    /// Path to the docker/ context directory containing the Dockerfile.
    ///
    /// Auto-detected by default: checks relative to the binary location
    /// (for installed binaries) and relative to the current working directory
    /// (for development). Use this flag if auto-detection fails.
    #[arg(long)]
    docker_context: Option<PathBuf>,

    /// Additional flags to pass to `claude` inside the container.
    ///
    /// These are appended after `claude -p "<prompt>" --dangerously-skip-permissions`.
    /// Example: `--claude-flags "--output-format stream-json"`
    #[arg(long)]
    claude_flags: Option<String>,

    /// Keep the container after exit instead of removing it.
    ///
    /// By default, containers are started with `--rm` so they are automatically
    /// deleted when the process exits. Use `--keep` to preserve the container
    /// for debugging (inspect with `docker exec -it <id> bash`).
    #[arg(long)]
    keep: bool,

    /// Print the extracted credential JSON to stdout and exit.
    ///
    /// Useful for debugging credential extraction without running a container.
    /// WARNING: This prints your raw OAuth tokens to stdout. Use with caution.
    #[arg(long)]
    dump_creds: bool,

    /// Enable verbose output showing the Docker commands being executed.
    #[arg(long, short)]
    verbose: bool,
}

/// Extracts Claude Code OAuth credentials from the macOS Keychain.
///
/// Claude Code stores its OAuth tokens as a "generic password" in the macOS Keychain
/// with service name `"Claude Code-credentials"` and the current OS username as the
/// account. The credential is a JSON blob containing access tokens, refresh tokens,
/// subscription type, and OAuth scopes.
///
/// We shell out to the `security` CLI rather than using the `security-framework`
/// Rust crate because:
/// - The `security-framework` crate requires the binary to be code-signed with
///   Keychain entitlements. Unsigned binaries get `errSecMissingEntitlement`.
/// - The `security` CLI is an Apple-signed system binary that already has these
///   entitlements, and presents a Keychain access prompt to the user if needed.
///
/// # Returns
///
/// The raw JSON string from the Keychain, validated to contain
/// `claudeAiOauth.accessToken`.
///
/// # Errors
///
/// - If the `security` command is not found (not on macOS).
/// - If the Keychain entry doesn't exist (user hasn't logged in with `claude`).
/// - If the returned data is not valid JSON.
/// - If the JSON is missing the expected `claudeAiOauth.accessToken` field.
fn extract_credentials() -> Result<String> {
    // Determine the current username from environment variables.
    // macOS sets USER; some environments use LOGNAME instead.
    let username = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .context("Cannot determine username from USER or LOGNAME env vars")?;

    // Shell out to macOS `security` CLI to read the Keychain entry.
    // -s: service name (how Claude Code registers its credential)
    // -a: account name (the OS username)
    // -w: output only the password value (the JSON blob), not metadata
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-a",
            &username,
            "-w",
        ])
        .output()
        .context("Failed to execute `security` command. Are you on macOS?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Keychain access failed: {stderr}\n\
             Hint: Run `claude` on the host first to complete OAuth login."
        );
    }

    let creds = String::from_utf8(output.stdout)
        .context("Keychain returned non-UTF8 data")?
        .trim()
        .to_string();

    // Validate that the JSON has the expected structure before we pipe it
    // into a container. This catches the case where the Keychain entry exists
    // but contains unexpected data (e.g., from a different version of Claude Code).
    let parsed: serde_json::Value =
        serde_json::from_str(&creds).context("Keychain data is not valid JSON")?;

    parsed
        .get("claudeAiOauth")
        .and_then(|v| v.get("accessToken"))
        .context("Credential JSON missing claudeAiOauth.accessToken")?;

    Ok(creds)
}

/// Resolves the path to the `docker/` context directory containing the Dockerfile.
///
/// Checks three locations in order:
/// 1. An explicitly provided `--docker-context` path (if given).
/// 2. Relative to the binary's location: `<binary_dir>/../../docker/`. This handles
///    the case where the binary is at `target/release/claude-dind` and the Dockerfile
///    is at `docker/Dockerfile` in the project root.
/// 3. Relative to the current working directory: `./docker/`. This handles running
///    from the project root directly.
///
/// # Errors
///
/// Returns an error with a helpful message if no Dockerfile can be found.
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
///
/// Runs `docker build -t <image_tag> .` in the context directory. In non-verbose
/// mode, the `--quiet` flag suppresses layer-by-layer build output.
///
/// # Errors
///
/// - If Docker is not running or the `docker` command is not found.
/// - If the build fails (e.g., network issues pulling base images, syntax errors).
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
///
/// This is the core function that ties everything together. It:
///
/// 1. Constructs the `docker run` command with:
///    - `--privileged`: Required for Docker-in-Docker (dockerd needs kernel capabilities
///      like creating cgroups, mounting filesystems, managing network namespaces).
///    - `-i`: Keeps stdin open so we can pipe credential data into the container.
///    - `--rm`: Auto-removes the container on exit (unless `--keep` is set).
///    - `--env CLAUDE_PROMPT=...`: Passes the user's prompt as an environment variable
///      (not via stdin, since stdin is used for credentials).
///    - `--env CLAUDE_FLAGS=...`: Optional extra flags for the `claude` CLI.
///
/// 2. Configures stdio:
///    - `stdin(Stdio::piped())`: Creates a pipe we can write credentials to.
///    - `stdout(Stdio::inherit())`: Claude's output goes directly to the user's terminal.
///    - `stderr(Stdio::inherit())`: Error messages and dockerd logs go to the terminal.
///
/// 3. Writes the credential JSON to the child's stdin pipe, then drops the `ChildStdin`
///    handle. Dropping closes the pipe's write end, sending EOF to the container. This
///    is how the entrypoint knows "all credentials have been sent, proceed."
///
/// 4. Waits for the container to exit and returns its exit code.
///
/// # Arguments
///
/// * `image` - Docker image tag (e.g., "claude-dind:latest")
/// * `prompt` - The prompt/task to send to Claude Code
/// * `creds_json` - The raw credential JSON string from the Keychain
/// * `keep` - If true, don't pass `--rm` (keep container after exit)
/// * `claude_flags` - Optional extra flags appended to the `claude` command
/// * `verbose` - If true, print the full docker command before running
///
/// # Returns
///
/// The container's exit code (0 = success, non-zero = failure).
fn run_container(
    image: &str,
    prompt: &str,
    creds_json: &str,
    keep: bool,
    claude_flags: Option<&str>,
    verbose: bool,
) -> Result<i32> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--privileged".into(), // Required for DinD (dockerd needs kernel capabilities)
        "-i".into(),           // Keep stdin open for credential piping
    ];

    if !keep {
        args.push("--rm".into()); // Auto-remove container on exit
    }

    // Pass the prompt as an environment variable (not stdin — stdin is for credentials)
    args.extend(["--env".into(), format!("CLAUDE_PROMPT={prompt}")]);

    // Pass optional extra flags for the claude CLI
    if let Some(flags) = claude_flags {
        args.extend(["--env".into(), format!("CLAUDE_FLAGS={flags}")]);
    }

    args.push(image.into());

    if verbose {
        eprintln!("[claude-dind] docker {}", args.join(" "));
    }

    let mut child = Command::new("docker")
        .args(&args)
        .stdin(Stdio::piped()) // We will write credentials to this pipe
        .stdout(Stdio::inherit()) // Stream container output to user's terminal
        .stderr(Stdio::inherit()) // Stream container errors to user's terminal
        .spawn()
        .context("Failed to start Docker container. Is Docker running?")?;

    // Write the credential JSON to the container's stdin, then close the pipe.
    // The block scope ensures ChildStdin is dropped (pipe closed) immediately
    // after writing, which sends EOF to the container's entrypoint.
    {
        let stdin = child.stdin.as_mut().context("Failed to open stdin pipe")?;
        stdin
            .write_all(creds_json.as_bytes())
            .context("Failed to write credentials to container stdin")?;
    } // <-- ChildStdin is dropped here, closing the pipe and sending EOF

    let status = child.wait().context("Failed to wait for container")?;
    Ok(status.code().unwrap_or(1))
}

/// Entry point. Parses CLI args, runs the main logic, and exits with the appropriate code.
///
/// Exit codes:
/// - 0: Success (Claude completed the prompt)
/// - 1: General error (credential extraction, Docker, or runtime failure)
/// - Other: Forwarded from Claude Code's own exit code inside the container
fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = run(&cli);
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("[claude-dind] Error: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Main orchestration logic. Separated from `main()` for cleaner error handling.
///
/// Execution order:
/// 1. Build the Docker image (if `--build` was passed). This happens first so we
///    don't extract credentials unnecessarily if the build fails.
/// 2. Extract credentials from the macOS Keychain.
/// 3. If `--dump-creds` was passed, print the credentials and exit (debug mode).
/// 4. Otherwise, launch the Docker container with the credentials piped via stdin.
/// 5. Return the container's exit code.
fn run(cli: &Cli) -> Result<i32> {
    // Step 1: Build image if requested (before credential extraction, so a build
    // failure doesn't trigger an unnecessary Keychain access prompt)
    if cli.build {
        let context = resolve_docker_context(cli.docker_context.as_ref())?;
        build_image(&context, &cli.image, cli.verbose)?;
    }

    // Step 2: Extract credentials from macOS Keychain
    eprintln!("[claude-dind] Extracting credentials from macOS Keychain...");
    let creds = extract_credentials()?;
    eprintln!("[claude-dind] Credentials extracted successfully.");

    // Step 3: Debug mode — print credentials and exit
    if cli.dump_creds {
        println!("{creds}");
        return Ok(0);
    }

    // Step 4: Run the container with credentials piped via stdin
    eprintln!("[claude-dind] Starting container (image: {})...", cli.image);
    let exit_code = run_container(
        &cli.image,
        &cli.prompt,
        &creds,
        cli.keep,
        cli.claude_flags.as_deref(),
        cli.verbose,
    )?;

    Ok(exit_code)
}
