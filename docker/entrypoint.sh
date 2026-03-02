#!/bin/bash
# entrypoint.sh — Container entrypoint for claude-dind (Docker-out-of-Docker)
#
# This script is the first thing that runs when the container starts.
# It supports two modes, controlled by the CLAUDE_MODE env var:
#
# CLAUDE_MODE=prompt (default):
#   Orchestrates four phases:
#     Phase 1: Match Docker socket GID so the claude user can access it
#     Phase 2: Read OAuth credential JSON from stdin (piped by the Rust CLI)
#     Phase 3: Write credentials to ~/.claude/.credentials.json
#     Phase 4: Run Claude Code as the non-root 'claude' user
#     Phase 5: Clean up credentials
#
# CLAUDE_MODE=interactive:
#   Matches the socket GID, then sleeps forever. Claude sessions are spawned
#   via `docker exec` by the host-side multiplexer. Credentials are
#   injected separately via `docker exec` after startup.
#
# Stdin protocol (prompt mode only):
#   The Rust CLI writes the full credential JSON blob to this container's stdin,
#   then closes the pipe (sends EOF). This script reads stdin with `cat`, which
#   blocks until EOF. Since the Rust CLI closes the pipe immediately after writing,
#   `cat` returns instantly with the full JSON.
#
# Environment variables (set by the Rust CLI via `docker run --env`):
#   CLAUDE_MODE    — Optional. "prompt" (default) or "interactive".
#   CLAUDE_PROMPT  — Required in prompt mode. The prompt/task to pass to `claude -p`.
#   CLAUDE_FLAGS   — Optional. Extra flags appended to the `claude` command.
#
# Exit codes:
#   0     — Claude completed successfully
#   2     — Credential error (missing, empty, or invalid JSON)
#   Other — Forwarded from Claude Code's exit code

set -euo pipefail

CLAUDE_MODE="${CLAUDE_MODE:-prompt}"
echo "[claude-dind] Starting (mode: ${CLAUDE_MODE})..." >&2

# ── Docker socket GID matching ───────────────────────────────────────────
#
# The host's Docker socket is bind-mounted at /var/run/docker.sock.
# Its GID on the host may differ from the container's `docker` group GID.
# We detect the socket's GID via `stat` and adjust the container's docker
# group to match, so the `claude` user (a member of `docker`) can access it.

DOCKER_SOCKET="/var/run/docker.sock"
if [ -S "$DOCKER_SOCKET" ]; then
    SOCKET_GID=$(stat -c '%g' "$DOCKER_SOCKET" 2>/dev/null || stat -f '%g' "$DOCKER_SOCKET" 2>/dev/null || echo "")
    if [ -n "$SOCKET_GID" ]; then
        CURRENT_GID=$(getent group docker | cut -d: -f3)
        if [ "$SOCKET_GID" != "$CURRENT_GID" ]; then
            echo "[claude-dind] Matching docker group GID to socket GID (${SOCKET_GID})..." >&2
            groupmod -g "$SOCKET_GID" docker 2>/dev/null || true
        fi
    fi

    # Verify Docker access
    if docker info > /dev/null 2>&1; then
        echo "[claude-dind] Docker socket accessible." >&2
    else
        echo "[claude-dind] WARNING: Docker socket mounted but not accessible. Docker commands may fail." >&2
    fi
else
    echo "[claude-dind] WARNING: No Docker socket found at ${DOCKER_SOCKET}. Docker commands will be unavailable." >&2
fi

# ── Interactive mode: sleep forever ──────────────────────────────────────
#
# In interactive mode, the container stays alive. The host-side multiplexer
# creates Claude sessions via `docker exec` and injects credentials separately.
# We trap SIGTERM/SIGINT for graceful shutdown.

if [ "$CLAUDE_MODE" = "interactive" ]; then
    echo "[claude-dind] Interactive mode — container ready for sessions." >&2
    echo "[claude-dind] Use 'docker exec' to create sessions." >&2

    # Trap signals for graceful shutdown
    cleanup() {
        echo "[claude-dind] Shutting down..." >&2
        exit 0
    }
    trap cleanup SIGTERM SIGINT

    # Sleep forever (exec replaces shell, PID 1 = sleep)
    # Using a loop so signals are handled between iterations
    while true; do
        sleep 86400 &
        wait $! || true
    done
fi

# ── Prompt mode: the original behavior ────────────────────────────────────

# ── Phase 2: Read credentials from stdin ──────────────────────────────────
#
# The Rust CLI pipes the credential JSON blob, then closes stdin (EOF).
# We use `cat` to read all of stdin into a variable.
#
# The `[ -t 0 ]` check detects if stdin is a terminal (interactive mode).
# If someone runs the container directly without piping credentials,
# this provides a clear error instead of hanging forever waiting for input.

if [ -t 0 ]; then
    echo "[claude-dind] ERROR: No credentials piped via stdin." >&2
    echo "[claude-dind] This container expects credential JSON on stdin." >&2
    echo "[claude-dind] Use the claude-dind Rust CLI to run this container." >&2
    exit 2
fi

CREDS_JSON=$(cat)

if [ -z "$CREDS_JSON" ]; then
    echo "[claude-dind] ERROR: Empty credentials received on stdin." >&2
    exit 2
fi

# Validate that the JSON contains the expected field.
# This catches corrupted or truncated credentials before Claude Code tries
# to use them (which would produce a confusing authentication error).
if ! echo "$CREDS_JSON" | jq -e '.claudeAiOauth.accessToken' > /dev/null 2>&1; then
    echo "[claude-dind] ERROR: Invalid credential JSON (missing claudeAiOauth.accessToken)." >&2
    exit 2
fi

echo "[claude-dind] Credentials received." >&2

# ── Phase 3: Write credentials to Claude Code's expected location ─────────
#
# On Linux, Claude Code reads credentials from ~/.claude/.credentials.json
# (the file-based fallback, since there is no macOS Keychain on Linux).
# We write the JSON blob to the 'claude' user's home directory and lock
# down permissions to owner-only (600).

mkdir -p /home/claude/.claude
echo "$CREDS_JSON" > /home/claude/.claude/.credentials.json
chmod 600 /home/claude/.claude/.credentials.json
chown -R claude:claude /home/claude/.claude

# ── Phase 4: Run Claude Code ─────────────────────────────────────────────
#
# Read the prompt from the CLAUDE_PROMPT environment variable (set by
# the Rust CLI via `docker run --env CLAUDE_PROMPT="..."`).
#
# We use `su -l claude` to switch to the non-root user because Claude Code
# refuses --dangerously-skip-permissions when running as root.
#
# The explicit PATH export is necessary because `su -l` starts a login shell
# that resets PATH, and /usr/local/bin (where `claude` is installed by npm)
# may not be in the default login PATH for the claude user.

PROMPT="${CLAUDE_PROMPT:?CLAUDE_PROMPT environment variable is required}"
EXTRA_FLAGS="${CLAUDE_FLAGS:-}"

echo "[claude-dind] Running claude as user 'claude'..." >&2

CLAUDE_EXIT=0
# shellcheck disable=SC2086
su -l claude -c "export PATH=/usr/local/bin:/usr/bin:/bin:\$PATH && cd /workspace && claude -p $(printf '%q' "$PROMPT") --dangerously-skip-permissions $EXTRA_FLAGS" || CLAUDE_EXIT=$?

echo "[claude-dind] Claude exited with code: $CLAUDE_EXIT" >&2

# ── Phase 5: Cleanup ─────────────────────────────────────────────────────
#
# Delete the credential file immediately. Even though --rm will destroy the
# entire container filesystem, explicit deletion is defense-in-depth in case
# the container is kept alive (--keep flag) or the process is interrupted.

rm -f /home/claude/.claude/.credentials.json

# Exit with Claude's exit code so the Rust CLI can forward it to the caller.
exit $CLAUDE_EXIT
