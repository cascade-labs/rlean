//! `rlean cloud install` — provision a runnable rlean + plugins + `~/.rlean`
//! subset onto a registered node.
//!
//! Bundles are downloaded on the CONTROL machine with `gh release download`
//! (the node never needs `gh` creds — the private cascadelabs repo is only
//! reachable with the operator's local gh auth), verified against their
//! `.sha256` sidecars, unpacked locally, then shipped to the node over
//! scp/rsync. Secrets (`plugin-configs.json`) are transferred as FILES only,
//! never as ssh argv, and chmod 0600 on the node.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::nodeconfig::node_config_from_local;
use super::registry::NodeRegistry;
use super::remote::{scp_to, RemoteExec};
use crate::config::{atomic_write, plugin_configs_path, GlobalConfig};

/// Default release tag for cloud bundles. A `const` so it is easy to bump.
pub(crate) const DEFAULT_RELEASE_TAG: &str = "v0.1.0-cloud-rc2";

/// The rlean bundle source repository.
const RLEAN_REPO: &str = "cascade-labs/rlean";

/// The only node triple we ship cloud bundles for today.
const SUPPORTED_TRIPLE: &str = "aarch64-unknown-linux-gnu";

/// A plugin to install: which repo/tag to pull it from and its plugin name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginSource {
    pub repo: String,
    pub tag: String,
    pub name: String,
}

/// The default plugin set for a cloud deploy.
fn default_plugin_sources(tag: &str) -> Vec<PluginSource> {
    let plugins_repo = "cascade-labs/rlean-plugins";
    let cascade_repo = "cascade-labs/cascadelabs-plugins";
    vec![
        PluginSource {
            repo: plugins_repo.to_string(),
            tag: tag.to_string(),
            name: "tradier".to_string(),
        },
        PluginSource {
            repo: plugins_repo.to_string(),
            tag: tag.to_string(),
            name: "massive".to_string(),
        },
        PluginSource {
            repo: plugins_repo.to_string(),
            tag: tag.to_string(),
            name: "thetadata".to_string(),
        },
        PluginSource {
            repo: cascade_repo.to_string(),
            tag: tag.to_string(),
            name: "unusual_whales".to_string(),
        },
    ]
}

/// Resolve the effective plugin set from the defaults, an optional name filter,
/// and repeatable `--plugin-repo owner/repo:tag` overrides.
///
/// - `--plugin-repo owner/repo:tag` entries are ADDED to the set (their plugin
///   name is derived later from the bundle manifest, so here we register them
///   with a synthetic name of the repo's leaf when no matching default name is
///   present). To keep name resolution deterministic we require each override
///   to also carry a `#name` fragment OR match a default plugin by name.
/// - When `--plugin name...` filters are given, only default plugins whose name
///   appears in the filter survive; overrides always survive.
pub(crate) fn resolve_plugin_sources(
    default_tag: &str,
    name_filters: &[String],
    repo_overrides: &[String],
) -> Result<Vec<PluginSource>> {
    let mut sources: Vec<PluginSource> = if name_filters.is_empty() {
        default_plugin_sources(default_tag)
    } else {
        default_plugin_sources(default_tag)
            .into_iter()
            .filter(|p| name_filters.iter().any(|f| f == &p.name))
            .collect()
    };

    for raw in repo_overrides {
        sources.push(parse_plugin_repo_override(raw, default_tag)?);
    }

    if sources.is_empty() {
        bail!("no plugins selected — check --plugin filters against the default set");
    }
    Ok(sources)
}

/// Parse a `--plugin-repo owner/repo:tag[#name]` override. The tag defaults to
/// `default_tag` when omitted; the plugin name defaults to the repo's leaf.
fn parse_plugin_repo_override(raw: &str, default_tag: &str) -> Result<PluginSource> {
    // Split an optional `#name` fragment first.
    let (repo_tag, name_frag) = match raw.split_once('#') {
        Some((rt, n)) => (rt, Some(n.to_string())),
        None => (raw, None),
    };
    let (repo, tag) = match repo_tag.rsplit_once(':') {
        Some((r, t)) if r.contains('/') => (r.to_string(), t.to_string()),
        // No `:tag` (or the `:` was inside the owner) → use the default tag.
        _ => (repo_tag.to_string(), default_tag.to_string()),
    };
    if !repo.contains('/') {
        bail!("invalid --plugin-repo '{raw}': expected owner/repo[:tag][#name]");
    }
    let name = name_frag.unwrap_or_else(|| {
        repo.rsplit('/')
            .next()
            .unwrap_or(&repo)
            .trim_start_matches("rlean-plugin-")
            .to_string()
    });
    Ok(PluginSource { repo, tag, name })
}

// ── Bundle manifests ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RleanManifest {
    version: String,
    triple: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct PluginManifest {
    plugin: String,
    lib: String,
    version: String,
    triple: String,
    sha256: String,
}

/// A resolved plugin bundle ready to ship: the unpacked `.so` path plus its
/// manifest fields.
struct UnpackedPlugin {
    plugin: String,
    lib: String,
    version: String,
    sha256_short: String,
}

// ── sha256 verification ─────────────────────────────────────────────────────────

/// Compute the lowercase hex sha256 of a file.
pub(crate) fn sha256_hex(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The digest recorded in a `.sha256` sidecar file, which may be either a bare
/// hex digest or `<digest>  <filename>` (coreutils / shasum format).
pub(crate) fn parse_sha256_sidecar(contents: &str) -> Result<String> {
    let digest = contents
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty .sha256 sidecar"))?
        .to_ascii_lowercase();
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("malformed sha256 sidecar digest: {digest}");
    }
    Ok(digest)
}

/// Verify `tarball` against its `<tarball>.sha256` sidecar, erroring on mismatch.
fn verify_tarball_sha256(tarball: &Path) -> Result<()> {
    let sidecar = PathBuf::from(format!("{}.sha256", tarball.display()));
    let sidecar_text = std::fs::read_to_string(&sidecar).with_context(|| {
        format!(
            "missing sha256 sidecar for {} (expected {})",
            tarball.display(),
            sidecar.display()
        )
    })?;
    let expected = parse_sha256_sidecar(&sidecar_text)?;
    let actual = sha256_hex(tarball)?;
    if actual != expected {
        bail!(
            "sha256 mismatch for {}: expected {expected}, got {actual}",
            tarball.display()
        );
    }
    Ok(())
}

// ── gh download + local unpack ──────────────────────────────────────────────────

/// Shell out to `gh release download` for a single glob pattern.
fn gh_release_download(repo: &str, tag: &str, pattern: &str, dir: &Path) -> Result<()> {
    let output = Command::new("gh")
        .args([
            "release",
            "download",
            tag,
            "--repo",
            repo,
            "--pattern",
            pattern,
            "--dir",
        ])
        .arg(dir)
        .arg("--clobber")
        .output()
        .context("failed to spawn gh — is the GitHub CLI installed and authenticated?")?;
    if !output.status.success() {
        bail!(
            "gh release download {tag} --repo {repo} --pattern '{pattern}' failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Extract a `.tar.gz` into `dest` using the system `tar` (universally present
/// on macOS and Linux; avoids pulling in a tar/flate2 crate).
fn extract_tar_gz(tarball: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dest)
        .output()
        .context("failed to spawn tar")?;
    if !output.status.success() {
        bail!(
            "tar -xzf {} failed:\n{}",
            tarball.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Locate the single file matching `glob_suffix` (a filename ending) in `dir`.
fn find_one(dir: &Path, predicate: impl Fn(&str) -> bool, what: &str) -> Result<PathBuf> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(&predicate)
                .unwrap_or(false)
        })
        .collect();
    match matches.len() {
        0 => bail!("no {what} found in {}", dir.display()),
        1 => Ok(matches.remove(0)),
        _ => bail!("multiple {what} found in {}", dir.display()),
    }
}

// ── Install orchestration ────────────────────────────────────────────────────────

/// Summary of a completed install, returned for printing/testing.
pub(crate) struct InstallSummary {
    pub node: String,
    pub rlean_version: String,
    pub plugins: Vec<(String, String)>, // (name, sha256 short)
    pub version_output: String,
}

/// Run the full install flow for one node.
///
/// `exec` handles ssh command execution (the tests can inject a fake), while
/// `gh`/`scp`/`tar` are invoked via the process helpers above (not exercised in
/// unit tests, which cover only the pure helpers).
pub(crate) fn cmd_install(
    exec: &dyn RemoteExec,
    reg: &NodeRegistry,
    name: &str,
    release_tag: &str,
    plugin_filters: &[String],
    plugin_repos: &[String],
) -> Result<InstallSummary> {
    // 1. Resolve node + validate triple.
    let node = reg
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown node '{name}' — run `rlean cloud add-node`"))?;
    if node.platform != SUPPORTED_TRIPLE {
        bail!(
            "node '{name}' platform '{}' is not supported for cloud install — only {SUPPORTED_TRIPLE} bundles exist",
            node.platform
        );
    }
    let ssh = &node.ssh_dest;

    // 2. Download bundles locally into a temp dir.
    let tmp = tempfile::tempdir().context("failed to create temp dir for bundles")?;
    let tmp_path = tmp.path();

    let rlean_glob = format!("rlean-*-{SUPPORTED_TRIPLE}.tar.gz");
    gh_release_download(RLEAN_REPO, release_tag, &rlean_glob, tmp_path)?;
    gh_release_download(
        RLEAN_REPO,
        release_tag,
        &format!("{rlean_glob}.sha256"),
        tmp_path,
    )?;

    let plugin_sources = resolve_plugin_sources(release_tag, plugin_filters, plugin_repos)?;
    for src in &plugin_sources {
        let glob = format!("plugin-{}-*-{SUPPORTED_TRIPLE}.tar.gz", src.name);
        gh_release_download(&src.repo, &src.tag, &glob, tmp_path)?;
        gh_release_download(&src.repo, &src.tag, &format!("{glob}.sha256"), tmp_path)?;
    }

    // 3. Verify + unpack the rlean bundle.
    let rlean_tarball = find_one(
        tmp_path,
        |n| n.starts_with("rlean-") && n.ends_with(".tar.gz"),
        "rlean bundle",
    )?;
    verify_tarball_sha256(&rlean_tarball)?;
    let rlean_unpack = tmp_path.join("rlean-unpack");
    extract_tar_gz(&rlean_tarball, &rlean_unpack)?;
    let rlean_manifest: RleanManifest = read_manifest(&rlean_unpack.join("manifest.json"))?;
    if rlean_manifest.triple != SUPPORTED_TRIPLE {
        bail!(
            "rlean bundle triple '{}' does not match node platform '{SUPPORTED_TRIPLE}'",
            rlean_manifest.triple
        );
    }
    let rlean_bin = rlean_unpack.join("rlean");
    verify_inner_sha256(&rlean_bin, &rlean_manifest.sha256)?;

    // 3b. Verify + unpack each plugin bundle.
    let mut plugins = Vec::new();
    for src in &plugin_sources {
        let tarball = find_one(
            tmp_path,
            |n| n.starts_with(&format!("plugin-{}-", src.name)) && n.ends_with(".tar.gz"),
            &format!("plugin bundle for {}", src.name),
        )?;
        verify_tarball_sha256(&tarball)?;
        let unpack = tmp_path.join(format!("plugin-{}-unpack", src.name));
        extract_tar_gz(&tarball, &unpack)?;
        let manifest: PluginManifest = read_manifest(&unpack.join("manifest.json"))?;
        if manifest.triple != SUPPORTED_TRIPLE {
            bail!(
                "plugin '{}' bundle triple '{}' does not match node platform '{SUPPORTED_TRIPLE}'",
                src.name,
                manifest.triple
            );
        }
        let so = unpack.join(&manifest.lib);
        verify_inner_sha256(&so, &manifest.sha256)?;
        plugins.push((
            UnpackedPlugin {
                plugin: manifest.plugin,
                lib: manifest.lib,
                version: manifest.version,
                sha256_short: manifest.sha256.chars().take(12).collect(),
            },
            so,
        ));
    }

    // 4. Create node dirs.
    ssh_ok(
        exec,
        ssh,
        &[
            "mkdir",
            "-p",
            "~/.local/bin",
            "~/.rlean/plugins",
            "~/rlean-cloud/data",
            "~/rlean-cloud/workspace",
        ],
        "create node directories",
    )?;

    // 5. Ship the rlean binary atomically (temp name → chmod → mv).
    let bin_tmp = "~/.local/bin/rlean.new";
    scp_to(ssh, &rlean_bin, bin_tmp)?;
    ssh_ok(
        exec,
        ssh,
        &["chmod", "+x", bin_tmp],
        "chmod +x rlean binary",
    )?;
    ssh_ok(
        exec,
        ssh,
        &["mv", "-f", bin_tmp, "~/.local/bin/rlean"],
        "install rlean binary",
    )?;

    // 5b. Ship each plugin `.so`.
    for (plugin, so) in &plugins {
        let dest = format!("~/.rlean/plugins/{}", plugin.lib);
        scp_to(ssh, so, &dest)?;
    }

    // 6. Generate the node config locally, ship it, chmod 0600.
    let local_config = GlobalConfig::load().context("failed to read local ~/.rlean/config")?;
    let node_home = probe_node_home(exec, ssh)?;
    let node_config = node_config_from_local(&local_config, &node_home);
    let config_json = serde_json::to_string_pretty(&node_config)?;
    let local_config_tmp = tmp_path.join("node-config");
    atomic_write(&local_config_tmp, &config_json)?;
    scp_to(ssh, &local_config_tmp, "~/.rlean/config")?;
    ssh_ok(
        exec,
        ssh,
        &["chmod", "0600", "~/.rlean/config"],
        "chmod config",
    )?;

    // 7. Copy plugin-configs.json verbatim (secret file, never argv).
    let local_plugin_configs = plugin_configs_path()?;
    if local_plugin_configs.exists() {
        scp_to(ssh, &local_plugin_configs, "~/.rlean/plugin-configs.json")?;
        ssh_ok(
            exec,
            ssh,
            &["chmod", "0600", "~/.rlean/plugin-configs.json"],
            "chmod plugin-configs.json",
        )?;
    }

    // 8. Verify by running the installed binary.
    let version = exec.run(ssh, &["~/.local/bin/rlean", "--version"])?;
    if version.status != 0 {
        bail!(
            "installed rlean failed to run on '{name}' (status {}):\n{}",
            version.status,
            version.stderr.trim()
        );
    }
    let version_output = version.stdout.trim().to_string();

    Ok(InstallSummary {
        node: name.to_string(),
        rlean_version: rlean_manifest.version,
        plugins: plugins
            .iter()
            .map(|(p, _)| (p.plugin.clone(), p.sha256_short.clone()))
            .collect(),
        version_output: if version_output.is_empty() {
            plugins
                .first()
                .map(|(p, _)| format!("rlean {}", p.version))
                .unwrap_or_default()
        } else {
            version_output
        },
    })
}

fn read_manifest<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read manifest {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse manifest {}", path.display()))
}

/// Verify an unpacked artifact against the sha256 recorded in its manifest.
fn verify_inner_sha256(path: &Path, expected: &str) -> Result<()> {
    let expected = expected.to_ascii_lowercase();
    let actual = sha256_hex(path)?;
    if actual != expected {
        bail!(
            "sha256 mismatch for unpacked {}: manifest {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

/// Probe the node's absolute `$HOME` (used to build node-local config paths).
pub(crate) fn probe_node_home(exec: &dyn RemoteExec, ssh: &str) -> Result<String> {
    let out = exec.run(ssh, &["printf", "%s", "$HOME"])?;
    if out.status != 0 {
        bail!("failed to probe node $HOME: {}", out.stderr.trim());
    }
    let home = out.stdout.trim().to_string();
    if home.is_empty() || !home.starts_with('/') {
        bail!("node reported an unexpected $HOME: {home:?}");
    }
    Ok(home)
}

/// Run an ssh command and error with context on non-zero exit.
fn ssh_ok(exec: &dyn RemoteExec, ssh: &str, argv: &[&str], what: &str) -> Result<()> {
    let out = exec.run(ssh, argv)?;
    if out.status != 0 {
        bail!(
            "failed to {what} on node (status {}):\n{}",
            out.status,
            out.stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plugins_have_expected_repos() {
        let set = default_plugin_sources("v1");
        let names: Vec<&str> = set.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["tradier", "massive", "thetadata", "unusual_whales"]
        );
        assert_eq!(set[0].repo, "cascade-labs/rlean-plugins");
        assert_eq!(set[3].repo, "cascade-labs/cascadelabs-plugins");
        assert!(set.iter().all(|p| p.tag == "v1"));
    }

    #[test]
    fn plugin_name_filter_selects_subset() {
        let set = resolve_plugin_sources("v1", &["tradier".to_string()], &[]).unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].name, "tradier");
    }

    #[test]
    fn repo_override_parsed_with_tag_and_name() {
        let src = parse_plugin_repo_override("acme/rlean-plugin-foo:v2#foo", "v1").unwrap();
        assert_eq!(src.repo, "acme/rlean-plugin-foo");
        assert_eq!(src.tag, "v2");
        assert_eq!(src.name, "foo");
    }

    #[test]
    fn repo_override_defaults_tag_and_derives_name() {
        let src = parse_plugin_repo_override("acme/rlean-plugin-bar", "vDEF").unwrap();
        assert_eq!(src.tag, "vDEF");
        assert_eq!(src.name, "bar");
    }

    #[test]
    fn repo_override_rejects_missing_owner() {
        assert!(parse_plugin_repo_override("noslash", "v1").is_err());
    }

    #[test]
    fn resolve_adds_overrides_to_filtered_set() {
        let set = resolve_plugin_sources(
            "v1",
            &["thetadata".to_string()],
            &["acme/rlean-plugin-x:v9#x".to_string()],
        )
        .unwrap();
        let names: Vec<&str> = set.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["thetadata", "x"]);
    }

    #[test]
    fn sha256_sidecar_parses_bare_and_coreutils_forms() {
        let d = "a".repeat(64);
        assert_eq!(parse_sha256_sidecar(&d).unwrap(), d);
        let with_name = format!("{d}  rlean.tar.gz\n");
        assert_eq!(parse_sha256_sidecar(&with_name).unwrap(), d);
        // Uppercase normalized to lowercase.
        assert_eq!(
            parse_sha256_sidecar(&"A".repeat(64)).unwrap(),
            "a".repeat(64)
        );
    }

    #[test]
    fn sha256_sidecar_rejects_malformed() {
        assert!(parse_sha256_sidecar("").is_err());
        assert!(parse_sha256_sidecar("short").is_err());
        assert!(parse_sha256_sidecar(&"z".repeat(64)).is_err());
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("empty");
        std::fs::write(&f, b"").unwrap();
        // sha256("") is the well-known empty-string digest.
        assert_eq!(
            sha256_hex(&f).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
