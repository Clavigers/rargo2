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
//! # Whether what git ignores is synced. Default: true (a generated file or
//! # a local `.env` may be an input). With false, it is not scanned, watched
//! # or sent, except what `include` names (same patterns as `exclude`; a
//! # file inside an ignored directory is named by its path).
//! gitignored = false
//! include = ["data/schema.sql"]
//! # What build scripts see as `.git`, never file contents from git:
//! # "head": HEAD (commit, branch, dirty or not) and the tags on it;
//! # "history": also every commit before it and the tags on them, so that
//! # `git describe --tags` and `git rev-list --count` answer as on the
//! # client; false: no `.git` at all. Default (true): "history" where a
//! # build script mentions `describe` or `rev-list`, or Cargo.lock names
//! # a crate that asks git for history (vergen, shadow-rs, git-version,
//! # built, git-testament), else "head".
//! git = "history"
//! ```
//!
//! The manifest is read from the snapshot, so editing it takes effect on
//! the next command and the policy flows into action keys through the
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
    /// Whether what git ignores is synced (default: yes).
    pub gitignored: Option<bool>,
    /// What is synced although git ignores it, with `gitignored = false`.
    #[serde(default)]
    pub include: Vec<String>,
    /// What the sandbox gets as `.git`.
    pub git: Option<GitSync>,
}

/// `[sync] git`: `true` (decided by what the build scripts ask), `false`
/// (no `.git`), `"head"` or `"history"`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum GitSync {
    Flag(bool),
    Depth(GitDepth),
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GitDepth {
    Head,
    History,
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
    #[serde(default)]
    pub files: Vec<String>,
}

pub const MANIFEST: &str = "Cargo.toml";

/// What `[workspace.metadata.rargo]` holds: the policy's tables and the
/// server's name (which the client reads; D67).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct InManifest {
    #[allow(dead_code)]
    host: Option<String>,
    #[serde(default)]
    env: EnvPolicy,
    #[serde(default)]
    network: NetPolicy,
    #[serde(default)]
    flake: FlakePolicy,
    #[serde(default)]
    sync: SyncPolicy,
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// The policy of the workspace or package that `dir` of the snapshot is in.
pub fn snapshot_policy(snapshot: &crate::snapshot::Snapshot, dir: &std::path::Path) -> Result<Policy, String> {
    use std::os::unix::ffi::OsStrExt;
    let read = |path: &std::path::Path| snapshot.read(path.as_os_str().as_bytes()).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let manifest = rargo_proto::workspace::manifest_for(dir, read).and_then(|(path, _)| read(&path));
    Policy::load(manifest.as_deref())
}

impl Policy {
    /// The policy a manifest holds under `[workspace.metadata.rargo]`
    /// (`[package.metadata.rargo]` where it has no workspace table); the
    /// default policy where it holds none.
    pub fn load(manifest: Option<&str>) -> Result<Policy, String> {
        let Some(manifest) = manifest else { return Ok(Policy::default()) };
        // A manifest cargo itself rejects is cargo's to report.
        let Ok(table) = manifest.parse::<toml::Table>() else { return Ok(Policy::default()) };
        let found = ["workspace", "package"]
            .iter()
            .find_map(|kind| Some((*kind, table.get(*kind)?.get("metadata")?.get("rargo")?.clone())));
        let Some((kind, value)) = found else { return Ok(Policy::default()) };
        let m: InManifest = value.try_into().map_err(|e: toml::de::Error| format!("{MANIFEST}: [{kind}.metadata.rargo]: {}", e.message()))?;
        Ok(Policy { env: m.env, network: m.network, flake: m.flake, sync: m.sync })
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
        let p = Policy::load(Some(
            r#"
            [workspace]
            members = ["crates/*"]
            [workspace.metadata.rargo]
            host = "rargo.rs"
            [workspace.metadata.rargo.env]
            pass = ["DATABASE_URL", "PROTOC*"]
            [workspace.metadata.rargo.network]
            test-packages = ["it"]
            build-scripts = ["fetcher-sys"]
            "#,
        ))
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
    }

    #[test]
    fn the_policy_is_the_manifests_table() {
        // A manifest that only names the server holds the default policy.
        let only_host = "[package]\nname = \"a\"\n[package.metadata.rargo]\nhost = \"rargo.rs\"\n";
        assert_eq!(Policy::load(Some(only_host)).unwrap(), Policy::default());
        // A package outside a workspace; other tools' metadata is not ours.
        let package = "[package]\nname = \"a\"\n[package.metadata.docs.rs]\nall-features = true\n[package.metadata.rargo.sync]\ngit = false\n";
        assert_eq!(Policy::load(Some(package)).unwrap().sync.git, Some(GitSync::Flag(false)));
        let history = "[workspace.metadata.rargo.sync]\ngit = \"history\"\n";
        assert_eq!(Policy::load(Some(history)).unwrap().sync.git, Some(GitSync::Depth(GitDepth::History)));
        let head = "[workspace.metadata.rargo.sync]\ngit = \"head\"\n";
        assert_eq!(Policy::load(Some(head)).unwrap().sync.git, Some(GitSync::Depth(GitDepth::Head)));
        assert!(Policy::load(Some("[workspace.metadata.rargo.sync]\ngit = \"all\"\n")).is_err());
        // A workspace table comes before a package table of the same file.
        let both = "[workspace.metadata.rargo.sync]\nexclude = [\"a\"]\n[package]\nname = \"p\"\n[package.metadata.rargo.sync]\nexclude = [\"b\"]\n";
        assert_eq!(Policy::load(Some(both)).unwrap().sync.exclude, ["a"]);
        // What git ignores, left out but for one path.
        let ignored = "[workspace.metadata.rargo.sync]\ngitignored = false\ninclude = [\"data/schema.sql\"]\n";
        let sync = Policy::load(Some(ignored)).unwrap().sync;
        assert_eq!((sync.gitignored, sync.include), (Some(false), vec!["data/schema.sql".to_string()]));
        // No manifest, or one without the table.
        assert_eq!(Policy::load(None).unwrap(), Policy::default());
        assert_eq!(Policy::load(Some("[package]\nname = \"a\"\n")).unwrap(), Policy::default());
    }

    #[test]
    fn unknown_keys_and_wrong_types_are_errors_that_name_the_table() {
        let e = Policy::load(Some("[workspace.metadata.rargo.env]\npas = []\n")).unwrap_err();
        assert!(e.contains("Cargo.toml") && e.contains("workspace.metadata.rargo"), "{e}");
        assert!(Policy::load(Some("[workspace.metadata.rargo.network]\ntests = \"yes\"\n")).is_err());
        assert!(Policy::load(Some("[package.metadata.rargo]\nbogus = 1\n")).is_err());
    }
}
