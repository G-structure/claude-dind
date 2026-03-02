//! Tree visualization widget for the Agentic Loom.
//!
//! Renders a git-log-style tree view of checkpoint snapshots using Unicode
//! box-drawing characters. The tree shows parent-child relationships with
//! connecting lines, highlights the currently selected and active nodes,
//! and displays relative timestamps.
//!
//! ```text
//!   Snapshot Tree (4 checkpoints)
//!   ─────────────────────────────
//!   * [1] initial                      2m ago
//!   ├─* [2] after-setup                1m ago
//!   │ └─* [4] experiment-B             30s ago
//!   └─* [3] experiment-A ●            45s ago  ← current
//!
//!   j/k navigate  Enter restore  d delete  q back
//! ```

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::loom::{self, FlatNode, LoomTree};

/// State for the tree view navigation.
pub struct TreeViewState {
    /// Flattened nodes for rendering.
    pub flat_nodes: Vec<FlatNode>,
    /// Currently selected index in the flat list.
    pub selected: usize,
    /// Scroll offset for large trees.
    pub scroll: usize,
}

impl TreeViewState {
    pub fn new() -> Self {
        Self {
            flat_nodes: Vec::new(),
            selected: 0,
            scroll: 0,
        }
    }

    /// Rebuild the flat node list from the tree and reset selection.
    pub fn refresh(&mut self, tree: &LoomTree) {
        self.flat_nodes = tree.build_flat_list();
        if self.selected >= self.flat_nodes.len() && !self.flat_nodes.is_empty() {
            self.selected = self.flat_nodes.len() - 1;
        }
    }

    /// Move selection up.
    pub fn up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    pub fn down(&mut self) {
        if !self.flat_nodes.is_empty() && self.selected < self.flat_nodes.len() - 1 {
            self.selected += 1;
        }
    }

    /// Get the node ID of the currently selected node.
    pub fn selected_node_id(&self) -> Option<u64> {
        self.flat_nodes.get(self.selected).map(|n| n.node_id)
    }
}

/// Render the full tree view panel: header, tree body, and footer keybind hints.
pub fn render_tree_view(
    frame: &mut Frame,
    area: Rect,
    tree: &LoomTree,
    state: &TreeViewState,
) {
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();

    // Header
    let count = tree.len();
    lines.push(Line::from(Span::styled(
        format!("  Snapshot Tree ({count} checkpoint{})", if count == 1 { "" } else { "s" }),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(area.width.saturating_sub(4) as usize)),
        Style::default().fg(Color::DarkGray),
    )));

    if state.flat_nodes.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  No checkpoints yet. Press Ctrl-b s to create one.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Compute tree prefixes using ancestor stack
        // We track which depths have a continuation line (│)
        let flat = &state.flat_nodes;

        // Build a set: for each depth, is there still a sibling coming?
        // We compute this by tracking is_last_sibling at each depth.
        let mut continuation: Vec<bool> = Vec::new(); // true = draw │ at this depth

        for (idx, node) in flat.iter().enumerate() {
            let depth = node.depth;

            // Adjust continuation stack to current depth
            while continuation.len() > depth {
                continuation.pop();
            }

            // Build the prefix string
            let mut prefix = String::from("  ");

            if depth == 0 {
                prefix.push_str("* ");
            } else {
                // Draw continuation lines for ancestors
                for d in 0..depth.saturating_sub(1) {
                    if d < continuation.len() && continuation[d] {
                        prefix.push_str("│ ");
                    } else {
                        prefix.push_str("  ");
                    }
                }

                // Draw connector for this node
                if node.is_last_sibling {
                    prefix.push_str("└─* ");
                } else {
                    prefix.push_str("├─* ");
                }
            }

            // Update continuation: at this depth, are there more siblings?
            if depth > 0 {
                let parent_depth = depth - 1;
                while continuation.len() <= parent_depth {
                    continuation.push(false);
                }
                continuation[parent_depth] = !node.is_last_sibling;
            }

            // Build the label part
            let current_marker = if node.is_current { " \u{25cf}" } else { "" };
            let time_str = loom::relative_time(node.timestamp);
            let label = format!("[{}] {}{}", node.node_id, node.label, current_marker);

            // Pad to align timestamps
            let content_len = prefix.len() + label.len();
            let available = area.width as usize;
            let time_with_pad = if content_len + time_str.len() + 2 < available {
                let padding = available - content_len - time_str.len() - 2;
                format!("{}{}", " ".repeat(padding), time_str)
            } else {
                format!("  {time_str}")
            };

            let is_selected = idx == state.selected;

            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if node.is_current {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let time_style = if is_selected {
                Style::default().fg(Color::Black).bg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(label, style),
                Span::styled(time_with_pad, time_style),
            ]));
        }
    }

    // Footer
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  j/k", Style::default().fg(Color::Green)),
        Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Green)),
        Span::styled(" restore  ", Style::default().fg(Color::DarkGray)),
        Span::styled("d", Style::default().fg(Color::Green)),
        Span::styled(" delete  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Green)),
        Span::styled(" back", Style::default().fg(Color::DarkGray)),
    ]));

    let block = Block::default()
        .style(Style::default().bg(Color::Black));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((state.scroll as u16, 0));

    frame.render_widget(paragraph, area);
}

/// Render the label input overlay at the bottom of the screen.
pub fn render_label_input(frame: &mut Frame, area: Rect, label_buffer: &str) {
    let h = 3u16;
    let y = area.height.saturating_sub(h);
    let overlay = Rect::new(area.x, area.y + y, area.width, h);

    frame.render_widget(Clear, overlay);

    let text = Line::from(vec![
        Span::styled("  Checkpoint label: ", Style::default().fg(Color::Yellow)),
        Span::styled(label_buffer, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("█", Style::default().fg(Color::White)),
    ]);

    let hint = Line::from(Span::styled(
        "  Enter to confirm, Esc to cancel",
        Style::default().fg(Color::DarkGray),
    ));

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let paragraph = Paragraph::new(vec![text, hint]).block(block);
    frame.render_widget(paragraph, overlay);
}
