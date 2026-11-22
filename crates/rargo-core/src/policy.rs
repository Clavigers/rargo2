//! Per-repository policy: the escape hatch for inputs that cannot be traced.
//!
//! Environment variables and the network are handled by rule, not by
//! observation (DESIGN, "Sandbox and tracing"). The rules live in
//! `.rargo.toml` at the worktree root, deliberately outside the cargo files:
//! cargo ignores the file, so a repository that has one still builds with
//! plain cargo.
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
}

pub const POLICY_FILE: &str = ".rargo.toml";

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

impl Policy {
    pub fn parse(text: &str) -> Result<Policy, String> {
        toml::from_str(text).map_err(|e| format!("{POLICY_FILE}: {}", e.message()))
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
    fn empty_and_invalid() {
        assert_eq!(Policy::parse("").unwrap(), Policy::default());
        assert!(Policy::parse("[env]\npas = []").is_err(), "unknown keys are errors, not silently ignored");
        assert!(Policy::parse("[network]\ntests = \"yes\"").is_err());
    }
}
