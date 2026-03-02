# claude-dind Docker Image
#
# A Docker-in-Docker image with Claude Code installed. Designed to receive
# OAuth credentials via stdin from the claude-dind Rust CLI on the host.
#
# Base: docker:dind (Alpine Linux with Docker daemon + CLI)
# Added: Node.js, npm, Claude Code CLI, non-root user, bash, jq
#
# The entrypoint (entrypoint.sh) handles:
#   1. Reading credential JSON from stdin
#   2. Writing it to ~/.claude/.credentials.json
#   3. Starting the Docker daemon (DinD)
#   4. Running `claude -p <prompt>` as a non-root user
#   5. Cleaning up credentials on exit
#
# Usage (via the Rust CLI — not intended to be run directly):
#   echo '{"claudeAiOauth":...}' | docker run --privileged -i --rm \
#     -e CLAUDE_PROMPT="Hello" claude-dind:latest
#
# Why docker:dind as the base?
#   Claude Code with --dangerously-skip-permissions can execute arbitrary
#   commands, including Docker commands. DinD provides a real Docker daemon
#   inside the container so Claude can build/run containers as part of its
#   work without affecting the host's Docker environment.

FROM docker:dind

# Install runtime dependencies:
#   nodejs, npm  — Required by Claude Code (it's a Node.js application)
#   bash         — Entrypoint uses bash features (set -euo pipefail, etc.)
#   jq           — Validates credential JSON structure in the entrypoint
#   shadow       — Provides useradd/usermod for creating the non-root user
#   sudo         — Allows the claude user to run privileged commands if needed
RUN apk add --no-cache nodejs npm bash jq shadow sudo

# Install Claude Code globally via npm.
# This places the `claude` binary at /usr/local/bin/claude.
RUN npm install -g @anthropic-ai/claude-code

# Create a non-root user for running Claude Code.
#
# Why? Claude Code refuses to run with --dangerously-skip-permissions as root:
#   "cannot be used with root/sudo privileges for security reasons"
#
# The entrypoint runs as root (needed for starting dockerd), then drops
# to this user via `su -l claude` for the actual Claude Code execution.
#
# - useradd -m: create home directory at /home/claude
# - useradd -s /bin/bash: set login shell to bash
# - sudoers entry: allow password-less sudo (for any commands Claude might need)
# - docker group: allow Docker CLI access without sudo
RUN useradd -m -s /bin/bash claude \
    && echo 'claude ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/claude \
    && usermod -aG docker claude 2>/dev/null || true

# Pre-create the Claude config directory and bypass the first-run onboarding prompt.
# Without this, Claude Code shows an interactive onboarding wizard that blocks
# non-interactive (headless) execution.
RUN mkdir -p /home/claude/.claude \
    && echo '{"hasCompletedOnboarding":true}' > /home/claude/.claude.json \
    && chown -R claude:claude /home/claude/.claude /home/claude/.claude.json

# Create and set ownership of the working directory.
# Claude Code operates in /workspace — any files it creates will be here.
RUN mkdir -p /workspace && chown claude:claude /workspace
WORKDIR /workspace

# Copy the entrypoint script that orchestrates the credential injection,
# dockerd startup, Claude execution, and cleanup.
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
