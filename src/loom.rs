//! Checkpoint tree data model for the Agentic Loom.
//!
//! Manages a tree of CRIU checkpoint snapshots. Each node represents a
//! `docker checkpoint create` snapshot of a running container's process
//! state. The tree tracks parent-child relationships, allowing navigation
//! between branching points in an agent's execution history.
//!
//! Persisted as JSON at the configured loom file path (default:
//! `~/.claude-dind/loom.json`). The tree is loaded on startup and saved
//! after every mutation.
//!
//! This module is pure data — no Docker operations, no TUI rendering.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single checkpoint snapshot in the loom tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotNode {
    /// Monotonic counter ID, unique within this tree.
    pub id: u64,
    /// Parent node ID. `None` for root nodes.
    pub parent_id: Option<u64>,
    /// User-provided label (e.g., "initial", "after-setup").
    pub label: String,
    /// Unix timestamp (seconds) when the checkpoint was taken.
    pub timestamp: u64,
    /// Docker checkpoint name (e.g., "loom-3-initial").
    pub checkpoint_name: String,
    /// Container ID that was checkpointed.
    pub source_container_id: String,
    /// Optional longer description.
    pub description: Option<String>,
}

/// The full checkpoint tree with persistence metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoomTree {
    /// Schema version for forward compatibility.
    pub schema_version: String,
    /// Next ID to assign (monotonic counter).
    pub next_id: u64,
    /// All nodes keyed by ID.
    pub nodes: HashMap<u64, SnapshotNode>,
    /// Which checkpoint we're currently "on" (last restored or created).
    pub current_node_id: Option<u64>,
}

/// Flattened node for rendering the tree in a terminal widget.
/// Produced by [`LoomTree::build_flat_list`] via DFS traversal.
#[derive(Debug, Clone)]
pub struct FlatNode {
    pub node_id: u64,
    pub depth: usize,
    pub label: String,
    pub timestamp: u64,
    pub is_current: bool,
    pub is_last_sibling: bool,
}

impl LoomTree {
    /// Create a new empty tree.
    fn new() -> Self {
        Self {
            schema_version: "loom-v1".to_string(),
            next_id: 1,
            nodes: HashMap::new(),
            current_node_id: None,
        }
    }

    /// Load tree from disk, or create a new one if the file doesn't exist.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let data = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read loom file: {}", path.display()))?;
            let tree: Self = serde_json::from_str(&data)
                .with_context(|| format!("Failed to parse loom file: {}", path.display()))?;
            Ok(tree)
        } else {
            Ok(Self::new())
        }
    }

    /// Save tree to disk as JSON.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }
        let data = serde_json::to_string_pretty(self)
            .context("Failed to serialize loom tree")?;
        std::fs::write(path, data)
            .with_context(|| format!("Failed to write loom file: {}", path.display()))?;
        Ok(())
    }

    /// Add a new checkpoint node to the tree. Returns the new node's ID.
    ///
    /// The node's parent is set to `parent_id` (typically `current_node_id`).
    /// After adding, `current_node_id` is updated to point to the new node.
    pub fn add_node(
        &mut self,
        parent_id: Option<u64>,
        label: &str,
        container_id: &str,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let checkpoint_name = format!("loom-{}-{}", id, sanitize_label(label));
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let node = SnapshotNode {
            id,
            parent_id,
            label: label.to_string(),
            timestamp,
            checkpoint_name,
            source_container_id: container_id.to_string(),
            description: None,
        };

        self.nodes.insert(id, node);
        self.current_node_id = Some(id);
        id
    }

    /// Get all children of a given node ID, sorted by ID.
    pub fn get_children(&self, parent_id: u64) -> Vec<&SnapshotNode> {
        let mut children: Vec<&SnapshotNode> = self
            .nodes
            .values()
            .filter(|n| n.parent_id == Some(parent_id))
            .collect();
        children.sort_by_key(|n| n.id);
        children
    }

    /// Get all root nodes (nodes with no parent), sorted by ID.
    pub fn roots(&self) -> Vec<&SnapshotNode> {
        let mut roots: Vec<&SnapshotNode> = self
            .nodes
            .values()
            .filter(|n| n.parent_id.is_none())
            .collect();
        roots.sort_by_key(|n| n.id);
        roots
    }

    /// Remove a node by ID. Also removes all descendants recursively.
    /// Returns the list of checkpoint names that were removed (for Docker cleanup).
    pub fn remove_node(&mut self, id: u64) -> Vec<String> {
        let mut removed = Vec::new();
        let mut to_remove = vec![id];

        while let Some(node_id) = to_remove.pop() {
            // Find children of this node
            let children: Vec<u64> = self
                .nodes
                .values()
                .filter(|n| n.parent_id == Some(node_id))
                .map(|n| n.id)
                .collect();
            to_remove.extend(children);

            if let Some(node) = self.nodes.remove(&node_id) {
                removed.push(node.checkpoint_name);
            }
        }

        // If current_node_id was removed, clear it
        if let Some(current) = self.current_node_id {
            if !self.nodes.contains_key(&current) {
                self.current_node_id = None;
            }
        }

        removed
    }

    /// Build a flat list of nodes via DFS traversal, suitable for rendering.
    pub fn build_flat_list(&self) -> Vec<FlatNode> {
        let mut flat = Vec::new();
        let roots = self.roots();

        for (i, root) in roots.iter().enumerate() {
            let is_last = i == roots.len() - 1;
            self.dfs_flatten(root.id, 0, is_last, &mut flat);
        }

        flat
    }

    /// Recursive DFS to flatten the tree.
    fn dfs_flatten(
        &self,
        node_id: u64,
        depth: usize,
        is_last_sibling: bool,
        flat: &mut Vec<FlatNode>,
    ) {
        if let Some(node) = self.nodes.get(&node_id) {
            flat.push(FlatNode {
                node_id: node.id,
                depth,
                label: node.label.clone(),
                timestamp: node.timestamp,
                is_current: self.current_node_id == Some(node.id),
                is_last_sibling,
            });

            let children = self.get_children(node_id);
            for (i, child) in children.iter().enumerate() {
                let is_last = i == children.len() - 1;
                self.dfs_flatten(child.id, depth + 1, is_last, flat);
            }
        }
    }

    /// Number of checkpoints in the tree.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Sanitize a label for use in Docker checkpoint names.
/// Replaces non-alphanumeric chars with hyphens, lowercases, truncates to 40 chars.
fn sanitize_label(label: &str) -> String {
    let sanitized: String = label
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.len() > 40 {
        trimmed[..40].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Format a Unix timestamp as a relative time string (e.g., "30s ago", "2m ago").
pub fn relative_time(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let diff = now.saturating_sub(timestamp);

    if diff < 60 {
        format!("{}s ago", diff)
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_tree_is_empty() {
        let tree = LoomTree::load_or_create(Path::new("/nonexistent/path.json"))
            .unwrap_or_else(|_| LoomTree::new());
        assert!(tree.is_empty());
        assert_eq!(tree.next_id, 1);
        assert_eq!(tree.current_node_id, None);
    }

    #[test]
    fn test_add_and_get_nodes() {
        let mut tree = LoomTree::new();

        let id1 = tree.add_node(None, "initial", "abc123");
        assert_eq!(id1, 1);
        assert_eq!(tree.current_node_id, Some(1));
        assert_eq!(tree.len(), 1);

        let id2 = tree.add_node(Some(id1), "after-setup", "abc123");
        assert_eq!(id2, 2);
        assert_eq!(tree.current_node_id, Some(2));

        let id3 = tree.add_node(Some(id1), "experiment-A", "abc123");
        assert_eq!(id3, 3);

        // Check tree structure
        let roots = tree.roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, 1);

        let children = tree.get_children(id1);
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].id, 2);
        assert_eq!(children[1].id, 3);
    }

    #[test]
    fn test_remove_node_cascades() {
        let mut tree = LoomTree::new();

        let id1 = tree.add_node(None, "root", "abc123");
        let id2 = tree.add_node(Some(id1), "child", "abc123");
        let _id3 = tree.add_node(Some(id2), "grandchild", "abc123");

        // Remove child — should also remove grandchild
        let removed = tree.remove_node(id2);
        assert_eq!(removed.len(), 2);
        assert_eq!(tree.len(), 1); // Only root remains
        assert!(tree.nodes.contains_key(&id1));
    }

    #[test]
    fn test_flat_list_ordering() {
        let mut tree = LoomTree::new();

        let id1 = tree.add_node(None, "root", "abc123");
        let id2 = tree.add_node(Some(id1), "child-a", "abc123");
        let _id3 = tree.add_node(Some(id1), "child-b", "abc123");
        let _id4 = tree.add_node(Some(id2), "grandchild", "abc123");

        let flat = tree.build_flat_list();
        assert_eq!(flat.len(), 4);
        assert_eq!(flat[0].node_id, id1);
        assert_eq!(flat[0].depth, 0);
        assert_eq!(flat[1].node_id, id2);
        assert_eq!(flat[1].depth, 1);
        assert_eq!(flat[2].depth, 2); // grandchild
        assert_eq!(flat[3].depth, 1); // child-b
    }

    #[test]
    fn test_sanitize_label() {
        assert_eq!(sanitize_label("Hello World!"), "hello-world");
        assert_eq!(sanitize_label("after-setup"), "after-setup");
        assert_eq!(sanitize_label("  spaces  "), "spaces");
    }

    #[test]
    fn test_checkpoint_name_format() {
        let mut tree = LoomTree::new();
        tree.add_node(None, "initial", "abc123");
        let node = tree.nodes.get(&1).unwrap();
        assert_eq!(node.checkpoint_name, "loom-1-initial");
    }

    #[test]
    fn test_save_and_load() {
        let dir = std::env::temp_dir().join("loom-test");
        let path = dir.join("test-loom.json");

        let mut tree = LoomTree::new();
        tree.add_node(None, "test-node", "container-123");
        tree.save(&path).unwrap();

        let loaded = LoomTree::load_or_create(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.next_id, 2);
        assert_eq!(loaded.current_node_id, Some(1));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
