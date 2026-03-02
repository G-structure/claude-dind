# claude-dind: Running Claude Code in Docker with Host Credentials (Docker-out-of-Docker)

## Table of Contents

- [Background and Motivation](#background-and-motivation)
- [How Claude Code Authentication Works](#how-claude-code-authentication-works)
  - [The OAuth 2.0 Flow](#the-oauth-20-flow)
  - [Token Format and Storage](#token-format-and-storage)
  - [Token Refresh](#token-refresh)
  - [Cross-Platform Credential Storage](#cross-platform-credential-storage)
- [Architecture Overview](#architecture-overview)
  - [Prompt Mode](#prompt-mode-architecture)
  - [Interactive Mode](#interactive-mode-architecture)
  - [Loom Mode (Checkpoint/Restore)](#loom-mode-checkpointrestore)
- [Module Structure](#module-structure)
- [Design Decisions](#design-decisions)
  - [Why Rust?](#why-rust)
  - [Why Shell Out to `security` Instead of Using a Crate?](#why-shell-out-to-security-instead-of-using-a-crate)
  - [Why stdin Piping Instead of Volume Mounts or Environment Variables?](#why-stdin-piping-instead-of-volume-mounts-or-environment-variables)
  - [Why Docker-out-of-Docker Instead of Docker-in-Docker?](#why-docker-out-of-docker-instead-of-docker-in-docker)
  - [Why a Non-Root User Inside the Container?](#why-a-non-root-user-inside-the-container)
  - [Why --dangerously-skip-permissions?](#why---dangerously-skip-permissions)
  - [Why shadow-terminal?](#why-shadow-terminal)
  - [Why a Long-Lived Container for Interactive Mode?](#why-a-long-lived-container-for-interactive-mode)
  - [Why CRIU for Checkpoint/Restore?](#why-criu-for-checkpointrestore)
- [Component Deep Dive](#component-deep-dive)
  - [Credential Extraction (`src/credentials.rs`)](#credential-extraction-srccredentialsrs)
  - [Container Management (`src/container.rs`)](#container-management-srccontainerrs)
  - [Session Management (`src/session.rs`)](#session-management-srcsessionrs)
  - [Checkpoint Tree (`src/loom.rs`)](#checkpoint-tree-srcloomrs)
  - [Tree Visualization (`src/loom_render.rs`)](#tree-visualization-srcloom_renderrs)
  - [Terminal Rendering (`src/render.rs`)](#terminal-rendering-srcrenderrs)
  - [Multiplexer Event Loop (`src/multiplexer.rs`)](#multiplexer-event-loop-srcmultiplexerrs)
  - [CLI Entry Point (`src/main.rs`)](#cli-entry-point-srcmainrs)
  - [The Dockerfile (`docker/Dockerfile`)](#the-dockerfile-dockerdockerfile)
  - [The Entrypoint Script (`docker/entrypoint.sh`)](#the-entrypoint-script-dockerentrypointsh)
- [Data Flow](#data-flow)
  - [Prompt Mode: End to End](#prompt-mode-end-to-end)
  - [Interactive Mode: End to End](#interactive-mode-end-to-end)
  - [Loom Checkpoint Flow](#loom-checkpoint-flow)
  - [Loom Restore Flow](#loom-restore-flow)
- [Security Model](#security-model)
- [Usage](#usage)
  - [Prerequisites](#prerequisites)
  - [Building](#building)
  - [Prompt Mode](#prompt-mode-usage)
  - [Interactive Mode](#interactive-mode-usage)
  - [Loom Mode (Checkpoint/Restore)](#loom-mode-usage)
  - [Keybindings](#keybindings)
  - [Exit Codes](#exit-codes)
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
4. **Interactive multiplexing** -- Manage multiple concurrent Claude Code sessions in a
   single container through a tmux-style terminal multiplexer, enabling parallel
   workflows with session switching.
5. **Memory forking (loom)** -- Snapshot a running container's process state via CRIU
   checkpoints, navigate a tree of snapshots, and restore to any prior state. Like
   `git checkout` for agent memory — full process state (V8 heap, conversation context,
   open file descriptors) preserved at each node.

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

claude-dind operates in two modes: **prompt** (ephemeral, one-shot) and **interactive**
(long-lived, multiplexed).

### Prompt Mode Architecture

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
 │  │ claude-dind prompt "..." │                        │
 │  │                          │                        │
 │  │  1. Extract creds        │                        │
 │  │  2. Validate JSON        │                        │
 │  │  3. Spawn docker run     │                        │
 │  │  4. Pipe creds to stdin  │                        │
 │  │  5. Drop stdin (EOF)     │                        │
 │  │  6. Stream output        │                        │
 │  └────────────┬─────────────┘                        │
 │               │                                      │
 │               │ docker run -i --rm                    │
 │               │ -v /var/run/docker.sock:...            │
 │               │ --security-opt no-new-privileges       │
 │               │ --env CLAUDE_PROMPT="..."             │
 │               │                                      │
 └───────────────┼──────────────────────────────────────┘
                 │
    ┌────────────▼──────────────────────────────────────┐
    │  Docker Container (claude-dind:latest)             │
    │  Base: docker:cli + nodejs + claude-code           │
    │  Mount: /var/run/docker.sock (host socket)         │
    │                                                    │
    │  entrypoint.sh (CLAUDE_MODE=prompt):               │
    │                                                    │
    │  Phase 1: Match docker group GID to socket GID     │
    │           Verify Docker socket access               │
    │                                                    │
    │  Phase 2: cat (stdin) -> $CREDS_JSON               │
    │           Validate with jq                         │
    │                                                    │
    │  Phase 3: Write to                                 │
    │           /home/claude/.claude/.credentials.json    │
    │           chmod 600                                │
    │                                                    │
    │  Phase 4: su -l claude -c "claude -p ..."          │
    │           (runs as non-root user)                  │
    │                                                    │
    │  Phase 5: rm .credentials.json                     │
    │           exit with claude's exit code             │
    │                                                    │
    └───────────────────────────────────────────────────┘
```

### Interactive Mode Architecture

```
                          macOS Host
 ┌───────────────────────────────────────────────────────────┐
 │                                                           │
 │  macOS Keychain                                           │
 │  ┌──────────────────────────┐                             │
 │  │ Claude Code-credentials  │                             │
 │  └────────────┬─────────────┘                             │
 │               │                                           │
 │  ┌────────────▼───────────────────────────────────────┐   │
 │  │ claude-dind interactive (Rust TUI on host)         │   │
 │  │                                                    │   │
 │  │  ┌──────────────────────────────────────────────┐  │   │
 │  │  │ Multiplexer (ratatui + crossterm)            │  │   │
 │  │  │                                              │  │   │
 │  │  │  Status bar: [0:claude-1*] [1:claude-2]      │  │   │
 │  │  │  Ctrl-b prefix keybindings (tmux-style)      │  │   │
 │  │  │                                              │  │   │
 │  │  │  ┌────────────┐  ┌────────────┐              │  │   │
 │  │  │  │ Session 1  │  │ Session 2  │  ...         │  │   │
 │  │  │  │ (active)   │  │            │              │  │   │
 │  │  │  └─────┬──────┘  └─────┬──────┘              │  │   │
 │  │  │        │               │                     │  │   │
 │  │  │  shadow-terminal ActiveTerminal instances     │  │   │
 │  │  └────────┼───────────────┼─────────────────────┘  │   │
 │  │           │               │                        │   │
 │  │     docker exec      docker exec                   │   │
 │  └───────────┼───────────────┼────────────────────────┘   │
 │              │               │                            │
 └──────────────┼───────────────┼────────────────────────────┘
                │               │
   ┌────────────▼───────────────▼───────────────────────┐
   │  Long-lived Container (DooD)                        │
   │  (CLAUDE_MODE=interactive)                         │
   │                                                    │
   │  /var/run/docker.sock (mounted from host)          │
   │                                                    │
   │  /home/claude/.claude/.credentials.json             │
   │  (injected via docker exec after startup)          │
   │                                                    │
   │  claude session 1  (docker exec -it ... claude)    │
   │  claude session 2  (docker exec -it ... claude)    │
   │  ...                                               │
   └────────────────────────────────────────────────────┘
```

### Loom Mode (Checkpoint/Restore)

Loom mode (`--loom`) extends interactive mode with CRIU-based checkpoint/restore. The
container starts with relaxed security flags required by CRIU, and the multiplexer gains
additional keybindings for snapshot management.

```
                Checkpoint Tree (loom.json)
                ──────────────────────────
                * [1] initial                      2m ago
                ├─* [2] after-setup                1m ago
                │ └─* [4] experiment-B             30s ago
                └─* [3] experiment-A ●            45s ago  ← current

  Ctrl-b s  →  Take checkpoint  →  docker checkpoint create --leave-running
  Ctrl-b t  →  Show tree view   →  Navigate + Enter to restore
                                    docker stop → docker start --checkpoint
```

Key differences from standard interactive mode:

- **Container flags**: `--net=host`, `seccomp=unconfined`, `apparmor=unconfined` (CRIU
  requires syscalls and memory inspection blocked by default Docker security profiles)
- **Credential protocol**: Credentials are stripped before each checkpoint (so they are
  not captured in the CRIU image) and re-injected after checkpoint/restore
- **Containerd workaround**: Stale content blobs are purged via `ctr -n moby content rm`
  before each checkpoint/restore operation (moby#42900)
- **Session lifecycle**: On restore, all `docker exec` sessions are terminated (they are
  not children of PID 1). The multiplexer creates fresh sessions that pick up Claude
  Code's persisted conversation state from `~/.claude/` inside the container

The system has three components:

1. **Rust CLI** (`claude-dind`) -- Runs on the macOS host. Extracts credentials from
   the Keychain, manages Docker containers, and in interactive mode provides a terminal
   multiplexer TUI.

2. **Docker image** (`claude-dind:latest`) -- A single image based on `docker:cli` with
   Node.js and Claude Code installed. The host Docker socket is bind-mounted for Docker
   access (Docker-out-of-Docker).

3. **Entrypoint script** -- Orchestrates the container startup. In prompt mode: matches
   the Docker socket GID, reads credentials from stdin, runs Claude, cleans up. In
   interactive mode: matches the socket GID, then sleeps forever (sessions created via
   `docker exec`).

---

## Module Structure

```
src/
├── main.rs            CLI entry point (clap subcommands: prompt, interactive)
├── credentials.rs     macOS Keychain extraction via `security` CLI
├── container.rs       ContainerManager: start/stop/attach/checkpoint/restore (DooD)
├── session.rs         SessionManager: shadow-terminal ActiveTerminal + docker exec
├── multiplexer.rs     TUI event loop, Ctrl-b prefix keybindings, input dispatch
├── render.rs          ratatui rendering: TerminalWidget, status bar, help overlay
├── loom.rs            Checkpoint tree data model + JSON persistence
└── loom_render.rs     Tree visualization ratatui widget (git-log-style)

docker/
├── Dockerfile         docker:cli + Node.js + Claude Code + non-root user
└── entrypoint.sh      Container startup (prompt mode and interactive mode)

extern/
└── shadow-terminal/   Headless terminal emulator (gitignored, cloned separately)
```

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

3. **Ecosystem for TUI applications.** ratatui + crossterm provide a mature, well-tested
   foundation for building terminal multiplexer UIs. The shadow-terminal crate (built on
   wezterm-term) gives us full ANSI terminal emulation in-process.

4. **Consistency with the parent project.** The `agent_swarm` project already uses Rust
   for its CLI tooling.

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

**Option C: stdin piping (chosen for prompt mode).** Write the JSON to the container's
stdin, then close the pipe.
- The credential exists only in memory on the host (in the Rust process's heap).
- The pipe is a kernel-level construct -- the data flows directly from the Rust process
  to the container's PID 1 without intermediate storage.
- Once the Rust process drops the stdin handle, the pipe is closed and the data cannot
  be re-read.
- Inside the container, the credential is written to a file only for the duration of the
  Claude Code process, then immediately deleted.

**Interactive mode uses `docker exec` injection.** Since the container is already running
(no stdin pipe available), credentials are written via `docker exec -i <id> sh -c 'cat >
/home/claude/.claude/.credentials.json'` with the JSON piped to the exec's stdin. This
avoids credentials appearing in process arguments.

### Why Docker-out-of-Docker Instead of Docker-in-Docker?

We evaluated three Docker architectures:

**Option A: True DinD** (`docker:dind` base, `--privileged`, nested daemon).
- Requires `--privileged`, granting full kernel capabilities.
- Heavy: starts a full Docker daemon (containerd + dockerd) inside the container.
- Slow startup: must wait for the nested daemon to be ready (2-5s typical).
- Stronger isolation: containers created by Claude are truly nested.

**Option B: Docker-out-of-Docker (chosen).**
- One image based on `docker:cli` with Node.js and Claude Code.
- Host Docker socket is bind-mounted into the container.
- No `--privileged` — uses `--security-opt no-new-privileges` instead.
- Lighter weight and faster: no nested daemon, instant Docker access.
- Trade-off: containers created by Claude run as siblings on the host daemon.

**Option C: No Docker access.**
- Simplest, but Claude cannot build/run containers as part of its work.
- Too limiting for agent workflows that involve Docker commands.

Option B wins on the balance of security and capability. The `--privileged` flag in
Option A grants the container full kernel capabilities — a significant attack surface.
DooD removes this requirement while still giving Claude a working `docker` command.
The trade-off is that Claude's Docker commands execute on the host daemon, creating
sibling containers rather than nested ones. This is mitigated by `--security-opt
no-new-privileges` and running Claude as a non-root user.

### Why a Non-Root User Inside the Container?

This was discovered during testing, not planned in advance. Claude Code v2.x refuses to
run with `--dangerously-skip-permissions` when the effective user is root:

```
--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons
```

This is a safety check in Claude Code itself. The solution is to create a dedicated
`claude` user inside the container and run the `claude` process via `su -l claude`. The
entrypoint still runs as root (needed for matching the Docker socket GID), but drops to the `claude`
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
1. The container is ephemeral (`--rm` deletes it after exit) or isolated (interactive mode).
2. The container is isolated from the host filesystem.
3. The user explicitly chose to run this command and provided the prompt.
4. Docker operations via the mounted socket create sibling containers, not nested ones.

### Why shadow-terminal?

Interactive mode needs to display Claude Code's TUI output (which uses ANSI escape
sequences, colors, Unicode, cursor movement) inside a ratatui widget. This requires
full terminal emulation -- not just capturing raw bytes, but parsing them into a virtual
screen buffer.

[shadow-terminal](https://github.com/nichochar/shadow-terminal) provides this via:

1. **wezterm-term** -- A battle-tested terminal emulator core (from the WezTerm project)
   that parses VT100/xterm escape sequences into a cell grid with color and attribute
   metadata.

2. **portable-pty** -- Cross-platform PTY abstraction. Each Claude session runs inside a
   real PTY allocated by portable-pty, so programs like `docker exec -it` that require
   `isatty()` to return true work correctly.

3. **Channel-based I/O** -- `ActiveTerminal` exposes async channels: `send_input()` for
   keystrokes and `surface_output_rx` for rendered terminal state (as termwiz `Surface`
   objects containing cell grids with full color/attribute information).

The alternative would be implementing our own VT100 parser or using a less featureful
library. shadow-terminal gives us Claude Code's full TUI rendered correctly, including
syntax highlighting, spinners, and markdown formatting.

### Why a Long-Lived Container for Interactive Mode?

Prompt mode creates an ephemeral container per invocation. Interactive mode instead starts
a single container that stays alive:

1. **Avoids re-injecting credentials** for every new session. Credentials are injected
   once after startup.
2. **Avoids repeating socket GID matching** every time. The socket permissions are set
   up once at container startup.
3. **Enables detach/reattach.** The user can `Ctrl-b d` to detach from the TUI while the
   container continues running. `claude-dind interactive --attach <id>` reconnects.
4. **Shared filesystem.** Multiple Claude sessions can read/write the same `/workspace`,
   enabling collaborative workflows.

### Why CRIU for Checkpoint/Restore?

The loom feature needs to capture and restore the **full process state** of a running
Claude Code session — not just files, but the V8 heap, conversation context held in
memory, open file descriptors, and TCP connections. We evaluated three approaches:

**Option A: Application-level serialization.** Have Claude Code export its internal state
to disk, then reload it.
- Problem: Claude Code is a third-party binary. We have no control over its internal
  state management. Its conversation context, model state, and runtime are opaque.

**Option B: Filesystem-only snapshots.** Copy `~/.claude/` between checkpoints.
- Problem: This captures persisted state but misses in-memory state. A restored session
  would start cold — no in-flight conversation, no loaded model context.

**Option C: CRIU process checkpoint (chosen).** Use Linux's CRIU (Checkpoint/Restore In
Userspace) via Docker's checkpoint API to snapshot the entire container's process tree.
- Captures everything: memory pages, file descriptors, network sockets, signal state.
- Restore recreates the exact process state. Claude Code's V8 heap and conversation
  context are preserved byte-for-byte.
- Integrated with Docker: `docker checkpoint create` / `docker start --checkpoint`.
- Trade-off: Requires Linux host with Docker experimental mode and CRIU 4.2+ installed.
  Not available on macOS (CRIU not in Docker Desktop VM).

The CRIU approach requires relaxed security flags because CRIU uses ptrace and other
syscalls blocked by Docker's default seccomp profile:
- `--security-opt seccomp=unconfined` — allows CRIU's syscalls
- `--security-opt apparmor=unconfined` — allows CRIU's memory inspection
- `--net=host` — containerd#12141 workaround (netns bind-mount failure during restore)

These flags are only applied when `--loom` is passed. Without `--loom`, the container
uses the standard `--security-opt no-new-privileges` flag.

---

## Component Deep Dive

### Credential Extraction (`src/credentials.rs`)

`extract_credentials() -> Result<String>` -- Reads the credential JSON from the macOS
Keychain. Determines the account name from `$USER` or `$LOGNAME`. Calls `security
find-generic-password -s "Claude Code-credentials" -a <username> -w`. Parses the
returned JSON and validates that `claudeAiOauth.accessToken` exists. Returns the raw
JSON string.

### Container Management (`src/container.rs`)

`ContainerManager` wraps a long-lived container with the host Docker socket mounted. Key methods:

**Lifecycle:**
- **`start(image, verbose, workspace, docker_socket, loom)`** -- `docker run -d` with
  socket mount and `CLAUDE_MODE=interactive`. Security flags depend on the `loom`
  parameter: when `false`, uses `--security-opt no-new-privileges`; when `true`, uses
  `--net=host`, `seccomp=unconfined`, and `apparmor=unconfined` (required by CRIU).
  Returns the container ID. Optionally mounts a host workspace directory.
- **`attach(container_id)`** -- Connects to an already-running container. Verifies it is
  still running via `docker inspect`.
- **`inject_credentials(creds_json)`** -- Writes the credential JSON into the container
  via `docker exec -i <id> sh -c 'cat > /home/claude/.claude/.credentials.json'`. The
  JSON is piped through stdin to avoid appearing in process arguments.
- **`wait_for_ready(timeout)`** -- Polls `docker exec <id> docker info` to verify the
  Docker socket is accessible. Warns instead of failing if Docker is not available.
- **`stop()`** -- `docker rm -f <id>`.
- **`is_running()`** -- `docker inspect --format {{.State.Running}}`.
- **`short_id()`** -- First 12 characters of the container ID for display.

**Checkpoint operations (loom):**
- **`checkpoint(checkpoint_name, creds_json)`** -- Takes a CRIU checkpoint of the running
  container. Protocol: (1) strip credentials from container filesystem, (2) purge stale
  containerd blobs, (3) `docker checkpoint create --leave-running`, (4) re-inject fresh
  credentials. The `--leave-running` flag keeps the container alive after snapshotting.
- **`restore_checkpoint(checkpoint_name, creds_json)`** -- Restores the container from a
  named checkpoint. Protocol: (1) `docker stop` (kills all exec sessions), (2) purge
  stale containerd blobs, (3) `docker start --checkpoint <name>`, (4) wait for
  readiness, (5) inject fresh credentials.
- **`ensure_experimental()`** (static) -- Checks `docker info --format '{{.ExperimentalBuild}}'`.
  Returns a clear error if Docker experimental mode is not enabled.
- **`list_checkpoints()`** -- `docker checkpoint ls <id> --format '{{.Name}}'`.
- **`remove_checkpoint(checkpoint_name)`** -- `docker checkpoint rm <id> <name>`.
- **`purge_containerd_blobs()`** (private static) -- Runs `sudo ctr -n moby content ls -q`
  and removes each blob. Workaround for moby#42900 where stale content blobs cause
  checkpoint create/restore to fail with "content already exists" errors.

### Session Management (`src/session.rs`)

`SessionManager` creates and manages Claude Code sessions inside the container, each
wrapped in a shadow-terminal `ActiveTerminal`.

**Session creation** (`create(width, height)`):
```
bash -c "docker exec -it <container_id> su -l claude -c \
  'export PATH=/usr/local/bin:/usr/bin:/bin:$PATH && \
   cd /workspace && \
   claude --dangerously-skip-permissions'"
```
The command is wrapped in `bash -c` to ensure proper TTY inheritance from portable-pty's
PTY slave to docker exec. Each session gets a shadow-terminal `Config` with the specified
dimensions and a 5000-line scrollback buffer.

**Input** (`send_input(idx, bytes)`): Chunks input bytes into 128-byte buffers
(shadow-terminal's `BytesFromSTDIN` type) and sends them via the `ActiveTerminal`'s
async channel.

**Output** (`poll_output(idx)`): Drains the `surface_output_rx` channel. Handles two
variants:
- `Output::Complete(CompleteSurface)` -- Full screen replacement (new `Surface`).
- `Output::Diff(SurfaceDiff)` -- Incremental changes applied to the existing `Surface`.

Other methods: `kill()`, `cleanup_exited()`, `resize_all()`, `next()`, `prev()`,
`switch_to()`, `count()`, `set_container_id()`, `container_id()`.

The `set_container_id()` method exists for loom checkpoint restore: when the container is
restored from a checkpoint, the container ID remains the same but all `docker exec`
sessions are terminated. The multiplexer kills its sessions, the container is restored,
and fresh sessions are created using the same container ID.

### Checkpoint Tree (`src/loom.rs`)

Pure data model for the checkpoint tree. No Docker operations, no TUI rendering. Unit-
testable in isolation.

**`SnapshotNode`** -- Metadata for a single checkpoint:
- `id: u64` -- Monotonic counter (unique within the tree)
- `parent_id: Option<u64>` -- `None` for root nodes
- `label: String` -- User-provided name (e.g., "initial", "after-setup")
- `timestamp: u64` -- Unix seconds when the checkpoint was taken
- `checkpoint_name: String` -- Docker checkpoint name (e.g., `loom-3-after-setup`)
- `source_container_id: String` -- Container that was checkpointed
- `description: Option<String>` -- Optional longer description

**`LoomTree`** -- The full checkpoint tree:
- `schema_version: String` -- Always `"loom-v1"` for forward compatibility
- `next_id: u64` -- Monotonic counter for generating node IDs
- `nodes: HashMap<u64, SnapshotNode>` -- All nodes indexed by ID
- `current_node_id: Option<u64>` -- Which checkpoint the container is currently "on"

Key methods:
- `load_or_create(path)` -- Loads from JSON file or creates a new empty tree
- `save(path)` -- Persists to JSON with pretty-printing
- `add_node(parent_id, label, checkpoint_name, container_id)` -- Adds a new node,
  increments `next_id`, returns the new node's ID
- `get_children(id)` -- Returns child node IDs sorted by timestamp
- `roots()` -- Returns root node IDs (no parent) sorted by timestamp
- `remove_node(id)` -- Removes a node and all its descendants recursively. Also removes
  any `current_node_id` reference if the removed node was current.
- `build_flat_list()` -- DFS traversal producing a `Vec<FlatNode>` for rendering

**`FlatNode`** -- Flattened representation for the tree widget:
- `node_id`, `depth`, `label`, `timestamp`, `is_current`, `is_last_sibling`
- Computed by `dfs_flatten()` which tracks sibling position for tree prefix rendering

Helper functions:
- `sanitize_label(label)` -- Normalizes user input to filesystem-safe names (lowercase,
  non-alphanumeric chars replaced with hyphens, truncated to 40 chars)
- `relative_time(timestamp)` -- Formats timestamps as human-readable relative strings
  ("30s ago", "2m ago", "1h ago", "3d ago")

Persisted as JSON at `~/.claude-dind/loom.json` (configurable via `--loom-file`).

### Tree Visualization (`src/loom_render.rs`)

Git-log-style tree widget for navigating checkpoints, built on ratatui.

**`TreeViewState`** -- Navigation state for the tree view:
- `flat_nodes: Vec<FlatNode>` -- Flattened tree from `LoomTree::build_flat_list()`
- `selected: usize` -- Currently highlighted index
- `scroll: usize` -- Scroll offset for tall trees
- Methods: `refresh(tree)`, `up()`, `down()`, `selected_node_id()`

**`render_tree_view(frame, area, tree, state)`** -- Renders the full tree panel:
```
  Snapshot Tree (4 checkpoints)
  ─────────────────────────────
  * [1] initial                      2m ago
  ├─* [2] after-setup                1m ago
  │ └─* [4] experiment-B             30s ago
  └─* [3] experiment-A ●            45s ago

  j/k navigate  Enter restore  d delete  q back
```
- Header with checkpoint count
- Tree body with Unicode box-drawing prefixes (`├─`, `└─`, `│ `)
- Current node marked with `●`, selected node in reverse colors
- Relative timestamps right-aligned
- Footer with keybind hints

**`render_label_input(frame, area, label_buffer)`** -- Bottom overlay for naming a new
checkpoint. Rendered on top of the terminal view when the user presses `Ctrl-b s`.

Tree prefix computation tracks continuation lines per depth level using a `Vec<bool>` to
determine whether to draw `│ ` (continuing) or `  ` (last sibling) at each depth.

### Terminal Rendering (`src/render.rs`)

**View mode dispatch**: `render_frame()` dispatches based on `ViewMode`:
- **`Terminal`** -- Default: renders the active session's terminal output.
- **`TreeView`** -- Loom tree view (delegates to `loom_render::render_tree_view()`).
- **`LabelInput`** -- Terminal view with a label input overlay at the bottom.

`render_frame()` splits the terminal into two areas:
1. **Terminal area** (fills available space) -- Renders the active session's termwiz
   `Surface` via a custom `TerminalWidget`, or the tree view when in `TreeView` mode.
2. **Status bar** (1 line) -- Shows session tabs, an optional checkpoint indicator
   (`● snap:N` when loom checkpoints exist), and a help hint.

**`TerminalWidget`**: Implements `ratatui::Widget`. Iterates through `surface.screen_lines()`
and `line.cells()`, mapping each termwiz cell to a ratatui buffer cell. Converts:
- `ColorAttribute` (Default, PaletteIndex, TrueColor variants) to `ratatui::Color`
- `CellAttributes` (intensity, italic, underline, strikethrough, reverse) to `ratatui::Modifier`
- Cell text via `cell.str()`

**Status bar**: `[0:claude-1*] [1:claude-2] ● snap:3  Ctrl-b ? for help` -- Active
session highlighted in green, exited sessions dimmed, checkpoint indicator in magenta
(when loom checkpoints exist), help hint right-aligned in yellow.

**Help overlay**: Centered bordered panel listing all keybindings, rendered above the
terminal content. When loom mode is active, the overlay includes an additional section
with loom-specific keybindings (`s` for snapshot, `t` for tree view).

### Multiplexer Event Loop (`src/multiplexer.rs`)

`run(container, detach_on_exit, creds_json, loom_path, verbose) -> Result<bool>` -- Main
async event loop.

**State machine** for input handling:
- **Normal mode**: All keys forwarded to the active session, except `Ctrl-b` which
  enters prefix mode.
- **Prefix mode**: Next key is interpreted as a multiplexer command:
  - `c` -- Create new session
  - `n` / `p` -- Next / previous session
  - `0`-`9` -- Jump to session by index
  - `x` -- Kill current session
  - `d` -- Detach (returns `true` to keep container running)
  - `?` -- Show help overlay
  - `s` -- Take snapshot (loom only; enters LabelInput mode)
  - `t` -- Show tree view (loom only; enters TreeView mode)
  - `Ctrl-b` -- Send literal `Ctrl-b` (0x02) to session
- **Help mode**: Any key dismisses the overlay.
- **TreeView mode** (loom): `j`/`k` or arrows navigate the tree, `Enter` restores
  from the selected checkpoint, `d` deletes a checkpoint, `q`/`Esc` returns to
  terminal view.
- **LabelInput mode** (loom): User types a label for the new checkpoint. `Enter`
  confirms (takes the checkpoint), `Esc` cancels.

**Key-to-bytes conversion** (`key_event_to_bytes`): Translates crossterm `KeyEvent` into
PTY-compatible byte sequences. Handles Ctrl+key combinations (0x01-0x1a), UTF-8
characters, special keys (Enter=`\r`, Backspace=0x7f, Esc=0x1b), arrow keys, function
keys (F1-F12), Home, End, PageUp, PageDown, Insert, Delete.

**Session lifecycle**: Polls `task_handle.is_finished()` on each frame to detect when a
session's docker exec process has exited. Marks it `exited = true` for the renderer.

**Frame rate**: Polls for keyboard events with a 16ms timeout (~60fps), balancing
responsiveness with CPU usage.

**Logging**: Writes debug output to `/tmp/claude-dind-mux.log` with timestamps. Logs
session creation, output events (first 50 frames), task completion, and input errors.

**Loom orchestration** (when `--loom` is active): The multiplexer manages additional
state: `loom_tree: LoomTree`, `tree_state: TreeViewState`, `label_buffer: String`, and
`creds_json: String`. Snapshot flow: Ctrl-b s → type label → Enter → checkpoint created,
node added to tree. Restore flow: Ctrl-b t → navigate → Enter → kill all sessions →
restore checkpoint → create fresh session. The `creds_json` is passed to checkpoint/
restore operations for credential stripping and re-injection.

### CLI Entry Point (`src/main.rs`)

Two clap subcommands:

**`claude-dind prompt <prompt> [options]`** -- Original ephemeral mode.
- Extracts credentials, spawns `docker run -i --rm` with socket mount and security opts,
  pipes credentials via stdin, streams output. Options: `--build`, `--image`,
  `--docker-context`, `-w`/`--workspace`, `--docker-socket`, `--claude-flags`, `--keep`,
  `--dump-creds`, `-v`.

**`claude-dind interactive [options]`** -- Multiplexer mode.
- Starts (or attaches to) a long-lived container. Injects credentials. Creates a tokio
  runtime and runs the multiplexer TUI. On detach, prints the container ID for
  reattachment. On normal exit (all sessions ended), stops the container. Options:
  `--build`, `--image`, `--docker-context`, `-w`/`--workspace`, `--docker-socket`,
  `--attach <id>`, `-v`, `--loom`, `--loom-file`.

When `--loom` is passed:
- Calls `ContainerManager::ensure_experimental()` to verify Docker experimental mode
- Passes `loom: true` to `ContainerManager::start()` for checkpoint-compatible flags
- Resolves `--loom-file` path (defaults to `~/.claude-dind/loom.json`, supports `~`
  expansion) and creates the parent directory
- Passes `creds_json` and `loom_path` to `multiplexer::run()` for checkpoint operations

Helper functions: `resolve_docker_context()` (finds the `docker/` directory),
`build_image()` (runs `docker build`), `run_container()` (prompt mode Docker lifecycle).

### The Dockerfile (`docker/Dockerfile`)

Built on `docker:cli` (Alpine Linux with Docker CLI only — no daemon).

Layer by layer:

1. **`apk add nodejs npm bash jq shadow sudo`** -- Node.js (for Claude Code), bash
   (entrypoint uses bash features), jq (credential validation), shadow (`useradd`/
   `usermod`), sudo (for the claude user).

2. **`npm install -g @anthropic-ai/claude-code`** -- Installs Claude Code globally at
   `/usr/local/bin/claude`.

3. **`useradd -m -s /bin/bash claude`** -- Creates the non-root user with sudo access
   and Docker group membership.

4. **Pre-creates `~/.claude/` and `~/.claude.json`** -- Bypasses the first-run
   onboarding prompt with `{"hasCompletedOnboarding": true}`.

5. **Creates `/workspace`** -- Working directory for Claude Code operations.

### The Entrypoint Script (`docker/entrypoint.sh`)

Supports two modes via `$CLAUDE_MODE`:

**Both modes** start by matching the Docker socket GID:
- Detects the mounted socket's GID via `stat`.
- Adjusts the container's `docker` group GID with `groupmod` to match.
- Verifies socket access with `docker info` (warns if unavailable).

**Interactive mode** (`CLAUDE_MODE=interactive`):
- After GID matching, traps SIGTERM/SIGINT for graceful shutdown.
- Enters a `while true; do sleep 86400 & wait; done` loop.
- Sessions and credentials are managed externally via `docker exec`.

**Prompt mode** (`CLAUDE_MODE=prompt`, the default):
- Phase 2: Reads credential JSON from stdin via `cat`. Validates with `jq`.
- Phase 3: Writes to `/home/claude/.claude/.credentials.json` with `chmod 600`.
- Phase 4: Runs `su -l claude -c "claude -p <prompt> --dangerously-skip-permissions"`.
- Phase 5: Deletes credentials, exits with Claude's exit code.

---

## Data Flow

### Prompt Mode: End to End

```
Step 1:  Rust CLI reads macOS Keychain
         security find-generic-password -s "Claude Code-credentials" -w
         -> returns JSON blob (in memory, never written to disk)

Step 2:  Rust CLI validates JSON
         serde_json::from_str() -> check .claudeAiOauth.accessToken exists

Step 3:  Rust CLI spawns Docker child process
         docker run -i --rm -v /var/run/docker.sock:... --env CLAUDE_PROMPT="..." claude-dind:latest

Step 4:  Rust CLI writes credentials to child's stdin pipe
         child.stdin.write_all(json_bytes)

Step 5:  Rust CLI drops stdin handle -> EOF sent to container

Step 6:  Container entrypoint matches Docker socket GID, verifies access

Step 7:  Container entrypoint reads stdin (gets full JSON)

Step 8:  Container validates with jq, writes to ~/.claude/.credentials.json

Step 9:  Container runs Claude Code as non-root user

Step 10: Claude Code authenticates with Anthropic OAuth servers

Step 11: Claude Code executes the prompt, output streams to container stdout

Step 12: Container stdout inherited by Rust CLI -> streams to user's terminal

Step 13: Claude Code exits -> entrypoint deletes .credentials.json

Step 14: Container exits -> Docker removes it (--rm) -> all traces gone

Step 15: Rust CLI forwards container's exit code as its own
```

### Interactive Mode: End to End

```
Step 1:  Rust CLI reads macOS Keychain (same as prompt mode)

Step 2:  Rust CLI starts detached container
         docker run -d -v /var/run/docker.sock:... --env CLAUDE_MODE=interactive claude-dind:latest

Step 3:  Container entrypoint matches Docker socket GID,
         then enters infinite sleep loop

Step 4:  Rust CLI waits for container to be ready (polls docker exec <id> docker info)

Step 5:  Rust CLI injects credentials via docker exec
         echo $JSON | docker exec -i <id> sh -c 'cat > ~/.claude/.credentials.json'

Step 6:  Rust CLI enters TUI mode (crossterm raw mode + ratatui alternate screen)

Step 7:  Multiplexer creates first session:
         shadow-terminal spawns:
         bash -c "docker exec -it <id> su -l claude -c 'claude --dangerously-skip-permissions'"

Step 8:  Session I/O loop:
         - Keyboard input -> key_event_to_bytes -> send_input -> PTY -> docker exec -> claude
         - claude output -> PTY -> shadow-terminal -> Surface -> TerminalWidget -> ratatui

Step 9:  User can create/switch/kill sessions via Ctrl-b prefix commands

Step 10: On detach (Ctrl-b d): TUI exits, container keeps running
         User sees: "Re-attach with: claude-dind interactive --attach <short-id>"

Step 11: On reattach: Rust CLI connects to existing container, re-enters TUI
         (existing sessions are gone; new sessions are created)

Step 12: When all sessions end: container is stopped and removed
```

### Loom Checkpoint Flow

```
Step 1:  User presses Ctrl-b s in the multiplexer

Step 2:  Multiplexer enters LabelInput mode, renders input overlay

Step 3:  User types a label (e.g., "after-setup") and presses Enter

Step 4:  Multiplexer calls container.checkpoint("loom-2-after-setup", &creds_json)
         4a. docker exec <id> rm -f /home/claude/.claude/.credentials.json
             (credentials stripped before snapshot)
         4b. sudo ctr -n moby content ls -q | ... content rm
             (purge stale containerd blobs — moby#42900 workaround)
         4c. docker checkpoint create --leave-running <id> loom-2-after-setup
             (CRIU snapshots entire process tree; container keeps running)
         4d. container.inject_credentials(&creds_json)
             (fresh credentials re-injected)

Step 5:  Multiplexer adds node to LoomTree
         (parent = current_node_id, label = "after-setup")

Step 6:  current_node_id updated to new node ID

Step 7:  LoomTree saved to ~/.claude-dind/loom.json

Step 8:  Multiplexer returns to Normal mode
```

### Loom Restore Flow

```
Step 1:  User presses Ctrl-b t, navigates tree, presses Enter on target node

Step 2:  Multiplexer kills all active sessions
         (sessions.kill() for each, then cleanup_exited())

Step 3:  Multiplexer calls container.restore_checkpoint("loom-1-initial", &creds_json)
         3a. docker stop <id>
             (container stops, all docker exec processes terminated)
         3b. sudo ctr -n moby content ls -q | ... content rm
             (purge stale containerd blobs)
         3c. docker start --checkpoint loom-1-initial <id>
             (CRIU restores process tree from checkpoint image)
         3d. container.wait_for_ready(10)
             (poll docker exec <id> docker info until socket accessible)
         3e. container.inject_credentials(&creds_json)
             (fresh credentials injected into restored container)

Step 4:  current_node_id updated to target node

Step 5:  LoomTree saved to ~/.claude-dind/loom.json

Step 6:  Multiplexer creates a fresh session
         (new docker exec process; Claude Code starts and picks up
          persisted conversation state from ~/.claude/ in the container)

Step 7:  Multiplexer returns to Normal mode with terminal view
```

---

## Security Model

**Threat: Credential exposure on host filesystem.**
Mitigation: Credentials are extracted from Keychain into Rust process memory and piped
directly to Docker's stdin (prompt mode) or via `docker exec -i` stdin (interactive
mode). They never exist as a file on the host.

**Threat: Credential exposure in Docker metadata.**
Mitigation: Credentials are passed via stdin, not environment variables. `docker inspect`
will not show them. The `CLAUDE_PROMPT` env var is visible, but it contains only the
user's prompt, not credentials.

**Threat: Credential persistence in container.**
Mitigation (prompt mode): The entrypoint explicitly deletes `.credentials.json` before
exiting. The `--rm` flag destroys the container filesystem.
Mitigation (interactive mode): Credentials persist in the container for its lifetime.
The container must be explicitly stopped. Use `--attach` to manage container lifecycle.

**Threat: Credential interception during pipe transfer.**
Mitigation: Unix pipes are kernel-level constructs. Data flows directly between process
file descriptors without hitting disk.

**Threat: Docker socket access (host daemon exposure).**
Accepted risk: Mounting the host Docker socket gives the container access to the host
Docker daemon. Claude can create/inspect/remove sibling containers. This is mitigated by:
`--security-opt no-new-privileges` in standard mode (prevents privilege escalation),
running Claude as a non-root user, and removing the `--privileged` flag entirely (no
extra kernel capabilities). In loom mode, the `no-new-privileges` flag is replaced with
CRIU-compatible flags (see "Relaxed security flags in loom mode" below).

**Threat: Concurrent token use triggers rate limiting or revocation.**
Observation: OAuth tokens are bearer tokens with no device binding. Using the same tokens
on the host and in a container simultaneously works, but may trigger rate limiting if
both are making API calls. Multiple interactive sessions share a single token inside the
container, which is indistinguishable from a single session from the server's perspective.

**Threat: Interactive mode credentials outlive the session.**
Mitigation: Credentials persist in the container's filesystem for its entire lifetime
(unlike prompt mode where they are deleted after use). This is a conscious tradeoff for
usability. When the container is stopped (`docker rm -f`), all data is destroyed.

**Threat: Credentials captured in CRIU checkpoint images (loom mode).**
Mitigation: The checkpoint protocol explicitly strips credentials from the container
filesystem (`rm -f .credentials.json`) before calling `docker checkpoint create`. This
ensures the CRIU image (which captures filesystem state) does not contain the credential
JSON. Fresh credentials are re-injected after every checkpoint and restore operation.

**Threat: Relaxed security flags in loom mode.**
Accepted risk: Loom mode requires `seccomp=unconfined`, `apparmor=unconfined`, and
`--net=host` for CRIU to function. This is a weaker security posture than standard
interactive mode. These flags are only applied when the user explicitly passes `--loom`.
Without `--loom`, the container uses `--security-opt no-new-privileges`. The security
implications are documented in the CLI help text and this document.

**Threat: Stale containerd blobs cause checkpoint failures.**
Mitigation: The `purge_containerd_blobs()` method runs `sudo ctr -n moby content rm`
before every checkpoint create and restore operation. This works around Docker/containerd
issue moby#42900 where leftover content blobs prevent checkpoint operations.

---

## Usage

### Prerequisites

- macOS (for Keychain access)
- Docker Desktop or OrbStack running
- Claude Code installed and logged in on the host (`claude` should work in your terminal)
- Rust toolchain (`cargo`)
- shadow-terminal cloned into `extern/shadow-terminal/` (for interactive mode):
  ```bash
  git clone https://github.com/nichochar/shadow-terminal extern/shadow-terminal
  ```

### Building

```bash
cd /path/to/claude-dind

# Build the Rust CLI
cargo build --release

# Build the Docker image (required on first run or after Dockerfile changes)
./target/release/claude-dind prompt --build "test prompt"
# Or manually:
docker build -t claude-dind:latest docker/
```

### Prompt Mode Usage

Run a single prompt in an ephemeral container:

```bash
# Basic usage
claude-dind prompt "List the files in the current directory"

# Build the Docker image first, then run
claude-dind prompt --build "Write a Python hello world script"

# With extra Claude flags
claude-dind prompt --claude-flags "--output-format stream-json" "Write hello world in Python"

# Keep the container after exit (for debugging)
claude-dind prompt --keep "Describe the Docker environment"

# Debug: print extracted credentials and exit
claude-dind prompt --dump-creds "ignored"

# Verbose: show Docker command being run
claude-dind prompt -v "Hello"

# Use a different image tag
claude-dind prompt --image my-custom-claude:v2 "Hello"

# Mount a host directory as the workspace
claude-dind prompt -w ./my-project "Describe the project structure"

# Use a non-standard Docker socket (e.g., OrbStack, Colima)
claude-dind prompt --docker-socket ~/.orbstack/run/docker.sock "Hello"
```

### Interactive Mode Usage

Launch a terminal multiplexer with multiple Claude Code sessions:

```bash
# Start interactive mode (build image first if needed)
claude-dind interactive --build

# Mount a host directory as the workspace
claude-dind interactive --build -w ./my-project

# Start interactive mode (image already built)
claude-dind interactive

# Re-attach to a running container
claude-dind interactive --attach abc123def456

# Verbose mode
claude-dind interactive -v
```

**Note:** Interactive mode must be run from a real terminal (Terminal.app, iTerm2, etc.),
not from within another TUI or a non-TTY environment.

### Loom Mode Usage

Loom mode extends interactive mode with CRIU-based checkpoint/restore. Requires a Linux
host with Docker experimental mode and CRIU installed.

**Prerequisites (Linux only):**
```bash
# Enable Docker experimental mode
echo '{"experimental": true}' | sudo tee /etc/docker/daemon.json
sudo systemctl restart docker

# Install CRIU 4.2+
sudo add-apt-repository -y ppa:criu/ppa
sudo apt install -y criu

# Verify
docker info | grep Experimental    # should show: Experimental: true
criu --version                      # should show 4.2+
```

**Starting loom mode:**
```bash
# Start with loom enabled (builds image first if needed)
claude-dind interactive --build --loom

# With a workspace mount
claude-dind interactive --build --loom -w ./my-project

# Custom loom file location
claude-dind interactive --loom --loom-file /path/to/loom.json
```

**Taking a checkpoint:**
1. Press `Ctrl-b s` — a label input overlay appears at the bottom
2. Type a name (e.g., "initial", "after-setup")
3. Press `Enter` — the checkpoint is created, container continues running
4. Press `Esc` to cancel without taking a checkpoint

**Viewing and restoring checkpoints:**
1. Press `Ctrl-b t` — the tree view shows all checkpoints
2. Navigate with `j`/`k` or arrow keys
3. Press `Enter` on a checkpoint to restore to that state
4. Press `d` on a checkpoint to delete it
5. Press `q` or `Esc` to return to the terminal view

**Note:** Loom mode is not available on macOS. CRIU is a Linux-only technology and is not
available inside the Docker Desktop VM. Standard interactive mode (without `--loom`)
works on both macOS and Linux.

### Keybindings

All keybindings use a **Ctrl-b prefix** (like tmux). Press `Ctrl-b`, release, then
press the command key.

| Key | Action |
|-----|--------|
| `Ctrl-b c` | Create a new Claude session |
| `Ctrl-b n` | Switch to the next session |
| `Ctrl-b p` | Switch to the previous session |
| `Ctrl-b 0`-`9` | Jump to session by index |
| `Ctrl-b x` | Kill the current session |
| `Ctrl-b d` | Detach (exit TUI, container keeps running) |
| `Ctrl-b ?` | Toggle help overlay |
| `Ctrl-b Ctrl-b` | Send a literal Ctrl-b to the active session |
| `Ctrl-b s` | Take snapshot / checkpoint (loom mode only) |
| `Ctrl-b t` | Show snapshot tree view (loom mode only) |

**Tree view keybindings** (when in tree view via `Ctrl-b t`):

| Key | Action |
|-----|--------|
| `j` / `↓` | Move selection down |
| `k` / `↑` | Move selection up |
| `Enter` | Restore from selected checkpoint |
| `d` | Delete selected checkpoint |
| `q` / `Esc` | Return to terminal view |

All other input is forwarded directly to the active session.

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success (prompt completed, or interactive mode exited normally) |
| 1 | General error (Docker, credential, or runtime failure) |
| 2 | Credential validation error (missing, empty, or malformed) |
| 3 | Reserved (previously: dockerd timeout; no longer used) |
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
- The non-root user's PATH may not include `/usr/local/bin`. The entrypoint and session
  commands export PATH explicitly to fix this. If you've modified either, ensure the
  PATH export is present.

**"Docker socket mounted but not accessible"**
- The host Docker socket GID may not match the container's docker group. The entrypoint
  attempts to fix this automatically with `groupmod`. If it still fails, check the
  socket permissions on the host: `ls -la /var/run/docker.sock`.
- If using a non-standard Docker socket (OrbStack, Colima), use the `--docker-socket` flag.

**"No Docker socket found"**
- Ensure Docker is running on the host and the socket exists at `/var/run/docker.sock`
  (or use `--docker-socket` for non-standard paths).

**Sessions die immediately in interactive mode**
- Check `/tmp/claude-dind-mux.log` for debug output. Look for "task finished" messages
  shortly after "session created".
- Ensure the container is running: `docker ps` should show the container.
- Verify docker exec works manually:
  `docker exec -it <container-id> su -l claude -c 'echo hello'`
- Ensure you are running from a real terminal, not from within another program that
  doesn't provide a proper TTY.

**"Device not configured (os error 6)"**
- This means the program is not connected to a real TTY. Run `claude-dind interactive`
  from Terminal.app or iTerm2, not from within a tool that doesn't provide a terminal.

**"Failed to send input: channel closed"**
- The session's docker exec process has exited. This is logged but not fatal. Check
  the log file for the underlying cause.

**TUI renders but terminal content is blank**
- shadow-terminal may not have received output yet. Wait a few seconds for Claude Code
  to start. Check if `docker exec -it <id> claude --dangerously-skip-permissions`
  works manually.

**"Docker experimental mode is not enabled" (loom mode)**
- CRIU checkpoints require Docker experimental mode. Enable it:
  ```bash
  echo '{"experimental": true}' | sudo tee /etc/docker/daemon.json
  sudo systemctl restart docker
  ```
- Verify: `docker info | grep Experimental` should show `true`.

**"docker checkpoint create failed" (loom mode)**
- Ensure CRIU 4.2+ is installed: `criu --version`. Install via `ppa:criu/ppa` on Ubuntu.
- The containerd blob purge may have failed. Check if `sudo ctr -n moby content ls -q`
  returns any entries. If so, manually remove them:
  `sudo ctr -n moby content ls -q | xargs -r sudo ctr -n moby content rm`
- Check Docker logs for CRIU errors: `sudo journalctl -u docker --since "5 min ago"`

**"docker start --checkpoint failed" (loom mode)**
- Same blob purge issue as above. Also verify the checkpoint exists:
  `docker checkpoint ls <container-id>`
- The checkpoint may be from a different container image version. Checkpoints are not
  portable across image rebuilds.

**Checkpoint takes a long time or hangs**
- CRIU must freeze all processes and dump their memory pages. Large working sets take
  longer. Check system memory and disk I/O.
- Ensure the container is not performing heavy I/O during the checkpoint.

**Restored session doesn't have my latest conversation**
- Checkpoints capture the container state at the moment they were taken. Any work done
  after the checkpoint was taken is not included. This is by design — checkpoints are
  save points, not snapshots of the future.

---

## Limitations and Future Work

1. **macOS-only credential extraction.** The Rust CLI currently only supports macOS
   Keychain. Adding Linux support (reading `~/.claude/.credentials.json`) would require
   detecting the OS and choosing the appropriate extraction method.

2. **Docker socket security.** The host Docker socket is mounted into the container,
   giving Claude access to the host Docker daemon. Containers created by Claude run as
   sibling containers on the host, not nested. In standard mode, this is mitigated by
   `--security-opt no-new-privileges` and running as non-root. In loom mode, security
   flags are relaxed further (`seccomp=unconfined`, `apparmor=unconfined`, `--net=host`)
   for CRIU compatibility — a weaker isolation boundary than either standard mode or
   true DinD.

3. **No token refresh persistence.** If Claude Code refreshes the access token inside the
   container, the new token is written to `.credentials.json` inside the container -- but
   in prompt mode, the container is destroyed on exit. The host Keychain still has the old
   (possibly expired) access token. This is fine because the refresh token is long-lived
   and Claude Code on the host will refresh independently. In interactive mode, refreshed
   tokens persist for the container's lifetime.

4. **Image size.** The `claude-dind:latest` image includes the Docker CLI, Node.js, and
   Claude Code. It's lighter than the previous DinD-based image since it no longer
   includes the full Docker daemon.

5. **Detach/reattach loses sessions.** When you detach from interactive mode and
   reattach, existing Claude sessions inside the container may still be running, but
   the multiplexer creates new shadow-terminal instances that don't reconnect to them.
   True session persistence would require a screen/tmux-like attach mechanism inside
   the container.

6. **shadow-terminal dependency.** shadow-terminal must be cloned separately into
   `extern/shadow-terminal/`. It is not published on crates.io. The `raw_string_direct_to_terminal`
   function in shadow-terminal is patched to a no-op to prevent it from writing escape
   codes directly to stdout while ratatui owns the terminal.

7. **Single-host only.** Both modes require Docker to be running on the same machine.
   Remote Docker host support (via `DOCKER_HOST`) is not tested.

8. **Loom mode is Linux-only.** CRIU is a Linux-only technology. Docker Desktop on macOS
   runs containers inside a Linux VM, but CRIU is not installed in that VM and Docker
   Desktop does not expose the checkpoint API. Loom mode requires a native Linux Docker
   host with CRIU 4.2+ and Docker experimental mode enabled.

9. **Checkpoints are not portable.** CRIU checkpoints are tied to the specific container
   ID, image, and kernel version. They cannot be moved between hosts, transferred
   across Docker image rebuilds, or shared between containers. The checkpoint tree
   (`loom.json`) persists on the host, but the actual checkpoint images live in Docker's
   internal storage.

10. **Restore kills all sessions.** When restoring from a checkpoint, all active `docker
    exec` sessions are terminated because they are not children of the container's PID 1.
    The multiplexer creates a fresh session after restore. Claude Code picks up its
    persisted conversation state from `~/.claude/` inside the restored container, but
    any in-flight interactive state (partially typed input, etc.) is lost.
