//! Cloud deployment registry — `~/.rlean/cloud-deployments.json`
//!
//! Records every `rlean cloud deploy` so `status`/`logs`/`portfolio` can locate
//! a node deployment without re-listing every node. Mirrors the persistence
//! patterns in [`super::registry`] and [`crate::config`]: atomic writes with
//! 0600 owner-only permissions under `~/.rlean`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{atomic_write, rlean_dir, secure_owner_read_write};

// ── Paths ─────────────────────────────────────────────────────────────────────

pub fn cloud_deployments_path() -> Result<PathBuf> {
    Ok(rlean_dir()?.join("cloud-deployments.json"))
}

// ── Record ─────────────────────────────────────────────────────────────────────

/// A single strategy deployment launched on a remote node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudDeployment {
    /// Registry name of the node the deployment runs on.
    pub node: String,
    /// Strategy name (LEAN-style, derived from the strategy path).
    pub strategy_name: String,
    /// Deploy id (`<YYYY-MM-DD_HHMMSS>_<strategy-name>`, UTC).
    pub deploy_id: String,
    /// Absolute strategy directory on the node (cwd for `rlean live` control).
    pub strategy_dir: String,
    /// Absolute deploy directory on the node
    /// (`<strategy_dir>/live/<deploy_id>`).
    pub deploy_dir: String,
    /// RFC3339 launch timestamp (control machine clock).
    pub launched_at: String,
    /// Remote PID captured from the launch command, when available.
    pub pid: Option<u32>,
}

// ── Registry ────────────────────────────────────────────────────────────────────

/// The persisted set of cloud deployments (`~/.rlean/cloud-deployments.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudDeploymentRegistry {
    #[serde(default)]
    pub deployments: Vec<CloudDeployment>,
}

impl CloudDeploymentRegistry {
    /// Load from `~/.rlean/cloud-deployments.json` (empty when missing).
    pub fn load() -> Result<Self> {
        Self::load_from(&cloud_deployments_path()?)
    }

    /// Persist to `~/.rlean/cloud-deployments.json` with 0600 permissions.
    pub fn save(&self) -> Result<()> {
        self.save_to(&cloud_deployments_path()?)
    }

    /// Load from an explicit path (empty when missing). Keeps tests hermetic.
    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
    }

    /// Persist to an explicit path (atomic write + 0600).
    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(path, &text)?;
        secure_owner_read_write(path)
    }

    /// Record a deployment. A repeated `(node, deploy_id)` replaces the prior
    /// record so re-runs don't accumulate duplicates.
    pub fn record(&mut self, deployment: CloudDeployment) {
        self.deployments
            .retain(|d| !(d.node == deployment.node && d.deploy_id == deployment.deploy_id));
        self.deployments.push(deployment);
    }

    /// All deployments recorded for the given node, most recent first.
    pub fn for_node(&self, node: &str) -> Vec<&CloudDeployment> {
        let mut rows: Vec<&CloudDeployment> =
            self.deployments.iter().filter(|d| d.node == node).collect();
        rows.sort_by(|a, b| b.launched_at.cmp(&a.launched_at));
        rows
    }

    /// Resolve the deploy record for a node when `--deploy-id` is omitted.
    ///
    /// Returns the single most-recent record. Errors when a node has more than
    /// one recorded deployment (the operator must disambiguate with
    /// `--deploy-id`) or none at all.
    pub fn resolve_latest(&self, node: &str) -> Result<&CloudDeployment> {
        let rows = self.for_node(node);
        match rows.len() {
            0 => bail!(
                "no recorded deployments for node '{node}' — pass --deploy-id or run `rlean cloud deploy`"
            ),
            1 => Ok(rows[0]),
            _ => bail!(
                "node '{node}' has {} recorded deployments — pass --deploy-id to pick one",
                rows.len()
            ),
        }
    }

    /// Look up a specific `(node, deploy_id)` record.
    pub fn find(&self, node: &str, deploy_id: &str) -> Option<&CloudDeployment> {
        self.deployments
            .iter()
            .find(|d| d.node == node && d.deploy_id == deploy_id)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(node: &str, deploy_id: &str, launched: &str) -> CloudDeployment {
        CloudDeployment {
            node: node.to_string(),
            strategy_name: "uw_control".to_string(),
            deploy_id: deploy_id.to_string(),
            strategy_dir: "/home/opc/rlean-cloud/workspace/uw_control".to_string(),
            deploy_dir: format!("/home/opc/rlean-cloud/workspace/uw_control/live/{deploy_id}"),
            launched_at: launched.to_string(),
            pid: Some(1234),
        }
    }

    #[test]
    fn round_trip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-deployments.json");

        let mut reg = CloudDeploymentRegistry::default();
        reg.record(sample(
            "n1",
            "2026-07-10_120000_uw_control",
            "2026-07-10T12:00:00+00:00",
        ));
        reg.save_to(&path).unwrap();

        let loaded = CloudDeploymentRegistry::load_from(&path).unwrap();
        assert_eq!(loaded.deployments, reg.deployments);
    }

    #[test]
    fn record_replaces_same_node_deploy_id() {
        let mut reg = CloudDeploymentRegistry::default();
        reg.record(sample("n1", "d1", "2026-07-10T12:00:00+00:00"));
        let mut updated = sample("n1", "d1", "2026-07-10T13:00:00+00:00");
        updated.pid = Some(999);
        reg.record(updated);
        assert_eq!(reg.deployments.len(), 1);
        assert_eq!(reg.deployments[0].pid, Some(999));
    }

    #[test]
    fn resolve_latest_picks_single_and_errors_on_ambiguity() {
        let mut reg = CloudDeploymentRegistry::default();
        // Only one for n1 → resolves.
        reg.record(sample("n1", "d1", "2026-07-10T12:00:00+00:00"));
        assert_eq!(reg.resolve_latest("n1").unwrap().deploy_id, "d1");

        // Two for n2 → ambiguous.
        reg.record(sample("n2", "e1", "2026-07-10T12:00:00+00:00"));
        reg.record(sample("n2", "e2", "2026-07-10T13:00:00+00:00"));
        assert!(reg.resolve_latest("n2").is_err());

        // None for n3 → error.
        assert!(reg.resolve_latest("n3").is_err());
    }

    #[test]
    fn for_node_sorted_most_recent_first() {
        let mut reg = CloudDeploymentRegistry::default();
        reg.record(sample("n1", "old", "2026-07-10T10:00:00+00:00"));
        reg.record(sample("n1", "new", "2026-07-10T14:00:00+00:00"));
        let rows = reg.for_node("n1");
        assert_eq!(rows[0].deploy_id, "new");
        assert_eq!(rows[1].deploy_id, "old");
    }
}
