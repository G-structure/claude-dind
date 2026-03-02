#!/bin/bash
# entrypoint.sh — Container entrypoint for claude-dind
#
# This script is the first thing that runs when the container starts.
# It orchestrates five phases:
#
#   Phase 1: Read OAuth credential JSON from stdin (piped by the Rust CLI)
#   Phase 2: Write credentials to ~/.claude/.credentials.json (Linux credential path)
#   Phase 3: Start the Docker daemon in the background (DinD)
#   Phase 4: Run Claude Code as the non-root 'claude' user
#   Phase 5: Clean up credentials and stop dockerd
#
# Stdin protocol:
#   The Rust CLI writes the full credential JSON blob to this container's stdin,
#   then closes the pipe (sends EOF). This script reads stdin with `cat`, which
#   blocks until EOF. Since the Rust CLI closes the pipe immediately after writing,
#   `cat` returns instantly with the full JSON.
#
# Environment variables (set by the Rust CLI via `docker run --env`):
#   CLAUDE_PROMPT  — Required. The prompt/task to pass to `claude -p`.
#   CLAUDE_FLAGS   — Optional. Extra flags appended to the `claude` command.
#
# Exit codes:
#   0     — Claude completed successfully
#   2     — Credential error (missing, empty, or invalid JSON)
#   3     — Docker daemon failed to start within timeout
#   Other — Forwarded from Claude Code's exit code

set -euo pipefail

echo "[claude-dind] Starting..." >&2

# ── Phase 1: Read credentials from stdin ──────────────────────────────────
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

# ── Phase 2: Write credentials to Claude Code's expected location ─────────
#
# On Linux, Claude Code reads credentials from ~/.claude/.credentials.json
# (the file-based fallback, since there is no macOS Keychain on Linux).
# We write the JSON blob to the 'claude' user's home directory and lock
# down permissions to owner-only (600).

mkdir -p /home/claude/.claude
echo "$CREDS_JSON" > /home/claude/.claude/.credentials.json
chmod 600 /home/claude/.claude/.credentials.json
chown -R claude:claude /home/claude/.claude

# ── Phase 3: Start dockerd in background (DinD) ──────────────────────────
#
# The docker:dind image includes `dockerd-entrypoint.sh` which handles
# storage driver setup, TLS certificate generation, and containerd startup.
# We run it in the background and poll `docker info` until the daemon is ready.
#
# The container must be started with `--privileged` for this to work — dockerd
# needs capabilities like SYS_ADMIN, NET_ADMIN, and access to cgroups.

echo "[claude-dind] Starting Docker daemon..." >&2
dockerd-entrypoint.sh dockerd &
DOCKERD_PID=$!

# Poll until dockerd is ready (or timeout after 30 seconds).
# On OrbStack: typically <1s. On Docker Desktop: 2-5s.
TIMEOUT=30
ELAPSED=0
while ! docker info > /dev/null 2>&1; do
    if [ $ELAPSED -ge $TIMEOUT ]; then
        echo "[claude-dind] ERROR: dockerd failed to start within ${TIMEOUT}s" >&2
        exit 3
    fi
    sleep 1
    ELAPSED=$((ELAPSED + 1))
done
echo "[claude-dind] Docker daemon ready (took ${ELAPSED}s)." >&2

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

# Gracefully stop the Docker daemon.
kill $DOCKERD_PID 2>/dev/null || true
wait $DOCKERD_PID 2>/dev/null || true

# Exit with Claude's exit code so the Rust CLI can forward it to the caller.
exit $CLAUDE_EXIT
