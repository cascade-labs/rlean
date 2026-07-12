//! Node registry — `~/.rlean/nodes.json`
//!
//! Mirrors the persistence patterns in [`crate::config`]: atomic writes and
//! 0600 owner-only permissions on the JSON file under `~/.rlean`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{atomic_write, rlean_dir, secure_owner_read_write};

// ── Paths ─────────────────────────────────────────────────────────────────────

pub fn nodes_path() -> Result<PathBuf> {
    Ok(rlean_dir()?.join("nodes.json"))
}

// ── Node ──────────────────────────────────────────────────────────────────────

/// A registered fleet node reachable over SSH.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Unique node name (defaults to the ssh alias).
    pub name: String,
    /// SSH destination (host or `~/.ssh/config` alias).
    pub ssh_dest: String,
    /// Operating system, lowercased (`linux` / `darwin` / other).
    pub os: String,
    /// Machine architecture as reported by `uname -m`.
    pub arch: String,
    /// Rust target triple derived from `os` + `arch`.
    pub platform: String,
    /// Full `uname -a` output captured at registration time.
    pub uname: String,
    /// Roles assigned to this node (e.g. `live`, `backtest`).
    pub roles: Vec<String>,
    /// RFC3339 timestamp of when the node was added.
    pub added_at: String,
    /// RFC3339 timestamp of the last successful probe.
    pub last_seen: String,
}

// ── Platform triple ───────────────────────────────────────────────────────────

/// Map `uname -s` (`os`) + `uname -m` (`arch`) to a Rust target triple.
///
/// Unknown combinations fall back to a best-effort lowercased
/// `"<arch>-<os>-unknown"` triple.
pub fn platform_triple(os: &str, arch: &str) -> String {
    match (os, arch) {
        ("Linux", "aarch64") | ("Linux", "arm64") => "aarch64-unknown-linux-gnu".to_string(),
        ("Linux", "x86_64") => "x86_64-unknown-linux-gnu".to_string(),
        ("Darwin", "arm64") | ("Darwin", "aarch64") => "aarch64-apple-darwin".to_string(),
        ("Darwin", "x86_64") => "x86_64-apple-darwin".to_string(),
        _ => format!("{}-{}-unknown", arch.to_lowercase(), os.to_lowercase()),
    }
}

// ── Node registry ─────────────────────────────────────────────────────────────

/// The persisted set of fleet nodes (`~/.rlean/nodes.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeRegistry {
    #[serde(default)]
    pub nodes: Vec<Node>,
}

impl NodeRegistry {
    /// Load the registry from `~/.rlean/nodes.json`, returning an empty registry
    /// when the file does not exist.
    pub fn load() -> Result<Self> {
        Self::load_from(&nodes_path()?)
    }

    /// Persist the registry to `~/.rlean/nodes.json` with 0600 permissions.
    pub fn save(&self) -> Result<()> {
        self.save_to(&nodes_path()?)
    }

    /// Load the registry from an explicit path (empty when missing).
    ///
    /// Keeps tests hermetic by avoiding `~/.rlean`.
    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
    }

    /// Persist the registry to an explicit path (atomic write + 0600).
    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(path, &text)?;
        secure_owner_read_write(path)
    }

    /// Add a node, erroring if one with the same name already exists.
    pub fn add(&mut self, node: Node) -> Result<()> {
        if self.nodes.iter().any(|n| n.name == node.name) {
            bail!("node '{}' already exists", node.name);
        }
        self.nodes.push(node);
        Ok(())
    }

    /// Remove a node by name, returning whether one was removed.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let before = self.nodes.len();
        self.nodes.retain(|n| n.name != name);
        Ok(self.nodes.len() != before)
    }

    /// Look up a node by name.
    pub fn get(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// Look up a node by name for mutation.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.name == name)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node(name: &str) -> Node {
        Node {
            name: name.to_string(),
            ssh_dest: name.to_string(),
            os: "linux".to_string(),
            arch: "aarch64".to_string(),
            platform: "aarch64-unknown-linux-gnu".to_string(),
            uname: "Linux box 6.1.0 aarch64 GNU/Linux".to_string(),
            roles: vec!["live".to_string()],
            added_at: "2026-01-01T00:00:00+00:00".to_string(),
            last_seen: "2026-01-01T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn round_trip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes.json");

        let mut reg = NodeRegistry::default();
        reg.add(sample_node("alpha")).unwrap();
        reg.add(sample_node("beta")).unwrap();
        reg.save_to(&path).unwrap();

        let loaded = NodeRegistry::load_from(&path).unwrap();
        assert_eq!(loaded.nodes, reg.nodes);
    }

    #[test]
    fn load_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let reg = NodeRegistry::load_from(&path).unwrap();
        assert!(reg.nodes.is_empty());
    }

    #[test]
    fn duplicate_name_errors() {
        let mut reg = NodeRegistry::default();
        reg.add(sample_node("alpha")).unwrap();
        assert!(reg.add(sample_node("alpha")).is_err());
    }

    #[test]
    fn remove_present_and_absent() {
        let mut reg = NodeRegistry::default();
        reg.add(sample_node("alpha")).unwrap();
        assert!(reg.remove("alpha").unwrap());
        assert!(!reg.remove("alpha").unwrap());
    }

    #[test]
    fn get_and_get_mut() {
        let mut reg = NodeRegistry::default();
        reg.add(sample_node("alpha")).unwrap();
        assert!(reg.get("alpha").is_some());
        assert!(reg.get("missing").is_none());
        reg.get_mut("alpha").unwrap().last_seen = "later".to_string();
        assert_eq!(reg.get("alpha").unwrap().last_seen, "later");
    }

    #[test]
    fn platform_triple_mappings() {
        assert_eq!(
            platform_triple("Linux", "aarch64"),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            platform_triple("Linux", "x86_64"),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(platform_triple("Darwin", "arm64"), "aarch64-apple-darwin");
        assert_eq!(platform_triple("Darwin", "x86_64"), "x86_64-apple-darwin");
        // Fallback: lowercased best-effort triple.
        assert_eq!(platform_triple("FreeBSD", "RISCV"), "riscv-freebsd-unknown");
    }
}
