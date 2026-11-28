use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub env: EnvPolicy,
    #[serde(default)]
    pub network: NetPolicy,
    #[serde(default)]
    pub flake: FlakePolicy,
    #[serde(default)]
    pub sync: SyncPolicy,
    pub nextest: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SyncPolicy {
    #[serde(default)]
    pub exclude: Vec<String>,
    pub gitignored: Option<bool>,
    #[serde(default)]
    pub include: Vec<String>,
    pub git: Option<GitSync>,
}

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
    #[serde(default, rename = "git-build-scripts-offline")]
    pub git_build_scripts_offline: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FlakePolicy {
    #[serde(default)]
    pub files: Vec<String>,
}

pub const MANIFEST: &str = "Cargo.toml";

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct InManifest {
    #[serde(default, rename = "host")]
    _host: Option<toml::Value>,
    #[serde(default)]
    env: EnvPolicy,
    #[serde(default)]
    network: NetPolicy,
    #[serde(default)]
    flake: FlakePolicy,
    #[serde(default)]
    sync: SyncPolicy,
    nextest: Option<String>,
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

pub fn snapshot_policy(snapshot: &crate::snapshot::Snapshot, dir: &std::path::Path) -> Result<Policy, String> {
    use std::os::unix::ffi::OsStrExt;
    let read = |path: &std::path::Path| snapshot.read(path.as_os_str().as_bytes()).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let manifest = rargo_proto::workspace::manifest_for(dir, read).and_then(|(path, _)| read(&path));
    Policy::load(manifest.as_deref())
}

pub fn from_git(manifest: &str) -> bool {
    let home = String::from_utf8_lossy(crate::buildview::CARGO_HOME_AT);
    manifest.strip_prefix(home.as_ref()).is_some_and(|rest| rest.starts_with("/git/checkouts/"))
}

impl Policy {
    pub fn load(manifest: Option<&str>) -> Result<Policy, String> {
        let Some(manifest) = manifest else { return Ok(Policy::default()) };
        let Ok(table) = manifest.parse::<toml::Table>() else { return Ok(Policy::default()) };
        let found = ["workspace", "package"]
            .iter()
            .find_map(|kind| Some((*kind, table.get(*kind)?.get("metadata")?.get("rargo")?.clone())));
        let Some((kind, value)) = found else { return Ok(Policy::default()) };
        let m: InManifest = value.try_into().map_err(|e: toml::de::Error| format!("{MANIFEST}: [{kind}.metadata.rargo]: {}", e.message()))?;
        Ok(Policy { env: m.env, network: m.network, flake: m.flake, sync: m.sync, nextest: m.nextest })
    }

    pub fn passes(&self, name: &str) -> bool {
        self.env.pass.iter().any(|p| matches(p, name))
    }

    pub fn passed_env<'a>(&self, client: impl IntoIterator<Item = (&'a String, &'a String)>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = client
            .into_iter()
            .filter(|(k, _)| self.env.pass.iter().any(|p| matches(p, k)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort();
        out
    }

    pub fn test_network(&self, package: &str) -> bool {
        self.network.tests || self.network.test_packages.iter().any(|p| p == package)
    }

    pub fn build_script_network(&self, package: &str, manifest: &str) -> bool {
        self.network.build_scripts.iter().any(|p| p == package) || (from_git(manifest) && !self.network.git_build_scripts_offline)
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
            nextest = "0.9.100"
            [workspace.metadata.rargo.env]
            pass = ["DATABASE_URL", "PROTOC*"]
            [workspace.metadata.rargo.network]
            test-packages = ["it"]
            build-scripts = ["fetcher-sys"]
            "#,
        ))
        .unwrap();
        let client: std::collections::BTreeMap<String, String> = [
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
        assert!(p.build_script_network("fetcher-sys", "/w/crates/fetcher-sys/Cargo.toml"));
        assert!(!p.build_script_network("util", "/w/crates/util/Cargo.toml"));
    }

    #[test]
    fn build_scripts_of_git_dependencies_have_network_unless_the_workspace_says_otherwise() {
        let git = "/c/git/checkouts/livekit-rust-sdks-6a2c/147fbca/webrtc-sys/Cargo.toml";
        let registry = "/c/registry/src/index.crates.io-1949cf8c6b5b557f/openssl-sys-0.9.104/Cargo.toml";
        let p = Policy::default();
        assert!(p.build_script_network("webrtc-sys", git));
        assert!(!p.build_script_network("openssl-sys", registry), "a registry crate builds offline, as on docs.rs");
        assert!(!p.build_script_network("app", "/w/Cargo.toml"));
        assert!(!p.build_script_network("x", "/w/c/git/checkouts/x/Cargo.toml"), "a workspace that happens to have such a directory");
        let offline = Policy::load(Some("[workspace.metadata.rargo.network]\ngit-build-scripts-offline = true\n")).unwrap();
        assert!(!offline.build_script_network("webrtc-sys", git));
    }

    #[test]
    fn the_policy_is_the_manifests_table() {
        let empty = "[package]\nname = \"a\"\n[package.metadata.rargo]\n";
        assert_eq!(Policy::load(Some(empty)).unwrap(), Policy::default());
        let host = "[package]\nname = \"a\"\n[package.metadata.rargo]\nhost = \"rargo.rs\"\n";
        assert_eq!(Policy::load(Some(host)).unwrap(), Policy::default());
        let package = "[package]\nname = \"a\"\n[package.metadata.docs.rs]\nall-features = true\n[package.metadata.rargo.sync]\ngit = false\n";
        assert_eq!(Policy::load(Some(package)).unwrap().sync.git, Some(GitSync::Flag(false)));
        let history = "[workspace.metadata.rargo.sync]\ngit = \"history\"\n";
        assert_eq!(Policy::load(Some(history)).unwrap().sync.git, Some(GitSync::Depth(GitDepth::History)));
        let head = "[workspace.metadata.rargo.sync]\ngit = \"head\"\n";
        assert_eq!(Policy::load(Some(head)).unwrap().sync.git, Some(GitSync::Depth(GitDepth::Head)));
        assert!(Policy::load(Some("[workspace.metadata.rargo.sync]\ngit = \"all\"\n")).is_err());
        let both = "[workspace.metadata.rargo.sync]\nexclude = [\"a\"]\n[package]\nname = \"p\"\n[package.metadata.rargo.sync]\nexclude = [\"b\"]\n";
        assert_eq!(Policy::load(Some(both)).unwrap().sync.exclude, ["a"]);
        let ignored = "[workspace.metadata.rargo.sync]\ngitignored = false\ninclude = [\"data/schema.sql\"]\n";
        let sync = Policy::load(Some(ignored)).unwrap().sync;
        assert_eq!((sync.gitignored, sync.include), (Some(false), vec!["data/schema.sql".to_string()]));
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
