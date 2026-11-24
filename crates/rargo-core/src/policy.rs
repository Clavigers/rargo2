//! Per-repository policy: the escape hatch for inputs that cannot be traced.
//!
//! Environment variables and the network are handled by rule, not by
//! observation (DESIGN, "Sandbox and tracing"). The rules live in the
//! active workspace or package manifest, under
//! `[workspace.metadata.rargo]` (`[package.metadata.rargo]` where there is no
//! workspace table): cargo ignores `metadata`, so the repository builds with
//! plain cargo as before, and it needs no file of rargo's own. The tables
//! are the ones below, each prefixed with `workspace.metadata.rargo.`.
//!
//! The older place, `.rargo.toml` at the worktree root with the same tables
//! unprefixed, is still read. A table the manifest has wins over the same
//! table in the file, and the build says so.
//!
//! ```toml
//! [env]
//! # Always given to every action (and therefore part of every key).
//! pass = ["DATABASE_URL", "PROTOC*"]
//!
//! [network]
//! # Give host networking to all test runs, or only some packages' tests.
//! tests = false
//! test-packages = ["integration-tests"]
//! # Build scripts of these packages may use the network.
//! build-scripts = ["some-sys"]
//!
//! [flake]
//! # Use the repository's flake devShell as the environment (default: true
//! # when flake.nix exists).
//! enabled = true
//! # Files and directories the flake reads besides `flake.nix`,
//! # `flake.lock`, `rust-toolchain(.toml)` and `.nix` files at the top and
//! # under `nix/`. The environment is evaluated again when one changes.
//! files = ["Cargo.lock", "script/cargo", "nix"]
//!
//! [sync]
//! # Never sent to the build server, in addition to `.git` and cargo target
//! # directories. A name matches at any depth; a path with a slash is taken
//! # from the worktree root.
//! exclude = ["node_modules", "data/fixtures-large"]
//! # Show build scripts a `.git` that answers questions about HEAD (commit,
//! # branch, dirty or not) without history or file contents. Default: true.
//! git = true
//! ```
//!
//! The file is read from the snapshot, so editing it takes effect on the
//! next command and its content flows into action keys through the
//! environment and network settings it produces.

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub env: EnvPolicy,
    #[serde(default)]
    pub network: NetPolicy,
    #[serde(default)]
    pub flake: FlakePolicy,
    /// Read by the client, which decides what is synced; accepted here so
    /// that the file means the same thing at both ends.
    #[serde(default)]
    pub sync: SyncPolicy,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SyncPolicy {
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Whether the sandbox gets a thin `.git` describing HEAD.
    pub git: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvPolicy {
    #[serde(default)]
    pub pass: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetPolicy {
    #[serde(default)]
    pub tests: bool,
    #[serde(default, rename = "test-packages")]
    pub test_packages: Vec<String>,
    #[serde(default, rename = "build-scripts")]
    pub build_scripts: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FlakePolicy {
    pub enabled: Option<bool>,
    #[serde(default)]
    pub files: Vec<String>,
}

pub const POLICY_FILE: &str = ".rargo.toml";
pub const MANIFEST: &str = "Cargo.toml";

/// What `[workspace.metadata.rargo]` may hold: the policy's tables and the
/// server's name (which the client reads; D67).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct InManifest {
    #[allow(dead_code)]
    host: Option<String>,
    env: Option<EnvPolicy>,
    network: Option<NetPolicy>,
    flake: Option<FlakePolicy>,
    sync: Option<SyncPolicy>,
}

/// A repository's policy and what should be said about how it was found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loaded {
    pub policy: Policy,
    pub notices: Vec<String>,
    /// Said once per command unless the client asks for quiet about it:
    /// the older file is still what the policy comes from.
    pub deprecations: Vec<String>,
}

/// The release in which `.rargo.toml` and `~/.config/rargo/config.toml`
/// stop being read.
pub const OLD_FILES_UNTIL: &str = "0.5.0";

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

pub fn snapshot_policy(snapshot: &crate::snapshot::Snapshot, dir: &std::path::Path) -> Result<Loaded, String> {
    use std::os::unix::ffi::OsStrExt;
    let read = |path: &std::path::Path| snapshot.read(path.as_os_str().as_bytes()).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let manifest = rargo_proto::workspace::manifest_for(dir, read).and_then(|(path, _)| read(&path));
    Policy::load(manifest.as_deref(), read(std::path::Path::new(POLICY_FILE)).as_deref())
}

impl Policy {
    pub fn parse(text: &str) -> Result<Policy, String> {
        toml::from_str(text).map_err(|e| format!("{POLICY_FILE}: {}", e.message()))
    }

    /// Parse policy tables from the selected Cargo manifest and the worktree policy file.
    pub fn load(manifest: Option<&str>, file: Option<&str>) -> Result<Loaded, String> {
        let mut out = Loaded { policy: match file {
            Some(text) => Policy::parse(text)?,
            None => Policy::default(),
        }, notices: Vec::new(), deprecations: Vec::new() };
        if file.is_some() {
            out.deprecations.push(format!(
                "{POLICY_FILE} is still read, until rargo {OLD_FILES_UNTIL}: its tables belong in {MANIFEST} under [workspace.metadata.rargo] (`rargo --rargo-migrate` moves them)"
            ));
        }
        let Some(manifest) = manifest else { return Ok(out) };
        // A manifest cargo itself would reject is cargo's to report.
        let Ok(table) = manifest.parse::<toml::Table>() else { return Ok(out) };
        let found = ["workspace", "package"]
            .iter()
            .find_map(|kind| Some((*kind, table.get(*kind)?.get("metadata")?.get("rargo")?.clone())));
        let Some((kind, value)) = found else { return Ok(out) };
        let m: InManifest = value.try_into().map_err(|e: toml::de::Error| format!("{MANIFEST}: [{kind}.metadata.rargo]: {}", e.message()))?;
        let in_file = file.and_then(|t| t.parse::<toml::Table>().ok()).unwrap_or_default();
        let mut shadowed: Vec<&str> = Vec::new();
        let mut take = |name: &'static str, has: bool| {
            if has && in_file.contains_key(name) {
                shadowed.push(name);
            }
            has
        };
        if take("env", m.env.is_some()) {
            out.policy.env = m.env.unwrap_or_default();
        }
        if take("network", m.network.is_some()) {
            out.policy.network = m.network.unwrap_or_default();
        }
        if take("flake", m.flake.is_some()) {
            out.policy.flake = m.flake.unwrap_or_default();
        }
        if take("sync", m.sync.is_some()) {
            out.policy.sync = m.sync.unwrap_or_default();
        }
        if !shadowed.is_empty() {
            out.notices.push(format!(
                "{POLICY_FILE}: {} ignored: {MANIFEST} has {} under [{kind}.metadata.rargo]",
                shadowed.iter().map(|t| format!("[{t}]")).collect::<Vec<_>>().join(", ") + if shadowed.len() == 1 { " is" } else { " are" },
                if shadowed.len() == 1 { "that table" } else { "those tables" },
            ));
        }
        Ok(out)
    }

    /// Client variables the policy passes to every action.
    pub fn passed_env(&self, client: &HashMap<String, String>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = client
            .iter()
            .filter(|(k, _)| self.env.pass.iter().any(|p| matches(p, k)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort();
        out
    }

    pub fn test_network(&self, package: &str) -> bool {
        self.network.tests || self.network.test_packages.iter().any(|p| p == package)
    }

    pub fn build_script_network(&self, package: &str) -> bool {
        self.network.build_scripts.iter().any(|p| p == package)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_applies() {
        let p = Policy::parse(
            r#"
            [env]
            pass = ["DATABASE_URL", "PROTOC*"]
            [network]
            test-packages = ["it"]
            build-scripts = ["fetcher-sys"]
            "#,
        )
        .unwrap();
        let client: HashMap<String, String> = [
            ("DATABASE_URL", "postgres://x"),
            ("PROTOC", "/usr/bin/protoc"),
            ("PROTOC_INCLUDE", "/usr/include"),
            ("SECRET", "no"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let passed: Vec<String> = p.passed_env(&client).into_iter().map(|(k, _)| k).collect();
        assert_eq!(passed, ["DATABASE_URL", "PROTOC", "PROTOC_INCLUDE"]);
        assert!(p.test_network("it") && !p.test_network("other"));
        assert!(p.build_script_network("fetcher-sys") && !p.build_script_network("util"));
        assert_eq!(p.flake.enabled, None);
    }

    #[test]
    fn the_manifest_holds_the_policy_and_wins_over_the_file_table_by_table() {
        let manifest = r#"
            [workspace]
            members = ["crates/*"]
            [workspace.metadata.rargo]
            host = "rargo.rs"
            [workspace.metadata.rargo.env]
            pass = ["DATABASE_URL"]
            [workspace.metadata.rargo.network]
            build-scripts = ["fetcher-sys"]
        "#;
        // The manifest alone.
        let l = Policy::load(Some(manifest), None).unwrap();
        assert_eq!(l.policy.env.pass, ["DATABASE_URL"]);
        assert!(l.policy.build_script_network("fetcher-sys"));
        assert!(l.notices.is_empty());
        // With a file: its [env] is ignored and said to be; its [sync], which
        // the manifest does not have, still counts.
        let file = "[env]\npass = [\"OLD\"]\n[sync]\nexclude = [\"node_modules\"]\n";
        let l = Policy::load(Some(manifest), Some(file)).unwrap();
        assert_eq!(l.policy.env.pass, ["DATABASE_URL"]);
        assert_eq!(l.policy.sync.exclude, ["node_modules"]);
        assert_eq!(l.notices.len(), 1);
        assert_eq!(l.deprecations.len(), 1, "the file is still read, and that is said");
        assert!(Policy::load(Some(manifest), None).unwrap().deprecations.is_empty());
        assert!(l.notices[0].contains("[env] is ignored") && l.notices[0].contains("[workspace.metadata.rargo]"), "{}", l.notices[0]);
        // A manifest that only names the server changes nothing.
        let only_host = "[package]\nname = \"a\"\n[package.metadata.rargo]\nhost = \"rargo.rs\"\n";
        let l = Policy::load(Some(only_host), Some(file)).unwrap();
        assert_eq!(l.policy.env.pass, ["OLD"]);
        assert!(l.notices.is_empty());
        // A package outside a workspace; other tools' metadata is not ours.
        let package = "[package]\nname = \"a\"\n[package.metadata.docs.rs]\nall-features = true\n[package.metadata.rargo.sync]\ngit = false\n";
        assert_eq!(Policy::load(Some(package), None).unwrap().policy.sync.git, Some(false));
        // Unknown keys are errors there too, and name the place.
        let bad = "[workspace.metadata.rargo.env]\npas = []\n";
        let e = Policy::load(Some(bad), None).unwrap_err();
        assert!(e.contains("Cargo.toml") && e.contains("workspace.metadata.rargo"), "{e}");
        // No manifest, no file.
        assert_eq!(Policy::load(None, None).unwrap(), Loaded::default());
    }

    #[test]
    fn empty_and_invalid() {
        assert_eq!(Policy::parse("").unwrap(), Policy::default());
        assert!(Policy::parse("[env]\npas = []").is_err(), "unknown keys are errors, not silently ignored");
        assert!(Policy::parse("[network]\ntests = \"yes\"").is_err());
    }
}
