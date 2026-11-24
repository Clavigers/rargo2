//! Flake-restricted environments.
//!
//! A workspace flake or the bundled flake defines the build environment: the variables it exports become the action
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
use crate::build::Daemon;
use crate::snapshot::Snapshot;
use crate::store::Store;
use rargo_proto::tree::Node;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
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

const CA_AT: &str = "/etc/ca-certificates.crt";

fn host_ca_bundle() -> Option<Vec<u8>> {
    ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/cert.pem"]
        .iter()
        .find_map(|p| std::fs::read(p).ok())
}

fn nix() -> Command {
    let mut c = Command::new("nix");
    c.args(["--extra-experimental-features", "nix-command", "--extra-experimental-features", "flakes"]);
    c
}

/// Whether a path (relative to the worktree root) is one of the files that
/// define the flake: `flake.nix`, `flake.lock`, every other `.nix` file at
/// the top level or under `nix/`, and the toolchain file a devShell
/// commonly reads.
fn defines_flake(rel: &[u8], extra: &[String]) -> bool {
    let top = !rel.contains(&b'/');
    let nix = rel.ends_with(b".nix");
    (top && (nix || rel == b"flake.lock" || rel == b"rust-toolchain" || rel == b"rust-toolchain.toml"))
        || (rel.starts_with(b"nix/") && nix)
        || extra.iter().any(|e| under(rel, e.as_bytes()))
}

/// `rel` is `dir` or inside it.
fn under(rel: &[u8], dir: &[u8]) -> bool {
    let dir = dir.strip_suffix(b"/").unwrap_or(dir);
    rel == dir || (rel.starts_with(dir) && rel.get(dir.len()) == Some(&b'/'))
}

/// Whether the walk has to enter directory `rel` to find defining files.
fn may_hold(rel: &[u8], extra: &[String]) -> bool {
    rel == b"nix" || rel.starts_with(b"nix/") || extra.iter().any(|e| under(rel, e.as_bytes()) || under(e.as_bytes(), rel))
}

const DEFAULT_FLAKE: &[u8] = include_bytes!("../../../nix/default/flake.nix");
const DEFAULT_LOCK: &[u8] = include_bytes!("../../../nix/default/flake.lock");

fn workspace_entry(root: &[u8], rel: &[u8], node: &Node, extra: &[String]) -> bool {
    if let Node::Dir(_) = node {
        if !root.is_empty() && under(root, rel) {
            return true;
        }
    }
    let inside = if root.is_empty() {
        Some(rel)
    } else {
        rel.strip_prefix(root).and_then(|suffix| suffix.strip_prefix(b"/"))
    };
    inside.is_some_and(|p| match node {
        Node::Dir(_) => may_hold(p, extra),
        _ => defines_flake(p, extra),
    })
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
fn evaluate(root: &Path, shell: &str, uid: u32, gid: u32) -> Result<FlakeEnv> {
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
    c.args(["print-dev-env", "--json", "--no-write-lock-file", shell]);
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
    // The server's certificate bundle is shown at a fixed place in the
    // sandbox (see `etc`); a dev shell entered by hand sees the machine's.
    if host_ca_bundle().is_some() {
        for var in ["SSL_CERT_FILE", "NIX_SSL_CERT_FILE", "CURL_CA_BUNDLE", "GIT_SSL_CAINFO"] {
            env.entry(var.to_string()).or_insert_with(|| CA_AT.to_string());
        }
    }
    if roots.is_empty() || !env.contains_key("PATH") {
        bail!("the flake's devShell exports no tools (no store paths, or no PATH)");
    }
    let mut c = Command::new("nix-store");
    c.arg("-qR");
    // A value can mention a path that is not there (a placeholder in a
    // prefix map) or run a store path into what follows it
    // (`-fmacro-prefix-map=/nix/store/..-dev=/nix/store/..`): `=` and `?`
    // are legal in names.
    let roots: BTreeSet<String> = roots
        .into_iter()
        .filter_map(|r| {
            let mut name = r.as_str();
            loop {
                if Path::new("/nix/store").join(name).exists() {
                    return Some(name.to_string());
                }
                name = name.strip_suffix(['=', '?', '.', '+'])?;
            }
        })
        .collect();
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

/// Resolve the workspace flake or the bundled shell for a workspace
/// without one. The action key includes both the evaluated environment
/// and the source bytes that select it.
pub fn resolve(d: &Daemon, snap: &Snapshot, dir: &Path, extra: &[String], uid: u32, gid: u32) -> Result<Arc<FlakeEnv>> {
    static NIX_READY: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    NIX_READY.get_or_init(|| match nix().arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
        Err(e) => Err(e.to_string()),
    }).as_ref().map_err(|e| anyhow::anyhow!("rargod needs Nix with flakes to build: {e}"))?;

    let manifest = rargo_proto::workspace::manifest_for(dir, |path| {
        snap.read(path.as_os_str().as_bytes()).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    });
    let workspace = manifest.as_ref().and_then(|(path, _)| path.parent()).unwrap_or(dir);
    let root = workspace.ancestors().find(|p| {
        matches!(snap.node(p.join("flake.nix").as_os_str().as_bytes()), Some(Node::File { .. }))
    });
    let toolchain = if root.is_none() {
        dir.ancestors().find_map(|parent| {
            ["rust-toolchain.toml", "rust-toolchain"].into_iter().find_map(|name| {
                snap.read(parent.join(name).as_os_str().as_bytes()).map(|data| (name, data))
            })
        })
    } else {
        None
    };
    let mut h = KeyHasher::new("flake-source-v3");
    if let Some(root) = root {
        let root = root.as_os_str().as_bytes();
        h.field(b"workspace");
        snap.walk(&mut |rel, node| {
            if !workspace_entry(root, rel, node, extra) {
                return false;
            }
            match node {
                Node::File { hash, .. } => { h.field(rel).field(b"file").field(hash); }
                Node::Symlink(target) => { h.field(rel).field(b"symlink").field(target); }
                Node::Dir(_) => { h.field(rel).field(b"directory"); }
            }
            true
        });
    } else {
        h.field(b"bundled").field(DEFAULT_FLAKE).field(DEFAULT_LOCK);
        if let Some((name, src)) = &toolchain {
            h.field(name.as_bytes()).field(src);
        }
    }
    let key = h.finish();
    let store = &d.engine.store;
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
    let scratch = d.scratch("flake")?;
    let env = (|| {
        if let Some(root) = root {
            snap.materialize(&scratch, uid, gid, &|rel, node| {
                workspace_entry(root.as_os_str().as_bytes(), rel, node, extra)
            })?;
            evaluate(&scratch.join(root), ".#default", uid, gid)
        } else {
            crate::snapshot::chown(&scratch, uid, gid);
            std::fs::write(scratch.join("flake.nix"), DEFAULT_FLAKE)?;
            std::fs::write(scratch.join("flake.lock"), DEFAULT_LOCK)?;
            if let Some((name, src)) = &toolchain {
                std::fs::write(scratch.join(name), src)?;
            }
            evaluate(&scratch, ".#default", uid, gid)
        }
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    let mut env = env?;
    env.id = KeyHasher::new("flake-action-v1").field(&key.0).field(&env.id.0).finish();
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
        // Names resolve for the actions the policy gives the host's
        // network; the others have nothing to reach a resolver with.
        file("nsswitch.conf", "passwd: files\ngroup: files\nhosts: files dns\n".into())?;
        if let Ok(resolv) = std::fs::read_to_string("/etc/resolv.conf") {
            file("resolv.conf", resolv)?;
        }
        if let Some(bundle) = host_ca_bundle() {
            let hash = store.put_bytes(&bundle)?;
            m.insert(b"ca-certificates.crt".to_vec(), SynthEnt::File { hash, size: bundle.len() as u64 });
        }
        Ok(Arc::new(m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_that_define_a_flake() {
        let none: Vec<String> = Vec::new();
        for f in ["flake.nix", "flake.lock", "shell.nix", "rust-toolchain.toml", "nix/a/b.nix"] {
            assert!(defines_flake(f.as_bytes(), &none), "{f}");
        }
        for f in ["Cargo.lock", "src/flake.nix", "nix/patch.diff", "script/cargo"] {
            assert!(!defines_flake(f.as_bytes(), &none), "{f}");
        }
        let extra = vec!["Cargo.lock".to_string(), "script/cargo".to_string(), "nix/".to_string(), "crates/zed".to_string()];
        for f in ["Cargo.lock", "script/cargo", "nix/patch.diff", "crates/zed/Cargo.toml"] {
            assert!(defines_flake(f.as_bytes(), &extra), "{f}");
        }
        for f in ["script/cargo2", "script/other", "crates/zed2/Cargo.toml", "Cargo.lock.bak"] {
            assert!(!defines_flake(f.as_bytes(), &extra), "{f}");
        }
        // Directories on the way to a named file are entered, others not.
        assert!(may_hold(b"script", &extra) && may_hold(b"crates", &extra) && may_hold(b"crates/zed/src", &extra));
        assert!(!may_hold(b"crates/gpui", &extra) && !may_hold(b"docs", &extra));
    }

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
