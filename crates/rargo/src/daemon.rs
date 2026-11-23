//! The client daemon: one per user and server on a client machine.
//!
//! The `rargo` command starts it on first use and talks to it over a Unix
//! socket. The daemon holds the one long-lived link to the server, so no
//! command pays for a connection, a key agreement or a slow start. It
//! watches the worktrees it has seen and pushes their changes to the server
//! as they happen, so a build usually finds the server up to date and the
//! command needs neither a scan nor an upload. It is still a dumb client:
//! it syncs files and environment and places what comes back; nothing
//! about the build is decided here.

use crate::mux::Mux;
use crate::place::{Placed, Placer};
use crate::watch::{Event, Watcher};
use crate::{Config, Policy, git, report, scan};
use rargo_net::link::Link;
use rargo_proto::{Blob, CONTROL, ClientMsg, Digest, LocalReply, LocalRequest, Offer, PROTO_VERSION, RunRequest, ServerMsg};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// A change smaller than this is pushed whole, without asking the server
/// what it lacks: an edit. Anything bigger (a worktree seen for the first
/// time, a branch switch) sends the manifests and asks, because another
/// client has usually uploaded most of it already.
const PUSH_BLIND: u64 = 1 << 20;
/// How long after a change the daemon waits before scanning: an editor's
/// write-and-rename or a checkout is many events.
const SETTLE: Duration = Duration::from_millis(20);

#[derive(Default)]
pub struct Counters {
    pub commands: AtomicU64,
    pub scans: AtomicU64,
    /// Commands that needed no scan: the watcher vouched for the worktree.
    pub scans_spared: AtomicU64,
    pub offers: AtomicU64,
    pub offered_bytes: AtomicU64,
    pub links: AtomicU64,
}

/// A worktree as one scan saw it.
struct Snap {
    scan: scan::Scan,
    policy: Policy,
    head: Option<git::Head>,
}

struct WtState {
    /// The index to start from before the first scan.
    loaded: Option<scan::Index>,
    last: Option<Arc<Snap>>,
    watched: HashSet<PathBuf>,
}

struct Worktree {
    id: u64,
    root: PathBuf,
    wt_id: String,
    wt_cache: PathBuf,
    state: tokio::sync::Mutex<WtState>,
    /// Something changed since the last scan (or may have).
    dirty: AtomicBool,
    /// Every directory is watched: no event means no change.
    vouched: AtomicBool,
    changed: tokio::sync::Notify,
    /// One command at a time places files (as cargo locks its build
    /// directory).
    running: tokio::sync::Mutex<()>,
    last_used: Mutex<Instant>,
}

pub struct Daemon {
    cfg: Config,
    cache: PathBuf,
    host: String,
    transport: String,
    mux: tokio::sync::Mutex<Option<Arc<Mux>>>,
    worktrees: Mutex<HashMap<PathBuf, Arc<Worktree>>>,
    watcher: Option<Arc<Watcher>>,
    next_wt: AtomicU64,
    /// Variable names servers have asked about.
    asked: Mutex<Vec<String>>,
    active: AtomicUsize,
    last_active: Mutex<Instant>,
    pub counters: Counters,
}

enum Outcome {
    Final(LocalReply),
    /// The worktree changed while it was being sent: scan again.
    Changed,
}

fn lost() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the connection to the server was lost")
}

fn var<'a>(vars: &'a [(String, String)], name: &str) -> Option<&'a str> {
    vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

impl Daemon {
    /// `watch`: follow worktrees with inotify (a daemon does; a command
    /// that runs without one has nothing to gain from it).
    pub fn new(cfg: Config, cache: PathBuf, watch: bool) -> Arc<Daemon> {
        let watcher = if watch {
            match Watcher::new() {
                Ok(w) => Some(Arc::new(w)),
                Err(e) => {
                    eprintln!("rargo daemon: cannot watch worktrees ({e}); every command will scan");
                    None
                }
            }
        } else {
            None
        };
        let d = Arc::new(Daemon {
            asked: Mutex::new(crate::load_asked(&cache)),
            cfg,
            cache,
            host: crate::hostname(),
            transport: std::env::var("RARGO_TRANSPORT").unwrap_or_default(),
            mux: tokio::sync::Mutex::new(None),
            worktrees: Mutex::new(HashMap::new()),
            watcher,
            next_wt: AtomicU64::new(1),
            active: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            counters: Counters::default(),
        });
        if let Some(w) = d.watcher.clone() {
            let d2 = d.clone();
            tokio::spawn(async move {
                loop {
                    let Ok(events) = w.next().await else { break };
                    let all: Vec<Arc<Worktree>> = d2.worktrees.lock().unwrap().values().cloned().collect();
                    for ev in events {
                        for wt in &all {
                            if matches!(ev, Event::Overflow) || matches!(ev, Event::Changed(id) if id == wt.id) {
                                wt.dirty.store(true, Ordering::SeqCst);
                                wt.changed.notify_one();
                            }
                        }
                    }
                }
            });
        }
        d
    }

    /// The link to the server, made if there is none (or none alive).
    async fn mux(&self) -> std::io::Result<Arc<Mux>> {
        let mut g = self.mux.lock().await;
        if let Some(m) = &*g {
            if !m.is_dead() {
                return Ok(m.clone());
            }
        }
        let addr = tokio::net::lookup_host(&self.cfg.server)
            .await?
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "the server's name has no address"))?;
        let link = Link::connect(addr, &self.cfg.user, &self.cfg.psk, &self.transport).await?;
        self.counters.links.fetch_add(1, Ordering::Relaxed);
        let m = Mux::new(link);
        *g = Some(m.clone());
        Ok(m)
    }

    /// The link, if one is up.
    async fn mux_if_up(&self) -> Option<Arc<Mux>> {
        self.mux.lock().await.clone().filter(|m| !m.is_dead())
    }

    fn worktree(self: &Arc<Self>, root: &Path) -> Arc<Worktree> {
        let mut all = self.worktrees.lock().unwrap();
        if let Some(wt) = all.get(root) {
            *wt.last_used.lock().unwrap() = Instant::now();
            return wt.clone();
        }
        let wt_id = {
            let mut h = blake3::Hasher::new();
            h.update(self.host.as_bytes());
            h.update(&[0]);
            h.update(root.as_os_str().as_bytes());
            h.finalize().to_hex()[..24].to_string()
        };
        let wt_cache = self.cache.join("wt").join(&wt_id);
        let wt = Arc::new(Worktree {
            id: self.next_wt.fetch_add(1, Ordering::Relaxed),
            root: root.to_path_buf(),
            state: tokio::sync::Mutex::new(WtState {
                loaded: Some(scan::Index::load(&wt_cache.join("index"))),
                last: None,
                watched: HashSet::new(),
            }),
            wt_id,
            wt_cache,
            dirty: AtomicBool::new(true),
            vouched: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
            running: tokio::sync::Mutex::new(()),
            last_used: Mutex::new(Instant::now()),
        });
        all.insert(root.to_path_buf(), wt.clone());
        if self.watcher.is_some() {
            // Changes travel to the server as they happen.
            let (d, w) = (self.clone(), Arc::downgrade(&wt));
            tokio::spawn(async move {
                loop {
                    let Some(wt) = w.upgrade() else { break };
                    wt.changed.notified().await;
                    // No longer followed.
                    if !d.worktrees.lock().unwrap().values().any(|x| x.id == wt.id) {
                        break;
                    }
                    tokio::time::sleep(SETTLE).await;
                    let Some(mux) = d.mux_if_up().await else { continue };
                    let mut st = wt.state.lock().await;
                    if let Ok((snap, _)) = d.fresh(&wt, &mut st).await {
                        let _ = d.offer(&mux, &wt, &snap).await;
                    }
                }
            });
        }
        wt
    }

    /// The worktree as it is now: the last scan if the watcher vouches for
    /// it, a new scan otherwise. The flag says whether a scan was needed.
    async fn fresh(&self, wt: &Arc<Worktree>, st: &mut WtState) -> std::io::Result<(Arc<Snap>, bool)> {
        // HEAD is not under the watched directories; reading it is cheap.
        let head = git::head(&wt.root);
        if let Some(last) = &st.last {
            let same_head = match (&last.head, &head) {
                (None, None) => true,
                (Some(a), Some(b)) => a.commit == b.commit && a.text == b.text,
                _ => false,
            };
            if same_head && wt.vouched.load(Ordering::SeqCst) && !wt.dirty.load(Ordering::SeqCst) {
                self.counters.scans_spared.fetch_add(1, Ordering::Relaxed);
                return Ok((last.clone(), false));
            }
        }
        // Cleared before looking: a change during the scan marks it again.
        wt.dirty.store(false, Ordering::SeqCst);
        self.counters.scans.fetch_add(1, Ordering::Relaxed);
        let (root, cache, wt_cache) = (wt.root.clone(), self.cache.clone(), wt.wt_cache.clone());
        let (previous, loaded) = (st.last.clone(), st.loaded.take());
        let snap = tokio::task::spawn_blocking(move || -> std::io::Result<Snap> {
            let mut policy = crate::load_policy(&root);
            // The client's own cache, should someone keep it in the worktree.
            if let Ok(inside) = cache.canonicalize().unwrap_or_else(|_| cache.clone()).strip_prefix(&root) {
                policy.exclude.push(format!("/{}", inside.to_string_lossy()));
            }
            let thin_git = match &head {
                Some(h) if policy.git => git::thin(&root, h, &cache),
                _ => None,
            };
            let empty = scan::Index::default();
            let old = match (&previous, &loaded) {
                (Some(p), _) => &p.scan.index,
                (None, Some(l)) => l,
                (None, None) => &empty,
            };
            let scan = scan::scan(&root, old, &policy.exclude, thin_git.as_deref())
                .map_err(|e| std::io::Error::new(e.kind(), format!("scanning {}: {e}", root.display())))?;
            if scan.index_changed {
                let _ = scan.index.save(&wt_cache.join("index"));
            }
            Ok(Snap { scan, policy, head })
        })
        .await
        .map_err(std::io::Error::other)??;
        // Watch what the scan found. A directory that was not watched
        // while it was scanned may have changed in between: look again
        // next time.
        if let Some(w) = &self.watcher {
            let now: HashSet<PathBuf> = snap.scan.dirs.iter().cloned().collect();
            let mut ok = true;
            let mut added = false;
            for dir in &snap.scan.dirs {
                if st.watched.contains(dir) {
                    continue;
                }
                match w.add(dir, wt.id) {
                    Ok(_) => {
                        st.watched.insert(dir.clone());
                        added = true;
                    }
                    // Removed since the scan: the parent's event says so.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => added = true,
                    Err(e) => {
                        if wt.vouched.swap(false, Ordering::SeqCst) || st.last.is_none() {
                            eprintln!("rargo daemon: cannot watch {} ({e}); it will be scanned for every command", dir.display());
                        }
                        ok = false;
                        break;
                    }
                }
            }
            st.watched.retain(|d| now.contains(d));
            wt.vouched.store(ok, Ordering::SeqCst);
            if added {
                wt.dirty.store(true, Ordering::SeqCst);
            }
        }
        let snap = Arc::new(snap);
        st.last = Some(snap.clone());
        Ok((snap, true))
    }

    /// Brings the server up to `snap` without being asked: everything of
    /// it the server is not believed to hold. Returns `false` if a file no
    /// longer has the content the scan saw.
    async fn offer(&self, mux: &Arc<Mux>, wt: &Arc<Worktree>, snap: &Arc<Snap>) -> std::io::Result<bool> {
        let (trees, blobs): (Vec<Digest>, Vec<(Digest, u64)>) = {
            let sent = mux.sent.lock().unwrap();
            (
                snap.scan.trees.keys().filter(|d| !sent.contains(*d)).copied().collect(),
                snap.scan.files.iter().filter(|(d, _)| !sent.contains(*d)).map(|(d, (_, size))| (*d, *size)).collect(),
            )
        };
        if trees.is_empty() && blobs.is_empty() {
            return Ok(true);
        }
        self.counters.offers.fetch_add(1, Ordering::Relaxed);
        if std::env::var_os("RARGO_DAEMON_TRACE").is_some() {
            // Which directories the new manifests are.
            let new: HashSet<&Digest> = trees.iter().collect();
            let mut paths: Vec<String> = Vec::new();
            let mut todo: Vec<(String, Digest)> = vec![(String::new(), snap.scan.root)];
            while let Some((path, d)) = todo.pop() {
                if new.contains(&d) {
                    paths.push(if path.is_empty() { ".".into() } else { path.clone() });
                }
                let Some(Ok(tree)) = snap.scan.trees.get(&d).map(|e| rargo_proto::tree::Tree::decode(e)) else { continue };
                for (name, node) in &tree.entries {
                    if let rargo_proto::tree::Node::Dir(sub) = node {
                        todo.push((format!("{path}/{}", String::from_utf8_lossy(name)), *sub));
                    }
                }
            }
            eprintln!(
                "rargo daemon: {}: pushing {} manifests ({}) and {} files",
                wt.root.display(),
                trees.len(),
                paths.join(" "),
                blobs.len()
            );
        }
        let encoded: Vec<Vec<u8>> = trees.iter().filter_map(|d| snap.scan.trees.get(d).cloned()).collect();
        let tree_bytes: u64 = encoded.iter().map(|t| t.len() as u64).sum();
        let blob_bytes: u64 = blobs.iter().map(|(_, s)| *s).sum();
        let id = mux.next_id();
        if blob_bytes <= PUSH_BLIND {
            // An edit: the files and the manifests on their paths, in one
            // message.
            let snap2 = snap.clone();
            let wanted: Vec<Digest> = blobs.iter().map(|(d, _)| *d).collect();
            let packed = tokio::task::spawn_blocking(move || -> Option<Vec<Blob>> {
                wanted
                    .iter()
                    .map(|h| {
                        let (path, _) = snap2.scan.files.get(h)?;
                        let bytes = std::fs::read(path).ok()?;
                        (*blake3::hash(&bytes).as_bytes() == *h).then(|| Blob::pack(*h, &bytes))
                    })
                    .collect()
            })
            .await
            .map_err(std::io::Error::other)?;
            let Some(packed) = packed else {
                wt.dirty.store(true, Ordering::SeqCst);
                return Ok(false);
            };
            self.counters.offered_bytes.fetch_add(tree_bytes + blob_bytes, Ordering::Relaxed);
            let msg = ClientMsg::Offer(Box::new(Offer { id, root: snap.scan.root, trees: encoded, blobs: packed, ask: false }));
            mux.post_bulk(CONTROL, &msg, (tree_bytes + blob_bytes) as usize).await?;
        } else {
            // Much is new to this daemon, not necessarily to the server:
            // the manifests go, and the server says what it lacks.
            let mut encoded = encoded;
            let mut tree_bytes = tree_bytes;
            let missing = loop {
                let answer = mux.expect_missing(id);
                self.counters.offered_bytes.fetch_add(tree_bytes, Ordering::Relaxed);
                let msg = ClientMsg::Offer(Box::new(Offer { id, root: snap.scan.root, trees: encoded, blobs: Vec::new(), ask: true }));
                mux.post_bulk(CONTROL, &msg, tree_bytes as usize).await?;
                let (lacking_trees, lacking_blobs) = answer.await.map_err(|_| lost())?;
                if lacking_trees.is_empty() {
                    break lacking_blobs;
                }
                // Manifests it was believed to have are gone there.
                encoded = lacking_trees.iter().filter_map(|d| snap.scan.trees.get(d).cloned()).collect();
                if encoded.len() != lacking_trees.len() {
                    return Err(std::io::Error::other("the server asked for a directory this worktree does not have"));
                }
                tree_bytes = encoded.iter().map(|t| t.len() as u64).sum();
            };
            let bytes: u64 = missing.iter().filter_map(|h| snap.scan.files.get(h)).map(|(_, s)| *s).sum();
            self.counters.offered_bytes.fetch_add(bytes, Ordering::Relaxed);
            if !upload(mux, CONTROL, snap, missing).await? {
                wt.dirty.store(true, Ordering::SeqCst);
                return Ok(false);
            }
        }
        let mut sent = mux.sent.lock().unwrap();
        // The belief is only a hint; kept from growing without bound.
        if sent.len() > 4_000_000 {
            sent.clear();
        }
        sent.extend(snap.scan.trees.keys().copied());
        sent.extend(snap.scan.files.keys().copied());
        Ok(true)
    }

    /// Runs one command for the `rargo` program at the other end of `out`.
    pub async fn command(self: Arc<Self>, req: LocalRequest, out: UnboundedSender<LocalReply>) {
        self.active.fetch_add(1, Ordering::SeqCst);
        struct Active<'a>(&'a Daemon);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.active.fetch_sub(1, Ordering::SeqCst);
                *self.0.last_active.lock().unwrap() = Instant::now();
            }
        }
        let _active = Active(&self);
        self.counters.commands.fetch_add(1, Ordering::Relaxed);
        let reply = match self.run(&req, &out).await {
            Ok(reply) => reply,
            Err(e) => {
                use std::io::ErrorKind::*;
                if matches!(e.kind(), ConnectionRefused | TimedOut | NotFound | HostUnreachable | NetworkUnreachable) {
                    LocalReply::Unreachable(e.to_string())
                } else {
                    let _ = out.send(LocalReply::Stderr(
                        format!("error: talking to the rargo server at {}: {e}\n", self.cfg.server).into_bytes(),
                    ));
                    LocalReply::Exit(101)
                }
            }
        };
        let _ = out.send(reply);
    }

    fn fail(out: &UnboundedSender<LocalReply>, text: String) -> LocalReply {
        let _ = out.send(LocalReply::Stderr(format!("error: {text}\n").into_bytes()));
        LocalReply::Exit(101)
    }

    async fn run(self: &Arc<Self>, req: &LocalRequest, out: &UnboundedSender<LocalReply>) -> std::io::Result<LocalReply> {
        if req.kind == "status" {
            return Ok(self.status(out).await);
        }
        let cwd = PathBuf::from(&req.cwd);
        let home = PathBuf::from(var(&req.vars, "HOME").unwrap_or("/"));
        let cargo_home = var(&req.vars, "CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cargo"));
        let base = RunRequest {
            version: PROTO_VERSION,
            args: req.args.clone(),
            root: req.cwd.clone(),
            cwd: req.cwd.clone(),
            cargo_home: cargo_home.to_string_lossy().into_owned(),
            env: Vec::new(),
            env_checked: Vec::new(),
            stdout_tty: req.stdout_tty,
            stderr_tty: req.stderr_tty,
            worktree: String::new(),
            commits: Vec::new(),
            tree: [0; 32],
            home_files: Vec::new(),
            who: format!("{}@{}", self.cfg.user, self.host),
            delivered: 0,
        };
        let env_for = |policy: &Policy| -> (Vec<(String, String)>, Vec<String>) {
            let asked = self.asked.lock().unwrap().clone();
            let env = req
                .vars
                .iter()
                .filter(|(k, _)| {
                    rargo_proto::env_allowed(k) || policy.pass.iter().any(|p| crate::pattern_matches(p, k)) || asked.contains(k)
                })
                .cloned()
                .collect();
            (env, asked)
        };
        let mut retried = false;
        if req.kind == "admin" {
            loop {
                let mux = self.mux().await?;
                let (env, env_checked) = env_for(&Policy::default());
                let run = RunRequest { env, env_checked, ..base.clone() };
                let said = AtomicBool::new(false);
                match self.converse(&mux, run, None, None, req, out, &said).await {
                    Ok(Outcome::Final(reply)) => return Ok(reply),
                    Ok(Outcome::Changed) => return Err(std::io::Error::other("unexpected answer from the server")),
                    // A link that died between commands: once more on a new one.
                    Err(_) if mux.is_dead() && !said.load(Ordering::SeqCst) && !retried => retried = true,
                    Err(e) => return Err(e),
                }
            }
        }
        let Some(root) = crate::find_root(&cwd) else {
            return Ok(Self::fail(
                out,
                format!("`{}` is not inside a git worktree or a cargo package; rargo needs to know what to sync", cwd.display()),
            ));
        };
        let wt = self.worktree(&root);
        let _running = wt.running.lock().await;
        let mut home_files = Vec::new();
        for name in ["config.toml", "config", "credentials.toml", "credentials"] {
            match std::fs::read(cargo_home.join(name)) {
                Ok(b) if b.len() <= 1 << 20 => home_files.push((name.to_string(), b)),
                _ => {}
            }
        }
        let stats = var(&req.vars, "RARGO_SYNC_STATS").is_some();
        let mut changes = 0;
        loop {
            let mux = self.mux().await?;
            let started = Instant::now();
            let snap = {
                let mut st = wt.state.lock().await;
                let (snap, scanned) = self.fresh(&wt, &mut st).await?;
                if stats {
                    let text = if scanned {
                        format!(
                            "rargo: scanned {} files in {:.1} ms ({} read, {:.1} MiB)\n",
                            snap.scan.file_count,
                            started.elapsed().as_secs_f64() * 1e3,
                            snap.scan.hashed_files,
                            snap.scan.hashed_bytes as f64 / 1048576.0
                        )
                    } else {
                        format!("rargo: no scan needed, {} files watched and unchanged\n", snap.scan.file_count)
                    };
                    let _ = out.send(LocalReply::Stderr(text.into_bytes()));
                }
                // Sent before the request and in order with it, so the
                // server has the change when it looks at the request.
                match self.offer(&mux, &wt, &snap).await {
                    Ok(true) => {}
                    Ok(false) => {
                        changes += 1;
                        if changes >= 5 {
                            return Ok(Self::fail(out, "the worktree kept changing while it was being synced".into()));
                        }
                        continue;
                    }
                    Err(_) if mux.is_dead() && !retried => {
                        retried = true;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
                snap
            };
            let (env, env_checked) = env_for(&snap.policy);
            let commits = match &snap.head {
                Some(h) => {
                    let (root, h, wt_cache) = (root.clone(), h.clone(), wt.wt_cache.clone());
                    tokio::task::spawn_blocking(move || crate::recent_commits(&root, &h, &wt_cache)).await.unwrap_or_default()
                }
                None => Vec::new(),
            };
            let delivered = std::fs::read_to_string(wt.wt_cache.join("delivered")).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0);
            let run = RunRequest {
                root: root.to_string_lossy().into_owned(),
                env,
                env_checked,
                worktree: wt.wt_id.clone(),
                commits,
                tree: snap.scan.root,
                home_files: home_files.clone(),
                delivered,
                ..base.clone()
            };
            let said = AtomicBool::new(false);
            match self.converse(&mux, run, Some(&snap), Some(&wt), req, out, &said).await {
                Ok(Outcome::Final(reply)) => return Ok(reply),
                Ok(Outcome::Changed) => {
                    wt.dirty.store(true, Ordering::SeqCst);
                    changes += 1;
                    if changes >= 5 {
                        return Ok(Self::fail(out, "the worktree kept changing while it was being synced".into()));
                    }
                }
                Err(_) if mux.is_dead() && !said.load(Ordering::SeqCst) && !retried => retried = true,
                Err(e) => return Err(e),
            }
        }
    }

    /// One command's conversation with the server, on a stream of its own.
    #[allow(clippy::too_many_arguments)]
    async fn converse(
        &self,
        mux: &Arc<Mux>,
        run: RunRequest,
        snap: Option<&Arc<Snap>>,
        wt: Option<&Arc<Worktree>>,
        req: &LocalRequest,
        out: &UnboundedSender<LocalReply>,
        said: &AtomicBool,
    ) -> std::io::Result<Outcome> {
        let (stream, tx, mut rx) = mux.open();
        // Whatever ends this conversation early (the command was
        // interrupted, an error) withdraws the request.
        struct Withdraw<'a>(&'a Mux, u32);
        impl Drop for Withdraw<'_> {
            fn drop(&mut self) {
                self.0.cancel(self.1);
            }
        }
        let _withdraw = Withdraw(mux, stream);
        mux.post(stream, &ClientMsg::Run(Box::new(run)));
        let placed_path = wt.map(|w| w.wt_cache.join("placed"));
        let mut placer = tokio::task::block_in_place(|| {
            Placer::new(&self.cache, placed_path.as_deref().map(Placed::load).unwrap_or_default())
        });
        let mut fetching: Option<u64> = None;
        let mut last: Option<LocalReply> = None;
        let mut token: Option<u64> = None;
        loop {
            if let (Some(_), None) = (&last, fetching) {
                break;
            }
            let msg = tokio::select! {
                m = rx.recv() => m,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if mux.is_dead() { None } else { continue }
                }
            };
            let Some(msg) = msg else { return Err(lost()) };
            match msg {
                ServerMsg::Stdout(b) => {
                    said.store(true, Ordering::SeqCst);
                    let _ = out.send(LocalReply::Stdout(b));
                }
                ServerMsg::Stderr(b) => {
                    said.store(true, Ordering::SeqCst);
                    let _ = out.send(LocalReply::Stderr(b));
                }
                ServerMsg::WantTrees(want) => {
                    let trees: Vec<Vec<u8>> = want.iter().filter_map(|d| snap.and_then(|s| s.scan.trees.get(d)).cloned()).collect();
                    mux.post(stream, &ClientMsg::Trees(trees));
                }
                ServerMsg::WantBlobs(want) => {
                    let ok = match snap {
                        Some(s) => upload(mux, stream, s, want).await?,
                        None => false,
                    };
                    if !ok {
                        mux.post(stream, &ClientMsg::Changed);
                        return Ok(Outcome::Changed);
                    }
                }
                ServerMsg::WantEnv(names) => {
                    let values: Vec<(String, Option<String>)> = names
                        .iter()
                        .map(|n| (n.clone(), req.vars.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone())))
                        .collect();
                    mux.post(stream, &ClientMsg::Env(values));
                    // Sent unasked from now on.
                    let mut asked = self.asked.lock().unwrap();
                    let before = asked.len();
                    for n in names {
                        if !asked.contains(&n) {
                            asked.push(n);
                        }
                    }
                    if asked.len() != before {
                        let _ = crate::write_atomic(&self.cache.join("env-names"), (asked.join("\n") + "\n").as_bytes());
                    }
                }
                ServerMsg::Deliver { files, writes, .. } => {
                    said.store(true, Ordering::SeqCst);
                    tokio::task::block_in_place(|| placer.deliver(files, writes))?;
                }
                ServerMsg::Blobs(blobs) => tokio::task::block_in_place(|| -> std::io::Result<()> {
                    for b in blobs {
                        let hash = b.hash;
                        placer.blob(hash, &b.unpack()?)?;
                    }
                    Ok(())
                })?,
                ServerMsg::BlobPart { hash, part, last } => {
                    tokio::task::block_in_place(|| -> std::io::Result<()> { placer.part(hash, &part.unpack()?, last) })?
                }
                ServerMsg::Delivered { .. } => {
                    // What the server did not send along, it thought was
                    // here: fetch the rest.
                    let wanting = placer.wanting();
                    if !wanting.is_empty() {
                        fetching = Some(mux.fetch(wanting, &tx));
                    }
                }
                ServerMsg::Fetched { id } => {
                    if fetching == Some(id) {
                        fetching = None;
                        if !placer.settled() {
                            return Err(std::io::Error::other("the server did not send every file it announced"));
                        }
                    }
                }
                ServerMsg::DeliveryToken(t) => token = Some(t),
                ServerMsg::Report(r) => {
                    let id = match tokio::task::block_in_place(|| report::save(&r)) {
                        Ok(id) => id,
                        Err(e) => {
                            let text = format!("warning: failed to write on-disk future incompatible report {}: {e}\n", r.path);
                            let _ = out.send(LocalReply::Stderr(text.into_bytes()));
                            0
                        }
                    };
                    mux.post(stream, &ClientMsg::ReportId(id));
                }
                ServerMsg::Exit(code) => last = Some(LocalReply::Exit(code)),
                ServerMsg::Exec(e) => last = Some(LocalReply::Exec(e)),
                ServerMsg::Packed(_) | ServerMsg::Missing { .. } | ServerMsg::Pong(_) => {}
            }
        }
        if !placer.settled() {
            return Err(std::io::Error::other("the server did not send every file it announced"));
        }
        tokio::task::block_in_place(|| {
            if let Some(path) = &placed_path {
                if placer.changed {
                    let _ = placer.placed.save(path);
                    placer.prune();
                }
            }
            if let (Some(t), Some(wt)) = (token, wt) {
                let _ = crate::write_atomic(&wt.wt_cache.join("delivered"), format!("{t}\n").as_bytes());
            }
        });
        Ok(Outcome::Final(last.expect("left the loop with a final message")))
    }

    async fn status(&self, out: &UnboundedSender<LocalReply>) -> LocalReply {
        let c = &self.counters;
        let (kind, bytes_out, bytes_in) = match self.mux_if_up().await {
            Some(m) => (m.kind(), m.bytes_out.load(Ordering::Relaxed), m.bytes_in.load(Ordering::Relaxed)),
            None => ("none", 0, 0),
        };
        let (worktrees, watched) = {
            let all = self.worktrees.lock().unwrap();
            (all.len(), all.values().filter(|w| w.vouched.load(Ordering::SeqCst)).count())
        };
        let text = format!(
            "server              {}\nlink                {kind}; {} made; {bytes_out} bytes sent, {bytes_in} bytes received on the current one\n\
             commands            {}\nworktrees           {worktrees} known, {watched} watched completely\n\
             scans               {} done, {} spared (nothing had changed)\n\
             pushed              {} changes, {} bytes\n",
            self.cfg.server,
            c.links.load(Ordering::Relaxed),
            c.commands.load(Ordering::Relaxed),
            c.scans.load(Ordering::Relaxed),
            c.scans_spared.load(Ordering::Relaxed),
            c.offers.load(Ordering::Relaxed),
            c.offered_bytes.load(Ordering::Relaxed),
        );
        let _ = out.send(LocalReply::Stdout(text.into_bytes()));
        LocalReply::Exit(0)
    }
}

/// Sends the contents the server lacks. Returns `false` if a file no
/// longer has the content the scan saw.
async fn upload(mux: &Arc<Mux>, stream: u32, snap: &Arc<Snap>, want: Vec<Digest>) -> std::io::Result<bool> {
    let mut small: Vec<(Digest, PathBuf, u64)> = Vec::new();
    let mut large: Vec<(Digest, PathBuf)> = Vec::new();
    for h in want {
        let Some((path, size)) = snap.scan.files.get(&h).cloned() else { return Ok(false) };
        if size as usize > rargo_proto::PART_SIZE { large.push((h, path)) } else { small.push((h, path, size)) }
    }
    // Small files travel in batches, read and compressed in parallel.
    let mut batches: Vec<Vec<(Digest, PathBuf)>> = vec![Vec::new()];
    let mut bytes = 0usize;
    for (h, p, size) in small {
        if bytes >= rargo_proto::BATCH_SIZE {
            batches.push(Vec::new());
            bytes = 0;
        }
        bytes += size as usize + 64;
        batches.last_mut().unwrap().push((h, p));
    }
    for window in batches.chunks(8) {
        let window = window.to_vec();
        let packed: Vec<Option<Vec<Blob>>> = tokio::task::spawn_blocking(move || {
            window
                .par_iter()
                .map(|batch| {
                    batch
                        .par_iter()
                        .map(|(h, p)| {
                            let bytes = std::fs::read(p).ok()?;
                            (*blake3::hash(&bytes).as_bytes() == *h).then(|| Blob::pack(*h, &bytes))
                        })
                        .collect::<Option<Vec<Blob>>>()
                })
                .collect()
        })
        .await
        .map_err(std::io::Error::other)?;
        for blobs in packed {
            let Some(blobs) = blobs else { return Ok(false) };
            if !blobs.is_empty() {
                let n = blobs.iter().map(|b| b.data.len()).sum();
                mux.post_bulk(stream, &ClientMsg::Blobs(blobs), n).await?;
            }
        }
    }
    for (h, path) in large {
        let Ok(mut f) = std::fs::File::open(&path) else { return Ok(false) };
        let mut hasher = blake3::Hasher::new();
        // One part is held back so that the last one is only sent once the
        // whole file is known to be what the scan saw.
        let mut held: Option<Vec<u8>> = None;
        loop {
            let (buf, file) = tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<u8>, std::fs::File)> {
                let mut buf = vec![0u8; rargo_proto::PART_SIZE];
                let mut n = 0;
                while n < buf.len() {
                    let got = f.read(&mut buf[n..])?;
                    if got == 0 {
                        break;
                    }
                    n += got;
                }
                buf.truncate(n);
                Ok((buf, f))
            })
            .await
            .map_err(std::io::Error::other)??;
            f = file;
            hasher.update(&buf);
            if buf.is_empty() {
                break;
            }
            if let Some(prev) = held.replace(buf) {
                let part = rargo_proto::Part::pack(prev);
                let n = part.data.len();
                mux.post_bulk(stream, &ClientMsg::BlobPart { hash: h, part, last: false }, n).await?;
            }
        }
        if *hasher.finalize().as_bytes() != h {
            return Ok(false);
        }
        let part = rargo_proto::Part::pack(held.unwrap_or_default());
        let n = part.data.len();
        mux.post_bulk(stream, &ClientMsg::BlobPart { hash: h, part, last: true }, n).await?;
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// The socket the `rargo` command talks to
// ---------------------------------------------------------------------------

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(rd: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len).await?;
    let n = u32::from_le_bytes(len) as usize;
    if n > 64 << 20 {
        return Err(std::io::Error::other("oversized request"));
    }
    let mut buf = vec![0u8; n];
    rd.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn serve_command(d: Arc<Daemon>, stream: tokio::net::UnixStream, socket: PathBuf) {
    let (mut rd, mut wr) = stream.into_split();
    let Ok(raw) = read_frame(&mut rd).await else { return };
    let Ok(req) = rargo_proto::decode::<LocalRequest>(&raw) else { return };
    if req.kind == "stop" {
        let _ = std::fs::remove_file(&socket);
        let done = rargo_proto::encode(&LocalReply::Exit(0));
        let _ = wr.write_all(&(done.len() as u32).to_le_bytes()).await;
        let _ = wr.write_all(&done).await;
        let _ = wr.shutdown().await;
        std::process::exit(0);
    }
    let (tx, mut rx) = unbounded_channel::<LocalReply>();
    let work = tokio::spawn(d.command(req, tx));
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            reply = rx.recv() => {
                let Some(reply) = reply else { break };
                let last = matches!(reply, LocalReply::Exit(_) | LocalReply::Exec(_) | LocalReply::Unreachable(_));
                let bytes = rargo_proto::encode(&reply);
                if wr.write_all(&(bytes.len() as u32).to_le_bytes()).await.is_err() || wr.write_all(&bytes).await.is_err() {
                    break;
                }
                if last {
                    let _ = wr.shutdown().await;
                    return;
                }
            }
            // The command sends nothing after its request: anything read
            // here is its end (interrupted, killed, its output closed).
            _ = rd.read(&mut probe) => break,
        }
    }
    // Nobody is listening any more: the request is withdrawn.
    work.abort();
}

/// Runs the daemon on `socket` until it has been idle for a while.
pub fn serve(socket: &Path) -> ! {
    let cfg = match crate::load_config() {
        Ok(c) => c,
        Err(e) => crate::die(&e),
    };
    let cache = crate::cache_dir();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime");
    let socket = socket.to_path_buf();
    rt.block_on(async move {
        // One daemon per socket: whoever holds the lock is it.
        let lock_path = socket.with_extension("lock");
        let lock = match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path) {
            Ok(f) => f,
            Err(e) => crate::die(&format!("{}: {e}", lock_path.display())),
        };
        {
            use std::os::fd::AsRawFd;
            // SAFETY: valid descriptor.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                // Another daemon is starting or running.
                std::process::exit(0);
            }
        }
        let _ = std::fs::remove_file(&socket);
        let listener = match tokio::net::UnixListener::bind(&socket) {
            Ok(l) => l,
            Err(e) => crate::die(&format!("{}: {e}", socket.display())),
        };
        let d = Daemon::new(cfg, cache, true);
        let idle = Duration::from_secs(
            std::env::var("RARGO_DAEMON_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1800),
        );
        eprintln!("rargo daemon: {} serving {} on {}", std::process::id(), d.cfg.server, socket.display());
        loop {
            tokio::select! {
                conn = listener.accept() => {
                    if let Ok((stream, _)) = conn {
                        *d.last_active.lock().unwrap() = Instant::now();
                        tokio::spawn(serve_command(d.clone(), stream, socket.clone()));
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    let quiet = d.active.load(Ordering::SeqCst) == 0 && d.last_active.lock().unwrap().elapsed() > idle;
                    // Someone removed the socket: nobody can reach us.
                    if quiet || !socket.exists() {
                        if quiet {
                            let _ = std::fs::remove_file(&socket);
                        }
                        break;
                    }
                    // Worktrees nobody has built in for a long time are
                    // not followed any more.
                    let stale: Vec<Arc<Worktree>> = {
                        let mut all = d.worktrees.lock().unwrap();
                        let old: Vec<PathBuf> = all
                            .iter()
                            .filter(|(_, w)| w.last_used.lock().unwrap().elapsed() > Duration::from_secs(4 * 3600))
                            .map(|(p, _)| p.clone())
                            .collect();
                        old.iter().filter_map(|p| all.remove(p)).collect()
                    };
                    for wt in stale {
                        if let Some(w) = &d.watcher {
                            w.forget(wt.id);
                        }
                        // Ends its sync task.
                        wt.changed.notify_one();
                    }
                }
            }
        }
        drop(lock);
    });
    std::process::exit(0)
}

/// Runs one command without a daemon, in this process: for machines where
/// a daemon cannot be started, and for `RARGO_NO_DAEMON=1`.
pub fn inline(cfg: Config, cache: PathBuf, req: LocalRequest, mut each: impl FnMut(LocalReply)) {
    let (tx, mut rx) = unbounded_channel::<LocalReply>();
    let worker = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
        rt.block_on(async move {
            let d = Daemon::new(cfg, cache, false);
            d.command(req, tx).await;
        });
    });
    while let Some(reply) = rx.blocking_recv() {
        each(reply);
    }
    let _ = worker.join();
}
