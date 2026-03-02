//! Rendering for the interactive terminal multiplexer.
//!
//! This module bridges two terminal abstraction layers:
//!
//! 1. **termwiz** (from shadow-terminal) — provides the virtual terminal state
//!    as a [`Surface`]: a 2D grid of cells, each with a character, foreground
//!    color, background color, and text attributes (bold, italic, underline, etc.).
//!
//! 2. **ratatui** — the TUI framework that renders to the real host terminal.
//!    It has its own cell buffer, color types, and style system.
//!
//! The [`TerminalWidget`] bridges these by iterating through the termwiz
//! `Surface` and writing each cell into the ratatui `Buffer`, converting
//! colors and attributes along the way.
//!
//! ## Layout
//!
//! ```text
//! ┌──────────────────────────────────────┐
//! │                                      │  ← Terminal area (TerminalWidget)
//! │  Active session's terminal output    │     Fills all available space
//! │  (rendered from termwiz Surface)     │
//! │                                      │
//! │                                      │
//! ├──────────────────────────────────────┤
//! │ [0:claude-1*] [1:claude-2]  Ctrl-b ? │  ← Status bar (1 line)
//! └──────────────────────────────────────┘
//! ```
//!
//! An optional help overlay can be displayed centered on screen.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use ratatui::Frame;
use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::surface::Surface;

use crate::session::SessionManager;

/// Render the complete multiplexer frame: terminal area + status bar + optional help.
///
/// This is called on every frame (~60fps). The layout is:
/// - Terminal area: fills all available space, renders the active session's screen
/// - Status bar: fixed 1-line height at the bottom
/// - Help overlay: centered panel, rendered on top of everything when `show_help` is true
pub fn render_frame(frame: &mut Frame, sessions: &SessionManager, show_help: bool) {
    let chunks = Layout::vertical([
        Constraint::Min(1),    // Terminal area
        Constraint::Length(1), // Status bar
    ])
    .split(frame.area());

    render_terminal_area(frame, chunks[0], sessions);
    render_status_bar(frame, chunks[1], sessions);

    if show_help {
        render_help_overlay(frame);
    }
}

/// Render the active session's terminal buffer.
fn render_terminal_area(frame: &mut Frame, area: Rect, sessions: &SessionManager) {
    if sessions.sessions.is_empty() {
        let msg = Paragraph::new("No sessions. Press Ctrl-b c to create one.")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(msg, area);
        return;
    }

    if let Some(session) = sessions.sessions.get(sessions.active) {
        let widget = TerminalWidget {
            surface: &session.screen,
        };
        frame.render_widget(widget, area);
    }
}

/// Render the status bar showing session list and key hints.
fn render_status_bar(frame: &mut Frame, area: Rect, sessions: &SessionManager) {
    let mut spans = Vec::new();

    for (i, session) in sessions.sessions.iter().enumerate() {
        let marker = if i == sessions.active { "*" } else { "" };
        let style = if i == sessions.active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else if session.exited {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        };

        spans.push(Span::styled(format!(" {}:{}{} ", i, session.name, marker), style));
        spans.push(Span::raw(" "));
    }

    if sessions.sessions.is_empty() {
        spans.push(Span::styled(
            " (no sessions) ",
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Right-align the help hint
    let hint = " Ctrl-b ? for help ";
    let content_len: usize = spans.iter().map(|s| s.width()).sum();
    let padding = area
        .width
        .saturating_sub(content_len as u16 + hint.len() as u16);
    spans.push(Span::raw(" ".repeat(padding as usize)));
    spans.push(Span::styled(
        hint,
        Style::default().fg(Color::Yellow).bg(Color::DarkGray),
    ));

    let bar = Line::from(spans);
    let paragraph = Paragraph::new(bar).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(paragraph, area);
}

/// Render the help overlay panel.
fn render_help_overlay(frame: &mut Frame) {
    let area = frame.area();
    // Center a box roughly 50x14
    let w = 50u16.min(area.width.saturating_sub(4));
    let h = 16u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let overlay = Rect::new(x, y, w, h);

    frame.render_widget(Clear, overlay);

    let help_text = vec![
        Line::from(Span::styled(
            "Keybindings (prefix: Ctrl-b)",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  c    ", Style::default().fg(Color::Green)),
            Span::raw("Create new session"),
        ]),
        Line::from(vec![
            Span::styled("  n    ", Style::default().fg(Color::Green)),
            Span::raw("Next session"),
        ]),
        Line::from(vec![
            Span::styled("  p    ", Style::default().fg(Color::Green)),
            Span::raw("Previous session"),
        ]),
        Line::from(vec![
            Span::styled("  0-9  ", Style::default().fg(Color::Green)),
            Span::raw("Jump to session by index"),
        ]),
        Line::from(vec![
            Span::styled("  x    ", Style::default().fg(Color::Green)),
            Span::raw("Kill current session"),
        ]),
        Line::from(vec![
            Span::styled("  d    ", Style::default().fg(Color::Green)),
            Span::raw("Detach (exit TUI, container keeps running)"),
        ]),
        Line::from(vec![
            Span::styled("  ?    ", Style::default().fg(Color::Green)),
            Span::raw("Toggle this help"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "All other input is forwarded to the active session.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Press any key to close this help.",
            Style::default().fg(Color::Yellow),
        )),
    ];

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let paragraph = Paragraph::new(help_text).block(block);
    frame.render_widget(paragraph, overlay);
}

/// A ratatui widget that renders a termwiz [`Surface`] into the host terminal.
///
/// This is the core rendering bridge between shadow-terminal and ratatui.
/// It iterates through the termwiz Surface's cell grid using:
///
/// 1. `surface.screen_lines()` — returns `Vec<Cow<Line>>` (rows of the terminal)
/// 2. `line.cells()` — returns `&[Cell]` (individual cells in a row)
/// 3. `cell.str()` — the character(s) in this cell
/// 4. `cell.attrs()` — the cell's visual attributes (colors, bold, etc.)
///
/// Each termwiz cell is mapped to the corresponding ratatui buffer cell with
/// converted colors and style modifiers. Cells beyond the widget's bounds are
/// skipped.
///
/// Note: termwiz's `Surface` does not have a `get_cell(x, y)` method — you
/// must iterate through `screen_lines()` and `cells()` sequentially.
struct TerminalWidget<'a> {
    surface: &'a Surface,
}

impl Widget for TerminalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = self.surface.screen_lines();

        for (row_idx, line) in lines.iter().enumerate() {
            if row_idx as u16 >= area.height {
                break;
            }
            let cells = line.cells();
            for (col_idx, cell) in cells.iter().enumerate() {
                if col_idx as u16 >= area.width {
                    break;
                }

                let buf_cell = &mut buf[(area.x + col_idx as u16, area.y + row_idx as u16)];

                let ch = cell.str();
                if !ch.is_empty() {
                    buf_cell.set_symbol(ch);
                }

                let attrs = cell.attrs();
                buf_cell.set_style(termwiz_attrs_to_ratatui_style(attrs));
            }
        }
    }
}

/// Convert a termwiz [`ColorAttribute`] to a ratatui [`Color`].
///
/// termwiz uses `ColorAttribute` (not `ColorSpec`) for foreground/background:
/// - `Default` — use the terminal's default color (returns `None`)
/// - `PaletteIndex(u8)` — a 256-color palette index (mapped to `Color::Indexed`)
/// - `TrueColorWithDefaultFallback(SrgbaTuple)` — 24-bit RGB, falls back to default
/// - `TrueColorWithPaletteFallback(SrgbaTuple, u8)` — 24-bit RGB, falls back to palette
///
/// `SrgbaTuple` stores RGBA as `(f32, f32, f32, f32)` in [0.0, 1.0] range.
/// We use `.as_rgba_u8()` to convert to `(u8, u8, u8, u8)` for ratatui.
fn termwiz_color_to_ratatui(color: ColorAttribute) -> Option<Color> {
    match color {
        ColorAttribute::Default => None,
        ColorAttribute::PaletteIndex(idx) => Some(Color::Indexed(idx)),
        ColorAttribute::TrueColorWithDefaultFallback(srgba) => {
            let (r, g, b, _) = srgba.as_rgba_u8();
            Some(Color::Rgb(r, g, b))
        }
        ColorAttribute::TrueColorWithPaletteFallback(srgba, _) => {
            let (r, g, b, _) = srgba.as_rgba_u8();
            Some(Color::Rgb(r, g, b))
        }
    }
}

/// Convert termwiz [`CellAttributes`] to a ratatui [`Style`].
///
/// Maps the following attributes:
/// - **Intensity**: `Bold` → `Modifier::BOLD`, `Half` → `Modifier::DIM`
///   (Note: termwiz uses `intensity()` returning an `Intensity` enum,
///   not a `bold()` boolean method.)
/// - **Italic**: `italic()` → `Modifier::ITALIC`
/// - **Underline**: `underline() != Underline::None` → `Modifier::UNDERLINED`
/// - **Strikethrough**: `strikethrough()` → `Modifier::CROSSED_OUT`
/// - **Reverse**: `reverse()` → `Modifier::REVERSED` (swaps fg/bg)
/// - **Foreground/Background**: via [`termwiz_color_to_ratatui`]
fn termwiz_attrs_to_ratatui_style(attrs: &CellAttributes) -> Style {
    let mut style = Style::default();

    if let Some(fg) = termwiz_color_to_ratatui(attrs.foreground()) {
        style = style.fg(fg);
    }
    if let Some(bg) = termwiz_color_to_ratatui(attrs.background()) {
        style = style.bg(bg);
    }

    if attrs.intensity() == Intensity::Bold {
        style = style.add_modifier(Modifier::BOLD);
    } else if attrs.intensity() == Intensity::Half {
        style = style.add_modifier(Modifier::DIM);
    }

    if attrs.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if attrs.underline() != Underline::None {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if attrs.strikethrough() {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if attrs.reverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }

    style
}
