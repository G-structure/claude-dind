//! Session management for the interactive terminal multiplexer.
//!
//! Each Claude Code session runs as a `docker exec -it <container> ...` process
//! inside the long-lived DinD container. The process is wrapped in a
//! shadow-terminal [`ActiveTerminal`] which provides:
//!
//! - A real PTY (via portable-pty) so `docker exec -it` detects a terminal
//! - Full ANSI/VT100 terminal emulation (via wezterm-term) into a cell grid
//! - Async channels for sending keystrokes and receiving rendered screen state
//!
//! The rendered screen state is a termwiz [`Surface`] — a 2D grid of cells,
//! each with a character, foreground/background color, and text attributes.
//! This `Surface` is what [`crate::render::TerminalWidget`] maps to ratatui
//! buffer cells for display.
//!
//! ## Double-PTY Pattern
//!
//! Sessions use a double-PTY arrangement:
//!
//! ```text
//! portable-pty (host) → bash -c "docker exec -it ... claude"
//!                                      ↓
//!                              docker allocates inner PTY
//!                                      ↓
//!                              claude runs with full TUI
//! ```
//!
//! The outer PTY (portable-pty) satisfies docker's `isatty()` check on stdin.
//! The inner PTY (allocated by docker exec `-t`) gives Claude Code a real
//! terminal to render its TUI into. This is analogous to running tmux inside
//! an ssh session.

use anyhow::{bail, Result};
use shadow_terminal::active_terminal::ActiveTerminal;
use shadow_terminal::output::native::{CompleteSurface, Output};
use shadow_terminal::shadow_terminal::Config;
use std::ffi::OsString;
use termwiz::surface::Surface;

/// Metadata about a single Claude Code session running inside the container.
///
/// Each session owns an [`ActiveTerminal`] that manages the underlying PTY
/// process and provides async I/O channels. The `screen` field holds the
/// latest terminal state as a termwiz [`Surface`], updated by [`SessionManager::poll_output`].
pub struct Session {
    /// The shadow-terminal `ActiveTerminal` wrapping the docker exec process.
    /// Provides `send_input()` for keystrokes and `surface_output_rx` for
    /// receiving rendered terminal state.
    pub terminal: ActiveTerminal,
    /// Human-readable name for the session (e.g., "claude-1", "claude-2").
    pub name: String,
    /// The latest terminal screen state. Updated by `poll_output()` from
    /// shadow-terminal's output channel. This is a termwiz `Surface` — a 2D
    /// grid of cells with characters, colors, and attributes.
    pub screen: Surface,
    /// Whether the session's underlying docker exec process has exited.
    /// Checked via `terminal.task_handle.is_finished()` in the event loop.
    pub exited: bool,
}

/// Manages multiple Claude Code sessions inside a single DinD container.
///
/// Each session is a `docker exec -it <container> su -l claude -c "claude ..."`
/// process, wrapped in a shadow-terminal [`ActiveTerminal`] for virtual
/// terminal emulation.
///
/// The multiplexer maintains an `active` index pointing to the currently
/// displayed session. Navigation methods (`next`, `prev`, `switch_to`) update
/// this index; the renderer reads it to determine which session's screen to
/// display.
pub struct SessionManager {
    /// All sessions, both active and exited. Exited sessions are cleaned up
    /// by [`cleanup_exited`](Self::cleanup_exited).
    pub sessions: Vec<Session>,
    /// Index of the currently active (displayed) session.
    pub active: usize,
    /// Docker container ID. Used to construct `docker exec` commands.
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

    /// Create a new Claude Code session inside the container.
    ///
    /// Spawns `docker exec -it <container> su -l claude -c "claude --dangerously-skip-permissions"`
    /// wrapped in a shadow-terminal [`ActiveTerminal`].
    ///
    /// The command is wrapped in `bash -c` to ensure proper TTY inheritance.
    /// portable-pty allocates a PTY slave, and `bash` inherits it as its
    /// controlling terminal. When bash then runs `docker exec -it`, docker's
    /// `isatty()` check on stdin succeeds, so it allocates an inner PTY inside
    /// the container for Claude Code.
    ///
    /// The `width` and `height` parameters set the initial terminal dimensions
    /// for both the shadow-terminal virtual screen and the PTY. Height should
    /// be the terminal height minus 1 (for the status bar).
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
    /// shadow-terminal's `send_input()` accepts `BytesFromSTDIN`, a `[u8; 128]`
    /// fixed-size buffer. The PTY reads bytes up to the first null (0x00) byte,
    /// so trailing zeros are harmless. For keyboard input we typically send
    /// 1-4 bytes (a single UTF-8 character or escape sequence).
    ///
    /// For inputs longer than 128 bytes (e.g., pasted text), the data is
    /// chunked into 128-byte segments and sent sequentially.
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
    /// Drains all available messages from the session's `surface_output_rx`
    /// channel (non-blocking). shadow-terminal sends two types of output:
    ///
    /// - **`Output::Complete`**: A full replacement of the terminal screen.
    ///   Contains a complete [`Surface`] with all cells. Received after large
    ///   changes or initial rendering.
    ///
    /// - **`Output::Diff`**: Incremental changes (a list of `Change` operations)
    ///   applied to the existing surface. More efficient for small updates like
    ///   cursor movement or single-line changes.
    ///
    /// Returns `true` if the screen was updated (caller should re-render).
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
