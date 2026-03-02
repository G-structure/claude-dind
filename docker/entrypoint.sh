#!/bin/bash
set -euo pipefail

echo "[claude-dind] Starting..." >&2

# ── Phase 1: Read credentials from stdin ──────────────────────
# The Rust CLI pipes the JSON blob then closes stdin (EOF).

if [ -t 0 ]; then
    echo "[claude-dind] ERROR: No credentials piped via stdin." >&2
    echo "[claude-dind] This container expects credential JSON on stdin." >&2
    exit 2
fi

CREDS_JSON=$(cat)

if [ -z "$CREDS_JSON" ]; then
    echo "[claude-dind] ERROR: Empty credentials received on stdin." >&2
    exit 2
fi

if ! echo "$CREDS_JSON" | jq -e '.claudeAiOauth.accessToken' > /dev/null 2>&1; then
    echo "[claude-dind] ERROR: Invalid credential JSON (missing claudeAiOauth.accessToken)." >&2
    exit 2
fi

echo "[claude-dind] Credentials received." >&2

# ── Phase 2: Write credentials to Claude Code's expected location ─
mkdir -p /home/claude/.claude
echo "$CREDS_JSON" > /home/claude/.claude/.credentials.json
chmod 600 /home/claude/.claude/.credentials.json
chown -R claude:claude /home/claude/.claude

# ── Phase 3: Start dockerd in background (DinD) ──────────────
echo "[claude-dind] Starting Docker daemon..." >&2
dockerd-entrypoint.sh dockerd &
DOCKERD_PID=$!

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

# ── Phase 4: Run Claude Code ─────────────────────────────────
PROMPT="${CLAUDE_PROMPT:?CLAUDE_PROMPT environment variable is required}"
EXTRA_FLAGS="${CLAUDE_FLAGS:-}"

echo "[claude-dind] Running claude as user 'claude'..." >&2

CLAUDE_EXIT=0
# shellcheck disable=SC2086
su -l claude -c "export PATH=/usr/local/bin:/usr/bin:/bin:\$PATH && cd /workspace && claude -p $(printf '%q' "$PROMPT") --dangerously-skip-permissions $EXTRA_FLAGS" || CLAUDE_EXIT=$?

echo "[claude-dind] Claude exited with code: $CLAUDE_EXIT" >&2

# ── Phase 5: Cleanup ─────────────────────────────────────────
rm -f /home/claude/.claude/.credentials.json

kill $DOCKERD_PID 2>/dev/null || true
wait $DOCKERD_PID 2>/dev/null || true

exit $CLAUDE_EXIT
