//! Workers: running actions on other machines.
//!
//! An action is a command, a view and a filesystem served on demand. On a
//! worker the view is a [`RemoteView`]: answers the server sent ahead
//! (predicted from the action's previous trace) are used directly, anything
//! else is asked for over the session. File content travels as chunks, and
//! only chunks the receiver does not already hold. The worker traces the
//! action exactly as the server would and sends the trace and outputs back.
//!
//! The server keeps all the state that matters: analysis, cache, scheduling.
//! A worker has a content cache and nothing else to lose.

use crate::chunk::{self, ChunkIndex, ChunkRef, Recipe};
use crate::engine::{Engine, Live, Observer, RemoteRunner, Request};
use crate::hash::Hash;
use crate::net::{Conn, Endpoint};
use crate::sandbox::NetMode;
use crate::store::Store;
use crate::trace::*;
use crate::view::*;
use anyhow::{Context, Result, anyhow, bail};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WLooked {
    Absent,
    Other,
    Dir { mode: u32, own: bool },
    Symlink(Vec<u8>),
    File { size: u64, mode: u32, own: bool, hash: Option<Hash>, sem: Option<Hash> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WSpec {
    pub kind: ActionKind,
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub cwd: Vec<u8>,
    pub mkdirs: Vec<Vec<u8>>,
    pub discard: Vec<Vec<u8>>,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WDone {
    pub status: i32,
    pub stats: Stats,
    pub obs: Vec<(VPath, Obs)>,
    pub entries: Vec<(Vec<u8>, MEntry)>,
    /// What changed in the directories kept on the side (see `ActionSpec::discard`).
    side: Vec<(Vec<u8>, MEntry)>,
    pub tainted: bool,
}

#[derive(Serialize, Deserialize)]
pub enum Req {
    Lookup { action: u64, dir: Vec<u8>, name: Vec<u8> },
    List { action: u64, dir: Vec<u8> },
    /// The content hash of a file whose hash was not sent with the lookup.
    Open { action: u64, dir: Vec<u8>, name: Vec<u8> },
    Recipe { blob: Hash },
    Chunks { blob: Hash, idx: Vec<u32> },
    /// Recipe plus, in the same reply, as many of the chunks the asker
    /// lacks as fit: one round trip for a small file.
    Blob { blob: Hash },
}

#[derive(Serialize, Deserialize)]
pub enum Resp {
    Looked(WLooked),
    Listing(Result<Vec<(Vec<u8>, u8)>, i32>),
    Opened(Result<Hash, i32>),
    Recipe(Option<Recipe>),
    Chunks(Vec<Vec<u8>>),
    Blob(Option<(Recipe, Vec<(u32, Vec<u8>)>)>),
    Err(String),
}

#[derive(Serialize, Deserialize)]
pub enum Msg {
    Hello { cores: u32, blobs: Vec<Hash> },
    Welcome,
    Req { rid: u64, body: Req },
    Resp { rid: u64, body: Resp },
    Run {
        action: u64,
        spec: WSpec,
        uid: u32,
        gid: u32,
        /// Answers to lookups the action is expected to make.
        prefill: Vec<(Vec<u8>, Vec<u8>, WLooked)>,
        listings: Vec<(Vec<u8>, Vec<(Vec<u8>, u8)>)>,
        untraced: Vec<Vec<u8>>,
        /// Blobs the server is about to push.
        incoming: Vec<Hash>,
    },
    /// Part of a blob the worker is expected to need: its recipe and the
    /// chunks the server believes the worker lacks.
    Push { blob: Hash, recipe: Recipe, chunks: Vec<(u32, Vec<u8>)>, last: bool },
    Stdout { action: u64, data: Vec<u8> },
    StderrLine { action: u64, line: Vec<u8> },
    /// A file the action finished early (an `.rmeta`).
    Early { action: u64, path: Vec<u8>, entry: MEntry },
    Done { action: u64, result: Result<WDone, String> },
}

fn dtype_to_u8(t: DType) -> u8 {
    match t {
        DType::Dir => 0,
        DType::File => 1,
        DType::Symlink => 2,
        DType::Other => 3,
    }
}

fn dtype_from_u8(b: u8) -> DType {
    match b {
        0 => DType::Dir,
        1 => DType::File,
        2 => DType::Symlink,
        _ => DType::Other,
    }
}

// ---------------------------------------------------------------------------
// RPC over a connection
// ---------------------------------------------------------------------------

/// Requests, replies and one-way messages over one [`Conn`].
pub struct Peer {
    pub conn: Arc<Conn>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Resp>>>,
    next: AtomicU64,
    out: mpsc::UnboundedSender<Vec<u8>>,
}

impl Peer {
    /// Wraps a connection. Messages other than replies are delivered to the
    /// returned receiver.
    pub fn new(conn: Arc<Conn>) -> (Arc<Peer>, mpsc::UnboundedReceiver<Msg>) {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let peer = Arc::new(Peer { conn: conn.clone(), pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1), out: out_tx });
        let c = conn.clone();
        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                let m = if m.len() >= COMPRESS_FROM { tokio::task::block_in_place(|| frame(m)) } else { frame(m) };
                if c.send(&m).await.is_err() {
                    break;
                }
            }
        });
        let p = peer.clone();
        tokio::spawn(async move {
            while let Some(raw) = conn.recv().await {
                let Some(raw) = unframe(&raw) else { continue };
                let Ok(msg) = bincode::deserialize::<Msg>(&raw) else { continue };
                match msg {
                    Msg::Resp { rid, body } => {
                        if let Some(tx) = p.pending.lock().remove(&rid) {
                            let _ = tx.send(body);
                        }
                    }
                    other => {
                        if in_tx.send(other).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        (peer, in_rx)
    }

    /// Sends a message without waiting.
    pub fn post(&self, m: &Msg) {
        if let Ok(b) = bincode::serialize(m) {
            let _ = self.out.send(b);
        }
    }

    fn start(&self, body: Req) -> oneshot::Receiver<Resp> {
        let rid = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(rid, tx);
        self.post(&Msg::Req { rid, body });
        rx
    }

    pub async fn call(&self, body: Req) -> Result<Resp> {
        match tokio::time::timeout(Duration::from_secs(120), self.start(body)).await {
            Ok(Ok(Resp::Err(e))) => bail!("peer: {e}"),
            Ok(Ok(r)) => Ok(r),
            Ok(Err(_)) => bail!("connection to peer lost"),
            Err(_) => bail!("peer did not answer"),
        }
    }

    /// For threads outside the async runtime (FUSE workers).
    pub fn call_blocking(&self, body: Req) -> Result<Resp> {
        let rx = self.start(body);
        // Usually called from FUSE threads; when called from an async
        // worker thread, that thread has to be handed over first.
        let got = if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| rx.blocking_recv())
        } else {
            rx.blocking_recv()
        };
        match got {
            Ok(Resp::Err(e)) => bail!("peer: {e}"),
            Ok(r) => Ok(r),
            Err(_) => bail!("connection to peer lost"),
        }
    }
}

// ---------------------------------------------------------------------------
// Blob transfer by chunk
// ---------------------------------------------------------------------------

/// Where the bytes of a blob can be read from on this side.
pub trait BlobSource: Send + Sync {
    fn open(&self, h: &Hash) -> io::Result<std::fs::File>;
}

struct StoreSource(Arc<Store>);
impl BlobSource for StoreSource {
    fn open(&self, h: &Hash) -> io::Result<std::fs::File> {
        self.0.open_blob(h)
    }
}

/// The chunks of `recipe` to send along with it: those the asker is not
/// known to have, up to about a megabyte.
fn inline_indices(recipe: &Recipe, has: &dyn Fn(&Hash) -> bool) -> Vec<u32> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for (i, c) in recipe.iter().enumerate() {
        if has(&c.hash) {
            continue;
        }
        if bytes + c.len as usize > BATCH_BYTES && !out.is_empty() {
            break;
        }
        bytes += c.len as usize;
        out.push(i as u32);
    }
    out
}

/// Shared by both ends: what we can serve and what we already hold.
pub struct Blobs {
    pub store: Arc<Store>,
    pub index: ChunkIndex,
    source: Box<dyn BlobSource>,
    pub fetched_bytes: AtomicU64,
    pub reused_bytes: AtomicU64,
    pub fetched_blobs: AtomicU64,
}

const BATCH_BYTES: usize = 1 << 20;

impl Blobs {
    pub fn new(store: Arc<Store>, source: Option<Box<dyn BlobSource>>) -> Blobs {
        let source = source.unwrap_or_else(|| Box::new(StoreSource(store.clone())));
        Blobs {
            store,
            index: ChunkIndex::default(),
            source,
            fetched_bytes: AtomicU64::new(0),
            reused_bytes: AtomicU64::new(0),
            fetched_blobs: AtomicU64::new(0),
        }
    }

    /// The recipe of a blob we can serve.
    pub fn recipe(&self, h: &Hash) -> Option<Arc<Recipe>> {
        if let Some(r) = self.index.recipe(h) {
            return Some(r);
        }
        let f = self.source.open(h).ok()?;
        let r = Arc::new(chunk::chunk_file(&f).ok()?);
        self.index.add_blob(*h, r.clone());
        Some(r)
    }

    pub fn serve_chunks(&self, h: &Hash, idx: &[u32]) -> Result<Vec<Vec<u8>>> {
        let recipe = self.recipe(h).ok_or_else(|| anyhow!("unknown blob {h}"))?;
        let f = self.source.open(h)?;
        let mut offsets = Vec::with_capacity(recipe.len());
        let mut off = 0u64;
        for c in recipe.iter() {
            offsets.push(off);
            off += c.len as u64;
        }
        idx.iter()
            .map(|&i| {
                let c = recipe.get(i as usize).ok_or_else(|| anyhow!("chunk index out of range"))?;
                Ok(chunk::read_chunk(&f, offsets[i as usize], c.len)?)
            })
            .collect()
    }

    /// A chunk we hold locally, if any.
    fn local_chunk(&self, c: &ChunkRef) -> Option<Vec<u8>> {
        let (blob, off, len) = self.index.locate(&c.hash)?;
        let f = self.source.open(&blob).ok()?;
        match chunk::read_chunk(&f, off, len) {
            Ok(b) if Hash::of(&b) == c.hash => Some(b),
            _ => {
                self.index.forget_chunk(&c.hash);
                None
            }
        }
    }

    /// Puts a blob together from its recipe: chunks given in `have`, chunks
    /// found locally, and the rest requested from the peer.
    pub async fn assemble(
        &self,
        peer: &Peer,
        h: &Hash,
        recipe: &Recipe,
        mut have: HashMap<u32, Vec<u8>>,
    ) -> Result<()> {
        if self.store.has(h) {
            return Ok(());
        }
        let tmp = self.store.tmp_path();
        let file = std::fs::File::create(&tmp)?;
        let mut off = 0u64;
        let mut missing: Vec<(u32, u64, u32)> = Vec::new();
        for (i, c) in recipe.iter().enumerate() {
            let i = i as u32;
            if let Some(b) = have.remove(&i) {
                self.fetched_bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
                file.write_all_at(&b, off)?;
            } else if let Some(b) = self.local_chunk(c) {
                self.reused_bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
                file.write_all_at(&b, off)?;
            } else {
                missing.push((i, off, c.len));
            }
            off += c.len as u64;
        }
        // Ask for what is still missing, a megabyte at a time, several
        // requests in flight.
        let mut batches: Vec<Vec<(u32, u64, u32)>> = Vec::new();
        let mut cur: Vec<(u32, u64, u32)> = Vec::new();
        let mut cur_bytes = 0usize;
        for m in missing {
            if cur_bytes + m.2 as usize > BATCH_BYTES && !cur.is_empty() {
                batches.push(std::mem::take(&mut cur));
                cur_bytes = 0;
            }
            cur_bytes += m.2 as usize;
            cur.push(m);
        }
        if !cur.is_empty() {
            batches.push(cur);
        }
        for group in batches.chunks(8) {
            let calls: Vec<_> = group
                .iter()
                .map(|b| peer.call(Req::Chunks { blob: *h, idx: b.iter().map(|x| x.0).collect() }))
                .collect();
            for (batch, call) in group.iter().zip(calls) {
                let Resp::Chunks(chunks) = call.await? else { bail!("unexpected reply to a chunk request") };
                if chunks.len() != batch.len() {
                    bail!("peer sent the wrong number of chunks");
                }
                for ((_, off, len), data) in batch.iter().zip(chunks) {
                    if data.len() != *len as usize {
                        bail!("peer sent a chunk of the wrong size");
                    }
                    self.fetched_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    file.write_all_at(&data, *off)?;
                }
            }
        }
        drop(file);
        // The whole must hash to what was asked for, whatever the pieces
        // claimed to be.
        let (got, size) = Hash::of_path(&tmp)?;
        if got != *h {
            let _ = std::fs::remove_file(&tmp);
            bail!("blob {h} arrived with the wrong content");
        }
        self.store.install_hashed(&tmp, got, size)?;
        self.index.add_blob(*h, Arc::new(recipe.clone()));
        self.fetched_blobs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Fetches a blob from the peer unless it is already here.
    pub async fn fetch(&self, peer: &Peer, h: &Hash) -> Result<()> {
        if self.store.has(h) {
            return Ok(());
        }
        let Resp::Blob(Some((recipe, chunks))) = peer.call(Req::Blob { blob: *h }).await? else {
            bail!("peer does not have blob {h}");
        };
        self.assemble(peer, h, &recipe, chunks.into_iter().collect()).await
    }
}

// ---------------------------------------------------------------------------
// Worker side
// ---------------------------------------------------------------------------

struct Incoming {
    recipe: Recipe,
    chunks: HashMap<u32, Vec<u8>>,
}

pub struct WorkerState {
    peer: Arc<Peer>,
    blobs: Arc<Blobs>,
    engine: Arc<Engine>,
    handle: tokio::runtime::Handle,
    /// Blobs announced by the server and not complete yet.
    announced: Mutex<HashMap<Hash, tokio::sync::watch::Receiver<bool>>>,
    announce_tx: Mutex<HashMap<Hash, tokio::sync::watch::Sender<bool>>>,
    partial: Mutex<HashMap<Hash, Incoming>>,
    pub lookups_local: AtomicU64,
    pub lookups_remote: AtomicU64,
}

impl WorkerState {
    /// Makes sure blob `h` is in the local store. Callable from any thread
    /// that is not an async worker thread.
    fn ensure_blob(self: &Arc<Self>, h: &Hash) -> io::Result<()> {
        if self.blobs.store.has(h) {
            return Ok(());
        }
        let this = self.clone();
        let h = *h;
        let fut = async move {
                // If the server is already pushing it, wait for that.
                let waiting = this.announced.lock().get(&h).cloned();
                if let Some(mut rx) = waiting {
                    let _ = tokio::time::timeout(Duration::from_secs(60), async {
                        while !*rx.borrow() {
                            if rx.changed().await.is_err() {
                                break;
                            }
                        }
                    })
                    .await;
                    if this.blobs.store.has(&h) {
                        return Ok(());
                    }
                }
                this.blobs.fetch(&this.peer, &h).await
        };
        let r = if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| self.handle.block_on(fut))
        } else {
            self.handle.block_on(fut)
        };
        r.map_err(|e| io::Error::other(format!("{e:#}")))
    }

    async fn on_push(self: &Arc<Self>, blob: Hash, recipe: Recipe, chunks: Vec<(u32, Vec<u8>)>, last: bool) {
        let complete = {
            let mut partial = self.partial.lock();
            let inc = partial.entry(blob).or_insert_with(|| Incoming { recipe: recipe.clone(), chunks: HashMap::new() });
            inc.chunks.extend(chunks);
            if last { partial.remove(&blob) } else { None }
        };
        if let Some(inc) = complete {
            if let Err(e) = self.blobs.assemble(&self.peer, &blob, &inc.recipe, inc.chunks).await {
                eprintln!("rargo worker: pushed blob {blob} could not be assembled: {e:#}");
            }
            self.announced.lock().remove(&blob);
            if let Some(tx) = self.announce_tx.lock().remove(&blob) {
                let _ = tx.send(true);
            }
        }
    }
}

struct RemoteFile {
    state: Arc<WorkerState>,
    action: u64,
    dir: Vec<u8>,
    name: Vec<u8>,
    hash: std::sync::OnceLock<Hash>,
}

impl LazyBlob for RemoteFile {
    fn materialize(&self, _store: &Store) -> io::Result<Hash> {
        let h = match self.hash.get() {
            Some(h) => *h,
            None => {
                let r = self
                    .state
                    .peer
                    .call_blocking(Req::Open { action: self.action, dir: self.dir.clone(), name: self.name.clone() })
                    .map_err(|e| io::Error::other(format!("{e:#}")))?;
                let Resp::Opened(Ok(h)) = r else {
                    return Err(io::Error::from_raw_os_error(match r {
                        Resp::Opened(Err(errno)) => errno,
                        _ => libc::EIO,
                    }));
                };
                let _ = self.hash.set(h);
                h
            }
        };
        self.state.ensure_blob(&h)?;
        Ok(h)
    }
}

/// The view of an action running on a worker.
pub struct RemoteView {
    state: Arc<WorkerState>,
    action: u64,
    entries: DashMap<(Vec<u8>, Vec<u8>), WLooked>,
    listings: DashMap<Vec<u8>, Listing>,
    untraced: Vec<Vec<u8>>,
}

impl RemoteView {
    fn convert(&self, dir: &[u8], name: &[u8], w: &WLooked) -> Looked {
        let owner = |mode: u32, own: bool| {
            if own { Owner::own(mode) } else { Owner { uid: 0, gid: 0, mode } }
        };
        match w {
            WLooked::Absent => Looked::Absent,
            WLooked::Other => Looked::Other,
            WLooked::Dir { mode, own } => Looked::Dir(owner(*mode, *own)),
            WLooked::Symlink(t) => Looked::Symlink(t.clone()),
            WLooked::File { size, mode, own, hash, sem } => Looked::File(FileInfo {
                size: *size,
                sem: *sem,
                owner: owner(*mode, *own),
                src: match hash {
                    Some(h) => FileSrc::Blob(*h),
                    None => FileSrc::Lazy(Arc::new(RemoteFile {
                        state: self.state.clone(),
                        action: self.action,
                        dir: dir.to_vec(),
                        name: name.to_vec(),
                        hash: std::sync::OnceLock::new(),
                    })),
                },
            }),
        }
    }
}

impl View for RemoteView {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
        let key = (dir.to_vec(), name.to_vec());
        if let Some(w) = self.entries.get(&key) {
            self.state.lookups_local.fetch_add(1, Ordering::Relaxed);
            return self.convert(dir, name, &w);
        }
        self.state.lookups_remote.fetch_add(1, Ordering::Relaxed);
        let w = match self.state.peer.call_blocking(Req::Lookup { action: self.action, dir: dir.to_vec(), name: name.to_vec() }) {
            Ok(Resp::Looked(w)) => w,
            _ => WLooked::Absent,
        };
        let l = self.convert(dir, name, &w);
        self.entries.insert(key, w);
        l
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        if let Some(l) = self.listings.get(dir) {
            return Ok(l.clone());
        }
        match self.state.peer.call_blocking(Req::List { action: self.action, dir: dir.to_vec() }) {
            Ok(Resp::Listing(Ok(l))) => {
                let l: Listing = l.into_iter().map(|(n, t)| (n, dtype_from_u8(t))).collect();
                self.listings.insert(dir.to_vec(), l.clone());
                Ok(l)
            }
            Ok(Resp::Listing(Err(errno))) => Err(io::Error::from_raw_os_error(errno)),
            _ => Err(io::Error::from_raw_os_error(libc::EIO)),
        }
    }

    fn traced(&self, dir: &[u8]) -> bool {
        !self.untraced.iter().any(|u| vpath_starts_with(dir, u))
    }
}

struct WorkerObserver {
    peer: Arc<Peer>,
    action: u64,
}

impl Observer for WorkerObserver {
    fn stdout(&self, data: &[u8]) {
        self.peer.post(&Msg::Stdout { action: self.action, data: data.to_vec() });
    }

    fn stderr_line(&self, line: &[u8], live: Option<&dyn Live>) {
        // An rmeta the compiler has finished is stored right away so the
        // server can hand it to dependents before the compile ends.
        if let (Some(live), true) = (live, line.starts_with(b"{")) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                if let Some(artifact) = v.get("artifact").and_then(|a| a.as_str()) {
                    if artifact.ends_with(".rmeta") {
                        if let Some(entry) = live.ingest(artifact.as_bytes()) {
                            self.peer.post(&Msg::Early { action: self.action, path: artifact.as_bytes().to_vec(), entry });
                        }
                    }
                }
            }
        }
        self.peer.post(&Msg::StderrLine { action: self.action, line: line.to_vec() });
    }
}

/// Runs a worker: connects to the server and executes what it is sent.
pub async fn worker_main(engine: Arc<Engine>, server: SocketAddr, cores: u32, key: crate::net::LinkKey) -> Result<()> {
    let endpoint = Endpoint::bind_keyed("0.0.0.0:0".parse().unwrap(), false, key).await?;
    let conn = endpoint.connect(server);
    let (peer, mut inbox) = Peer::new(conn.clone());
    let blobs = Arc::new(Blobs::new(engine.store.clone(), None));
    let state = Arc::new(WorkerState {
        peer: peer.clone(),
        blobs: blobs.clone(),
        engine: engine.clone(),
        handle: tokio::runtime::Handle::current(),
        announced: Mutex::new(HashMap::new()),
        announce_tx: Mutex::new(HashMap::new()),
        partial: Mutex::new(HashMap::new()),
        lookups_local: AtomicU64::new(0),
        lookups_remote: AtomicU64::new(0),
    });
    let s2 = state.clone();
    engine.content.set_fetcher(Arc::new(move |h| s2.ensure_blob(h)));
    peer.post(&Msg::Hello { cores, blobs: engine.store.all_hashes() });
    eprintln!("rargo worker: connected to {server}, {cores} cores");
    loop {
        let msg = tokio::select! {
            m = inbox.recv() => m,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                if conn.is_closed() { None } else { continue }
            }
        };
        let Some(msg) = msg else { bail!("server connection closed") };
        match msg {
            Msg::Welcome => {}
            Msg::Req { rid, body } => {
                let (peer, blobs) = (peer.clone(), blobs.clone());
                tokio::task::spawn_blocking(move || {
                    let resp = match body {
                        Req::Recipe { blob } => Resp::Recipe(blobs.recipe(&blob).map(|r| (*r).clone())),
                        Req::Blob { blob } => Resp::Blob(blobs.recipe(&blob).map(|r| {
                            let idx = inline_indices(&r, &|_| false);
                            let chunks = blobs.serve_chunks(&blob, &idx).unwrap_or_default();
                            ((*r).clone(), idx.into_iter().zip(chunks).collect())
                        })),
                        Req::Chunks { blob, idx } => match blobs.serve_chunks(&blob, &idx) {
                            Ok(c) => Resp::Chunks(c),
                            Err(e) => Resp::Err(format!("{e:#}")),
                        },
                        _ => Resp::Err("not a worker request".into()),
                    };
                    peer.post(&Msg::Resp { rid, body: resp });
                });
            }
            Msg::Push { blob, recipe, chunks, last } => {
                let s = state.clone();
                tokio::spawn(async move { s.on_push(blob, recipe, chunks, last).await });
            }
            Msg::Run { action, spec, uid, gid, prefill, listings, untraced, incoming } => {
                {
                    let mut ann = state.announced.lock();
                    let mut txs = state.announce_tx.lock();
                    for h in incoming {
                        if !state.blobs.store.has(&h) && !ann.contains_key(&h) {
                            let (tx, rx) = tokio::sync::watch::channel(false);
                            ann.insert(h, rx);
                            txs.insert(h, tx);
                        }
                    }
                }
                let state = state.clone();
                tokio::spawn(async move {
                    let view = Arc::new(RemoteView {
                        state: state.clone(),
                        action,
                        entries: prefill.into_iter().map(|(d, n, w)| ((d, n), w)).collect(),
                        listings: listings
                            .into_iter()
                            .map(|(d, l)| (d, l.into_iter().map(|(n, t)| (n, dtype_from_u8(t))).collect()))
                            .collect(),
                        untraced,
                    });
                    let aspec = ActionSpec {
                        kind: spec.kind,
                        argv: spec.argv,
                        env: spec.env.into_iter().collect::<BTreeMap<_, _>>(),
                        cwd: spec.cwd,
                        net: NetMode::Loopback,
                        mkdirs: spec.mkdirs,
                        root_id: Hash::ZERO,
                        discard: spec.discard,
                        label: spec.label,
                    };
                    // The worker's own scheduler shares its cores between
                    // the actions it was given and their helper threads.
                    let permit = state.engine.sched.acquire(crate::sched::Priority::Agent, 0).await;
                    let obs = WorkerObserver { peer: state.peer.clone(), action };
                    let r = state.engine.execute(&aspec, view, uid, gid, &obs).await;
                    drop(permit);
                    let result = match r {
                        Ok((res, obs, tainted, side)) => Ok(WDone {
                            status: res.status,
                            stats: res.stats,
                            obs,
                            entries: res.outputs.entries.clone(),
                            side,
                            tainted,
                        }),
                        Err(e) => Err(format!("{e:#}")),
                    };
                    state.peer.post(&Msg::Done { action, result });
                });
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

enum Event {
    Stdout(Vec<u8>),
    Line(Vec<u8>),
    Early(Vec<u8>, MEntry),
    Done(Result<WDone, String>),
}

struct RemoteAction {
    view: Arc<dyn View>,
    events: mpsc::UnboundedSender<Event>,
    /// Host files whose content was served; rechecked when the action ends.
    served: Mutex<Vec<FileSrc>>,
    /// Questions the worker asked for this action (for `RARGO_TRACE_REMOTE`).
    asked: AtomicU64,
}

/// Messages bigger than this are compressed (zstd level 1) when that makes
/// them smaller. rustc's outputs shrink several times (a 478 MB rlib with
/// debug info to 81 MB) at a few hundred MB/s, which is much faster than
/// the link.
const COMPRESS_FROM: usize = 4 << 10;

fn frame(b: Vec<u8>) -> Vec<u8> {
    if b.len() >= COMPRESS_FROM {
        if let Ok(z) = zstd::bulk::compress(&b, 1) {
            if z.len() < b.len() {
                let mut out = Vec::with_capacity(z.len() + 9);
                out.push(1);
                out.extend_from_slice(&(b.len() as u64).to_le_bytes());
                out.extend_from_slice(&z);
                return out;
            }
        }
    }
    let mut out = Vec::with_capacity(b.len() + 1);
    out.push(0);
    out.extend_from_slice(&b);
    out
}

fn unframe(raw: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
    match raw.split_first()? {
        (0, rest) => Some(std::borrow::Cow::Borrowed(rest)),
        (1, rest) if rest.len() >= 8 => {
            let len = u64::from_le_bytes(rest[..8].try_into().ok()?) as usize;
            zstd::bulk::decompress(&rest[8..], len).ok().map(std::borrow::Cow::Owned)
        }
        _ => None,
    }
}

/// `RARGO_DISPATCH=eager` in rargod's environment: dispatch whenever no
/// local core is free, without weighing transfer against waiting.
fn eager_dispatch() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RARGO_DISPATCH").ok().as_deref() == Some("eager"))
}

/// `RARGO_TRACE_REMOTE` in rargod's environment: one line per remote
/// action on its stderr, with where the time went.
fn trace_remote() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RARGO_TRACE_REMOTE").is_some())
}

pub struct Worker {
    pub id: u64,
    peer: Arc<Peer>,
    pub cores: usize,
    free: AtomicUsize,
    has_blobs: Mutex<HashSet<Hash>>,
    has_chunks: Mutex<HashSet<Hash>>,
    actions: DashMap<u64, Arc<RemoteAction>>,
    alive: std::sync::atomic::AtomicBool,
    pub pushed_bytes: AtomicU64,
    pub skipped_bytes: AtomicU64,
    /// Moving average, in microseconds, of what running an action here cost
    /// beyond the action itself (questions, pushes, bringing outputs home).
    overhead_us: AtomicU64,
    overhead_samples: AtomicU64,
}

impl Worker {
    fn note_overhead(&self, d: Duration) {
        let us = d.as_micros() as u64;
        let n = self.overhead_samples.fetch_add(1, Ordering::Relaxed);
        let old = self.overhead_us.load(Ordering::Relaxed);
        // The first samples count fully, later ones by a quarter.
        let new = if n == 0 { us } else { (old * 3 + us) / 4 };
        self.overhead_us.store(new, Ordering::Relaxed);
    }

    /// What dispatching costs beyond the action, once measured.
    fn overhead(&self) -> Option<Duration> {
        (self.overhead_samples.load(Ordering::Relaxed) >= 3)
            .then(|| Duration::from_micros(self.overhead_us.load(Ordering::Relaxed)))
    }
}

/// Serves blobs by hash: from the store, or from the host file an action
/// was shown under that hash.
struct ServerSource {
    store: Arc<Store>,
    host: Arc<DashMap<Hash, FileSrc>>,
    content: Arc<Content>,
}

impl BlobSource for ServerSource {
    fn open(&self, h: &Hash) -> io::Result<std::fs::File> {
        if self.store.has(h) {
            return self.store.open_blob(h);
        }
        let src = self.host.get(h).map(|s| s.clone()).ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        let (f, _, _) = self.content.open(&src)?;
        Ok(f)
    }
}

#[derive(Default)]
pub struct PoolStats {
    pub dispatched: AtomicU64,
    pub kept_local_no_wait: AtomicU64,
    pub kept_local_transfer: AtomicU64,
    pub worker_failures: AtomicU64,
    /// Dispatched, then run here because a local core fell idle first.
    pub hedged: AtomicU64,
    pub prefilled_entries: AtomicU64,
    pub lookups_served: AtomicU64,
    pub opens_served: AtomicU64,
}

/// The server's set of workers.
pub struct Pool {
    workers: Mutex<Vec<Arc<Worker>>>,
    blobs: Arc<Blobs>,
    host_srcs: Arc<DashMap<Hash, FileSrc>>,
    content: Arc<Content>,
    next_action: AtomicU64,
    /// Path prefixes whose observations are specific to one crate and are
    /// not borrowed from another action's trace (worktree, cargo home).
    pub profile_exclude: Vec<Vec<u8>>,
    /// Bytes per second assumed for transfer estimates.
    pub bandwidth: AtomicU64,
    pub stats: PoolStats,
}

fn to_wire(l: &Looked, content: &Content) -> WLooked {
    match l {
        Looked::Absent => WLooked::Absent,
        Looked::Other => WLooked::Other,
        Looked::Dir(o) => WLooked::Dir { mode: o.mode, own: o.uid == OWNER_SELF },
        Looked::Symlink(t) => WLooked::Symlink(t.clone()),
        Looked::File(f) => WLooked::File {
            size: f.size,
            mode: f.owner.mode,
            own: f.owner.uid == OWNER_SELF,
            hash: content.known_hash(&f.src),
            sem: f.sem,
        },
    }
}

impl Pool {
    /// Starts listening for workers.
    pub async fn listen(engine: &Arc<Engine>, addr: SocketAddr, key: crate::net::LinkKey) -> Result<Arc<Pool>> {
        let endpoint = Endpoint::bind_keyed(addr, true, key).await.with_context(|| format!("binding {addr}"))?;
        let host_srcs: Arc<DashMap<Hash, FileSrc>> = Arc::new(DashMap::new());
        let blobs = Arc::new(Blobs::new(
            engine.store.clone(),
            Some(Box::new(ServerSource {
                store: engine.store.clone(),
                host: host_srcs.clone(),
                content: engine.content.clone(),
            })),
        ));
        let pool = Arc::new(Pool {
            workers: Mutex::new(Vec::new()),
            blobs,
            host_srcs,
            content: engine.content.clone(),
            // Action ids start somewhere unpredictable, so that messages
            // recorded from an earlier session mean nothing in this one.
            next_action: AtomicU64::new({
                let mut b = [0u8; 8];
                // SAFETY: filling a valid 8-byte buffer.
                unsafe { libc::getrandom(b.as_mut_ptr() as *mut _, 8, 0) };
                u64::from_le_bytes(b) >> 1
            }),
            profile_exclude: vec![
                crate::buildview::WORKTREE_AT.to_vec(),
                crate::buildview::CARGO_HOME_AT.to_vec(),
            ],
            bandwidth: AtomicU64::new(100 << 20),
            stats: PoolStats::default(),
        });
        let p = pool.clone();
        tokio::spawn(async move {
            while let Some(conn) = endpoint.accept().await {
                let p = p.clone();
                tokio::spawn(async move { p.serve_worker(conn).await });
            }
        });
        Ok(pool)
    }

    pub fn worker_count(&self) -> usize {
        self.workers.lock().iter().filter(|w| w.alive.load(Ordering::Relaxed)).count()
    }

    /// `(pushed, skipped)` bytes over all workers: what was sent ahead and
    /// what did not need sending because the worker already had it.
    pub fn transfer_totals(&self) -> (u64, u64, u64, u64) {
        let ws = self.workers.lock();
        (
            ws.iter().map(|w| w.pushed_bytes.load(Ordering::Relaxed)).sum(),
            ws.iter().map(|w| w.skipped_bytes.load(Ordering::Relaxed)).sum(),
            self.blobs.fetched_bytes.load(Ordering::Relaxed),
            self.blobs.reused_bytes.load(Ordering::Relaxed),
        )
    }

    async fn serve_worker(self: Arc<Self>, conn: Arc<Conn>) {
        let (peer, mut inbox) = Peer::new(conn.clone());
        let mut worker: Option<Arc<Worker>> = None;
        loop {
            let msg = tokio::select! {
                m = inbox.recv() => m,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {
                    // A worker with actions outstanding that has been silent
                    // for a while is gone.
                    let busy = worker.as_ref().is_some_and(|w| !w.actions.is_empty());
                    if busy && conn.idle() > Duration::from_secs(30) { None } else { continue }
                }
            };
            let Some(msg) = msg else { break };
            match msg {
                Msg::Hello { cores, blobs } => {
                    let w = Arc::new(Worker {
                        id: conn.id,
                        peer: peer.clone(),
                        cores: cores as usize,
                        free: AtomicUsize::new(cores as usize),
                        has_blobs: Mutex::new(blobs.into_iter().collect()),
                        has_chunks: Mutex::new(HashSet::new()),
                        actions: DashMap::new(),
                        alive: std::sync::atomic::AtomicBool::new(true),
                        pushed_bytes: AtomicU64::new(0),
                        skipped_bytes: AtomicU64::new(0),
                        overhead_us: AtomicU64::new(0),
                        overhead_samples: AtomicU64::new(0),
                    });
                    eprintln!("rargod: worker {:x} joined from {} with {cores} cores", conn.id, conn.peer());
                    self.workers.lock().push(w.clone());
                    worker = Some(w);
                    peer.post(&Msg::Welcome);
                }
                Msg::Req { rid, body } => {
                    let Some(w) = worker.clone() else { continue };
                    let pool = self.clone();
                    let peer = peer.clone();
                    tokio::task::spawn_blocking(move || {
                        let resp = pool.answer(&w, body);
                        peer.post(&Msg::Resp { rid, body: resp });
                    });
                }
                Msg::Stdout { action, data } => self.event(&worker, action, Event::Stdout(data)),
                Msg::StderrLine { action, line } => self.event(&worker, action, Event::Line(line)),
                Msg::Early { action, path, entry } => self.event(&worker, action, Event::Early(path, entry)),
                Msg::Done { action, result } => self.event(&worker, action, Event::Done(result)),
                _ => {}
            }
        }
        if let Some(w) = worker {
            eprintln!("rargod: worker {:x} is gone", w.id);
            w.alive.store(false, Ordering::SeqCst);
            // Whatever it was running is rerun locally.
            for a in w.actions.iter() {
                let _ = a.events.send(Event::Done(Err("worker connection lost".into())));
            }
            self.workers.lock().retain(|x| x.id != w.id);
        }
    }

    fn event(&self, worker: &Option<Arc<Worker>>, action: u64, ev: Event) {
        if let Some(a) = worker.as_ref().and_then(|w| w.actions.get(&action)) {
            let _ = a.events.send(ev);
        }
    }

    /// Answers a worker's request (runs on a blocking thread).
    fn answer(&self, w: &Worker, body: Req) -> Resp {
        match body {
            Req::Lookup { action, dir, name } => {
                let Some(a) = w.actions.get(&action).map(|a| a.clone()) else { return Resp::Err("no such action".into()) };
                self.stats.lookups_served.fetch_add(1, Ordering::Relaxed);
                a.asked.fetch_add(1, Ordering::Relaxed);
                let looked = a.view.lookup(&dir, &name);
                let wire = to_wire(&looked, &self.content);
                // If the answer names a hash, the worker may ask for those
                // bytes; for a host file we must remember where they are.
                if let (Looked::File(f), WLooked::File { hash: Some(h), .. }) = (&looked, &wire) {
                    self.remember_src(&a, *h, &f.src);
                }
                Resp::Looked(wire)
            }
            Req::List { action, dir } => {
                let Some(a) = w.actions.get(&action).map(|a| a.clone()) else { return Resp::Err("no such action".into()) };
                Resp::Listing(
                    a.view
                        .list(&dir)
                        .map(|l| l.into_iter().map(|(n, t)| (n, dtype_to_u8(t))).collect())
                        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO)),
                )
            }
            Req::Open { action, dir, name } => {
                let Some(a) = w.actions.get(&action).map(|a| a.clone()) else { return Resp::Err("no such action".into()) };
                self.stats.opens_served.fetch_add(1, Ordering::Relaxed);
                a.asked.fetch_add(1, Ordering::Relaxed);
                let Looked::File(f) = a.view.lookup(&dir, &name) else {
                    return Resp::Opened(Err(libc::ENOENT));
                };
                match self.content.hash(&f.src) {
                    Ok(h) => {
                        self.remember_src(&a, h, &f.src);
                        Resp::Opened(Ok(h))
                    }
                    Err(e) => Resp::Opened(Err(e.raw_os_error().unwrap_or(libc::EIO))),
                }
            }
            Req::Recipe { blob } => {
                let r = self.blobs.recipe(&blob).map(|r| (*r).clone());
                if let Some(r) = &r {
                    // The worker is about to hold these chunks.
                    w.has_chunks.lock().extend(r.iter().map(|c| c.hash));
                    w.has_blobs.lock().insert(blob);
                }
                Resp::Recipe(r)
            }
            Req::Chunks { blob, idx } => match self.blobs.serve_chunks(&blob, &idx) {
                Ok(c) => Resp::Chunks(c),
                Err(e) => Resp::Err(format!("{e:#}")),
            },
            Req::Blob { blob } => {
                let Some(r) = self.blobs.recipe(&blob) else { return Resp::Blob(None) };
                let idx = {
                    let has = w.has_chunks.lock();
                    inline_indices(&r, &|h| has.contains(h))
                };
                let chunks = self.blobs.serve_chunks(&blob, &idx).unwrap_or_default();
                w.has_chunks.lock().extend(r.iter().map(|c| c.hash));
                w.has_blobs.lock().insert(blob);
                Resp::Blob(Some(((*r).clone(), idx.into_iter().zip(chunks).collect())))
            }
        }
    }

    fn remember_src(&self, a: &RemoteAction, h: Hash, src: &FileSrc) {
        if let FileSrc::Host { .. } = src {
            self.host_srcs.entry(h).or_insert_with(|| src.clone());
            a.served.lock().push(src.clone());
        }
    }

    /// Picks a worker with a free core.
    fn lease(&self) -> Option<Arc<Worker>> {
        let ws = self.workers.lock();
        let mut best: Option<&Arc<Worker>> = None;
        for w in ws.iter().filter(|w| w.alive.load(Ordering::Relaxed)) {
            if w.free.load(Ordering::Relaxed) > 0 && best.is_none_or(|b| w.free.load(Ordering::Relaxed) > b.free.load(Ordering::Relaxed)) {
                best = Some(w);
            }
        }
        let w = best?.clone();
        // Racy by design: a brief oversubscription just queues on the
        // worker's own scheduler.
        w.free.fetch_sub(1, Ordering::Relaxed);
        Some(w)
    }

    /// Sends a blob the worker is expected to need, leaving out the chunks
    /// it is known to hold.
    fn push_blob(&self, w: &Worker, h: Hash) {
        let Some(recipe) = self.blobs.recipe(&h) else { return };
        let missing: Vec<u32> = {
            let has = w.has_chunks.lock();
            (0..recipe.len() as u32).filter(|i| !has.contains(&recipe[*i as usize].hash)).collect()
        };
        let total: u64 = recipe.iter().map(|c| c.len as u64).sum();
        let need: u64 = missing.iter().map(|i| recipe[*i as usize].len as u64).sum();
        w.pushed_bytes.fetch_add(need, Ordering::Relaxed);
        w.skipped_bytes.fetch_add(total - need, Ordering::Relaxed);
        let mut batch: Vec<u32> = Vec::new();
        let mut bytes = 0usize;
        let mut sent_any = false;
        let mut flush = |batch: &mut Vec<u32>, last: bool| {
            let chunks = match self.blobs.serve_chunks(&h, batch) {
                Ok(c) => batch.iter().copied().zip(c).collect(),
                Err(_) => Vec::new(),
            };
            w.peer.post(&Msg::Push { blob: h, recipe: if sent_any { Vec::new() } else { (*recipe).clone() }, chunks, last });
            sent_any = true;
            batch.clear();
        };
        for i in missing {
            if bytes + recipe[i as usize].len as usize > BATCH_BYTES && !batch.is_empty() {
                flush(&mut batch, false);
                bytes = 0;
            }
            bytes += recipe[i as usize].len as usize;
            batch.push(i);
        }
        flush(&mut batch, true);
        w.has_chunks.lock().extend(recipe.iter().map(|c| c.hash));
        w.has_blobs.lock().insert(h);
    }
}

/// Early outputs of a remote action, for the unit that is waiting on them.
struct RemoteLive<'a> {
    early: &'a Mutex<HashMap<Vec<u8>, MEntry>>,
    pool: &'a Pool,
    worker: &'a Worker,
}

impl Live for RemoteLive<'_> {
    fn ingest(&self, path: &[u8]) -> Option<MEntry> {
        let entry = self.early.lock().get(path).cloned()?;
        if let MEntry::File { hash, .. } = &entry {
            // Dependents may run here; the file has to be in our store.
            let (pool, worker, hash) = (self.pool, self.worker, *hash);
            let ok = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(pool.blobs.fetch(&worker.peer, &hash)).is_ok()
            });
            if !ok {
                return None;
            }
        }
        Some(entry)
    }
}

type RunResult = Result<(ActionResult, Vec<(VPath, Obs)>, bool, Vec<(Vec<u8>, MEntry)>)>;

impl Pool {
    async fn run_on(
        &self,
        w: &Arc<Worker>,
        engine: &Engine,
        spec: &ActionSpec,
        key: &Hash,
        req: &Request,
        spoke: &std::sync::atomic::AtomicBool,
    ) -> RunResult {
        let action = self.next_action.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t0 = std::time::Instant::now();
        let ra = Arc::new(RemoteAction {
            view: req.view.clone(),
            events: tx,
            served: Mutex::new(Vec::new()),
            asked: AtomicU64::new(0),
        });
        w.actions.insert(action, ra.clone());
        struct Cleanup<'a>(&'a Worker, u64);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                self.0.actions.remove(&self.1);
                self.0.free.fetch_add(1, Ordering::Relaxed);
            }
        }
        let _cleanup = Cleanup(w, action);

        // Predict what the action will ask, from its last trace, and answer
        // in advance with what our view says now.
        let mut prefill = Vec::new();
        let mut listings = Vec::new();
        let mut incoming: Vec<Hash> = Vec::new();
        let own = engine.traces.traces(key).first().map(|(_, t)| t.clone());
        // No history of its own: borrow another run of the same program for
        // everything outside the worktree and cargo home (the toolchain and
        // system files every compile reads), and add the outputs of the
        // units this one depends on.
        let predicted: Option<Vec<(VPath, Obs)>> = match &own {
            Some(t) => Some(t.obs.clone()),
            None => {
                let mut obs: Vec<(VPath, Obs)> = engine
                    .profile(spec)
                    .map(|t| {
                        t.obs
                            .iter()
                            .filter(|(p, _)| !self.profile_exclude.iter().any(|x| vpath_starts_with(p, x)))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                // Hinted paths are looked up, not necessarily read.
                obs.extend(req.view.hints().into_iter().map(|p| (VPath::from(p), Obs::Dir)));
                (!obs.is_empty()).then_some(obs)
            }
        };
        if let Some(obs) = predicted {
            let view = req.view.clone();
            let content = self.content.clone();
            let ra2 = ra.clone();
            let host_srcs = self.host_srcs.clone();
            let (p, l, files) = tokio::task::spawn_blocking(move || {
                let mut prefill = Vec::new();
                let mut listings = Vec::new();
                let mut files = Vec::new();
                let mut seen_dirs: HashSet<Vec<u8>> = HashSet::new();
                for (path, o) in &obs {
                    if let Obs::Listing(_) = o {
                        if let Ok(l) = view.list(path) {
                            listings.push((path.to_vec(), l.into_iter().map(|(n, t)| (n, dtype_to_u8(t))).collect()));
                        }
                        continue;
                    }
                    // The walk to this path asks about every parent first.
                    let mut cur: &[u8] = path;
                    let mut chain = Vec::new();
                    while let Some((dir, name)) = vsplit(cur) {
                        chain.push((dir.to_vec(), name.to_vec()));
                        cur = dir;
                    }
                    for (dir, name) in chain.into_iter().rev() {
                        let full = vjoin(&dir, &name);
                        if !seen_dirs.insert(full) {
                            continue;
                        }
                        let looked = view.lookup(&dir, &name);
                        let mut w = to_wire(&looked, &content);
                        // Content the action read last time will be read
                        // again: resolve its hash now and send the bytes
                        // ahead.
                        if let (Looked::File(f), true) = (&looked, matches!(o, Obs::File { .. }) && vjoin(&dir, &name) == path[..]) {
                            if let Ok(h) = content.hash(&f.src) {
                                if let FileSrc::Host { .. } = &f.src {
                                    host_srcs.entry(h).or_insert_with(|| f.src.clone());
                                    ra2.served.lock().push(f.src.clone());
                                }
                                if let WLooked::File { hash, .. } = &mut w {
                                    *hash = Some(h);
                                }
                                files.push(h);
                            }
                        }
                        prefill.push((dir, name, w));
                    }
                }
                (prefill, listings, files)
            })
            .await?;
            prefill = p;
            listings = l;
            let has = w.has_blobs.lock();
            incoming = files.into_iter().filter(|h| !has.contains(h)).collect();
            incoming.sort();
            incoming.dedup();
        }
        self.stats.prefilled_entries.fetch_add(prefill.len() as u64, Ordering::Relaxed);
        w.peer.post(&Msg::Run {
            action,
            spec: WSpec {
                kind: spec.kind,
                argv: spec.argv.clone(),
                env: spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                cwd: spec.cwd.clone(),
                mkdirs: spec.mkdirs.clone(),
                discard: spec.discard.clone(),
                label: spec.label.clone(),
            },
            uid: req.uid,
            gid: req.gid,
            prefill,
            listings,
            untraced: spec.discard.clone(),
            incoming: incoming.clone(),
        });
        if !incoming.is_empty() {
            // Push in the background; the action starts meanwhile and waits
            // only for what it actually opens.
            let w2 = w.clone();
            let blobs = incoming;
            let this: &Pool = self;
            // The pushes only enqueue messages, so doing them inline on a
            // blocking thread keeps their order without holding up events.
            tokio::task::block_in_place(|| {
                for h in blobs {
                    this.push_blob(&w2, h);
                }
            });
        }

        let early: Mutex<HashMap<Vec<u8>, MEntry>> = Mutex::new(HashMap::new());
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let t_sent = t0.elapsed();
        let done = loop {
            let Some(ev) = rx.recv().await else { bail!("worker connection lost") };
            if !matches!(ev, Event::Done(_)) {
                spoke.store(true, Ordering::SeqCst);
            }
            match ev {
                Event::Stdout(d) => {
                    req.observer.stdout(&d);
                    stdout.extend_from_slice(&d);
                }
                Event::Early(path, entry) => {
                    early.lock().insert(path, entry);
                }
                Event::Line(line) => {
                    let live = RemoteLive { early: &early, pool: self, worker: w };
                    let obs = req.observer.clone();
                    tokio::task::block_in_place(|| obs.stderr_line(&line, Some(&live)));
                    stderr.extend_from_slice(&line);
                    stderr.push(b'\n');
                }
                Event::Done(r) => break r.map_err(|e| anyhow!("worker: {e}"))?,
            }
        };

        let t_done = t0.elapsed();
        // Bring the outputs home, and what the action left in the
        // directories that are kept on the side (its incremental state: the
        // next compile of this crate, wherever it runs, starts from it).
        let wanted: Vec<Hash> = done
            .entries
            .iter()
            .filter(|(p, _)| !spec.discard.iter().any(|d| vpath_starts_with(p, d)))
            .chain(done.side.iter())
            .filter_map(|(_, e)| match e {
                MEntry::File { hash, .. } if !engine.store.has(hash) => Some(*hash),
                _ => None,
            })
            .collect();
        let wanted_bytes: u64 = done
            .entries
            .iter()
            .chain(done.side.iter())
            .filter_map(|(_, e)| match e {
                MEntry::File { hash, size, .. } if wanted.contains(hash) => Some(*size),
                _ => None,
            })
            .sum();
        for group in wanted.chunks(16) {
            let fetches: Vec<_> = group.iter().map(|h| self.blobs.fetch(&w.peer, h)).collect();
            for f in fetches {
                f.await.context("fetching an output from the worker")?;
            }
        }
        {
            let mut has = w.has_blobs.lock();
            for (_, e) in done.entries.iter().chain(done.side.iter()) {
                if let MEntry::File { hash, .. } = e {
                    has.insert(*hash);
                }
            }
        }
        // A host file we served may have changed since.
        let mut tainted = done.tainted;
        for src in ra.served.lock().iter() {
            if let FileSrc::Host { path, sig } = src {
                match std::fs::symlink_metadata(path) {
                    Ok(m) if HostSig::of(&m) == *sig => {}
                    _ => tainted = true,
                }
            }
        }
        let entries: Vec<_> = done
            .entries
            .into_iter()
            .filter(|(p, _)| !spec.discard.iter().any(|d| vpath_starts_with(p, d)))
            .collect();
        let side = done.side;
        let result = ActionResult {
            status: done.status,
            stdout: engine.store.put_bytes(&stdout)?,
            stderr: engine.store.put_bytes(&stderr)?,
            outputs: Arc::new(Manifest { entries }),
            stats: done.stats,
        };
        w.note_overhead(t0.elapsed().saturating_sub(Duration::from_micros(result.stats.wall_us)));
        // Bandwidth as seen by output fetches of a meaningful size.
        {
            let bytes: u64 = wanted_bytes;
            let secs = (t0.elapsed() - t_done).as_secs_f64();
            if bytes > (4 << 20) && secs > 0.01 {
                let seen = (bytes as f64 / secs) as u64;
                let old = self.bandwidth.load(Ordering::Relaxed);
                self.bandwidth.store((old * 3 + seen) / 4, Ordering::Relaxed);
            }
        }
        if trace_remote() {
            eprintln!(
                "rargod: remote {:x} `{}`: {:.2}s total = {:.2}s to send, {:.2}s until done (worker ran it {:.2}s, {} questions), {:.2}s fetching outputs",
                w.id,
                spec.label,
                t0.elapsed().as_secs_f64(),
                t_sent.as_secs_f64(),
                (t_done - t_sent).as_secs_f64(),
                result.stats.wall_us as f64 / 1e6,
                ra.asked.load(Ordering::Relaxed),
                (t0.elapsed() - t_done).as_secs_f64(),
            );
        }
        Ok((result, done.obs, tainted, side))
    }
}

impl RemoteRunner for Pool {
    fn try_run<'a>(
        &'a self,
        engine: &'a Engine,
        spec: &'a ActionSpec,
        key: &'a Hash,
        req: &'a Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RunResult>> + Send + 'a>> {
        Box::pin(async move {
            // Planning needs the host; host networking has no meaning
            // elsewhere; a flake root would need the closure on the worker.
            if spec.kind == ActionKind::Plan || spec.net != NetMode::Loopback || spec.root_id != Hash::ZERO {
                return None;
            }
            // Only when a local core is not available right now.
            let (running, waiting, _, _) = engine.sched.snapshot();
            if running < engine.sched.tokens() && waiting == 0 {
                self.stats.kept_local_no_wait.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let w = self.lease()?;
            // Is shipping the inputs cheaper than waiting for a core?
            let wait = engine.avg_action_wall().mul_f64((waiting + 1) as f64 / engine.sched.tokens().max(1) as f64);
            let (missing, unknown) = match engine.traces.traces(key).first() {
                Some((_, t)) => {
                    let has = w.has_blobs.lock();
                    let bytes: u64 = t
                        .obs
                        .iter()
                        .filter_map(|(_, o)| match o {
                            Obs::File { hash, .. } if !has.contains(hash) => Some(engine.store.size_of(hash).unwrap_or(256 << 10)),
                            _ => None,
                        })
                        .sum();
                    (bytes, 10u32)
                }
                // Never run before: nothing to predict; each lookup is a
                // round trip.
                None => (8 << 20, 300),
            };
            let rtt = w.peer.conn.rtt().unwrap_or(Duration::from_millis(1));
            let bytes_time = Duration::from_secs_f64(missing as f64 / self.bandwidth.load(Ordering::Relaxed).max(1) as f64);
            // Once this worker has run a few actions, what they really cost
            // beyond running replaces the guess about round trips.
            let transfer = match w.overhead() {
                Some(o) => bytes_time + o,
                None => bytes_time + rtt * unknown,
            };
            if transfer > wait && !eager_dispatch() {
                w.free.fetch_add(1, Ordering::Relaxed);
                self.stats.kept_local_transfer.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            self.stats.dispatched.fetch_add(1, Ordering::Relaxed);
            engine.stats.remote_runs.fetch_add(1, Ordering::Relaxed);
            // Hedge: should a local core fall idle before the worker has
            // said anything for this action (it may be fetching a toolchain,
            // or simply be slower), the action runs here instead. Once the
            // worker's output has started, the remote run is kept: showing
            // output twice, or announcing its metadata twice, is worse.
            let spoke = std::sync::atomic::AtomicBool::new(false);
            let remote = self.run_on(&w, engine, spec, key, req, &spoke);
            tokio::pin!(remote);
            let mem = engine.estimate_mem_kb(key, spec.kind);
            let result = loop {
                tokio::select! {
                    r = &mut remote => break r,
                    permit = engine.sched.acquire(crate::sched::Priority::Idle, mem), if !spoke.load(Ordering::SeqCst) => {
                        if spoke.load(Ordering::SeqCst) {
                            drop(permit);
                            continue;
                        }
                        self.stats.hedged.fetch_add(1, Ordering::Relaxed);
                        engine.stats.runs.fetch_add(1, Ordering::Relaxed);
                        let started = std::time::Instant::now();
                        let r = engine.execute(spec, req.view.clone(), req.uid, req.gid, &*req.observer).await;
                        drop(permit);
                        engine.note_action_wall(started.elapsed());
                        // Dropping `remote` withdraws the action from the worker.
                        return Some(r);
                    }
                }
            };
            match result {
                Ok(r) => Some(Ok(r)),
                Err(e) => {
                    // The worker failed, not the action: run it here.
                    eprintln!("rargod: worker {:x} failed `{}`: {e:#}; running locally", w.id, spec.label);
                    self.stats.worker_failures.fetch_add(1, Ordering::Relaxed);
                    engine.stats.remote_failed.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        })
    }
}
