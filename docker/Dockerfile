FROM docker:dind

# Install Node.js, npm, and utilities
RUN apk add --no-cache nodejs npm bash jq shadow sudo

# Install Claude Code globally
RUN npm install -g @anthropic-ai/claude-code

# Create non-root user for Claude Code (--dangerously-skip-permissions refuses root)
RUN useradd -m -s /bin/bash claude \
    && echo 'claude ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/claude \
    && usermod -aG docker claude 2>/dev/null || true

# Pre-create config directory and skip onboarding prompt
RUN mkdir -p /home/claude/.claude \
    && echo '{"hasCompletedOnboarding":true}' > /home/claude/.claude.json \
    && chown -R claude:claude /home/claude/.claude /home/claude/.claude.json

# Working directory for Claude Code tasks
RUN mkdir -p /workspace && chown claude:claude /workspace
WORKDIR /workspace

# Copy entrypoint
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
