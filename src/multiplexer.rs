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

/// The prefix key state machine.
enum InputMode {
    /// Normal mode: all input goes to the active session, except the prefix key.
    Normal,
    /// Prefix mode: the next key is interpreted as a multiplexer command.
    Prefix,
    /// Help overlay is shown; any key dismisses it.
    Help,
}

/// Result of processing a key event in prefix mode.
enum PrefixAction {
    CreateSession,
    NextSession,
    PrevSession,
    JumpToSession(usize),
    KillSession,
    Detach,
    ShowHelp,
    /// The prefix key was pressed again — send a literal Ctrl-b to the session.
    SendPrefix,
    /// Unknown key after prefix — ignore.
    Ignore,
}

/// Main entry point for the interactive multiplexer TUI.
///
/// This function:
/// 1. Sets up crossterm raw mode and alternate screen
/// 2. Creates a ratatui terminal
/// 3. Runs the main event loop (poll terminal output + handle keyboard input)
/// 4. Cleans up on exit
///
/// Returns `true` if the user detached (container should keep running),
/// `false` if all sessions ended or an error occurred.
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

/// Convert a crossterm KeyEvent into bytes to send to the PTY.
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
