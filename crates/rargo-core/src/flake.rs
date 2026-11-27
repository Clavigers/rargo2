#[cfg(test)]
use rargo_machine::StateMachine;
use crate::io::file::PathRing as _;
use crate::buildview::SynthEnt;
use crate::db::Table;
use crate::hash::{Hash, KeyHasher};
use crate::build::Daemon;
use crate::snapshot::Snapshot;
use crate::store::Store;
use rargo_proto::tree::Node;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use crate::io::process::Command;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlakeEnv {
    pub env: BTreeMap<String, String>,
    pub closure: Vec<String>,
    pub roots: Vec<Hash>,
    pub id: Hash,
}

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
        .find_map(|p| crate::io::file::read(p).ok())
}

fn nix() -> Command {
    let mut c = Command::new("nix");
    c.args(["--extra-experimental-features", "nix-command", "--extra-experimental-features", "flakes"]);
    c
}

fn defines_flake(rel: &[u8], extra: &[String]) -> bool {
    let top = !rel.contains(&b'/');
    let name = rel.rsplit(|b| *b == b'/').next().unwrap_or(rel);
    (top && (rel.ends_with(b".nix") || rel == b"rust-toolchain" || rel == b"rust-toolchain.toml"))
        || name == b"flake.nix"
        || name == b"flake.lock"
        || rel.starts_with(b"nix/")
        || extra.iter().any(|e| under(rel, e.as_bytes()))
}

fn under(rel: &[u8], dir: &[u8]) -> bool {
    let dir = dir.strip_suffix(b"/").unwrap_or(dir);
    rel == dir || (rel.starts_with(dir) && rel.get(dir.len()) == Some(&b'/'))
}

fn may_hold(rel: &[u8], extra: &[String]) -> bool {
    rel == b"nix" || rel.starts_with(b"nix/") || extra.iter().any(|e| under(rel, e.as_bytes()) || under(e.as_bytes(), rel))
}

const DEFAULT_FLAKE: &[u8] = include_bytes!("../../../nix/default/flake.nix");
const DEFAULT_LOCK: &[u8] = include_bytes!("../../../nix/default/flake.lock");

fn inside<'a>(root: &[u8], rel: &'a [u8]) -> Option<&'a [u8]> {
    if root.is_empty() {
        Some(rel)
    } else {
        rel.strip_prefix(root).and_then(|suffix| suffix.strip_prefix(b"/"))
    }
}

fn above(root: &[u8], rel: &[u8], node: &Node) -> bool {
    matches!(node, Node::Dir(_)) && !root.is_empty() && under(root, rel)
}

fn workspace_entry(root: &[u8], rel: &[u8], node: &Node, extra: &[String]) -> bool {
    above(root, rel, node)
        || inside(root, rel).is_some_and(|p| match node {
            Node::Dir(_) => may_hold(p, extra),
            _ => defines_flake(p, extra),
        })
}

fn evaluated_entry(root: &[u8], rel: &[u8], node: &Node) -> bool {
    above(root, rel, node) || inside(root, rel).is_some_and(|p| !under(p, b"target") && !under(p, b".git"))
}

fn store_names(text: &str, out: &mut BTreeSet<String>) {
    const P: &str = "/nix/store/";
    let mut rest = text;
    while let Some(i) = rest.find(P) {
        let after = &rest[i + P.len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '?' | '=')))
            .unwrap_or(after.len());
        if end > 33 && after.as_bytes()[32] == b'-' {
            out.insert(after[..end].to_string());
        }
        rest = &after[end..];
    }
}

/// The directory for temporary files: `TMPDIR`, or `/tmp` without it.
fn temp_dir() -> std::path::PathBuf {
    crate::io::env::var_os("TMPDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "/tmp".into())
}

fn run_nix(root: &Path, mut c: Command, uid: u32, gid: u32) -> std::result::Result<Vec<u8>, String> {
    use std::os::unix::process::CommandExt;
    c.current_dir(root).stdin(crate::io::process::Stdio::null());
    if uid != crate::io::process::euid() {
        c.std_mut().uid(uid).gid(gid);
    }
    let out = c.output().map_err(|e| format!("running nix: {e}"))?;
    if !out.status.success() {
        return Err(format!("nix failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

fn candidates(names: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for r in names {
        let mut name = r.as_str();
        loop {
            out.insert(name.to_string());
            match name.strip_suffix(['=', '?', '.', '+']) {
                Some(shorter) => name = shorter,
                None => break,
            }
        }
    }
    out
}

fn store_roots(names: &BTreeSet<String>, present: &BTreeSet<String>) -> BTreeSet<String> {
    names
        .iter()
        .filter_map(|r| {
            let mut name = r.as_str();
            loop {
                if present.contains(name) {
                    return Some(name.to_string());
                }
                name = name.strip_suffix(['=', '?', '.', '+'])?;
            }
        })
        .collect()
}

pub trait Worktree {
    fn node(&self, rel: &[u8]) -> Option<Node>;
    fn read(&self, rel: &[u8]) -> Option<Vec<u8>>;
    fn walk(&self, f: &mut dyn FnMut(&[u8], &Node) -> bool);
}

impl Worktree for Snapshot {
    fn node(&self, rel: &[u8]) -> Option<Node> {
        Snapshot::node(self, rel)
    }

    fn read(&self, rel: &[u8]) -> Option<Vec<u8>> {
        Snapshot::read(self, rel)
    }

    fn walk(&self, f: &mut dyn FnMut(&[u8], &Node) -> bool) {
        Snapshot::walk(self, f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub root: Option<std::path::PathBuf>,
    pub toolchain: Option<(&'static str, Vec<u8>)>,
    pub key: Hash,
}

pub fn source(tree: &dyn Worktree, dir: &Path, extra: &[String]) -> Source {
    let manifest = rargo_proto::workspace::manifest_for(dir, |path| {
        tree.read(path.as_os_str().as_bytes()).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    });
    let workspace = manifest.as_ref().and_then(|(path, _)| path.parent()).unwrap_or(dir);
    let root = workspace
        .ancestors()
        .find(|p| matches!(tree.node(p.join("flake.nix").as_os_str().as_bytes()), Some(Node::File { .. })))
        .map(Path::to_path_buf);
    let toolchain = if root.is_none() {
        dir.ancestors().find_map(|parent| {
            ["rust-toolchain.toml", "rust-toolchain"]
                .into_iter()
                .find_map(|name| tree.read(parent.join(name).as_os_str().as_bytes()).map(|data| (name, data)))
        })
    } else {
        None
    };
    let mut h = KeyHasher::new("flake-source-v3");
    if let Some(root) = &root {
        let root = root.as_os_str().as_bytes();
        h.field(b"workspace");
        tree.walk(&mut |rel, node| {
            if !evaluated_entry(root, rel, node) {
                return false;
            }
            if !workspace_entry(root, rel, node, extra) {
                return matches!(node, Node::Dir(_));
            }
            match node {
                Node::File { hash, .. } => {
                    h.field(rel).field(b"file").field(hash);
                }
                Node::Symlink(target) => {
                    h.field(rel).field(b"symlink").field(target);
                }
                Node::Dir(_) => {
                    h.field(rel).field(b"directory");
                }
            }
            true
        });
    } else {
        h.field(b"bundled").field(DEFAULT_FLAKE).field(DEFAULT_LOCK);
        if let Some((name, src)) = &toolchain {
            h.field(name.as_bytes()).field(src);
        }
    }
    Source { root, toolchain, key: h.finish() }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Need {
    Exists(Vec<String>),
    Evaluate,
    Closure(Vec<String>),
    Manifest(Vec<String>),
    Done { env: FlakeEnv, save: bool },
    Failed(String),
}

impl Need {
    /// Whether this step runs Nix to evaluate or build the toolchain, which the client is told it waits for.
    pub fn prepares_toolchain(&self) -> bool {
        matches!(self, Need::Evaluate | Need::Closure(_))
    }
}

#[derive(Clone, Debug)]
enum Stage {
    Cached(FlakeEnv),
    Evaluating,
    Rooting { env: BTreeMap<String, String>, names: BTreeSet<String> },
    Closing { env: BTreeMap<String, String>, roots: BTreeSet<String> },
    Manifesting { env: FlakeEnv, was: Option<Hash> },
    Done { env: FlakeEnv, save: bool },
    Failed(String),
}

pub struct Resolution {
    key: Hash,
    ca: bool,
    stage: Stage,
}

impl Resolution {
    pub fn new(key: Hash, cached: Option<FlakeEnv>, ca_available: bool) -> Resolution {
        let stage = match cached {
            Some(env) => Stage::Cached(env),
            None => Stage::Evaluating,
        };
        Resolution { key, ca: ca_available, stage }
    }

    pub fn need(&self) -> Need {
        match &self.stage {
            Stage::Cached(env) => Need::Exists(env.closure.clone()),
            Stage::Evaluating => Need::Evaluate,
            Stage::Rooting { names, .. } => Need::Exists(candidates(names).into_iter().collect()),
            Stage::Closing { roots, .. } => Need::Closure(roots.iter().cloned().collect()),
            Stage::Manifesting { env, .. } => Need::Manifest(env.closure.clone()),
            Stage::Done { env, save } => Need::Done { env: env.clone(), save: *save },
            Stage::Failed(e) => Need::Failed(e.clone()),
        }
    }

    pub fn exists(&mut self, present: &BTreeSet<String>) {
        self.stage = match std::mem::replace(&mut self.stage, Stage::Evaluating) {
            Stage::Cached(env) if env.closure.iter().all(|c| present.contains(c)) => {
                let was = Some(env.id);
                Stage::Manifesting { env, was }
            }
            Stage::Cached(_) => Stage::Evaluating,
            Stage::Rooting { env, names } => Stage::Closing { env, roots: store_roots(&names, present) },
            other => other,
        };
    }

    pub fn evaluated(&mut self, output: std::result::Result<Vec<u8>, String>) {
        if !matches!(self.stage, Stage::Evaluating) {
            return;
        }
        self.stage = match output.and_then(|out| parse_dev_env(&out, self.ca).map_err(|e| format!("{e:#}"))) {
            Ok((env, names)) => Stage::Rooting { env, names },
            Err(e) => Stage::Failed(e),
        };
    }

    pub fn closed(&mut self, output: std::result::Result<Vec<u8>, String>) {
        let Stage::Closing { env, .. } = &self.stage else { return };
        self.stage = match output {
            Ok(out) => Stage::Manifesting { env: flake_env(env.clone(), &out), was: None },
            Err(e) => Stage::Failed(e),
        };
    }

    pub fn manifested(&mut self, roots: Vec<Hash>) {
        let Stage::Manifesting { env, was } = &self.stage else { return };
        let mut env = env.clone();
        env.roots = roots;
        env.id = action_id(&self.key, &env);
        let save = *was != Some(env.id);
        self.stage = Stage::Done { env, save };
    }
}

#[cfg(test)]
impl StateMachine for Resolution {
    fn invariants(&self) {
        let sorted = |c: &[String]| assert!(c.windows(2).all(|w| w[0] < w[1]), "the closure is sorted and has no repeats");
        match &self.stage {
            Stage::Cached(env) | Stage::Manifesting { env, .. } => sorted(&env.closure),
            Stage::Evaluating => {}
            Stage::Rooting { env, names } => {
                assert!(!names.is_empty(), "rooting has names to root");
                assert!(env.contains_key("PATH"), "an evaluated environment has a PATH");
            }
            Stage::Closing { env, roots } => {
                assert!(env.contains_key("PATH"), "an evaluated environment has a PATH");
                assert!(roots.iter().all(|r| r.as_bytes().get(32) == Some(&b'-')), "every root is a store path name");
            }
            Stage::Done { env, .. } => {
                sorted(&env.closure);
                assert!(env.id == action_id(&self.key, env), "a done environment's id is the action id of its key and contents");
                assert_eq!(env.roots.len(), env.closure.len(), "every closure path has a root");
            }
            Stage::Failed(e) => assert!(!e.is_empty(), "a failure says why"),
        }
    }
}

type DevEnv = (BTreeMap<String, String>, BTreeSet<String>);

fn parse_dev_env(output: &[u8], ca_available: bool) -> Result<DevEnv> {
    let json: serde_json::Value = serde_json::from_slice(output).context("parsing `nix print-dev-env`")?;
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
    if ca_available {
        for var in ["SSL_CERT_FILE", "NIX_SSL_CERT_FILE", "CURL_CA_BUNDLE", "GIT_SSL_CAINFO"] {
            env.entry(var.to_string()).or_insert_with(|| CA_AT.to_string());
        }
    }
    if roots.is_empty() || !env.contains_key("PATH") {
        bail!("the flake's devShell exports no tools (no store paths, or no PATH)");
    }
    Ok((env, roots))
}

fn flake_env(env: BTreeMap<String, String>, closure_output: &[u8]) -> FlakeEnv {
    let mut closure = BTreeSet::new();
    store_names(&String::from_utf8_lossy(closure_output), &mut closure);
    let closure: Vec<String> = closure.into_iter().collect();
    let mut h = KeyHasher::new("flake-env-v1");
    for c in &closure {
        h.field(c.as_bytes());
    }
    for (k, v) in &env {
        h.field(k.as_bytes()).field(v.as_bytes());
    }
    FlakeEnv { env, closure, roots: Vec::new(), id: h.finish() }
}

fn action_id(source: &Hash, env: &FlakeEnv) -> Hash {
    let mut h = KeyHasher::new("flake-action-v2");
    h.field(&source.0);
    for name in &env.closure {
        h.field(name.as_bytes());
    }
    for (k, v) in &env.env {
        h.field(k.as_bytes()).field(v.as_bytes());
    }
    h.finish()
}

pub fn resolve(d: &Daemon, snap: &Snapshot, dir: &Path, extra: &[String], uid: u32, gid: u32, preparing: &dyn Fn()) -> Result<Arc<FlakeEnv>> {
    resolve_shell(d, snap, dir, extra, (uid, gid), "default", preparing)
}

/// Resolve a development shell and keep its store closure separate from the other shells.
pub(crate) fn resolve_shell(
    d: &Daemon,
    snap: &Snapshot,
    dir: &Path,
    extra: &[String],
    (uid, gid): (u32, u32),
    shell: &str,
    preparing: &dyn Fn(),
) -> Result<Arc<FlakeEnv>> {
    crate::io::nix::ready(|| match nix().arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
        Err(e) => Err(e.to_string()),
    })
    .map_err(|e| anyhow::anyhow!("rargod needs Nix with flakes to build: {e}"))?;

    let src = source(snap, dir, extra);
    let shell_key = if shell == "default" {
        src.key
    } else {
        let mut hash = KeyHasher::new("flake-shell-v1");
        hash.field(&src.key.0).field(shell.as_bytes());
        hash.finish()
    };
    let store = &d.engine.store;
    let mut k = b"flake:".to_vec();
    k.extend_from_slice(&shell_key.0);
    let nix_store = Path::new("/nix/store");
    let hosts = d.pool.get().map(|p| p.host_sources().clone());
    let tables: Arc<dyn crate::db::Tables> = store.db().clone();
    let disk: Arc<dyn crate::nixmanifest::StoreFs> = Arc::new(crate::nixmanifest::Disk);
    let db = store.db().clone();
    let cached = crate::io::cpu(|| db.get(Table::Meta, &k).and_then(|v| bincode::deserialize::<FlakeEnv>(&v).ok()));
    let mut resolution = Resolution::new(shell_key, cached, host_ca_bundle().is_some());
    let mut scratch: Option<std::path::PathBuf> = None;
    let mut said = false;
    let done = loop {
        let need = resolution.need();
        if need.prepares_toolchain() && !said {
            said = true;
            preparing();
        }
        match need {
            Need::Exists(names) => {
                let present = crate::io::cpu(|| names.into_iter().filter(|n| nix_store.join(n).ring_exists()).collect());
                resolution.exists(&present);
            }
            Need::Evaluate => {
                let at = d.scratch("flake")?;
                scratch = Some(at.clone());
                let out = (|| -> Result<Vec<u8>> {
                    let dir = (|| -> Result<std::path::PathBuf> { Ok(match &src.root {
                        Some(root) => {
                            snap.materialize(&at, uid, gid, &|rel, node| evaluated_entry(root.as_os_str().as_bytes(), rel, node))?;
                            at.join(root)
                        }
                        None => {
                            crate::snapshot::chown(&at, uid, gid);
                            crate::io::file::write(at.join("flake.nix"), DEFAULT_FLAKE)?;
                            crate::io::file::write(at.join("flake.lock"), DEFAULT_LOCK)?;
                            if let Some((name, data)) = &src.toolchain {
                                crate::io::file::write(at.join(name), data)?;
                            }
                            at.clone()
                        }
                    }) })()?;
                    let mut c = nix();
                    c.args(["print-dev-env", "--json", "--no-write-lock-file"]).arg(format!(".#{shell}"));
                    run_nix(&dir, c, uid, gid).map_err(anyhow::Error::msg)
                })();
                resolution.evaluated(out.map_err(|e| format!("{e:#}")));
            }
            Need::Closure(roots) => {
                let mut c = Command::new("nix-store");
                c.arg("-qR");
                for r in &roots {
                    c.arg(nix_store.join(r));
                }
                let at = scratch.clone().unwrap_or_else(temp_dir);
                resolution.closed(run_nix(&at, c, uid, gid));
            }
            Need::Manifest(closure) => {
                let attached = &d.nix_attached;
                let prepared = crate::nixmanifest::schedule(d.nix_hashing(), &tables, &disk, nix_store, &closure, hosts.as_ref(), attached);
                let prepared = match prepared {
                    Ok(p) => p,
                    Err(e) => break Err(e),
                };
                crate::nixmanifest::log_cost(&prepared.cost);
                resolution.manifested(prepared.roots);
            }
            Need::Done { env, save } => break Ok((env, save)),
            Need::Failed(e) => break Err(anyhow::anyhow!(e)),
        }
    };
    if let Some(at) = scratch {
        crate::io::cpu(move || {
            let _ = crate::io::file::remove_dir_all(at);
        });
    }
    let (env, save) = done?;
    if save {
        let value = bincode::serialize(&env)?;
        crate::io::cpu(|| db.put(Table::Meta, &k, &value));
    }
    Ok(Arc::new(env))
}

impl FlakeEnv {
    pub fn closure_set(&self) -> Arc<BTreeSet<Vec<u8>>> {
        Arc::new(self.closure.iter().map(|c| c.as_bytes().to_vec()).collect())
    }

    pub fn which(&self, program: &str) -> Option<String> {
        self.env.get("PATH")?.split(':').map(|d| format!("{d}/{program}")).find(|p| Path::new(p).ring_is_file())
    }

    pub fn etc(&self, store: &Store, uid: u32, gid: u32) -> Result<Arc<BTreeMap<Vec<u8>, SynthEnt>>> {
        let mut m = BTreeMap::new();
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
        file("nsswitch.conf", "passwd: files\ngroup: files\nhosts: files dns\n".into())?;
        if let Ok(resolv) = crate::io::file::read_to_string("/etc/resolv.conf") {
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
    use crate::test_support::Transcript;
    use expect_test::expect;

    #[test]
    fn the_default_toolchain_has_the_standard_library_s_sources_whatever_the_toolchain_file_asks() {
        let flake = std::str::from_utf8(DEFAULT_FLAKE).unwrap();
        let rust = flake.lines().find(|l| l.trim_start().starts_with("rust = ")).expect("the default flake defines its toolchain as `rust`");
        assert!(rust.contains("fromRustupToolchain (toolchain //"), "every toolchain file, and none, goes through one expression: {rust}");
        let components = flake.lines().find(|l| l.trim_start().starts_with("components = ")).expect("the toolchain names its components");
        assert!(components.contains("toolchain.components or []"), "the toolchain file's own components are kept: {components}");
        for wanted in ["\"rust-src\"", "\"clippy\"", "\"rustfmt\""] {
            assert!(components.contains(wanted), "{wanted} is added to every default toolchain: {components}");
        }
    }

    #[test]
    fn files_that_define_a_flake() {
        let none: Vec<String> = Vec::new();
        for f in ["flake.nix", "flake.lock", "shell.nix", "rust-toolchain.toml", "nix/a/b.nix", "nix/dev/flake.lock", "nix/patch.diff", "tools/sub/flake.nix", "tools/sub/flake.lock"] {
            assert!(defines_flake(f.as_bytes(), &none), "{f}");
        }
        for f in ["Cargo.lock", "src/default.nix", "script/cargo", "src/flake.nix.bak"] {
            assert!(!defines_flake(f.as_bytes(), &none), "{f}");
        }
        let extra = vec!["Cargo.lock".to_string(), "script/cargo".to_string(), "nix/".to_string(), "crates/zed".to_string()];
        for f in ["Cargo.lock", "script/cargo", "nix/patch.diff", "crates/zed/Cargo.toml"] {
            assert!(defines_flake(f.as_bytes(), &extra), "{f}");
        }
        for f in ["script/cargo2", "script/other", "crates/zed2/Cargo.toml", "Cargo.lock.bak"] {
            assert!(!defines_flake(f.as_bytes(), &extra), "{f}");
        }
        assert!(may_hold(b"script", &extra) && may_hold(b"crates", &extra) && may_hold(b"crates/zed/src", &extra));
        assert!(!may_hold(b"crates/gpui", &extra) && !may_hold(b"docs", &extra));
    }

    const DEV_ENV: &[u8] = include_bytes!("flake/print-dev-env.json");
    const CLOSURE: &[u8] = include_bytes!("flake/closure.txt");

    fn lines(out: &[u8]) -> BTreeSet<String> {
        String::from_utf8_lossy(out).lines().map(|l| l.trim_start_matches("/nix/store/").to_string()).collect()
    }

    fn drive(r: &mut Resolution, present: &BTreeSet<String>, dev_env: &[u8], closure: &[u8]) -> Need {
        for _ in 0..8 {
            r.invariants();
            match r.need() {
                Need::Exists(names) => r.exists(&names.into_iter().filter(|n| present.contains(n)).collect()),
                Need::Evaluate => r.evaluated(Ok(dev_env.to_vec())),
                Need::Closure(_) => r.closed(Ok(closure.to_vec())),
                Need::Manifest(c) => r.manifested(c.iter().map(|n| Hash::of(n.as_bytes())).collect()),
                done => return done,
            }
        }
        panic!("the resolution did not finish");
    }

    #[test]
    fn the_real_dev_environment_resolves_to_its_closure() {
        let mut outcomes = Transcript::default();
        let key = Hash::of(b"source");
        let closure = lines(CLOSURE);
        let mut r = Resolution::new(key, None, true);
        outcomes.record("need", r.need());
        r.evaluated(Ok(DEV_ENV.to_vec()));
        r.invariants();
        let Need::Exists(asked) = r.need() else { panic!("the roots are checked against the store") };
        assert!(asked.iter().all(|n| n.as_bytes().get(32) == Some(&b'-')));
        r.exists(&closure);
        let Need::Closure(roots) = r.need() else { panic!("the closure of the roots is asked for") };
        outcomes.record("closure root count", roots.len());
        assert!(roots.iter().all(|root| closure.contains(root)));
        assert!(roots.iter().any(|root| root.contains("-rust")), "{roots:?}");
        r.closed(Ok(CLOSURE.to_vec()));
        let Need::Manifest(manifest) = r.need() else { panic!("the closure is manifested") };
        outcomes.record("manifest path count", manifest.len());
        assert_eq!(manifest, closure.iter().cloned().collect::<Vec<_>>());
        r.manifested(manifest.iter().map(|n| Hash::of(n.as_bytes())).collect());
        r.invariants();
        let Need::Done { env, save: true } = r.need() else { panic!("a fresh evaluation is saved") };
        assert!(env.env["PATH"].split(':').all(|p| p.starts_with("/nix/store/")));
        outcomes.record("SSL_CERT_FILE", &env.env["SSL_CERT_FILE"]);
        assert!(!env.env.keys().any(|k| k.starts_with("BASH") || k.starts_with("__") || (DROP.contains(&k.as_str()) && env.env[k] != CA_AT)));
        assert!(!env.env.contains_key("HOME") && !env.env.contains_key("NIX_BUILD_TOP") && !env.env.contains_key("shellHook"));
        outcomes.record("environment id", env.id);
        let without_ca = drive(&mut Resolution::new(key, None, false), &closure, DEV_ENV, CLOSURE);
        let Need::Done { env: plain, .. } = without_ca else { panic!("{without_ca:?}") };
        assert!(!plain.env.contains_key("SSL_CERT_FILE") || plain.env["SSL_CERT_FILE"] != CA_AT);
        assert_ne!(plain.id, env.id);
        let mut reversed: Vec<&[u8]> = CLOSURE.split(|b| *b == b'\n').collect();
        reversed.reverse();
        let reversed = reversed.join(&b'\n');
        let Need::Done { env: again, .. } = drive(&mut Resolution::new(key, None, true), &closure, DEV_ENV, &reversed) else { panic!() };
        assert_eq!(again, env, "the closure order does not matter");

        let mut cached = Resolution::new(key, Some(env.clone()), true);
        assert_eq!(cached.need(), Need::Exists(env.closure.clone()));
        assert_eq!(drive(&mut cached, &closure, b"", b""), Need::Done { env: env.clone(), save: false });
        let mut stale = env.clone();
        stale.id = Hash::ZERO;
        assert_eq!(drive(&mut Resolution::new(key, Some(stale), true), &closure, b"", b""), Need::Done { env: env.clone(), save: true });
        let mut gone = closure.clone();
        gone.remove(&env.closure[7]);
        let mut cached = Resolution::new(key, Some(env.clone()), true);
        cached.exists(&gone);
        // a cached closure missing a path is evaluated again.
        outcomes.record("need", cached.need());
        expect![[r#"
            need: Evaluate
            closure root count: 39
            manifest path count: 163
            SSL_CERT_FILE: "/etc/ca-certificates.crt"
            environment id: 0ba5d1d686b3f387
            need: Evaluate
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn only_a_flake_that_must_be_evaluated_or_built_prepares_the_toolchain() {
        let mut outcomes = Transcript::default();
        let key = Hash::of(b"source");
        let closure = lines(CLOSURE);
        let prepares = |r: &mut Resolution, present: &BTreeSet<String>| {
            let mut steps = Vec::new();
            for _ in 0..8 {
                let need = r.need();
                steps.push(need.prepares_toolchain());
                match need {
                    Need::Exists(names) => r.exists(&names.into_iter().filter(|n| present.contains(n)).collect()),
                    Need::Evaluate => r.evaluated(Ok(DEV_ENV.to_vec())),
                    Need::Closure(_) => r.closed(Ok(CLOSURE.to_vec())),
                    Need::Manifest(c) => r.manifested(c.iter().map(|n| Hash::of(n.as_bytes())).collect()),
                    Need::Done { env, .. } => return (steps, Some(env)),
                    Need::Failed(_) => return (steps, None),
                }
            }
            panic!("the resolution did not finish");
        };
        let (fresh, env) = prepares(&mut Resolution::new(key, None, true), &closure);
        outcomes.record("fresh", fresh);
        let env = env.expect("resolves");
        let (cached, _) = prepares(&mut Resolution::new(key, Some(env.clone()), true), &closure);
        outcomes.record("cached", cached);
        let mut gone = closure.clone();
        gone.remove(&env.closure[7]);
        let (missing, _) = prepares(&mut Resolution::new(key, Some(env), true), &gone);
        outcomes.record("stale", missing);
        expect![[r#"
            fresh: [true, false, true, false, false]
            cached: [false, false, false]
            stale: [false, true, false, true, false, false]
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn failures_and_answers_out_of_turn() {
        let mut outcomes = Transcript::default();
        let mut r = Resolution::new(Hash::ZERO, None, true);
        r.closed(Ok(CLOSURE.to_vec()));
        r.manifested(Vec::new());
        r.exists(&BTreeSet::new());
        // answers to what was not asked change nothing.
        outcomes.record("need", r.need());
        r.evaluated(Err("nix failed: boom".into()));
        outcomes.record("need", r.need());
        let mut r = Resolution::new(Hash::ZERO, None, true);
        r.evaluated(Ok(br#"{"variables":{"PATH":{"type":"exported","value":"/usr/bin"}}}"#.to_vec()));
        outcomes.record("no tools", r.need());
        let mut r = Resolution::new(Hash::ZERO, None, true);
        r.evaluated(Ok(b"not json".to_vec()));
        outcomes.record("malformed environment", r.need());
        let mut r = Resolution::new(Hash::ZERO, None, true);
        r.evaluated(Ok(DEV_ENV.to_vec()));
        r.evaluated(Err("late".into()));
        r.exists(&lines(CLOSURE));
        r.closed(Err("nix failed: no store".into()));
        r.invariants();
        outcomes.record("need", r.need());
        expect![[r#"
            need: Evaluate
            need: Failed("nix failed: boom")
            no tools: Failed("the flake's devShell exports no tools (no store paths, or no PATH)")
            malformed environment: Failed("parsing `nix print-dev-env`: expected ident at line 1 column 2")
            need: Failed("nix failed: no store")
        "#]].assert_eq(&outcomes.0);
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    #[test]
    fn generated_environments_resolve_each_root_to_its_longest_present_name() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut names = BTreeSet::new();
            let mut store = BTreeSet::new();
            for i in 0..1 + rng.below(8) {
                let base = format!("{:032}-pkg{i}", rng.below(1 << 40));
                let junk: String = (0..rng.below(4)).map(|_| ['=', '?', '.', '+'][rng.below(4) as usize]).collect();
                let name = format!("{base}{junk}");
                let mut cut = name.as_str();
                loop {
                    if rng.below(3) == 0 {
                        store.insert(cut.to_string());
                    }
                    match cut.strip_suffix(['=', '?', '.', '+']) {
                        Some(shorter) => cut = shorter,
                        None => break,
                    }
                }
                names.insert(name);
            }
            let path: Vec<String> = names.iter().map(|n| format!("/nix/store/{n}/bin")).collect();
            let dev_env = serde_json::json!({"variables": {
                "PATH": {"type": "exported", "value": path.join(":")},
                "BASHOPTS": {"type": "exported", "value": "x"},
                "list": {"type": "array", "value": ["/nix/store/00000000000000000000000000000000-no"]},
            }})
            .to_string();
            let want_roots: BTreeSet<String> = names
                .iter()
                .filter_map(|n| {
                    let mut cut = n.as_str();
                    loop {
                        if store.contains(cut) {
                            return Some(cut.to_string());
                        }
                        cut = cut.strip_suffix(['=', '?', '.', '+'])?;
                    }
                })
                .collect();
            let mut closure: BTreeSet<String> = want_roots.clone();
            for d in 0..rng.below(5) {
                closure.insert(format!("{:032}-dep{d}", rng.below(1 << 40)));
            }
            let mut out: Vec<String> = closure.iter().map(|c| format!("/nix/store/{c}")).collect();
            for i in (1..out.len()).rev() {
                out.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let out = out.join("\n");
            let key = Hash::of(&seed.to_le_bytes());
            let mut r = Resolution::new(key, None, rng.below(2) == 0);
            let fail_eval = rng.below(10) == 0;
            let fail_close = rng.below(10) == 0;
            let mut steps = 0;
            let end = loop {
                rargo_machine::check(&r, format_args!("seed {seed}"));
                steps += 1;
                assert!(steps < 10, "seed {seed}");
                let before = r.need();
                match (rng.below(3), &before) {
                    (0, Need::Closure(_)) | (1, Need::Manifest(_)) | (2, Need::Evaluate) => {}
                    (0, _) => r.closed(Ok(b"/nix/store/11111111111111111111111111111111-stray".to_vec())),
                    (1, _) => r.manifested(vec![Hash::ZERO]),
                    _ => r.evaluated(Ok(b"{}".to_vec())),
                }
                assert_eq!(r.need(), before, "seed {seed}: an answer out of turn is ignored");
                match r.need() {
                    Need::Evaluate if fail_eval => r.evaluated(Err("nix failed".into())),
                    Need::Evaluate => r.evaluated(Ok(dev_env.clone().into_bytes())),
                    Need::Exists(asked) => {
                        assert_eq!(asked.iter().cloned().collect::<BTreeSet<_>>(), candidates(&names), "seed {seed}");
                        r.exists(&asked.into_iter().filter(|n| store.contains(n)).collect());
                    }
                    Need::Closure(roots) => {
                        assert_eq!(roots.into_iter().collect::<BTreeSet<_>>(), want_roots, "seed {seed}");
                        r.closed(if fail_close { Err("nix failed".into()) } else { Ok(out.clone().into_bytes()) });
                    }
                    Need::Manifest(c) => r.manifested(c.iter().map(|n| Hash::of(n.as_bytes())).collect()),
                    done => break done,
                }
            };
            match end {
                Need::Failed(_) => assert!(fail_eval || fail_close, "seed {seed}"),
                Need::Done { env, save } => {
                    assert!(save && !fail_eval && !fail_close, "seed {seed}");
                    assert_eq!(env.closure, closure.into_iter().collect::<Vec<_>>(), "seed {seed}");
                    assert_eq!(env.env["PATH"], path.join(":"));
                    assert!(!env.env.contains_key("BASHOPTS") && !env.env.contains_key("list"));
                    assert_eq!(env.id, action_id(&key, &env));
                    let mut cached = Resolution::new(key, Some(env.clone()), r.ca);
                    let mut world = store.clone();
                    world.extend(env.closure.iter().cloned());
                    assert_eq!(drive(&mut cached, &world, b"", b""), Need::Done { env: env.clone(), save: false }, "seed {seed}");
                }
                other => panic!("seed {seed}: {other:?}"),
            }
        }
    }

    #[derive(Default)]
    struct MemTree(BTreeMap<Vec<u8>, (Node, Vec<u8>)>);

    impl MemTree {
        fn file(&mut self, path: &str, data: &str) {
            let p = path.as_bytes();
            for (i, b) in p.iter().enumerate() {
                if *b == b'/' {
                    self.0.entry(p[..i].to_vec()).or_insert((Node::Dir([0; 32]), Vec::new()));
                }
            }
            let node = Node::File { hash: Hash::of(data.as_bytes()).0, size: data.len() as u64, exec: false };
            self.0.insert(p.to_vec(), (node, data.as_bytes().to_vec()));
        }
    }

    impl Worktree for MemTree {
        fn node(&self, rel: &[u8]) -> Option<Node> {
            self.0.get(rel).map(|(n, _)| n.clone())
        }

        fn read(&self, rel: &[u8]) -> Option<Vec<u8>> {
            self.0.get(rel).filter(|(n, _)| matches!(n, Node::File { .. })).map(|(_, d)| d.clone())
        }

        fn walk(&self, f: &mut dyn FnMut(&[u8], &Node) -> bool) {
            fn go(t: &MemTree, dir: &[u8], f: &mut dyn FnMut(&[u8], &Node) -> bool) {
                for (path, (node, _)) in &t.0 {
                    let parent = path.iter().rposition(|b| *b == b'/').map_or(&path[..0], |i| &path[..i]);
                    if parent == dir && f(path, node) && matches!(node, Node::Dir(_)) {
                        go(t, path, f);
                    }
                }
            }
            go(self, b"", f)
        }
    }

    #[test]
    fn the_source_key_follows_the_files_that_define_the_flake() {
        let none: Vec<String> = Vec::new();
        let mut t = MemTree::default();
        t.file("ws/Cargo.toml", "[workspace]\n");
        t.file("ws/app/Cargo.toml", "[package]\nname = \"app\"\n");
        t.file("ws/app/src/main.rs", "fn main() {}");
        t.file("ws/rust-toolchain.toml", "[toolchain]\nchannel = \"1.90\"\n");
        let bundled = source(&t, Path::new("ws/app"), &none);
        assert_eq!(bundled.root, None);
        assert_eq!(bundled.toolchain.as_ref().map(|(n, _)| *n), Some("rust-toolchain.toml"));
        t.file("ws/app/src/main.rs", "fn main() { changed() }");
        assert_eq!(source(&t, Path::new("ws/app"), &none), bundled, "sources do not key the bundled flake");
        t.file("ws/rust-toolchain.toml", "[toolchain]\nchannel = \"1.91\"\n");
        let other_toolchain = source(&t, Path::new("ws/app"), &none);
        assert_ne!(other_toolchain.key, bundled.key, "the toolchain file keys the bundled flake");

        t.file("ws/flake.nix", "{ outputs = _: {}; }");
        let own = source(&t, Path::new("ws/app"), &none);
        assert_eq!(own.root.as_deref(), Some(Path::new("ws")), "the flake beside the workspace manifest is used");
        assert_eq!(own.toolchain, None);
        assert_ne!(own.key, other_toolchain.key);
        t.file("ws/app/src/main.rs", "fn main() { again() }");
        t.file("ws/README", "words");
        assert_eq!(source(&t, Path::new("ws/app"), &none), own);
        t.file("ws/nix/overlay.nix", "self: super: {}");
        let overlay = source(&t, Path::new("ws/app"), &none);
        assert_ne!(overlay.key, own.key, "a nix file under the workspace keys its flake");
        t.file("ws/scripts/tool", "#!/bin/sh");
        assert_eq!(source(&t, Path::new("ws/app"), &none).key, overlay.key);
        assert_ne!(source(&t, Path::new("ws/app"), &["scripts/tool".into()]).key, overlay.key, "an extra file keys the flake");
        t.file("flake.nix", "{ outputs = _: {}; }");
        assert_eq!(source(&t, Path::new("ws/app"), &none).root.as_deref(), Some(Path::new("ws")), "the nearest flake wins");
    }

    #[test]
    fn a_flake_is_evaluated_against_its_whole_tree_but_keyed_by_the_files_that_define_it() {
        let none: Vec<String> = Vec::new();
        let mut t = MemTree::default();
        t.file("ws/Cargo.toml", "[workspace]\n");
        t.file("ws/flake.nix", "{ outputs = _: {}; }");
        t.file("ws/nix/modules/partitions.nix", "{ partitions.dev.extraInputsFlake = ../dev; }");
        t.file("ws/nix/dev/flake.nix", "{ outputs = _: {}; }");
        t.file("ws/nix/dev/flake.lock", "{}");
        t.file("ws/script/cargo", "#!/bin/sh");
        t.file("ws/crates/zed/Cargo.toml", "[package]\nname = \"zed\"\n");
        t.file("ws/target/debug/zed", "binary");
        t.file("ws/.git", "gitdir: /elsewhere/.git/worktrees/ws");
        let mut copied = Vec::new();
        t.walk(&mut |rel, node| {
            let take = evaluated_entry(b"ws", rel, node);
            if take && !matches!(node, Node::Dir(_)) {
                copied.push(String::from_utf8_lossy(rel).into_owned());
            }
            take
        });
        assert_eq!(
            copied,
            ["ws/Cargo.toml", "ws/crates/zed/Cargo.toml", "ws/flake.nix", "ws/nix/dev/flake.lock", "ws/nix/dev/flake.nix", "ws/nix/modules/partitions.nix", "ws/script/cargo"],
            "nix sees the whole tree of the flake but its build outputs and its git"
        );
        let key = source(&t, Path::new("ws"), &none).key;
        t.file("ws/nix/dev/flake.lock", "{ \"version\": 7 }");
        let relocked = source(&t, Path::new("ws"), &none).key;
        assert_ne!(relocked, key, "the lock of a flake under nix/ keys it");
        t.file("ws/crates/zed/Cargo.toml", "[package]\nname = \"zed\"\nversion = \"2\"\n");
        t.file("ws/target/debug/zed", "another binary");
        assert_eq!(source(&t, Path::new("ws"), &none).key, relocked, "sources and outputs do not key it");
        t.file("ws/tools/sub/flake.lock", "{}");
        assert_ne!(source(&t, Path::new("ws"), &none).key, relocked, "a nested flake keys it");
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
