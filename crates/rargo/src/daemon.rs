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
use crate::outfs::{OutFs, VFile};
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

/// Manifests larger than this in all are offered by their root first.
const PROBE_FROM: u64 = 32 << 10;

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
    /// Scans that looked only at some directories, and how many they
    /// looked at.
    pub rescans: AtomicU64,
    pub rescanned_dirs: AtomicU64,
    /// Commands that needed no scan: every directory is watched and none
    /// had an event.
    pub scans_spared: AtomicU64,
    pub offers: AtomicU64,
    /// Large trees the server said it had when shown the root alone.
    pub probes_known: AtomicU64,
    pub offered_bytes: AtomicU64,
    pub links: AtomicU64,
    /// Contents fetched because something opened them: whole, by chunk,
    /// and the bytes of them that the cache already held.
    pub fetched_contents: AtomicU64,
    pub fetched_bytes: AtomicU64,
    pub reused_bytes: AtomicU64,
}

/// A worktree's output directory, mounted.
struct Mounted {
    fs: Arc<OutFs>,
    session: Arc<fusewire::Session>,
    dir: PathBuf,
    /// The device number the mount has: if the directory shows another,
    /// something else has been mounted over it (another daemon for the
    /// same worktree) or it was unmounted.
    dev: u64,
}

/// A worktree as one scan saw it.
struct Snap {
    scan: scan::Scan,
    policy: Policy,
    head: Option<git::Head>,
    /// What the scan left out because git ignores it (`gitignored = false`).
    left_out: Option<scan::Ignored>,
}

struct WtState {
    policy_dir: PathBuf,
    /// The index to start from before the first scan.
    loaded: Option<scan::Index>,
    last: Option<Arc<Snap>>,
    /// The directories of the last scan that have a watch, and its
    /// descriptor.
    watched: HashMap<PathBuf, i32>,
    /// Those that have none (the watch limit, or an error).
    unwatched: HashSet<PathBuf>,
    /// The watch limit was reported for this worktree.
    told_limit: bool,
    /// For the user, with the next command's output.
    note: Option<String>,
}

impl Worktree {
    /// Something changed, where is not known: the next scan is of
    /// everything.
    fn rescan_all(&self) {
        self.everything.store(true, Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
    }
}

struct Worktree {
    id: u64,
    root: PathBuf,
    wt_id: String,
    wt_cache: PathBuf,
    state: tokio::sync::Mutex<WtState>,
    /// Something changed since the last scan (or may have).
    dirty: AtomicBool,
    /// Where: the directories with events since the last scan.
    events: Mutex<HashSet<PathBuf>>,
    /// Somewhere unknown: the next scan is of the whole worktree.
    everything: AtomicBool,
    /// How many directories of the last scan have no watch.
    unwatched: AtomicUsize,
    changed: tokio::sync::Notify,
    /// One command at a time places files (as cargo locks its build
    /// directory).
    running: tokio::sync::Mutex<()>,
    last_used: Mutex<Instant>,
    /// The virtual output directory, if this worktree has one.
    out: Mutex<Option<Mounted>>,
    out_failed: AtomicBool,
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
    /// One first sync of a worktree at a time (see `offer`).
    big_sync: tokio::sync::Mutex<()>,
    /// One fetch per content at a time.
    fetching: Mutex<HashMap<Digest, Arc<tokio::sync::Mutex<()>>>>,
    /// Chunks the cache holds: in which content, where. Kept in the cache
    /// across daemons (see `known`).
    chunks: Mutex<crate::known::Chunks>,
    chunks_changed: AtomicBool,
    /// Trees and file contents the server is believed to hold: what was
    /// sent to it or what it said it has, by this daemon or earlier ones.
    /// Only ever a hint: the server asks for whatever it turns out to lack.
    held: crate::known::Held,
    handle: tokio::runtime::Handle,
}

enum Outcome {
    Final(LocalReply),
    /// The worktree changed while it was being sent: scan again.
    Changed,
    /// The request went out on the bet that the server had the worktree's
    /// tree, and it had not: offer it (in the way given) and ask again.
    NotThere(Probe),
}

/// How a tree that is new to this daemon is offered.
#[derive(Clone, Copy, PartialEq)]
enum Probe {
    /// By its root first, and the server's answer is waited for.
    Wait,
    /// By its root, and the answer is left to the caller, who sends the
    /// request at once: a server that has the tree (a commit someone
    /// built) then answers the request one round trip earlier.
    Bet,
    /// With all its manifests (the server is known to lack some).
    Whole,
}

enum Offered {
    /// The server has what the snapshot holds.
    Sent,
    /// The worktree changed while it was read.
    Changed,
    /// The root went out; the answer says what the server lacks.
    Bet(tokio::sync::oneshot::Receiver<(Vec<Digest>, Vec<Digest>)>),
}

fn lost() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the connection to the server was lost")
}

fn var<'a>(vars: &'a [(String, String)], name: &str) -> Option<&'a str> {
    vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

/// The link to the server could not be made: the only failure reported as
/// a server that cannot be reached (an error of the same kind from anything
/// else, a local file that is missing say, is reported as itself).
#[derive(Debug)]
struct NoLink(std::io::Error);

impl std::fmt::Display for NoLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for NoLink {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// `e`, from making the link, marked as the server being out of reach if it
/// says so (a server that answers and refuses is not).
fn no_link(e: std::io::Error) -> std::io::Error {
    use std::io::ErrorKind::*;
    if matches!(e.kind(), ConnectionRefused | TimedOut | NotFound | HostUnreachable | NetworkUnreachable) {
        std::io::Error::new(e.kind(), NoLink(e))
    } else {
        e
    }
}

fn is_no_link(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<NoLink>())
}

impl Daemon {
    /// `watch`: follow worktrees with inotify (a daemon does; a command
    /// that runs without one has nothing to gain from it).
    pub fn new(cfg: Config, cache: PathBuf, watch: bool) -> Arc<Daemon> {
        let chunks = crate::known::load_chunks(&cache);
        let held = crate::known::Held::load(&cache, &cfg.server, &cfg.user);
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
            big_sync: tokio::sync::Mutex::new(()),
            fetching: Mutex::new(HashMap::new()),
            chunks: Mutex::new(chunks),
            chunks_changed: AtomicBool::new(false),
            held,
            handle: tokio::runtime::Handle::current(),
        });
        if let Some(w) = d.watcher.clone() {
            let d2 = d.clone();
            tokio::spawn(async move {
                loop {
                    let Ok(events) = w.next().await else { break };
                    let all: Vec<Arc<Worktree>> = d2.worktrees.lock().unwrap().values().cloned().collect();
                    for ev in events {
                        for wt in &all {
                            match &ev {
                                Event::Changed(id, dir) if *id == wt.id => {
                                    wt.events.lock().unwrap().insert(dir.clone());
                                }
                                Event::Moved(id) if *id == wt.id => wt.everything.store(true, Ordering::SeqCst),
                                Event::Overflow => wt.everything.store(true, Ordering::SeqCst),
                                _ => continue,
                            }
                            wt.dirty.store(true, Ordering::SeqCst);
                            wt.changed.notify_one();
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
            .await
            .map_err(no_link)?
            .next()
            .ok_or_else(|| no_link(std::io::Error::new(std::io::ErrorKind::NotFound, "the server's name has no address")))?;
        let link = Link::connect(addr, &self.cfg.user, &self.cfg.psk, &self.transport).await.map_err(no_link)?;
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
                policy_dir: PathBuf::new(),
                loaded: Some(scan::Index::load(&wt_cache.join("index"))),
                last: None,
                watched: HashMap::new(),
                unwatched: HashSet::new(),
                told_limit: false,
                note: None,
            }),
            wt_id,
            wt_cache,
            dirty: AtomicBool::new(true),
            events: Mutex::new(HashSet::new()),
            everything: AtomicBool::new(true),
            unwatched: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
            running: tokio::sync::Mutex::new(()),
            last_used: Mutex::new(Instant::now()),
            out: Mutex::new(None),
            out_failed: AtomicBool::new(false),
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
                    // The same lock a command holds, so a sync and this push
                    // cannot each send the one edit.
                    let _running = wt.running.lock().await;
                    let mut st = wt.state.lock().await;
                    if let Ok((snap, _)) = d.fresh(&wt, &mut st).await {
                        let _ = d.offer(&mux, &wt, &snap, Probe::Wait).await;
                    }
                }
            });
        }
        wt
    }

    /// The worktree as it is now: the last scan if every directory is
    /// watched and none had an event, a new scan otherwise: of the
    /// directories that had events or have no watch, or of everything when
    /// where is not known. The flag says whether a scan was needed.
    async fn fresh(&self, wt: &Arc<Worktree>, st: &mut WtState) -> std::io::Result<(Arc<Snap>, bool)> {
        // HEAD is not under the watched directories; reading it is cheap.
        let head = git::head(&wt.root);
        let watching = self.watcher.is_some();
        if let Some(last) = &st.last {
            let same_head = match (&last.head, &head) {
                (None, None) => true,
                (Some(a), Some(b)) => a.commit == b.commit && a.text == b.text && a.tags == b.tags,
                _ => false,
            };
            if same_head && watching && st.unwatched.is_empty() && !wt.dirty.load(Ordering::SeqCst) {
                self.counters.scans_spared.fetch_add(1, Ordering::Relaxed);
                return Ok((last.clone(), false));
            }
        }
        // Cleared before looking: a change during the scan marks it again.
        wt.dirty.store(false, Ordering::SeqCst);
        let everything = wt.everything.swap(false, Ordering::SeqCst) || !watching;
        let mut changed: HashSet<PathBuf> = std::mem::take(&mut *wt.events.lock().unwrap());
        changed.extend(st.unwatched.iter().cloned());
        if let Some(w) = &self.watcher {
            // A directory removed (and perhaps made again) lost its watch.
            changed.extend(st.watched.iter().filter(|(d, wd)| !w.watches(**wd, d)).map(|(d, _)| d.clone()));
        }
        let (root, cache, wt_cache) = (wt.root.clone(), self.cache.clone(), wt.wt_cache.clone());
        // Taken, so that a rescan can edit it rather than copy it.
        let (previous, loaded) = (st.last.take(), st.loaded.take());
        let policy_dir = st.policy_dir.clone();
        let snap = tokio::task::spawn_blocking(move || -> std::io::Result<Snap> {
            let mut policy = crate::load_policy(&root, &policy_dir);
            // The client's own cache, should someone keep it in the worktree.
            if let Ok(inside) = cache.canonicalize().unwrap_or_else(|_| cache.clone()).strip_prefix(&root) {
                policy.exclude.push(format!("/{}", inside.to_string_lossy()));
            }
            let left_out = if policy.gitignored { None } else { scan::Ignored::from_git(&root, &policy.include) };
            // Only directories that may have changed are looked at, unless
            // what is synced at all changed since.
            let reusable = previous.as_ref().is_some_and(|p| {
                !everything
                    && p.policy.exclude == policy.exclude
                    && p.policy.gitignored == policy.gitignored
                    && p.policy.include == policy.include
                    && p.left_out == left_out
            });
            let whole = |old: &scan::Index| scan::scan(&root, old, &policy.exclude, left_out.as_ref());
            let scanned = match previous {
                Some(p) if reusable => {
                    let prev = Arc::try_unwrap(p).map(|s| s.scan).unwrap_or_else(|shared| shared.scan.clone());
                    // Should the scan before not add up, the whole worktree
                    // is scanned, against the index on disk.
                    scan::rescan(&root, prev, &changed, &policy.exclude, left_out.as_ref())
                        .or_else(|_| whole(&scan::Index::load(&wt_cache.join("index"))))
                }
                Some(p) => whole(&p.scan.index),
                None => whole(&loaded.unwrap_or_default()),
            };
            let mut scan = scanned.map_err(|e| std::io::Error::new(e.kind(), format!("scanning {}: {e}", root.display())))?;
            if scan.index_changed {
                let _ = scan.index.save(&wt_cache.join("index"));
            }
            if let Some(h) = &head {
                let history = match policy.git {
                    git::Mode::Off => None,
                    git::Mode::Head => Some(false),
                    git::Mode::History => Some(true),
                    git::Mode::Detect => Some(git::asks_for_history(&root, scan.index.files.keys().map(|k| k.as_slice()))),
                };
                // History that cannot be had (git failed) leaves HEAD.
                let objects = if history == Some(true) { git::history(&root, h, &cache) } else { None };
                let thin = history.and_then(|_| git::thin(&root, h, &cache, objects.is_some()));
                if let Some(thin) = thin {
                    let stored = match &objects {
                        Some(dir) => {
                            // One index for all of a repository's HEADs:
                            // they link the same pieces under the same names.
                            let at = dir.parent().and_then(Path::parent).unwrap_or(dir).join("index");
                            let s = scan::scan_store(dir, &scan::Index::load(&at))?;
                            if s.index_changed {
                                let _ = s.index.save(&at);
                            }
                            Some(s)
                        }
                        None => None,
                    };
                    scan::graft_git(&mut scan, &root, &thin, stored)?;
                }
            }
            Ok(Snap { scan, policy, head, left_out })
        })
        .await
        .map_err(std::io::Error::other)??;
        if snap.scan.listed_dirs < snap.scan.dirs.len() as u64 {
            self.counters.rescans.fetch_add(1, Ordering::Relaxed);
            self.counters.rescanned_dirs.fetch_add(snap.scan.listed_dirs, Ordering::Relaxed);
        } else {
            self.counters.scans.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(w) = &self.watcher {
            self.watch(w, wt, st, &snap);
        }
        let snap = Arc::new(snap);
        st.last = Some(snap.clone());
        Ok((snap, true))
    }

    /// Watches the directories of `snap` that have no watch, and stops
    /// watching those it no longer has. A directory watched only now may
    /// have changed while it was scanned: the next scan looks at it again.
    /// What cannot be watched is looked at by every scan; the watch limit
    /// is said once, with the setting that raises it.
    fn watch(&self, w: &Watcher, wt: &Worktree, st: &mut WtState, snap: &Snap) {
        let now: HashSet<&PathBuf> = snap.scan.dirs.iter().collect();
        let mut unwatched = HashSet::new();
        let mut added = Vec::new();
        let mut over_limit = 0usize;
        let mut failed = None;
        for dir in &snap.scan.dirs {
            if st.watched.get(dir).is_some_and(|wd| w.watches(*wd, dir)) {
                continue;
            }
            st.watched.remove(dir);
            match w.add(dir, wt.id) {
                Ok(wd) => {
                    st.watched.insert(dir.clone(), wd);
                    added.push(dir.clone());
                }
                // Removed since the scan: its parent had an event.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => added.push(dir.clone()),
                Err(e) => {
                    if e.raw_os_error() == Some(libc::ENOSPC) {
                        over_limit += 1;
                    } else if failed.is_none() && !st.unwatched.contains(dir) {
                        failed = Some(format!("rargo: cannot watch {} ({e}); it is looked at again for every command\n", dir.display()));
                    }
                    unwatched.insert(dir.clone());
                }
            }
        }
        st.watched.retain(|d, wd| {
            let keep = now.contains(d);
            if !keep {
                w.remove(*wd, d);
            }
            keep
        });
        if !added.is_empty() {
            wt.events.lock().unwrap().extend(added);
            wt.dirty.store(true, Ordering::SeqCst);
        }
        let mut note = failed.unwrap_or_default();
        if over_limit > 0 && !st.told_limit {
            let limit = Watcher::limit().unwrap_or(0);
            // Room for these and as many again, in round numbers.
            let enough = (limit + 2 * over_limit as u64).div_ceil(65536) * 65536;
            note.push_str(&format!(
                "rargo: {over_limit} of the {} directories of {} are not watched: the kernel allows {limit} inotify watches per user, and they are in use. \
                 Those directories are looked at again for every command. To watch them all: sysctl fs.inotify.max_user_watches={enough}\n",
                snap.scan.dirs.len(),
                wt.root.display()
            ));
        }
        st.told_limit = over_limit > 0;
        wt.unwatched.store(unwatched.len(), Ordering::SeqCst);
        st.unwatched = unwatched;
        if !note.is_empty() {
            st.note = Some(note);
        }
    }

    /// Brings the server up to `snap` without being asked: everything of
    /// it the server is not believed to hold. Returns `false` if a file no
    /// longer has the content the scan saw.
    async fn offer(&self, mux: &Arc<Mux>, wt: &Arc<Worktree>, snap: &Arc<Snap>, probe: Probe) -> std::io::Result<Offered> {
        let delta = || -> (Vec<Digest>, Vec<(Digest, u64)>) {
            (
                self.held.lacking(snap.scan.trees.keys().map(|d| (d, ()))).into_iter().map(|(d, ())| d).collect(),
                self.held.lacking(snap.scan.files.iter().map(|(d, (_, size))| (d, *size))),
            )
        };
        let (mut trees, mut blobs) = delta();
        if trees.is_empty() && blobs.is_empty() {
            return Ok(Offered::Sent);
        }
        // Much to send: one such sync at a time. Twenty worktrees of one
        // commit that start together would otherwise each upload the
        // repository; after the first, the others have next to nothing
        // to send.
        let _one_at_a_time = if blobs.iter().map(|(_, s)| *s).sum::<u64>() > PUSH_BLIND {
            let turn = self.big_sync.lock().await;
            (trees, blobs) = delta();
            if trees.is_empty() && blobs.is_empty() {
                return Ok(Offered::Sent);
            }
            Some(turn)
        } else {
            None
        };
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
        // A caller that is waiting (a sync, or a command that asked to)
        // needs the server to have applied the offer before this returns.
        // A small edit otherwise goes out with no reply.
        if blob_bytes <= PUSH_BLIND && probe != Probe::Wait {
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
                wt.rescan_all();
                return Ok(Offered::Changed);
            };
            self.counters.offered_bytes.fetch_add(tree_bytes + blob_bytes, Ordering::Relaxed);
            let msg = ClientMsg::Offer(Box::new(Offer { id, root: snap.scan.root, trees: encoded, blobs: packed, ask: false }));
            mux.post_bulk(CONTROL, &msg, (tree_bytes + blob_bytes) as usize).await?;
        } else {
            // Much is new to this daemon, not necessarily to the server:
            // the manifests go, and the server says what it lacks.
            let mut encoded = encoded;
            let mut tree_bytes = tree_bytes;
            // First the root alone: a server that has this tree (another
            // worktree's commit, a branch another agent built) says so in
            // one small round trip, where the manifests of a repository
            // take several to get through a link that is just starting.
            // One that lacks any manifest is sent them all, as before.
            let mut known = if tree_bytes > PROBE_FROM && probe != Probe::Whole {
                let answer = mux.expect_missing(id);
                let msg = ClientMsg::Offer(Box::new(Offer { id, root: snap.scan.root, trees: Vec::new(), blobs: Vec::new(), ask: true }));
                mux.post_bulk(CONTROL, &msg, 0).await?;
                if probe == Probe::Bet {
                    return Ok(Offered::Bet(answer));
                }
                let (lacking_trees, lacking_blobs) = answer.await.map_err(|_| lost())?;
                lacking_trees.is_empty().then_some(lacking_blobs)
            } else {
                None
            };
            let missing = loop {
                if let Some(blobs) = known.take() {
                    self.counters.probes_known.fetch_add(1, Ordering::Relaxed);
                    break blobs;
                }
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
                wt.rescan_all();
                return Ok(Offered::Changed);
            }
        }
        self.has_now(snap);
        Ok(Offered::Sent)
    }

    /// Notes that the server holds everything of `snap`.
    fn has_now(&self, snap: &Snap) {
        self.held.add(snap.scan.trees.keys().chain(snap.scan.files.keys()));
    }

    /// Writes down what outlives this daemon: what the server holds and
    /// where the cache holds chunks.
    pub fn save_known(&self) {
        if let Err(e) = self.held.save() {
            eprintln!("rargo daemon: cannot keep what the server holds: {e}");
        }
        if self.chunks_changed.swap(false, Ordering::SeqCst) {
            let chunks = self.chunks.lock().unwrap().clone();
            if let Err(e) = crate::known::save_chunks(&self.cache, &chunks) {
                eprintln!("rargo daemon: cannot keep the chunk index: {e}");
            }
        }
    }

    /// The worktree's virtual output directory, mounted if it can be and
    /// the command does not say otherwise (`RARGO_OUTPUT=copy`, an output
    /// directory of its own choosing).
    fn out_for(self: &Arc<Self>, wt: &Arc<Worktree>, req: &LocalRequest) -> Option<(Arc<OutFs>, PathBuf)> {
        let elsewhere = var(&req.vars, "CARGO_TARGET_DIR").is_some()
            || var(&req.vars, "CARGO_BUILD_TARGET_DIR").is_some()
            || req.args.iter().any(|a| a == "--target-dir" || a.starts_with("--target-dir="));
        // A command in another mount namespace (a container, a sandbox)
        // would not see what this daemon mounts.
        let same_ns = req.mount_ns == crate::mount_ns();
        let want = self.watcher.is_some() && var(&req.vars, "RARGO_OUTPUT") != Some("copy") && !elsewhere && same_ns;
        let mut g = wt.out.lock().unwrap();
        if !want {
            if let Some(m) = g.take() {
                m.fs.save();
                m.session.unmount();
            }
            return None;
        }
        if let Some(m) = &*g {
            use std::os::unix::fs::MetadataExt;
            if m.session.shown() && std::fs::metadata(&m.dir).map(|md| md.dev()).ok() == Some(m.dev) {
                return Some((m.fs.clone(), m.dir.clone()));
            }
            // No longer what is seen at the directory (another daemon, for
            // another server, mounted its own there): start over.
            if let Some(m) = g.take() {
                m.fs.save();
                m.session.unmount();
            }
        }
        if wt.out_failed.load(Ordering::SeqCst) && var(&req.vars, "RARGO_OUTPUT") != Some("fuse") {
            return None;
        }
        match self.mount_out(wt) {
            Ok(m) => {
                let r = (m.fs.clone(), m.dir.clone());
                *g = Some(m);
                Some(r)
            }
            Err(e) => {
                if !wt.out_failed.swap(true, Ordering::SeqCst) {
                    eprintln!("rargo daemon: {}: no virtual output directory ({e}); results are copied into it", wt.root.display());
                }
                None
            }
        }
    }

    fn mount_out(self: &Arc<Self>, wt: &Arc<Worktree>) -> std::io::Result<Mounted> {
        let dir = wt.root.join("target");
        // What a daemon that is gone, or another version of this one, left
        // mounted there.
        release_mounts(&dir);
        if std::fs::symlink_metadata(&dir).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err(std::io::Error::other("`target` is a symbolic link"));
        }
        std::fs::create_dir_all(&dir)?;
        let tag = dir.join("CACHEDIR.TAG");
        if !tag.exists() {
            std::fs::write(&tag, rargo_proto::CACHEDIR_TAG)?;
        }
        let weak = Arc::downgrade(self);
        let handle = self.handle.clone();
        let fs = OutFs::new(
            &dir,
            &wt.wt_cache.join("virtual"),
            Box::new(move |hash, size| {
                let d = weak.upgrade().ok_or_else(|| std::io::Error::other("the daemon is stopping"))?;
                let (hash, handle) = (*hash, handle.clone());
                handle.block_on(async move { d.fetch_content(hash, size).await })
            }),
        )?;
        // SAFETY: no arguments, cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        // auto_unmount: a daemon that is killed must not leave `target/` as
        // a dead mount for editors, stock cargo and the next `ls` to trip on.
        let opts = fusewire::MountOptions {
            fsname: "rargo".into(),
            allow_other: root,
            default_permissions: true,
            passthrough: true,
            auto_unmount: true,
        };
        let session = Arc::new(fusewire::Session::mount(&dir, &opts)?);
        fs.attach(session.clone());
        // Enough threads that a parallel build's name operations (a
        // compiler per core, each creating and renaming files) do not
        // queue behind each other.
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).clamp(8, 32);
        session.spawn_workers(fs.clone(), threads)?;
        let dev = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir)?.dev()
        };
        Ok(Mounted { fs, session, dir, dev })
    }

    /// Unmounts every virtual output directory (the daemon is going).
    fn unmount_all(&self) {
        let all: Vec<Arc<Worktree>> = self.worktrees.lock().unwrap().values().cloned().collect();
        for wt in all {
            if let Some(m) = wt.out.lock().unwrap().take() {
                m.fs.save();
                m.session.unmount();
            }
        }
    }

    /// Gives a worktree's output directory back to plain files: unmounted,
    /// and nothing delivered remembered (`cargo clean` is about to run).
    fn release(&self, cwd: &Path) {
        let Some(root) = crate::find_root(cwd) else { return };
        let wt = self.worktrees.lock().unwrap().get(&root).cloned();
        if let Some(wt) = wt {
            if let Some(m) = wt.out.lock().unwrap().take() {
                m.session.unmount();
            }
            let _ = std::fs::remove_file(wt.wt_cache.join("virtual"));
            let _ = std::fs::remove_file(wt.wt_cache.join("delivered"));
        }
    }

    /// Makes sure a content is in the cache, fetching it from the server
    /// if need be: by chunk when it is large, so that what the cache holds
    /// of it already (an earlier version of the same binary, say) is not
    /// sent again.
    async fn fetch_content(self: &Arc<Self>, hash: Digest, size: u64) -> std::io::Result<PathBuf> {
        if let Some(p) = crate::place::cached(&self.cache, &hash, size) {
            return Ok(p);
        }
        let turn = self.fetching.lock().unwrap().entry(hash).or_default().clone();
        let _turn = turn.lock().await;
        if let Some(p) = crate::place::cached(&self.cache, &hash, size) {
            return Ok(p);
        }
        let mux = self.mux().await?;
        self.counters.fetched_contents.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = unbounded_channel();
        if size >= rargo_proto::CHUNKED_FROM {
            let id = mux.ask(&tx, |id| ClientMsg::Recipe { id, hash });
            let mut recipe: Vec<(Digest, u32)> = Vec::new();
            loop {
                match next_msg(&mux, &mut rx).await? {
                    ServerMsg::Recipe { chunks, .. } => recipe = chunks,
                    ServerMsg::Fetched { id: got } if got == id => break,
                    _ => {}
                }
            }
            if !recipe.is_empty() {
                let slot = crate::place::cache_slot(&self.cache, &hash);
                let tmp = slot.with_file_name(format!(".incoming-{}-{}", std::process::id(), mux.next_id()));
                if let Some(parent) = slot.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let file = std::fs::File::create(&tmp)?;
                let mut offsets = Vec::with_capacity(recipe.len());
                let mut at = 0u64;
                for (_, len) in &recipe {
                    offsets.push(at);
                    at += *len as u64;
                }
                // What the cache holds already.
                let mut need: Vec<u32> = Vec::new();
                tokio::task::block_in_place(|| {
                    use std::os::unix::fs::FileExt;
                    for (i, (chunk, len)) in recipe.iter().enumerate() {
                        let held = self.chunks.lock().unwrap().get(chunk).copied();
                        let local = held.and_then(|(blob, off, l)| {
                            let path = crate::place::cached(&self.cache, &blob, std::fs::metadata(crate::place::cache_slot(&self.cache, &blob)).ok()?.len())?;
                            let mut buf = vec![0u8; l as usize];
                            std::fs::File::open(path).ok()?.read_exact_at(&mut buf, off).ok()?;
                            (l == *len && *blake3::hash(&buf).as_bytes() == *chunk).then_some(buf)
                        });
                        match local {
                            Some(buf) => {
                                self.counters.reused_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
                                let _ = file.write_all_at(&buf, offsets[i]);
                            }
                            None => need.push(i as u32),
                        }
                    }
                });
                if !need.is_empty() {
                    let id = mux.ask(&tx, |id| ClientMsg::Chunks { id, hash, idx: need });
                    loop {
                        match next_msg(&mux, &mut rx).await? {
                            ServerMsg::Chunks { chunks, .. } => tokio::task::block_in_place(|| -> std::io::Result<()> {
                                use std::os::unix::fs::FileExt;
                                for (i, part) in chunks {
                                    let data = part.unpack()?;
                                    self.counters.fetched_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                                    let at = *offsets.get(i as usize).ok_or_else(|| std::io::Error::other("chunk out of range"))?;
                                    file.write_all_at(&data, at)?;
                                }
                                Ok(())
                            })?,
                            ServerMsg::Fetched { id: got } if got == id => break,
                            _ => {}
                        }
                    }
                }
                drop(file);
                let whole = tokio::task::block_in_place(|| scan::hash_file(&tmp, size));
                if whole.ok() != Some(hash) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(std::io::Error::other("the server sent content that does not match its hash"));
                }
                let _ = std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o444));
                std::fs::rename(&tmp, &slot)?;
                let mut index = self.chunks.lock().unwrap();
                if index.len() > crate::known::MOST_CHUNKS {
                    index.clear();
                }
                for (i, (chunk, len)) in recipe.iter().enumerate() {
                    index.insert(*chunk, (hash, offsets[i], *len));
                }
                self.chunks_changed.store(true, Ordering::SeqCst);
                return Ok(slot);
            }
        }
        // Small, or unknown to the server's chunker: whole.
        let id = mux.fetch(vec![hash], &tx);
        let mut placer = Placer::new(&self.cache, Placed::default());
        loop {
            match next_msg(&mux, &mut rx).await? {
                ServerMsg::Blobs(blobs) => tokio::task::block_in_place(|| -> std::io::Result<()> {
                    for b in blobs {
                        let h = b.hash;
                        let bytes = b.unpack()?;
                        self.counters.fetched_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        placer.blob(h, &bytes)?;
                    }
                    Ok(())
                })?,
                ServerMsg::BlobPart { hash: h, part, last } => tokio::task::block_in_place(|| -> std::io::Result<()> {
                    let bytes = part.unpack()?;
                    self.counters.fetched_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    placer.part(h, &bytes, last)
                })?,
                ServerMsg::Fetched { id: got } if got == id => break,
                _ => {}
            }
        }
        crate::place::cached(&self.cache, &hash, size)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "the server no longer has this content"))
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
                if is_no_link(&e) {
                    LocalReply::Unreachable(e.to_string())
                } else {
                    // A server that does not know us: no token, or one that
                    // was revoked or has expired.
                    let hint = if e.to_string().contains(rargo_proto::REFUSED) {
                        "       (this machine is not authenticated there, or its token no longer works: run `rargo auth`)\n"
                    } else {
                        ""
                    };
                    let _ = out.send(LocalReply::Stderr(
                        format!("error: talking to the rargo server at {}: {e}\n{hint}", self.cfg.server).into_bytes(),
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
            lazy: false,
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
                match self.converse(&mux, run, None, None, None, req, out, &said, None).await {
                    Ok(Outcome::Final(reply)) => return Ok(reply),
                    Ok(Outcome::Changed | Outcome::NotThere(_)) => return Err(std::io::Error::other("unexpected answer from the server")),
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
        let Some(policy_dir) = rargo_proto::workspace::command_dir(&root, &cwd, &req.args) else {
            return Ok(Self::fail(out, "the manifest must be inside the synced worktree".into()));
        };
        let wt = self.worktree(&root);
        let _running = wt.running.lock().await;
        let shown = if req.kind == "sync" { None } else { self.out_for(&wt, req) };
        let mut home_files = Vec::new();
        for name in ["config.toml", "config", "credentials.toml", "credentials"] {
            match std::fs::read(cargo_home.join(name)) {
                Ok(b) if b.len() <= 1 << 20 => home_files.push((name.to_string(), b)),
                _ => {}
            }
        }
        let stats = var(&req.vars, "RARGO_SYNC_STATS").is_some();
        let mut changes = 0;
        let taken_up = Instant::now();
        let phase = |what: &str| {
            if stats {
                let _ = out.send(LocalReply::Stderr(format!("rargo: phase {:>8.3} s  {what}\n", taken_up.elapsed().as_secs_f64()).into_bytes()));
            }
        };
        // `RARGO_OFFER_WAIT=1` on a command: no request is sent before the
        // server has said that it holds the worktree's tree.
        let mut probe = if req.kind == "sync" || var(&req.vars, "RARGO_OFFER_WAIT").is_some() { Probe::Wait } else { Probe::Bet };
        loop {
            let mux = self.mux().await?;
            phase("link to the server ready");
            let started = Instant::now();
            let mut bet = None;
            let snap = {
                let mut st = wt.state.lock().await;
                if st.policy_dir != policy_dir {
                    st.policy_dir.clone_from(&policy_dir);
                    wt.rescan_all();
                }
                let (snap, scanned) = self.fresh(&wt, &mut st).await?;
                if let Some(note) = st.note.take() {
                    let _ = out.send(LocalReply::Stderr(note.into_bytes()));
                }
                if stats {
                    let text = if scanned {
                        format!(
                            "rargo: scanned {} files in {:.1} ms ({} read, {:.1} MiB), looking at {} of {} directories\n",
                            snap.scan.file_count,
                            started.elapsed().as_secs_f64() * 1e3,
                            snap.scan.hashed_files,
                            snap.scan.hashed_bytes as f64 / 1048576.0,
                            snap.scan.listed_dirs,
                            snap.scan.dirs.len()
                        )
                    } else {
                        format!("rargo: no scan needed, {} files watched and unchanged\n", snap.scan.file_count)
                    };
                    let _ = out.send(LocalReply::Stderr(text.into_bytes()));
                }
                // Sent before the request and in order with it, so the
                // server has the change when it looks at the request.
                phase("worktree read");
                match self.offer(&mux, &wt, &snap, probe).await {
                    Ok(Offered::Sent) => phase("worktree offered and what the server lacked sent"),
                    Ok(Offered::Bet(answer)) => {
                        phase("worktree offered by its root, answer not awaited");
                        bet = Some(answer);
                    }
                    Ok(Offered::Changed) => {
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
            if req.kind == "sync" {
                return Ok(LocalReply::Exit(0));
            }
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
                lazy: shown.is_some(),
                ..base.clone()
            };
            let said = AtomicBool::new(false);
            phase("request sent");
            match self.converse(&mux, run, Some(&snap), Some(&wt), shown.as_ref(), req, out, &said, bet).await {
                Ok(Outcome::Final(reply)) => {
                    phase("answer complete");
                    return Ok(reply);
                }
                Ok(Outcome::NotThere(how)) => {
                    phase("the server lacked the tree: offering it");
                    probe = how;
                }
                Ok(Outcome::Changed) => {
                    wt.rescan_all();
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
        shown: Option<&(Arc<OutFs>, PathBuf)>,
        req: &LocalRequest,
        out: &UnboundedSender<LocalReply>,
        said: &AtomicBool,
        mut bet: Option<tokio::sync::oneshot::Receiver<(Vec<Digest>, Vec<Digest>)>>,
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
        let mut fetching: Option<(u64, Vec<Digest>)> = None;
        let mut delivery_ready = false;
        let mut last: Option<LocalReply> = None;
        let mut token: Option<u64> = None;
        // What the server said on stderr, kept while the worktree leaves out
        // what git ignores: a failure that names such a path is explained.
        let left_out = snap.and_then(|s| s.left_out.as_ref()).zip(wt);
        let mut heard: Vec<u8> = Vec::new();
        loop {
            if let (Some(_), None) = (&last, &fetching) {
                break;
            }
            let msg = tokio::select! {
                // The answer to the root this request was sent behind.
                lacking = async { bet.as_mut().unwrap().await }, if bet.is_some() => {
                    bet = None;
                    let (trees, blobs) = lacking.map_err(|_| lost())?;
                    if trees.is_empty() && blobs.is_empty() {
                        if let Some(s) = snap {
                            self.has_now(s);
                        }
                        self.counters.probes_known.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // The server could not have started on it: withdrawn,
                    // and asked again when it has the tree.
                    mux.post(stream, &ClientMsg::Changed);
                    return Ok(Outcome::NotThere(if trees.is_empty() { Probe::Wait } else { Probe::Whole }));
                }
                m = rx.recv() => m,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if mux.is_dead() { None } else { continue }
                }
            };
            let Some(msg) = msg else { return Err(lost()) };
            if bet.is_some() && matches!(msg, ServerMsg::WantTrees(_) | ServerMsg::WantBlobs(_)) {
                // The server is asking for the tree piece by piece: it
                // lacks it. Sent whole instead.
                mux.post(stream, &ClientMsg::Changed);
                return Ok(Outcome::NotThere(Probe::Whole));
            }
            match msg {
                ServerMsg::Stdout(b) => {
                    said.store(true, Ordering::SeqCst);
                    let _ = out.send(LocalReply::Stdout(b));
                }
                ServerMsg::Stderr(b) => {
                    said.store(true, Ordering::SeqCst);
                    if left_out.is_some() && heard.len() < HEARD_MAX {
                        heard.extend_from_slice(&b[..b.len().min(HEARD_MAX - heard.len())]);
                    }
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
                    tokio::task::block_in_place(|| -> std::io::Result<()> {
                        // What belongs in a virtual output directory is
                        // shown there; the rest is placed as files.
                        let (files, writes) = match shown {
                            Some((fs, dir)) => show(fs, dir, files, writes)?,
                            None => (files, writes),
                        };
                        placer.deliver(files, writes)?;
                        Ok(())
                    })?;
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
                ServerMsg::Delivered { .. } => delivery_ready = true,
                ServerMsg::Fetched { id } => {
                    if let Some((sent, hashes)) = &fetching && *sent == id {
                        if hashes.iter().any(|hash| placer.awaiting(hash)) {
                            return Err(std::io::Error::other("the server did not send every file it announced"));
                        }
                        fetching = None;
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
                ServerMsg::Exit(code) => {
                    if let (true, Some((ignored, wt))) = (code != 0, left_out) {
                        if let Some(note) = left_out_note(ignored, &wt.root, &heard) {
                            let _ = out.send(LocalReply::Stderr(note.into_bytes()));
                        }
                    }
                    last = Some(LocalReply::Exit(code))
                }
                ServerMsg::Exec(e) => last = Some(LocalReply::Exec(e)),
                ServerMsg::Packed(_)
                | ServerMsg::Missing { .. }
                | ServerMsg::Pong(_)
                | ServerMsg::Recipe { .. }
                | ServerMsg::Chunks { .. } => {}
            }
            // A second delivery can finish while its predecessor's fetch
            // is still in flight. Let that fetch finish before requesting
            // more digests so multipart replies for a hash cannot mix.
            if delivery_ready && fetching.is_none() {
                delivery_ready = false;
                let hashes = placer.wanting();
                if !hashes.is_empty() {
                    let id = mux.fetch(hashes.clone(), &tx);
                    fetching = Some((id, hashes));
                }
            }
        }
        if let Some((fs, _)) = shown {
            tokio::task::block_in_place(|| fs.save());
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
            (all.len(), all.values().filter(|w| w.unwatched.load(Ordering::SeqCst) == 0).count())
        };
        let outs = {
            let all = self.worktrees.lock().unwrap();
            let mut t = (0usize, 0usize, 0u64, 0u64);
            for w in all.values() {
                if let Some(m) = &*w.out.lock().unwrap() {
                    t.0 += 1;
                    t.1 += m.fs.virtual_count();
                    t.2 += m.fs.opened_virtual.load(Ordering::Relaxed);
                    t.3 += m.fs.made_real.load(Ordering::Relaxed);
                }
            }
            t
        };
        let text = format!(
            "server              {}\nlink                {kind}; {} made; {bytes_out} bytes sent, {bytes_in} bytes received on the current one\n\
             commands            {}\nworktrees           {worktrees} known, {watched} watched completely\n\
             scans               {} of whole worktrees, {} of {} directories that had changed or have no watch, {} spared (nothing had changed)\n\
             pushed              {} changes, {} bytes; {} trees the server had already (asked by root)\n\
             held                {} trees and contents the server is believed to hold (kept across daemons)\n\
             outputs             {} worktrees with a virtual output directory showing {} files; {} opened, {} made real; {} contents fetched on open, {} bytes fetched, {} bytes reused from the cache\n",
            self.cfg.server,
            c.links.load(Ordering::Relaxed),
            c.commands.load(Ordering::Relaxed),
            c.scans.load(Ordering::Relaxed),
            c.rescans.load(Ordering::Relaxed),
            c.rescanned_dirs.load(Ordering::Relaxed),
            c.scans_spared.load(Ordering::Relaxed),
            c.offers.load(Ordering::Relaxed),
            c.offered_bytes.load(Ordering::Relaxed),
            c.probes_known.load(Ordering::Relaxed),
            self.held.len(),
            outs.0,
            outs.1,
            outs.2,
            outs.3,
            c.fetched_contents.load(Ordering::Relaxed),
            c.fetched_bytes.load(Ordering::Relaxed),
            c.reused_bytes.load(Ordering::Relaxed),
        );
        let mut text = text;
        for w in self.worktrees.lock().unwrap().values() {
            if let Some(m) = &*w.out.lock().unwrap() {
                text.push_str(&format!("operations          {}: {}\n", m.dir.display(), m.fs.op_report()));
            }
        }
        let _ = out.send(LocalReply::Stdout(text.into_bytes()));
        LocalReply::Exit(0)
    }
}

/// The next message on a channel the link feeds, or an error once the
/// link is dead.
async fn next_msg(mux: &Arc<Mux>, rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMsg>) -> std::io::Result<ServerMsg> {
    loop {
        tokio::select! {
            m = rx.recv() => return m.ok_or_else(lost),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if mux.is_dead() {
                    return Err(lost());
                }
            }
        }
    }
}

/// Unmounts whatever is mounted at `dir` (lazily: a file system whose
/// daemon is gone cannot be asked).
fn release_mounts(dir: &Path) {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else { return };
    let here = dir.to_string_lossy();
    let mounts = info.lines().filter(|l| l.split(' ').nth(4) == Some(here.as_ref())).count();
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return };
    for _ in 0..mounts {
        // SAFETY: valid NUL-terminated path.
        if unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) } != 0 {
            // Not allowed to: the helper that mounted it unmounts it.
            fusewire::unmount_with_helper(dir);
        }
    }
}

/// Shows the part of a delivery that lies in the virtual output directory
/// mounted at `dir`, and returns the rest.
fn show(
    fs: &Arc<OutFs>,
    dir: &Path,
    files: Vec<rargo_proto::Delivery>,
    writes: Vec<rargo_proto::Write>,
) -> std::io::Result<(Vec<rargo_proto::Delivery>, Vec<rargo_proto::Write>)> {
    use rargo_proto::Write;
    let inside = |path: &str| -> Option<Vec<u8>> {
        Path::new(path).strip_prefix(dir).ok().filter(|r| !r.as_os_str().is_empty()).map(|r| r.as_os_str().as_bytes().to_vec())
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    fs.begin_delivery();
    let mut rest_writes = Vec::new();
    for w in writes {
        let path = match &w {
            Write::Dir { path } | Write::Remove { path } | Write::Symlink { path, .. } | Write::File { path, .. } | Write::Text { path, .. } => path,
        };
        let Some(rel) = inside(path) else {
            rest_writes.push(w);
            continue;
        };
        match w {
            Write::Dir { .. } => {
                std::fs::create_dir_all(fs.real_for_write(&join_rel(&rel))?)?;
                fs.wrote(&rel);
            }
            Write::Remove { .. } | Write::Text { text: None, .. } => fs.remove(&rel),
            Write::Symlink { target, .. } => {
                let real = fs.real_for_write(&rel)?;
                let _ = std::fs::remove_file(&real);
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), &real)?;
                fs.wrote(&rel);
            }
            Write::Text { text: Some(text), .. } => {
                let real = fs.real_for_write(&rel)?;
                // Unchanged: leave the file (and its mtime) alone.
                if std::fs::read_to_string(&real).ok().as_deref() != Some(text.as_str()) {
                    crate::write_atomic(&real, text.as_bytes())?;
                    fs.wrote(&rel);
                }
            }
            Write::File { hash, size, exec, .. } => {
                if !fs.shows(&rel, &hash) {
                    fs.deliver(&rel, VFile { hash, size, exec, mtime_ns: now });
                }
            }
        }
    }
    let mut rest_files = Vec::new();
    for f in files {
        match inside(&f.path) {
            Some(rel) => {
                if !fs.shows(&rel, &f.hash) {
                    fs.deliver(&rel, VFile { hash: f.hash, size: f.size, exec: f.exec, mtime_ns: now });
                }
            }
            None => rest_files.push(f),
        }
    }
    Ok((rest_files, rest_writes))
}

/// The path of a directory inside the mount, as `real_for_write` wants it
/// (it makes the parents of what it is given).
fn join_rel(rel: &[u8]) -> Vec<u8> {
    let mut p = rel.to_vec();
    p.extend_from_slice(b"/.");
    p
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
    if req.kind == "release" {
        d.release(Path::new(&req.cwd));
        let done = rargo_proto::encode(&LocalReply::Exit(0));
        let _ = wr.write_all(&(done.len() as u32).to_le_bytes()).await;
        let _ = wr.write_all(&done).await;
        return;
    }
    if req.kind == "stop" {
        d.unmount_all();
        d.save_known();
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
            // A daemon that was told to stop lets go of the lock as it
            // exits, after it answered: it is waited for. One that holds
            // it longer is running (or starting) and serves the socket.
            let started = Instant::now();
            // SAFETY: valid descriptor.
            while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                if started.elapsed() > Duration::from_secs(3) {
                    std::process::exit(0);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
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
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).expect("signal handler");
        loop {
            tokio::select! {
                // Told to go: the output directories go back to being
                // plain directories first.
                _ = term.recv() => {
                    d.unmount_all();
                    d.save_known();
                    let _ = std::fs::remove_file(&socket);
                    break;
                }
                _ = int.recv() => {
                    d.unmount_all();
                    d.save_known();
                    let _ = std::fs::remove_file(&socket);
                    break;
                }
                conn = listener.accept() => {
                    if let Ok((stream, _)) = conn {
                        *d.last_active.lock().unwrap() = Instant::now();
                        tokio::spawn(serve_command(d.clone(), stream, socket.clone()));
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // A daemon that shows output directories stays: what
                    // it shows would vanish with it.
                    let showing = d.worktrees.lock().unwrap().values().any(|w| w.out.lock().unwrap().is_some());
                    let quiet = !showing && d.active.load(Ordering::SeqCst) == 0 && d.last_active.lock().unwrap().elapsed() > idle;
                    // Someone removed the socket: nobody can reach us.
                    d.save_known();
                    if quiet || !socket.exists() {
                        d.unmount_all();
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
                            .filter(|(_, w)| w.out.lock().unwrap().is_none() && w.last_used.lock().unwrap().elapsed() > Duration::from_secs(4 * 3600))
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
            d.clone().command(req, tx).await;
            d.save_known();
        });
    });
    while let Some(reply) = rx.blocking_recv() {
        each(reply);
    }
    let _ = worker.join();
}

/// How much of a command's stderr is kept to explain a failure.
const HEARD_MAX: usize = 1 << 20;

/// A note for a failed command whose error output names a path the
/// worktree leaves out because git ignores it: the build server never had
/// it. At most three paths are named.
fn left_out_note(ignored: &scan::Ignored, root: &Path, heard: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(heard);
    let mut named: Vec<String> = Vec::new();
    for rel in paths_in(&text, &root.to_string_lossy()) {
        if named.len() == 3 {
            break;
        }
        if ignored.left_out(rel.as_bytes()) && !named.contains(&rel) {
            named.push(rel);
        }
    }
    if named.is_empty() {
        return None;
    }
    let one = named.len() == 1;
    let list = named.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ");
    let (is, it) = if one { ("is", "it") } else { ("are", "them") };
    Some(format!(
        "note: {list} {is} not on the build server: git ignores {it}, and the project's `metadata.rargo.sync` says \
         `gitignored = false`.\n      To send {it}, name {it} in `include` there.\n"
    ))
}

/// The paths in `text` that lie in the worktree at `root`, from its root,
/// with `.` and `..` resolved: absolute ones under the root, and relative
/// ones taken from the root.
fn paths_in<'t>(text: &'t str, root: &'t str) -> impl Iterator<Item = String> + 't {
    let path_char = |c: char| c.is_alphanumeric() || "._-/+@".contains(c);
    let rooted = format!("{}/", root.trim_end_matches('/'));
    text.split(move |c: char| !path_char(c)).filter_map(move |token| {
        let token = token.trim_end_matches('.');
        let rel = match token.strip_prefix(rooted.as_str()) {
            Some(rel) => rel,
            None if token.starts_with('/') || !token.contains(['/', '.']) => return None,
            None => token,
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in rel.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                p => parts.push(p),
            }
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    })
}

#[cfg(test)]
mod left_out_tests {
    use super::*;

    #[test]
    fn a_failure_that_names_a_path_git_ignores_is_explained() {
        let root = "/home/ann/proj";
        let found = |text: &str| paths_in(text, root).collect::<Vec<_>>();
        // As rustc and build scripts write paths.
        assert_eq!(found("error: couldn't read `/home/ann/proj/crates/util/src/../local-note.txt`: No such file"), ["crates/util/local-note.txt"]);
        assert_eq!(found("open data/big.bin: not found."), ["data/big.bin"]);
        assert_eq!(found("at /home/ann/proj/src/main.rs:12:5"), ["src/main.rs"]);
        // Not elsewhere, and not above the root.
        assert!(found("couldn't read `/elsewhere/data/big.bin` or /home/ann/proj/../x").is_empty());

        let ignored = scan::Ignored::for_test(&["junk/", "crates/util/local-note.txt"], &[]);
        let failed = b"error: couldn't read `/home/ann/proj/crates/util/src/../local-note.txt`: No such file or directory\n";
        let note = left_out_note(&ignored, Path::new(root), failed).unwrap();
        assert!(
            note.starts_with("note: `crates/util/local-note.txt` is not on the build server: git ignores it")
                && note.contains("gitignored = false")
                && note.contains("`include`"),
            "{note}"
        );
        let note = left_out_note(&ignored, Path::new(root), b"cannot open /home/ann/proj/junk/blob and local-note.txt\n").unwrap();
        assert!(note.starts_with("note: `junk/blob` is not"), "a file inside an ignored directory: {note}");
        assert!(left_out_note(&ignored, Path::new(root), b"error[E0308]: mismatched types\n  --> src/main.rs:3:5\n").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_link_that_cannot_be_made_is_an_unreachable_server() {
        use std::io::{Error, ErrorKind};
        let refused = no_link(Error::new(ErrorKind::ConnectionRefused, "connection refused"));
        assert!(is_no_link(&refused));
        assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);
        assert_eq!(refused.to_string(), "connection refused");
        // A missing local file is reported as itself.
        assert!(!is_no_link(&Error::from_raw_os_error(libc::ENOENT)));
        // A server that answers and turns the token down is not out of reach.
        assert!(!is_no_link(&no_link(Error::new(ErrorKind::PermissionDenied, rargo_proto::REFUSED))));
        // Through `?` the mark stays.
        let passed = (|| -> std::io::Result<()> { Err(no_link(Error::new(ErrorKind::TimedOut, "timed out")))? })();
        assert!(is_no_link(&passed.unwrap_err()));
    }
}
