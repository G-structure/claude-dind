# claude-dind: Running Claude Code in Docker-in-Docker with Host Credentials

## Table of Contents

- [Background and Motivation](#background-and-motivation)
- [How Claude Code Authentication Works](#how-claude-code-authentication-works)
  - [The OAuth 2.0 Flow](#the-oauth-20-flow)
  - [Token Format and Storage](#token-format-and-storage)
  - [Token Refresh](#token-refresh)
  - [Cross-Platform Credential Storage](#cross-platform-credential-storage)
- [Architecture Overview](#architecture-overview)
- [Design Decisions](#design-decisions)
  - [Why Rust?](#why-rust)
  - [Why Shell Out to `security` Instead of Using a Crate?](#why-shell-out-to-security-instead-of-using-a-crate)
  - [Why stdin Piping Instead of Volume Mounts or Environment Variables?](#why-stdin-piping-instead-of-volume-mounts-or-environment-variables)
  - [Why a Single DinD Image Instead of Nested Containers?](#why-a-single-dind-image-instead-of-nested-containers)
  - [Why a Non-Root User Inside the Container?](#why-a-non-root-user-inside-the-container)
  - [Why --dangerously-skip-permissions?](#why---dangerously-skip-permissions)
- [Component Deep Dive](#component-deep-dive)
  - [The Rust CLI (`src/main.rs`)](#the-rust-cli-srcmainrs)
  - [The Dockerfile (`docker/Dockerfile`)](#the-dockerfile-dockerdockerfile)
  - [The Entrypoint Script (`docker/entrypoint.sh`)](#the-entrypoint-script-dockerentrypointsh)
- [Data Flow: End to End](#data-flow-end-to-end)
- [Security Model](#security-model)
- [Usage](#usage)
- [Troubleshooting](#troubleshooting)
- [Limitations and Future Work](#limitations-and-future-work)

---

## Background and Motivation

Claude Code is Anthropic's CLI tool that lets developers interact with Claude directly
from their terminal. When you subscribe to Claude Max or Claude Pro, Claude Code
authenticates via an OAuth 2.0 flow against `claude.ai` -- not via API keys. This
means your credentials are tied to your browser-based login and stored in your operating
system's credential manager (macOS Keychain on Mac, `libsecret` or a file fallback on
Linux).

The problem this project solves: **how do you run Claude Code inside a Docker container
while using your existing Max/Pro subscription credentials from the host?**

This matters for several use cases:

1. **Sandboxed agent execution** -- Run Claude Code in an isolated environment where it
   can execute arbitrary commands (including Docker commands) without risking the host.
2. **Reproducible environments** -- Ensure Claude Code operates in a consistent Linux
   environment regardless of the host OS.
3. **Agent swarm infrastructure** -- Spin up multiple Claude Code instances in parallel,
   each in its own container, all authenticated with the same subscription.

The challenge is that Claude Code's authentication system was not designed for this. The
tokens live in the macOS Keychain, and there is no documented mechanism to transplant
them into a container. This project reverse-engineers the authentication system and
builds a bridge between the host credential store and the containerized Claude Code
instance.

---

## How Claude Code Authentication Works

This section documents what we discovered by reverse-engineering the Claude Code binary
(a compiled Bun-bundled Node.js application distributed as a Mach-O arm64 executable).

### The OAuth 2.0 Flow

Claude Code uses **OAuth 2.0 Authorization Code flow with PKCE** (Proof Key for Code
Exchange, using the S256 challenge method). This is the same flow used by many modern
CLI tools (GitHub CLI, Azure CLI, etc.) to authenticate users via their browser.

When you run `claude` for the first time:

1. The CLI generates a random `code_verifier` and derives a `code_challenge` from it
   using SHA-256.
2. It spins up a **local HTTP server** on `http://localhost:<random_port>/callback` to
   receive the OAuth redirect.
3. It opens your browser to the authorization URL:
   ```
   https://claude.ai/oauth/authorize
     ?client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e
     &redirect_uri=http://localhost:<port>/callback
     &response_type=code
     &code_challenge=<base64url_encoded_challenge>
     &code_challenge_method=S256
     &scope=user:profile user:inference user:sessions:claude_code user:mcp_servers
   ```
4. You authenticate on claude.ai in your browser.
5. The browser redirects back to `localhost:<port>/callback?code=<auth_code>`.
6. The CLI exchanges the authorization code for tokens:
   ```
   POST https://platform.claude.com/v1/oauth/token
   Content-Type: application/x-www-form-urlencoded

   grant_type=authorization_code
   code=<auth_code>
   redirect_uri=http://localhost:<port>/callback
   client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e
   code_verifier=<original_verifier>
   ```
7. The server responds with an access token, refresh token, and expiry.

**Key endpoints** (extracted from the binary via `strings`):

| Purpose | URL |
|---------|-----|
| Authorization (consumer) | `https://claude.ai/oauth/authorize` |
| Authorization (platform) | `https://platform.claude.com/oauth/authorize` |
| Token exchange | `https://platform.claude.com/v1/oauth/token` |
| Success redirect | `https://platform.claude.com/oauth/code/success?app=claude-code` |
| Manual callback | `https://platform.claude.com/oauth/code/callback` |
| API key creation | `https://api.anthropic.com/api/oauth/claude_cli/create_api_key` |
| Roles | `https://api.anthropic.com/api/oauth/claude_cli/roles` |
| MCP proxy | `https://mcp-proxy.anthropic.com/v1/mcp/{server_id}` |

The **OAuth client ID** is `9d1c250a-e61b-44d9-88ed-5944d1962f5e`. This is a public
client -- no client secret is required. The beta header used is `oauth-2025-04-20`.

### Token Format and Storage

The credential is a JSON blob stored with the following structure:

```json
{
  "claudeAiOauth": {
    "accessToken": "sk-ant-oat01-...",
    "refreshToken": "sk-ant-ort01-...",
    "expiresAt": 1772415973294,
    "scopes": [
      "user:inference",
      "user:mcp_servers",
      "user:profile",
      "user:sessions:claude_code"
    ],
    "subscriptionType": "max",
    "rateLimitTier": "default_claude_max_20x"
  },
  "mcpOAuth": { ... }
}
```

**Token prefix conventions:**

| Prefix | Meaning |
|--------|---------|
| `sk-ant-oat01-` | OAuth Access Token |
| `sk-ant-ort01-` | OAuth Refresh Token |

The `subscriptionType` field is `"max"` or `"pro"` and determines which models and rate
limits you get. The `rateLimitTier` field (e.g., `default_claude_max_20x`) controls the
specific rate limiting bucket.

**OAuth scopes for Max/Pro subscriptions:**

| Scope | Purpose |
|-------|---------|
| `user:inference` | Grants actual model access (the key scope) |
| `user:profile` | Access to user profile information |
| `user:sessions:claude_code` | Session management for Claude Code |
| `user:mcp_servers` | Access to MCP (Model Context Protocol) servers |

There is a separate scope set for the console/API key flow: `org:create_api_key` and
`user:profile`. These are not used for Max/Pro subscription authentication.

### Token Refresh

Access tokens expire (the `expiresAt` field is a Unix timestamp in milliseconds). When
an access token expires, Claude Code automatically refreshes it:

```
POST https://platform.claude.com/v1/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token
refresh_token=sk-ant-ort01-...
client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e
scope=user:profile user:inference user:sessions:claude_code user:mcp_servers
```

The response includes a new `access_token`, optionally a new `refresh_token`, and an
`expires_in` value. The updated credentials are written back to storage. This means that
even if the access token in the credential blob has expired, the **refresh token** will
still work -- Claude Code will automatically obtain a fresh access token on startup.

### Cross-Platform Credential Storage

**macOS:** Stored in the **macOS Keychain** (`login.keychain-db`):
- Service name: `Claude Code-credentials`
- Account: your OS username
- The credential is stored as a "generic password" (the JSON blob is the password value)

You can extract it manually:
```bash
security find-generic-password -s "Claude Code-credentials" -a "$(whoami)" -w
```

**Linux:** Stored as a **plaintext JSON file** at `~/.claude/.credentials.json`. This
is the file-based fallback used when `libsecret` (the Linux equivalent of Keychain) is
not available. Claude Code uses `keytar` (a Node.js credential storage library) which
automatically falls back to file storage on Linux.

This difference in storage mechanisms is what makes the containerization approach work:
on macOS, we extract from the Keychain; inside the Linux container, we write to the file
path that Claude Code expects.

---

## Architecture Overview

```
                          macOS Host
 ┌──────────────────────────────────────────────────────┐
 │                                                      │
 │  macOS Keychain                                      │
 │  ┌──────────────────────────┐                        │
 │  │ Claude Code-credentials  │                        │
 │  │ (JSON blob)              │                        │
 │  └────────────┬─────────────┘                        │
 │               │                                      │
 │               │ security find-generic-password -w     │
 │               │                                      │
 │  ┌────────────▼─────────────┐                        │
 │  │ claude-dind (Rust CLI)   │                        │
 │  │                          │                        │
 │  │  1. Extract creds        │                        │
 │  │  2. Validate JSON        │                        │
 │  │  3. Spawn docker run     │                        │
 │  │  4. Pipe creds to stdin  │                        │
 │  │  5. Drop stdin (EOF)     │                        │
 │  │  6. Stream output        │                        │
 │  └────────────┬─────────────┘                        │
 │               │                                      │
 │               │ docker run --privileged -i --rm       │
 │               │ --env CLAUDE_PROMPT="..."             │
 │               │                                      │
 └───────────────┼──────────────────────────────────────┘
                 │
    ┌────────────▼──────────────────────────────────────┐
    │  Docker Container (claude-dind:latest)             │
    │  Base: docker:dind + nodejs + claude-code          │
    │                                                    │
    │  entrypoint.sh:                                    │
    │                                                    │
    │  Phase 1: cat (stdin) → $CREDS_JSON               │
    │           Validate with jq                         │
    │                                                    │
    │  Phase 2: Write to                                 │
    │           /home/claude/.claude/.credentials.json    │
    │           chmod 600                                │
    │                                                    │
    │  Phase 3: dockerd-entrypoint.sh dockerd &          │
    │           Wait for docker info to succeed           │
    │                                                    │
    │  Phase 4: su -l claude -c "claude -p ..."          │
    │           (runs as non-root user)                  │
    │                                                    │
    │  Phase 5: rm .credentials.json                     │
    │           kill dockerd                             │
    │           exit with claude's exit code             │
    │                                                    │
    └───────────────────────────────────────────────────┘
```

The system has three components:

1. **Rust CLI** (`claude-dind`) -- Runs on the macOS host. Extracts credentials from
   the Keychain, manages the Docker lifecycle, and pipes credentials into the container.

2. **Docker image** (`claude-dind:latest`) -- A single image based on `docker:dind` with
   Node.js and Claude Code installed. Provides both a Docker daemon (for DinD) and the
   Claude Code CLI.

3. **Entrypoint script** -- Orchestrates the container startup: reads credentials from
   stdin, writes them to disk, starts the Docker daemon, runs Claude Code as a non-root
   user, then cleans up.

---

## Design Decisions

### Why Rust?

Rust was chosen for the host-side CLI because:

1. **Single binary distribution.** `cargo build --release` produces one static-ish
   binary with no runtime dependencies. No Python virtualenvs, no Node.js installations,
   no shell script quoting nightmares.

2. **Correct process management.** Rust's `std::process::Command` gives precise control
   over stdin/stdout/stderr piping, which is critical for the credential injection flow.
   The borrow checker ensures the stdin pipe is dropped (closed) at exactly the right
   moment.

3. **Consistency with the parent project.** The `agent_swarm` project already uses Rust
   for its CLI tooling.

4. **Minimal dependencies.** The project uses only four crates: `anyhow` (error
   handling), `clap` (argument parsing), `serde`/`serde_json` (JSON validation). No
   HTTP clients, no async runtimes, no credential libraries.

### Why Shell Out to `security` Instead of Using a Crate?

The Rust ecosystem has crates for macOS Keychain access (`security-framework`,
`keyring`, `keychain-services`). We chose to shell out to the `security` CLI tool
instead because:

1. **Code signing requirements.** The `security-framework` crate uses the Security
   framework's C API, which on modern macOS requires the binary to be code-signed with
   specific entitlements to access Keychain items created by other applications. An
   unsigned binary gets `errSecMissingEntitlement`. The `security` CLI tool, being an
   Apple-signed system binary, already has these entitlements.

2. **Simplicity.** The `security find-generic-password -s "Claude Code-credentials" -a
   <username> -w` command does exactly what we need in one line. The Keychain UI will
   prompt the user to allow access if needed -- this is actually a feature, not a bug,
   as it provides a clear authorization point.

3. **No native compilation issues.** The `security-framework` crate links against
   Security.framework, which means cross-compilation becomes harder. Shelling out to
   `security` has zero native dependencies.

The tradeoff is that this approach only works on macOS. A future enhancement could add
Linux support by reading `~/.claude/.credentials.json` directly when not on macOS.

### Why stdin Piping Instead of Volume Mounts or Environment Variables?

We considered three approaches for injecting credentials into the container:

**Option A: Environment variable.** Pass the JSON as `-e CLAUDE_CREDS='{...}'`.
- Problem: Environment variables are visible in `docker inspect`, in `/proc/<pid>/environ`
  inside the container, in Docker's event logs, and in the process listing on the host
  (`ps auxe`). The credential JSON contains long-lived refresh tokens that should not be
  this easily exposed.

**Option B: Volume mount.** Write a temp file on the host and mount it with `-v`.
- Problem: The credential touches disk on the host. Even with a temp file that's deleted
  after, there's a window where the token exists as a file, and it may survive in
  filesystem journals or swap. This violates the design goal of credentials never touching
  the host filesystem.

**Option C: stdin piping (chosen).** Write the JSON to the container's stdin, then close
the pipe.
- The credential exists only in memory on the host (in the Rust process's heap).
- The pipe is a kernel-level construct -- the data flows directly from the Rust process
  to the container's PID 1 without intermediate storage.
- Once the Rust process drops the stdin handle, the pipe is closed and the data cannot
  be re-read.
- Inside the container, the credential is written to a file only for the duration of the
  Claude Code process, then immediately deleted.

The stdin approach provides the strongest security guarantees with the simplest
implementation.

### Why a Single DinD Image Instead of Nested Containers?

We evaluated three architectures:

**Option A: True nesting** (outer DinD container starts an inner Claude Code container).
- Requires piping credentials through two container layers.
- Race conditions with dockerd startup inside the outer container.
- Two Dockerfiles to maintain.
- More complex debugging.

**Option B: Single custom DinD image (chosen).**
- One image based on `docker:dind` that also has Node.js and Claude Code.
- Claude Code runs directly in the DinD container alongside the Docker daemon.
- stdin piping is trivial -- only one layer.
- One Dockerfile, one entrypoint.

**Option C: DinD outer with baked-in inner image.**
- The outer image contains the inner image via `docker save`/`docker load`.
- Extremely large outer image.
- Complex build pipeline.

Option B wins on simplicity. The "Docker-in-Docker" aspect still works: the container
runs a real Docker daemon, so if Claude Code's prompt involves building or running Docker
containers (common in agent workflows), it has a working `docker` command available.
The DinD is for Claude's use, not for isolating Claude from itself.

### Why a Non-Root User Inside the Container?

This was discovered during testing, not planned in advance. Claude Code v2.x refuses to
run with `--dangerously-skip-permissions` when the effective user is root:

```
--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons
```

This is a safety check in Claude Code itself. The solution is to create a dedicated
`claude` user inside the container and run the `claude` process via `su -l claude`. The
entrypoint still runs as root (needed for starting `dockerd`), but drops to the `claude`
user for the actual Claude Code execution.

The `claude` user has:
- A home directory at `/home/claude` (where `.claude/.credentials.json` is written)
- `sudo` access with `NOPASSWD` (so Claude Code can run privileged commands if needed)
- Membership in the `docker` group (so it can use the Docker CLI without sudo)

### Why --dangerously-skip-permissions?

Claude Code normally prompts for permission before executing commands (file writes, shell
commands, etc.). In a non-interactive container context where stdin is consumed by the
credential pipe, there is no way for the user to respond to permission prompts. The
`--dangerously-skip-permissions` flag tells Claude Code to auto-approve all tool uses.

This is acceptable because:
1. The container is ephemeral (`--rm` deletes it after exit).
2. The container is isolated from the host filesystem.
3. The user explicitly chose to run this command and provided the prompt.
4. The DinD environment means even Docker operations are contained.

---

## Component Deep Dive

### The Rust CLI (`src/main.rs`)

The CLI has five functions:

**`extract_credentials()`** -- Calls `security find-generic-password` to read the
credential JSON from the macOS Keychain. It determines the account name from the `USER`
or `LOGNAME` environment variable. After extraction, it validates that the JSON parses
correctly and contains the expected `claudeAiOauth.accessToken` field. This fail-fast
validation catches the case where the user hasn't logged into Claude Code yet.

**`resolve_docker_context()`** -- Finds the `docker/` directory containing the Dockerfile.
It checks three locations: an explicit `--docker-context` argument, the path relative to
the binary's location (for installed binaries), and the path relative to the current
working directory (for development). This means `cargo run -- --build "..."` works from
the project root without any extra arguments.

**`build_image()`** -- Runs `docker build -t <tag> .` in the context directory. In
non-verbose mode, it passes `--quiet` to suppress layer-by-layer output.

**`run_container()`** -- The core function. It spawns `docker run` with:
- `--privileged` (required for DinD -- dockerd needs kernel capabilities)
- `-i` (keep stdin open so the pipe works)
- `--rm` (auto-remove container after exit, unless `--keep` is set)
- `--env CLAUDE_PROMPT=...` (pass the user's prompt)
- `--env CLAUDE_FLAGS=...` (optional extra flags)

The function uses `Stdio::piped()` for stdin and `Stdio::inherit()` for stdout/stderr.
After spawning, it writes the credential JSON to the child's stdin, then drops the
`ChildStdin` handle. In Rust, dropping the handle closes the pipe's write end, which
sends EOF to the container. This is the mechanism that tells the entrypoint "I'm done
sending credentials, you can proceed."

**`run()`** -- Orchestrates the flow: build (optional) -> extract credentials -> run
container. Returns the container's exit code, which the `main()` function forwards as
the process exit code.

### The Dockerfile (`docker/Dockerfile`)

Built on `docker:dind` (Alpine Linux with Docker daemon and CLI).

Layer by layer:

1. **`apk add nodejs npm bash jq shadow sudo`** -- Installs Node.js (for Claude Code),
   bash (the entrypoint uses bash features), jq (for credential validation), shadow
   (provides `useradd`/`usermod`), and sudo (for the claude user).

2. **`npm install -g @anthropic-ai/claude-code`** -- Installs Claude Code globally.
   This places the `claude` binary in `/usr/local/bin/`.

3. **`useradd -m -s /bin/bash claude`** -- Creates the non-root user. Adds sudo access
   and Docker group membership.

4. **Pre-creates `~/.claude/` and `~/.claude.json`** -- The `.claude.json` file with
   `{"hasCompletedOnboarding": true}` bypasses the first-run onboarding prompt that
   would otherwise block non-interactive execution.

5. **Creates `/workspace`** -- The working directory where Claude Code operates.

### The Entrypoint Script (`docker/entrypoint.sh`)

The entrypoint runs as root (PID 1 in the container) and has five phases:

**Phase 1: Read credentials from stdin.**
- First checks if stdin is a terminal (`[ -t 0 ]`). If it is, the container was started
  interactively without piped input -- this is an error.
- Reads all of stdin into `$CREDS_JSON` using `cat`. Because the Rust CLI has already
  closed the pipe by this point, `cat` returns immediately with the full JSON blob.
- Validates the JSON with `jq -e '.claudeAiOauth.accessToken'`. This catches corrupted
  or truncated credentials before Claude Code tries to use them.

**Phase 2: Write credentials to disk.**
- Writes the JSON to `/home/claude/.claude/.credentials.json` -- the path Claude Code
  checks on Linux for its file-based credential storage.
- Sets `chmod 600` (owner read/write only) and `chown claude:claude`.

**Phase 3: Start the Docker daemon.**
- Runs `dockerd-entrypoint.sh dockerd &` in the background. This is the standard DinD
  startup script included in the `docker:dind` image.
- Polls `docker info` in a loop, waiting up to 30 seconds for the daemon to be ready.
  In practice, dockerd starts in under 1 second on OrbStack and 2-5 seconds on Docker
  Desktop.

**Phase 4: Run Claude Code.**
- Reads the prompt from the `CLAUDE_PROMPT` environment variable.
- Runs `su -l claude -c "claude -p <prompt> --dangerously-skip-permissions"`.
- The `su -l` starts a login shell for the `claude` user. The explicit `PATH` export
  ensures `/usr/local/bin` (where `claude` is installed) is in the path, since `su -l`
  resets the environment.

**Phase 5: Cleanup.**
- Deletes `.credentials.json` from disk.
- Kills the dockerd process.
- Exits with Claude Code's exit code, so the Rust CLI can forward it to the caller.

---

## Data Flow: End to End

```
Step 1: Rust CLI reads macOS Keychain
        security find-generic-password -s "Claude Code-credentials" -a "<your-username>" -w
        → returns JSON blob (in memory, never written to disk)

Step 2: Rust CLI validates JSON
        serde_json::from_str() → check .claudeAiOauth.accessToken exists

Step 3: Rust CLI spawns Docker child process
        Command::new("docker")
          .args(["run", "--privileged", "-i", "--rm", "--env", "CLAUDE_PROMPT=...", "claude-dind:latest"])
          .stdin(Stdio::piped())
          .stdout(Stdio::inherit())
          .stderr(Stdio::inherit())
          .spawn()

Step 4: Rust CLI writes credentials to child's stdin pipe
        child.stdin.write_all(json_bytes)

Step 5: Rust CLI drops stdin handle
        } // ChildStdin dropped here → write end of pipe closed → EOF sent

Step 6: Container entrypoint reads stdin
        CREDS_JSON=$(cat)  ← reads until EOF, gets full JSON

Step 7: Container validates with jq
        echo "$CREDS_JSON" | jq -e '.claudeAiOauth.accessToken'

Step 8: Container writes credential file
        echo "$CREDS_JSON" > /home/claude/.claude/.credentials.json

Step 9: Container starts dockerd
        dockerd-entrypoint.sh dockerd &
        while ! docker info; do sleep 1; done

Step 10: Container runs Claude Code as non-root user
         su -l claude -c "claude -p '$PROMPT' --dangerously-skip-permissions"

Step 11: Claude Code reads ~/.claude/.credentials.json, authenticates with
         Anthropic's OAuth servers using the access token (or refreshes if expired)

Step 12: Claude Code executes the prompt, output streams to container stdout

Step 13: Container stdout/stderr inherited by Rust CLI → streams to user's terminal

Step 14: Claude Code exits → entrypoint deletes .credentials.json → kills dockerd

Step 15: Container exits → Docker removes it (--rm) → all traces gone

Step 16: Rust CLI forwards container's exit code as its own
```

---

## Security Model

**Threat: Credential exposure on host filesystem.**
Mitigation: Credentials are extracted from Keychain into Rust process memory and piped
directly to Docker's stdin. They never exist as a file on the host.

**Threat: Credential exposure in Docker metadata.**
Mitigation: Credentials are passed via stdin, not environment variables. `docker inspect`
will not show them. The `CLAUDE_PROMPT` env var is visible, but it contains only the
user's prompt, not credentials.

**Threat: Credential persistence in container.**
Mitigation: The entrypoint explicitly deletes `.credentials.json` before exiting. The
`--rm` flag ensures the container's entire filesystem is destroyed. Even if the container
crashes before cleanup, the next invocation starts fresh.

**Threat: Credential interception during pipe transfer.**
Mitigation: Unix pipes are kernel-level constructs. Data flows directly between process
file descriptors without hitting disk. The pipe is not accessible to other processes on
the system (only the parent and child process have the file descriptors).

**Threat: Docker `--privileged` escalation.**
Accepted risk: `--privileged` grants the container full kernel capabilities. This is
required for DinD (dockerd needs to create cgroups, mount filesystems, etc.). The
container is ephemeral and isolated. If this is unacceptable for your threat model,
consider using Docker socket mounting instead of true DinD, which trades container
isolation for reduced privilege requirements.

**Threat: Concurrent token use triggers rate limiting or revocation.**
Observation: OAuth tokens are bearer tokens with no device binding. Using the same tokens
on the host and in a container simultaneously works, but may trigger rate limiting if
both are making API calls. The `rateLimitTier` field controls the bucket.

---

## Usage

### Prerequisites

- macOS (for Keychain access)
- Docker Desktop or OrbStack running
- Claude Code installed and logged in on the host (`claude` should work in your terminal)
- Rust toolchain (`cargo`)

### Building

```bash
cd /path/to/claude-dind

# Build the Rust CLI
cargo build --release

# Build the Docker image
./target/release/claude-dind --build "test prompt"
# Or manually:
docker build -t claude-dind:latest docker/
```

### Running

```bash
# Basic usage: run a prompt
./target/release/claude-dind "List the files in the current directory"

# With extra Claude flags
./target/release/claude-dind --claude-flags "--output-format stream-json" "Write hello world in Python"

# Keep the container after exit (for debugging)
./target/release/claude-dind --keep "Describe the Docker environment"

# Debug: print extracted credentials
./target/release/claude-dind --dump-creds "ignored"

# Verbose: show Docker command being run
./target/release/claude-dind -v "Hello"

# Use a different image tag
./target/release/claude-dind --image my-custom-claude:v2 "Hello"
```

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error (Docker, credential, or runtime failure) |
| 2 | Credential validation error (missing, empty, or malformed) |
| 3 | dockerd failed to start within timeout |
| Other | Forwarded from Claude Code's own exit code |

---

## Troubleshooting

**"Keychain access failed"**
- Run `claude` on the host first to complete the OAuth login flow.
- If the Keychain is locked, macOS will show a GUI dialog asking for your login password.

**"--dangerously-skip-permissions cannot be used with root/sudo privileges"**
- This means the entrypoint is running Claude as root. Check that the `claude` user was
  created correctly in the Dockerfile and that the `su -l claude` command in the
  entrypoint is working.

**"claude: command not found" inside container**
- The non-root user's PATH may not include `/usr/local/bin`. The entrypoint exports PATH
  explicitly to fix this. If you've modified the entrypoint, ensure the PATH export is
  present.

**"dockerd failed to start within 30s"**
- Ensure your Docker runtime supports `--privileged` containers. OrbStack and Docker
  Desktop both support this. Some cloud container runtimes (ECS, Cloud Run) do not.

**Verbose dockerd output in stderr**
- The DinD Docker daemon logs to stderr. This is normal. The Claude Code output will
  appear interspersed with dockerd log lines. A future enhancement could redirect dockerd
  logs to a file inside the container.

---

## Limitations and Future Work

1. **macOS-only credential extraction.** The Rust CLI currently only supports macOS
   Keychain. Adding Linux support (reading `~/.claude/.credentials.json`) would require
   detecting the OS and choosing the appropriate extraction method.

2. **No interactive mode.** The container runs `claude -p` (print mode), which executes
   a single prompt and exits. An interactive mode would require passing through a TTY,
   which conflicts with the stdin credential pipe.

3. **dockerd noise.** The Docker daemon's logs go to stderr and mix with Claude's output.
   Redirecting dockerd to a log file inside the container would clean up the output.

4. **No token refresh persistence.** If Claude Code refreshes the access token inside the
   container, the new token is written to `.credentials.json` inside the container -- but
   the container is destroyed on exit. The host Keychain still has the old (possibly
   expired) access token. This is fine because the refresh token is long-lived and Claude
   Code on the host will refresh independently.

5. **Single-prompt limitation.** Each invocation starts a new container, starts dockerd,
   and runs one prompt. For multiple prompts, a persistent container with a command queue
   would be more efficient.

6. **Image size.** The `claude-dind:latest` image includes the full Docker daemon,
   Node.js, and Claude Code. It's roughly 400-500MB. A slimmer variant without DinD
   (just Node.js + Claude Code) could be offered for use cases that don't need Docker
   inside the container.
