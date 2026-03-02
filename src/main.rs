use anyhow::{bail, Context, Result};
use clap::Parser;
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

#[derive(Parser, Debug)]
#[command(
    name = "claude-dind",
    version,
    about = "Run Claude Code in a Docker-in-Docker container with host credentials"
)]
struct Cli {
    /// The prompt or command to pass to Claude Code
    prompt: String,

    /// Build the Docker image before running
    #[arg(long)]
    build: bool,

    /// Docker image tag to use
    #[arg(long, default_value = "claude-dind:latest")]
    image: String,

    /// Path to the docker/ context directory (auto-detected by default)
    #[arg(long)]
    docker_context: Option<PathBuf>,

    /// Additional flags to pass to `claude` inside the container
    #[arg(long)]
    claude_flags: Option<String>,

    /// Keep the container after exit (don't use --rm)
    #[arg(long)]
    keep: bool,

    /// Print the extracted credential JSON to stdout and exit
    #[arg(long)]
    dump_creds: bool,

    /// Verbose output
    #[arg(long, short)]
    verbose: bool,
}

fn extract_credentials() -> Result<String> {
    let username = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .context("Cannot determine username from USER or LOGNAME env vars")?;

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

    // Validate structure
    let parsed: serde_json::Value =
        serde_json::from_str(&creds).context("Keychain data is not valid JSON")?;

    parsed
        .get("claudeAiOauth")
        .and_then(|v| v.get("accessToken"))
        .context("Credential JSON missing claudeAiOauth.accessToken")?;

    Ok(creds)
}

fn resolve_docker_context(explicit: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.clone());
    }

    // Try relative to the binary location
    if let Ok(exe) = env::current_exe() {
        let candidate = exe
            .parent()
            .unwrap_or(exe.as_ref())
            .join("../../docker");
        if candidate.join("Dockerfile").exists() {
            return Ok(candidate);
        }
    }

    // Try relative to cwd
    let cwd_candidate = PathBuf::from("docker");
    if cwd_candidate.join("Dockerfile").exists() {
        return Ok(cwd_candidate);
    }

    bail!(
        "Cannot find docker/ context directory.\n\
         Use --docker-context to specify the path, or run from the project root."
    );
}

fn build_image(context_dir: &PathBuf, image_tag: &str, verbose: bool) -> Result<()> {
    eprintln!("[claude-dind] Building image {image_tag} from {}", context_dir.display());

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
        "--privileged".into(),
        "-i".into(),
    ];

    if !keep {
        args.push("--rm".into());
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

    // Write credentials to container stdin, then close the pipe (sends EOF)
    {
        let stdin = child.stdin.as_mut().context("Failed to open stdin pipe")?;
        stdin
            .write_all(creds_json.as_bytes())
            .context("Failed to write credentials to container stdin")?;
    } // stdin dropped here → EOF sent to container

    let status = child.wait().context("Failed to wait for container")?;
    Ok(status.code().unwrap_or(1))
}

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

fn run(cli: &Cli) -> Result<i32> {
    // Step 1: Build image if requested (before credential extraction)
    if cli.build {
        let context = resolve_docker_context(cli.docker_context.as_ref())?;
        build_image(&context, &cli.image, cli.verbose)?;
    }

    // Step 2: Extract credentials
    eprintln!("[claude-dind] Extracting credentials from macOS Keychain...");
    let creds = extract_credentials()?;
    eprintln!("[claude-dind] Credentials extracted successfully.");

    if cli.dump_creds {
        println!("{creds}");
        return Ok(0);
    }

    // Step 3: Run container
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
