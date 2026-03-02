//! The interactive terminal multiplexer event loop.
//!
//! This module implements the main TUI loop that drives the multiplexer.
//! It handles three concerns:
//!
//! 1. **Keyboard input** — Reads crossterm events, routes them through a
//!    tmux-style prefix key state machine, and either dispatches multiplexer
//!    commands or forwards raw bytes to the active session's PTY.
//!
//! 2. **Terminal output** — Polls each session's shadow-terminal for new
//!    screen state and triggers re-renders when updates are available.
//!
//! 3. **Session lifecycle** — Detects when session processes exit and marks
//!    them as dead. Exits the loop when all sessions end or the user detaches.
//!
//! ## Input State Machine
//!
//! ```text
//!  ┌──────────┐  Ctrl-b   ┌──────────┐
//!  │  Normal  │ ────────> │  Prefix  │
//!  │          │ <──────── │          │
//!  └──────────┘  any key  └──────────┘
//!       │                      │
//!       │   ?                  │
//!       │   ┌──────────┐      │
//!       └──>│   Help   │<─────┘
//!           │          │
//!           └──────────┘
//!            any key → Normal
//! ```
//!
//! ## Frame Rate
//!
//! The event loop polls for keyboard input with a 16ms timeout (~60fps).
//! Between polls, it drains all available shadow-terminal output. This
//! balances responsiveness with CPU usage.
//!
//! ## Logging
//!
//! Debug output is written to `/tmp/claude-dind-mux.log` with timestamps.
//! Useful for diagnosing session lifecycle issues without interfering with
//! the TUI display.

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::fs::OpenOptions;
use std::io;
use std::io::Write as _;
use std::time::Duration;

use crate::container::ContainerManager;
use crate::render;
use crate::session::SessionManager;

/// The prefix key state machine for input handling.
///
/// In normal mode, all keystrokes are forwarded to the active session.
/// Pressing Ctrl-b enters prefix mode, where the next key is interpreted
/// as a multiplexer command. The help overlay is a third state where any
/// key dismisses it and returns to normal mode.
enum InputMode {
    /// Normal mode: all input goes to the active session, except the prefix key.
    Normal,
    /// Prefix mode: the next key is interpreted as a multiplexer command.
    Prefix,
    /// Help overlay is shown; any key dismisses it.
    Help,
}

/// Result of processing a key event in prefix mode.
///
/// After the user presses Ctrl-b (the prefix key), the next keystroke
/// is decoded into one of these actions.
enum PrefixAction {
    /// `c` — Create a new Claude Code session.
    CreateSession,
    /// `n` — Switch to the next session (wraps around).
    NextSession,
    /// `p` — Switch to the previous session (wraps around).
    PrevSession,
    /// `0`-`9` — Jump directly to a session by its index.
    JumpToSession(usize),
    /// `x` — Kill the currently active session.
    KillSession,
    /// `d` — Detach from the TUI (container keeps running).
    Detach,
    /// `?` — Toggle the help overlay.
    ShowHelp,
    /// `Ctrl-b` — The prefix key was pressed again; send a literal 0x02 byte
    /// to the active session (escape hatch for programs that use Ctrl-b).
    SendPrefix,
    /// Any other key — ignore and return to normal mode.
    Ignore,
}

/// Main entry point for the interactive multiplexer TUI.
///
/// This function takes ownership of the host terminal for the duration of
/// the multiplexer session. It:
///
/// 1. Enables crossterm raw mode (disables line buffering, echo, signal handling)
/// 2. Enters the alternate screen buffer (preserves the user's terminal history)
/// 3. Creates a ratatui terminal backed by crossterm
/// 4. Creates the first Claude session automatically
/// 5. Runs the main event loop until all sessions end or the user detaches
/// 6. Restores the terminal to its original state on exit
///
/// # Returns
///
/// - `Ok(true)` if the user detached (`Ctrl-b d`) — the container should keep running
/// - `Ok(false)` if all sessions ended naturally — the container should be stopped
/// - `Err(...)` on unrecoverable errors
///
/// # Arguments
///
/// - `container` — The running DinD container to create sessions in
/// - `detach_on_exit` — If true, exit the loop when all sessions end instead of
///   keeping the TUI alive waiting for new sessions
pub async fn run(container: &ContainerManager, detach_on_exit: bool) -> Result<bool> {
    // Open a log file for debugging (append mode)
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/claude-dind-mux.log")
        .ok();

    macro_rules! log {
        ($($arg:tt)*) => {
            if let Some(ref mut f) = log {
                let _ = writeln!(f, "[{}] {}", chrono_now(), format!($($arg)*));
                let _ = f.flush();
            }
        };
    }

    fn chrono_now() -> String {
        use std::time::SystemTime;
        let d = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        format!("{}.{:03}", d.as_secs(), d.subsec_millis())
    }

    log!("Starting multiplexer for container {}", container.short_id());

    // Set up terminal
    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)
        .context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).context("Failed to create terminal")?;

    let size = terminal.size().context("Failed to get terminal size")?;
    log!("Terminal size: {}x{}", size.width, size.height);

    let mut sessions = SessionManager::new(container.container_id.clone());
    let mut mode = InputMode::Normal;
    let mut detached = false;

    // Create the first session automatically
    sessions.create(size.width, size.height.saturating_sub(1))?;
    log!("First session created");

    // Main event loop
    let mut frame_count: u64 = 0;
    loop {
        frame_count += 1;

        // Poll all sessions for output updates
        for i in 0..sessions.sessions.len() {
            let got_output = sessions.poll_output(i);
            if got_output && frame_count <= 50 {
                log!("Session {} got output (frame {})", i, frame_count);
            }
        }

        // Check if active session's task has finished
        if let Some(session) = sessions.sessions.get_mut(sessions.active) {
            if session.terminal.task_handle.is_finished() && !session.exited {
                log!("Session {} task finished", sessions.active);
                session.exited = true;
            }
        }

        // Render
        terminal.draw(|frame| {
            render::render_frame(frame, &sessions, matches!(mode, InputMode::Help));
        })?;

        // Poll for keyboard events with a short timeout to keep output responsive
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    match mode {
                        InputMode::Help => {
                            // Any key dismisses the help overlay
                            mode = InputMode::Normal;
                        }
                        InputMode::Prefix => {
                            match decode_prefix_key(key) {
                                PrefixAction::CreateSession => {
                                    let size =
                                        terminal.size().context("Failed to get terminal size")?;
                                    sessions
                                        .create(size.width, size.height.saturating_sub(1))?;
                                    sessions.active = sessions.sessions.len() - 1;
                                }
                                PrefixAction::NextSession => {
                                    sessions.next();
                                }
                                PrefixAction::PrevSession => {
                                    sessions.prev();
                                }
                                PrefixAction::JumpToSession(idx) => {
                                    sessions.switch_to(idx);
                                }
                                PrefixAction::KillSession => {
                                    if !sessions.sessions.is_empty() {
                                        let idx = sessions.active;
                                        sessions.kill(idx)?;
                                        sessions.cleanup_exited();
                                    }
                                }
                                PrefixAction::Detach => {
                                    detached = true;
                                    break;
                                }
                                PrefixAction::ShowHelp => {
                                    mode = InputMode::Help;
                                    continue;
                                }
                                PrefixAction::SendPrefix => {
                                    // Send a literal Ctrl-b to the active session
                                    if !sessions.sessions.is_empty() {
                                        let _ = sessions
                                            .send_input(sessions.active, &[0x02])
                                            .await;
                                    }
                                }
                                PrefixAction::Ignore => {}
                            }
                            mode = InputMode::Normal;
                        }
                        InputMode::Normal => {
                            // Check for prefix key: Ctrl-b
                            if key.modifiers.contains(KeyModifiers::CONTROL)
                                && key.code == KeyCode::Char('b')
                            {
                                mode = InputMode::Prefix;
                                continue;
                            }

                            // Forward input to the active session
                            if !sessions.sessions.is_empty() {
                                if let Some(bytes) = key_event_to_bytes(key) {
                                    if let Err(e) = sessions.send_input(sessions.active, &bytes).await {
                                        log!("send_input error: {e}");
                                    }
                                }
                            }
                        }
                    }
                }
                Event::Resize(w, h) => {
                    // Propagate resize to all sessions
                    sessions.resize_all(w, h.saturating_sub(1));
                }
                _ => {}
            }
        }

        // If no sessions remain and not detaching, exit
        if sessions.sessions.is_empty() && !detach_on_exit {
            break;
        }
    }

    // Cleanup terminal
    terminal::disable_raw_mode().context("Failed to disable raw mode")?;
    crossterm::execute!(io::stdout(), LeaveAlternateScreen)
        .context("Failed to leave alternate screen")?;

    Ok(detached)
}

/// Decode a key event in prefix mode into a multiplexer action.
///
/// Called after the user has pressed Ctrl-b (the prefix key). Maps the
/// follow-up keystroke to a [`PrefixAction`].
fn decode_prefix_key(key: KeyEvent) -> PrefixAction {
    match key.code {
        KeyCode::Char('c') => PrefixAction::CreateSession,
        KeyCode::Char('n') => PrefixAction::NextSession,
        KeyCode::Char('p') => PrefixAction::PrevSession,
        KeyCode::Char('x') => PrefixAction::KillSession,
        KeyCode::Char('d') => PrefixAction::Detach,
        KeyCode::Char('?') => PrefixAction::ShowHelp,
        KeyCode::Char(c) if c.is_ascii_digit() => {
            let idx = c as usize - '0' as usize;
            PrefixAction::JumpToSession(idx)
        }
        // Ctrl-b again sends a literal Ctrl-b
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            PrefixAction::SendPrefix
        }
        _ => PrefixAction::Ignore,
    }
}

/// Convert a crossterm [`KeyEvent`] into raw bytes to send to the PTY.
///
/// Translates keyboard events into the byte sequences that a terminal
/// program expects to receive:
///
/// - **Ctrl+key**: Ctrl+a = 0x01, Ctrl+b = 0x02, ..., Ctrl+z = 0x1a
/// - **Printable characters**: UTF-8 encoded bytes
/// - **Enter**: `\r` (carriage return, not `\n`)
/// - **Backspace**: 0x7f (DEL, the standard terminal backspace)
/// - **Escape sequences**: Arrow keys, function keys, Home/End, etc.
///   use VT100/xterm escape sequences (e.g., `\x1b[A` for Up arrow)
///
/// Returns `None` for keys that have no PTY representation (e.g.,
/// modifier-only keys, unrecognized function keys).
fn key_event_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    // Handle Ctrl+key combinations
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char(c) => {
                // Ctrl+a = 0x01, Ctrl+b = 0x02, ..., Ctrl+z = 0x1a
                let byte = (c as u8).wrapping_sub(b'a').wrapping_add(1);
                if byte <= 26 {
                    return Some(vec![byte]);
                }
                return None;
            }
            _ => return None,
        }
    }

    match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            Some(s.as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        _ => None,
    }
}
