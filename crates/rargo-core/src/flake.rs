//! Flake-restricted environments.
//!
//! When a repository has a `flake.nix`, its default devShell defines the
//! build environment: the variables it exports become the action
//! environment, and the sandbox root contains only the closure of store
//! paths those variables refer to (plus the worktree, cargo home and a
//! minimal `/etc`). Nothing of the host's `/usr` or `/home` is visible, so
//! an action's key and observations mention only paths that are the same on
//! every machine with the same flake.
//!
//! Evaluating the flake needs Nix, the network on first use, and seconds of
//! time; the result is remembered under a hash of the flake's files.

use crate::buildview::SynthEnt;
use crate::db::Table;
use crate::hash::{Hash, KeyHasher};
use crate::store::Store;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlakeEnv {
    /// Exported variables of the devShell, minus per-invocation noise.
    pub env: BTreeMap<String, String>,
    /// Store path names (`<hash>-<name>`, without `/nix/store/`).
    pub closure: Vec<String>,
    /// Identifies the environment; part of every action key.
    pub id: Hash,
}

/// Variables `nix print-dev-env` reports that describe the Nix build that
/// produced the shell rather than the shell's tools.
const DROP: &[&str] = &[
    "HOME", "TMP", "TMPDIR", "TEMP", "TEMPDIR", "PWD", "OLDPWD", "SHLVL", "TERM", "NIX_BUILD_TOP",
    "NIX_BUILD_CORES", "NIX_LOG_FD", "NIX_STORE", "NIX_ENFORCE_PURITY", "SSL_CERT_FILE", "NIX_SSL_CERT_FILE", "out",
    "outputs", "name", "builder", "system", "shell", "SHELL", "shellHook", "phases", "dontAddDisableDepTrack",
    "buildPhase", "preferLocalBuild", "stdenv", "__structuredAttrs", "OPTERR", "OSTYPE", "HOSTTYPE", "MACHTYPE",
    "LINENO", "IFS", "PS4", "SOURCE_DATE_EPOCH_PLACEHOLDER", "patches", "doCheck", "doInstallCheck", "strictDeps",
    "mesonFlags", "cmakeFlags", "configureFlags", "depsBuildBuild", "depsBuildBuildPropagated", "depsBuildTarget",
    "depsBuildTargetPropagated", "depsHostHost", "depsHostHostPropagated", "depsTargetTarget",
    "depsTargetTargetPropagated", "buildInputs", "nativeBuildInputs", "propagatedBuildInputs",
    "propagatedNativeBuildInputs",
];

fn nix() -> Command {
    let mut c = Command::new("nix");
    c.args(["--extra-experimental-features", "nix-command", "--extra-experimental-features", "flakes"]);
    c
}

/// Hash of the files that define the flake: `flake.nix`, `flake.lock` and
/// every other `.nix` file at the top level or under `nix/`.
pub fn source_key(root: &Path) -> Result<Hash> {
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for e in std::fs::read_dir(root)?.flatten() {
        let p = e.path();
        if p.is_file() && (p.extension().is_some_and(|x| x == "nix") || p.file_name().is_some_and(|n| n == "flake.lock")) {
            files.push(p);
        }
    }
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "nix") {
                out.push(p);
            }
        }
    }
    walk(&root.join("nix"), &mut files);
    files.sort();
    let mut h = KeyHasher::new("flake-source-v1");
    for f in files {
        h.field(f.strip_prefix(root).unwrap_or(&f).as_os_str().as_encoded_bytes());
        h.field(&std::fs::read(&f)?);
    }
    Ok(h.finish())
}

fn store_names(text: &str, out: &mut BTreeSet<String>) {
    const P: &str = "/nix/store/";
    let mut rest = text;
    while let Some(i) = rest.find(P) {
        let after = &rest[i + P.len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '?' | '=')))
            .unwrap_or(after.len());
        // A store path name is a 32-character hash, a dash and a name.
        if end > 33 && after.as_bytes()[32] == b'-' {
            out.insert(after[..end].to_string());
        }
        rest = &after[end..];
    }
}

/// Evaluates the flake's devShell (as `uid`/`gid`, in `root`).
fn evaluate(root: &Path, uid: u32, gid: u32) -> Result<FlakeEnv> {
    use std::os::unix::process::CommandExt;
    let run = |mut c: Command| -> Result<Vec<u8>> {
        c.current_dir(root).stdin(std::process::Stdio::null());
        if uid != unsafe { libc::geteuid() } {
            c.uid(uid).gid(gid);
        }
        let out = c.output().context("running nix")?;
        if !out.status.success() {
            bail!("nix failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    };
    let mut c = nix();
    c.args(["print-dev-env", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&run(c)?).context("parsing `nix print-dev-env`")?;
    let mut env = BTreeMap::new();
    let mut roots = BTreeSet::new();
    if let Some(vars) = json.get("variables").and_then(|v| v.as_object()) {
        for (name, v) in vars {
            if v.get("type").and_then(|t| t.as_str()) != Some("exported") {
                continue;
            }
            let Some(value) = v.get("value").and_then(|x| x.as_str()) else { continue };
            store_names(value, &mut roots);
            if DROP.contains(&name.as_str()) || name.starts_with("__") || name.starts_with("BASH") {
                continue;
            }
            env.insert(name.clone(), value.to_string());
        }
    }
    if roots.is_empty() || !env.contains_key("PATH") {
        bail!("the flake's devShell exports no tools (no store paths, or no PATH)");
    }
    let mut c = Command::new("nix-store");
    c.arg("-qR");
    for r in &roots {
        c.arg(format!("/nix/store/{r}"));
    }
    let mut closure = BTreeSet::new();
    store_names(&String::from_utf8_lossy(&run(c)?), &mut closure);
    let closure: Vec<String> = closure.into_iter().collect();
    let mut h = KeyHasher::new("flake-env-v1");
    for c in &closure {
        h.field(c.as_bytes());
    }
    for (k, v) in &env {
        h.field(k.as_bytes()).field(v.as_bytes());
    }
    Ok(FlakeEnv { env, closure, id: h.finish() })
}

/// The flake environment of the worktree at `root`, evaluated once per
/// distinct set of flake files.
pub fn resolve(store: &Store, root: &Path, uid: u32, gid: u32) -> Result<Arc<FlakeEnv>> {
    let key = source_key(root)?;
    let mut k = b"flake:".to_vec();
    k.extend_from_slice(&key.0);
    if let Some(v) = store.db().get(Table::Meta, &k) {
        if let Ok(env) = bincode::deserialize::<FlakeEnv>(&v) {
            // The closure may have been garbage-collected since.
            if env.closure.iter().all(|c| Path::new("/nix/store").join(c).exists()) {
                return Ok(Arc::new(env));
            }
        }
    }
    let env = evaluate(root, uid, gid)?;
    store.db().put(Table::Meta, &k, &bincode::serialize(&env)?);
    Ok(Arc::new(env))
}

impl FlakeEnv {
    pub fn closure_set(&self) -> Arc<HashSet<Vec<u8>>> {
        Arc::new(self.closure.iter().map(|c| c.as_bytes().to_vec()).collect())
    }

    /// First match for `program` in the environment's PATH.
    pub fn which(&self, program: &str) -> Option<String> {
        self.env.get("PATH")?.split(':').map(|d| format!("{d}/{program}")).find(|p| Path::new(p).is_file())
    }

    /// A minimal `/etc`: enough for programs that look up the current user
    /// or `localhost`.
    pub fn etc(&self, store: &Store, uid: u32, gid: u32) -> Result<Arc<HashMap<Vec<u8>, SynthEnt>>> {
        let mut m = HashMap::new();
        let mut file = |name: &str, content: String| -> Result<()> {
            let hash = store.put_bytes(content.as_bytes())?;
            m.insert(name.as_bytes().to_vec(), SynthEnt::File { hash, size: content.len() as u64 });
            Ok(())
        };
        let mut passwd = String::from("root:x:0:0:root:/root:/bin/sh\n");
        if uid != 0 {
            passwd.push_str(&format!("user:x:{uid}:{gid}:user:/tmp:/bin/sh\n"));
        }
        file("passwd", passwd)?;
        let mut group = String::from("root:x:0:\n");
        if gid != 0 {
            group.push_str(&format!("user:x:{gid}:\n"));
        }
        file("group", group)?;
        file("hosts", "127.0.0.1 localhost\n::1 localhost\n".into())?;
        file("nsswitch.conf", "passwd: files\ngroup: files\nhosts: files\n".into())?;
        Ok(Arc::new(m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_store_path_names() {
        let mut s = BTreeSet::new();
        store_names(
            "/nix/store/a2kr44ny9hl19lwp1331mg7902yhv06r-rustc-1.97.1/bin:/nix/store/0592hadxldzhhd3ppc6nskslzd39fwlz-gcc-wrapper-15.3.0/bin:/usr/bin /nix/store/short",
            &mut s,
        );
        assert_eq!(
            s.into_iter().collect::<Vec<_>>(),
            ["0592hadxldzhhd3ppc6nskslzd39fwlz-gcc-wrapper-15.3.0", "a2kr44ny9hl19lwp1331mg7902yhv06r-rustc-1.97.1"]
        );
    }
}
