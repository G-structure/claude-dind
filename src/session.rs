use anyhow::{bail, Result};
use shadow_terminal::active_terminal::ActiveTerminal;
use shadow_terminal::output::native::{CompleteSurface, Output};
use shadow_terminal::shadow_terminal::Config;
use std::ffi::OsString;
use termwiz::surface::Surface;

/// Metadata about a single Claude session running inside the container.
pub struct Session {
    /// The shadow-terminal ActiveTerminal wrapping the docker exec process.
    pub terminal: ActiveTerminal,
    /// Human-readable name for the session.
    pub name: String,
    /// The latest complete screen surface from shadow-terminal.
    pub screen: Surface,
    /// Whether the session's underlying process has exited.
    pub exited: bool,
}

/// Manages multiple Claude sessions inside a single DinD container.
///
/// Each session is a `docker exec -it <container> su -l claude -c "claude ..."`
/// process, wrapped in a shadow-terminal `ActiveTerminal` for virtual
/// terminal emulation.
pub struct SessionManager {
    pub sessions: Vec<Session>,
    pub active: usize,
    container_id: String,
}

impl SessionManager {
    pub fn new(container_id: String) -> Self {
        Self {
            sessions: Vec::new(),
            active: 0,
            container_id,
        }
    }

    /// Create a new Claude session inside the container.
    ///
    /// Spawns `docker exec -it <container> su -l claude -c "claude --dangerously-skip-permissions"`
    /// wrapped in a shadow-terminal `ActiveTerminal`.
    pub fn create(&mut self, width: u16, height: u16) -> Result<usize> {
        let idx = self.sessions.len();
        let name = format!("claude-{}", idx + 1);

        // Build the command that shadow-terminal will execute as a PTY process.
        // docker exec -it <container> su -l claude -c "..."
        // Run through bash -c so that docker exec inherits a proper TTY
        // from portable-pty's PTY slave. Using bash -c ensures the shell
        // sets up the TTY correctly before calling docker exec.
        let docker_cmd = format!(
            "docker exec -it {} su -l claude -c 'export PATH=/usr/local/bin:/usr/bin:/bin:$PATH && cd /workspace && claude --dangerously-skip-permissions'",
            self.container_id
        );

        let command: Vec<OsString> = vec![
            "bash".into(),
            "-c".into(),
            docker_cmd.into(),
        ];

        let config = Config {
            width,
            height,
            command,
            scrollback_size: 5000,
            ..Config::default()
        };

        let terminal = ActiveTerminal::start(config);
        let screen = Surface::new(width as usize, height as usize);

        self.sessions.push(Session {
            terminal,
            name,
            screen,
            exited: false,
        });

        eprintln!("[claude-dind] Session {} created.", idx + 1);
        Ok(idx)
    }

    /// Send raw bytes to a session's PTY input.
    ///
    /// BytesFromSTDIN is `[u8; 128]`. For keyboard input we typically send
    /// 1-4 bytes; the trailing zeros are harmless since the PTY only reads
    /// as many bytes as were written.
    pub async fn send_input(&self, session_idx: usize, bytes: &[u8]) -> Result<()> {
        if session_idx >= self.sessions.len() {
            bail!("Invalid session index: {session_idx}");
        }

        let session = &self.sessions[session_idx];
        if session.exited {
            bail!("Session {} has exited", session_idx);
        }

        for chunk in bytes.chunks(128) {
            let mut buf = [0u8; 128];
            buf[..chunk.len()].copy_from_slice(chunk);
            session
                .terminal
                .send_input(buf)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send input: {e}"))?;
        }

        Ok(())
    }

    /// Poll for output updates from a session's shadow-terminal.
    ///
    /// Returns true if the screen was updated. Updates the session's
    /// stored `screen` Surface with the latest data.
    pub fn poll_output(&mut self, session_idx: usize) -> bool {
        if session_idx >= self.sessions.len() {
            return false;
        }

        let session = &mut self.sessions[session_idx];
        let mut updated = false;

        // Drain all available output from the channel
        while let Ok(output) = session.terminal.surface_output_rx.try_recv() {
            match output {
                Output::Complete(surface) => {
                    match surface {
                        CompleteSurface::Screen(screen) => {
                            session.screen = screen.surface;
                        }
                        CompleteSurface::Scrollback(scrollback) => {
                            session.screen = scrollback.surface;
                        }
                        _ => {}
                    }
                    updated = true;
                }
                Output::Diff(diff) => {
                    // Apply diff changes to the existing surface
                    match diff {
                        shadow_terminal::output::native::SurfaceDiff::Screen(screen_diff) => {
                            session.screen.add_changes(screen_diff.changes);
                            updated = true;
                        }
                        shadow_terminal::output::native::SurfaceDiff::Scrollback(
                            scrollback_diff,
                        ) => {
                            session.screen.add_changes(scrollback_diff.changes);
                            updated = true;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        updated
    }

    /// Kill a session by index.
    pub fn kill(&mut self, session_idx: usize) -> Result<()> {
        if session_idx >= self.sessions.len() {
            bail!("Invalid session index: {session_idx}");
        }

        let session = &mut self.sessions[session_idx];
        let _ = session.terminal.kill();
        session.exited = true;

        eprintln!("[claude-dind] Session {} killed.", session_idx + 1);
        Ok(())
    }

    /// Remove exited sessions and adjust the active index.
    pub fn cleanup_exited(&mut self) {
        self.sessions.retain(|s| !s.exited);
        if self.active >= self.sessions.len() && !self.sessions.is_empty() {
            self.active = self.sessions.len() - 1;
        }
    }

    /// Resize all sessions' terminals.
    pub fn resize_all(&self, width: u16, height: u16) {
        for session in &self.sessions {
            if !session.exited {
                let _ = session.terminal.resize(width, height);
            }
        }
    }

    /// Number of active (non-exited) sessions.
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.sessions.iter().filter(|s| !s.exited).count()
    }

    /// Switch to next session.
    pub fn next(&mut self) {
        if !self.sessions.is_empty() {
            self.active = (self.active + 1) % self.sessions.len();
        }
    }

    /// Switch to previous session.
    pub fn prev(&mut self) {
        if !self.sessions.is_empty() {
            self.active = if self.active == 0 {
                self.sessions.len() - 1
            } else {
                self.active - 1
            };
        }
    }

    /// Switch to session by index (0-based).
    pub fn switch_to(&mut self, idx: usize) {
        if idx < self.sessions.len() {
            self.active = idx;
        }
    }
}
