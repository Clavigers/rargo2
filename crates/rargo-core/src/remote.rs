//! Workers: running actions on other machines.
//!
//! An action is a command, a view and a filesystem served on demand. On a
//! worker the view is a [`RemoteView`]: answers the server sent ahead
//! (predicted from the action's previous trace) are used directly, anything
//! else is asked for over the session. A closure's store paths arrive as
//! trees with the first action that needs them, and lookups under those
//! paths are answered on the worker. File content travels as chunks, and
//! only chunks the receiver does not already hold, and only when something
//! opens the file. The worker traces the action exactly as the server would
//! and sends the trace and outputs back.
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
        /// Directories among `listings` that nothing will be added to while
        /// the action runs (the crate's sources): a name that is not listed
        /// does not exist, without asking.
        closed: Vec<Vec<u8>>,
        /// This action's closure: store path name and manifest identity,
        /// sorted by name. Only these names exist in its `/nix/store`.
        closure: Vec<(Vec<u8>, Hash)>,
        /// Manifests for closure paths this worker does not yet hold.
        store_paths: Vec<crate::nixmanifest::WirePath>,
    },
    /// Part of a blob the worker is expected to need: its recipe and the
    /// chunks the server believes the worker lacks.
    /// One of `of` messages that together push a blob. They may arrive
    /// in any order (the transport delivers messages independently); the
    /// recipe is in the one numbered 0.
    Push { blob: Hash, recipe: Recipe, chunks: Vec<(u32, Vec<u8>)>, part: u32, of: u32 },
    Stdout { action: u64, data: Vec<u8> },
    StderrLine { action: u64, line: Vec<u8> },
    /// A file the action finished early (an `.rmeta`).
    Early { action: u64, path: Vec<u8>, entry: MEntry },
    Done { action: u64, result: Result<WDone, String> },
    /// Nobody wants this action's result any more: stop it.
    Cancel { action: u64 },
    /// The worker has finished with a blob it was sent: the bytes are in
    /// its store, or they could not be put there. Either way it is no
    /// longer on the way.
    Stored { blob: Hash },
    /// The worker is leaving. The server drops it as soon as this arrives.
    Bye,
}

fn dtype_to_u8(t: DType) -> u8 {
    let b = match t {
        DType::Dir => 0,
        DType::File => 1,
        DType::Symlink => 2,
        DType::Other => 3,
    };
    assert_eq!(std::mem::discriminant(&dtype_from_u8(b)), std::mem::discriminant(&t), "the encoding round-trips");
    b
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
    /// Questions, answers, actions and their results.
    out: mpsc::UnboundedSender<Vec<u8>>,
    /// File contents. Sent when nothing waits in `out`, so that a question
    /// is not held up by the megabytes queued before it.
    bulk: mpsc::UnboundedSender<Vec<u8>>,
}

/// Whether a message carries file contents (and goes behind the others).
/// The transport delivers messages independently and nothing relies on
/// contents arriving before the message that needs them: an action waits
/// for what it was told is coming.
fn is_bulk(m: &Msg) -> bool {
    matches!(m, Msg::Push { .. } | Msg::Resp { body: Resp::Chunks(_) | Resp::Blob(_), .. })
}

impl Peer {
    /// Wraps a connection. Messages other than replies are delivered to the
    /// returned receiver.
    pub fn new(conn: Arc<Conn>) -> (Arc<Peer>, mpsc::UnboundedReceiver<Msg>) {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (bulk_tx, mut bulk_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let peer = Arc::new(Peer { conn: conn.clone(), pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1), out: out_tx, bulk: bulk_tx });
        let c = conn.clone();
        tokio::spawn(async move {
            loop {
                let m = tokio::select! {
                    biased;
                    m = out_rx.recv() => m,
                    m = bulk_rx.recv() => m,
                };
                let Some(m) = m else { break };
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
            let _ = if is_bulk(m) { self.bulk.send(b) } else { self.out.send(b) };
        }
    }

    /// Sends `m` and returns when the peer has acknowledged the datagram.
    /// The peer has the bytes; it may still be acting on them.
    pub async fn deliver(&self, m: &Msg) {
        let Ok(b) = bincode::serialize(m) else { return };
        let framed = if b.len() >= COMPRESS_FROM { tokio::task::block_in_place(|| frame(b)) } else { frame(b) };
        if self.conn.send(&framed).await.is_err() {
            return;
        }
        loop {
            let wake = self.conn.send_settled.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.conn.send_idle() {
                return;
            }
            wake.await;
        }
    }

    fn start(&self, body: Req) -> oneshot::Receiver<Resp> {
        let rid = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(rid > 0, "request ids start at 1");
        let (tx, rx) = oneshot::channel();
        assert!(self.pending.lock().insert(rid, tx).is_none(), "request ids are not reused");
        self.post(&Msg::Req { rid, body });
        rx
    }

    /// Sends a request now; the returned future waits for its answer.
    /// Several calls made before the first is awaited are in flight
    /// together.
    pub fn call(&self, body: Req) -> impl std::future::Future<Output = Result<Resp>> + Send + 'static {
        let rx = self.start(body);
        async move {
            match tokio::time::timeout(Duration::from_secs(120), rx).await {
                Ok(Ok(Resp::Err(e))) => bail!("peer: {e}"),
                Ok(Ok(r)) => Ok(r),
                Ok(Err(_)) => bail!("connection to peer lost"),
                Err(_) => bail!("peer did not answer"),
            }
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
    assert!(out.windows(2).all(|w| w[0] < w[1]), "indices ascend");
    assert!(out.len() <= recipe.len());
    assert!(out.len() <= 1 || bytes <= BATCH_BYTES);
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
        assert_eq!(offsets.len(), recipe.len());
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
        assert_eq!(off, recipe.iter().map(|c| c.len as u64).sum::<u64>());
        let missing_count = missing.len();
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
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), missing_count);
        assert!(batches.iter().all(|b| !b.is_empty()));
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
        assert!(self.store.has(h));
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

    /// Fetches several blobs from the peer, a few at a time.
    pub async fn fetch_all(self: &Arc<Self>, peer: &Arc<Peer>, blobs: &[Hash]) -> Result<()> {
        for group in blobs.chunks(FETCH_TOGETHER) {
            let tasks: Vec<_> = group
                .iter()
                .filter(|h| !self.store.has(h))
                .map(|h| {
                    let (this, peer, h) = (self.clone(), peer.clone(), *h);
                    tokio::spawn(async move { this.fetch(&peer, &h).await })
                })
                .collect();
            let mut first: Result<()> = Ok(());
            for t in tasks {
                let r = t.await.map_err(|e| anyhow!("fetch task: {e}")).and_then(|r| r);
                if first.is_ok() {
                    first = r;
                }
            }
            first?;
        }
        Ok(())
    }
}

/// Blobs fetched from a peer at the same time.
const FETCH_TOGETHER: usize = 8;

// ---------------------------------------------------------------------------
// Worker side
// ---------------------------------------------------------------------------

struct Incoming {
    recipe: Option<Recipe>,
    chunks: HashMap<u32, Vec<u8>>,
    parts: u32,
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
    /// Contents an action needed that could not be had from the server.
    starved: AtomicU64,
    /// Store paths whose manifests this worker holds.
    nix: DashMap<Vec<u8>, Arc<crate::nixmanifest::Installed>>,
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
        r.map_err(|e| {
            self.starved.fetch_add(1, Ordering::SeqCst);
            eprintln!("rargo worker: content {h} could not be had from the server: {e:#}");
            io::Error::other(format!("{e:#}"))
        })
    }

    async fn on_push(self: &Arc<Self>, blob: Hash, recipe: Recipe, chunks: Vec<(u32, Vec<u8>)>, part: u32, of: u32) {
        // The messages of one push overtake each other: the blob is put
        // together when all of them are here, whichever came last. (The
        // recipe travels in the message numbered 0, which need not be the
        // first to arrive; putting the blob together on the last-numbered
        // message would use whatever recipe had come by then.)
        let complete = {
            let mut partial = self.partial.lock();
            let inc = partial.entry(blob).or_insert_with(|| Incoming { recipe: None, chunks: HashMap::new(), parts: 0 });
            if part == 0 {
                inc.recipe = Some(recipe);
            }
            inc.chunks.extend(chunks);
            inc.parts += 1;
            if inc.parts >= of.max(1) && inc.recipe.is_some() { partial.remove(&blob) } else { None }
        };
        if let Some(inc) = complete {
            let recipe = inc.recipe.unwrap_or_default();
            if let Err(e) = self.blobs.assemble(&self.peer, &blob, &recipe, inc.chunks).await {
                eprintln!("rargo worker: pushed blob {blob} could not be assembled: {e:#}");
            }
            // Said either way, so the server does not keep waiting on a blob
            // that will never arrive.
            self.peer.post(&Msg::Stored { blob });
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
    /// Directories whose listing in `listings` is complete.
    closed: HashSet<Vec<u8>>,
    /// Store path names in this action's closure, sorted.
    closure: Vec<Vec<u8>>,
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
        // The tree names every file by its content hash. An answer sent
        // ahead may not, and would then ask the server to open the file.
        if let Some(l) = crate::nixmanifest::lookup_at(|n| self.state.nix.get(n).map(|e| Arc::clone(e.value())), &self.closure, dir, name) {
            self.state.lookups_local.fetch_add(1, Ordering::Relaxed);
            return l;
        }
        let key = (dir.to_vec(), name.to_vec());
        if let Some(w) = self.entries.get(&key) {
            self.state.lookups_local.fetch_add(1, Ordering::Relaxed);
            return self.convert(dir, name, &w);
        }
        if self.closed.contains(dir) && self.listings.get(dir).is_some_and(|l| !l.iter().any(|(n, _)| n == name)) {
            self.state.lookups_local.fetch_add(1, Ordering::Relaxed);
            self.entries.insert(key, WLooked::Absent);
            return Looked::Absent;
        }
        self.state.lookups_remote.fetch_add(1, Ordering::Relaxed);
        let w = match self.state.peer.call_blocking(Req::Lookup { action: self.action, dir: dir.to_vec(), name: name.to_vec() }) {
            Ok(Resp::Looked(w)) => w,
            other => {
                // Not an answer: the action was withdrawn, or the link is
                // in trouble. Said aloud, because "absent" is what the
                // compiler will make of it.
                let why = match other {
                    Err(e) => format!("{e:#}"),
                    Ok(_) => "unexpected reply".to_string(),
                };
                eprintln!(
                    "rargo worker: action {:x}: no answer about {}/{}: {why}",
                    self.action,
                    String::from_utf8_lossy(dir),
                    String::from_utf8_lossy(name)
                );
                WLooked::Absent
            }
        };
        let l = self.convert(dir, name, &w);
        self.entries.insert(key, w);
        l
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        if let Some(l) = crate::nixmanifest::list_at(|n| self.state.nix.get(n).map(|e| Arc::clone(e.value())), &self.closure, dir) {
            return l;
        }
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
        // Store paths are not traced: their identity is part of the action key.
        if !self.closure.is_empty() && crate::nixmanifest::place(dir).is_some() {
            return false;
        }
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
    let conn = loop {
        match endpoint.connect(server).await {
            Ok(c) => break c,
            // The server may simply not be up yet.
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => eprintln!("rargo worker: {server}: {e}; trying again"),
            Err(e) => return Err(e).with_context(|| format!("connecting to the server at {server}")),
        }
    };
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
        starved: AtomicU64::new(0),
        nix: DashMap::new(),
    });
    let s2 = state.clone();
    engine.content.set_fetcher(Arc::new(move |h| s2.ensure_blob(h)));
    let tasks: Arc<Mutex<HashMap<u64, tokio::task::AbortHandle>>> = Arc::new(Mutex::new(HashMap::new()));
    peer.post(&Msg::Hello { cores, blobs: engine.store.all_hashes() });
    eprintln!("rargo worker: connected to {server}, {cores} cores");
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
    let mut intr = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).expect("signal");
    loop {
        let msg = tokio::select! {
            m = inbox.recv() => m,
            _ = term.recv() => {
                // The server drops this worker once it has the notice.
                // Returning after the acknowledgement means the notice is
                // there before this process is.
                peer.deliver(&Msg::Bye).await;
                return Ok(());
            }
            _ = intr.recv() => {
                peer.deliver(&Msg::Bye).await;
                return Ok(());
            }
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
            Msg::Push { blob, recipe, chunks, part, of } => {
                let s = state.clone();
                tokio::spawn(async move { s.on_push(blob, recipe, chunks, part, of).await });
            }
            Msg::Stored { .. } => {}
            Msg::Run { action, spec, uid, gid, prefill, listings, untraced, incoming, closed, closure, store_paths } => {
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
                // Installed before the action is spawned, so a later action on
                // this connection sees the trees even when its own message
                // did not carry them.
                for path in &store_paths {
                    if state.nix.contains_key(&path.name) {
                        continue;
                    }
                    match crate::nixmanifest::Installed::from_kind(&path.kind) {
                        Ok(inst) => {
                            state.nix.insert(path.name.clone(), Arc::new(inst));
                        }
                        Err(e) => eprintln!(
                            "rargo worker: nix path {} is not a tree: {e}",
                            String::from_utf8_lossy(&path.name)
                        ),
                    }
                }
                let mut closure_names: Vec<Vec<u8>> = closure.into_iter().map(|(n, _)| n).collect();
                closure_names.sort();
                let state = state.clone();
                let tasks2 = tasks.clone();
                // Held across the spawn so that the task cannot finish and
                // deregister before it is registered.
                let mut registry = tasks.lock();
                let task = tokio::spawn(async move {
                    let view = Arc::new(RemoteView {
                        state: state.clone(),
                        action,
                        entries: prefill.into_iter().map(|(d, n, w)| ((d, n), w)).collect(),
                        listings: listings
                            .into_iter()
                            .map(|(d, l)| (d, l.into_iter().map(|(n, t)| (n, dtype_from_u8(t))).collect()))
                            .collect(),
                        untraced,
                        closed: closed.into_iter().collect(),
                        closure: closure_names,
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
                    let starved_before = state.starved.load(Ordering::SeqCst);
                    let r = state.engine.execute(&aspec, view, uid, gid, &obs).await;
                    drop(permit);
                    // A file the action was shown could not be fetched: what
                    // the action made of that (a compiler error about a
                    // missing or unreadable file) is this worker's failure,
                    // not the action's result. The server runs it itself.
                    let r = if state.starved.load(Ordering::SeqCst) != starved_before {
                        Err(anyhow!("an input's content could not be fetched from the server while the action ran"))
                    } else {
                        r
                    };
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
                    tasks2.lock().remove(&action);
                    state.peer.post(&Msg::Done { action, result });
                });
                registry.insert(action, task.abort_handle());
            }
            Msg::Cancel { action } => {
                // Dropping the task kills the action's process if it had
                // started, or gives up its place in the queue if not.
                if let Some(task) = tasks.lock().remove(&action) {
                    task.abort();
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

const NOBODY: u8 = 0;
const REMOTE: u8 = 1;
const LOCAL: u8 = 2;

/// Which of two copies of one action is the one whose output is shown.
#[derive(Default)]
struct Voice {
    owner: std::sync::atomic::AtomicU8,
    changed: tokio::sync::Notify,
}

impl Voice {
    fn owner(&self) -> u8 {
        self.owner.load(Ordering::SeqCst)
    }

    /// `who` is about to produce output: true if it is (now) the copy being
    /// shown.
    fn claim(&self, who: u8) -> bool {
        match self.owner.compare_exchange(NOBODY, who, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => {
                self.changed.notify_one();
                true
            }
            Err(current) => current == who,
        }
    }
}

/// The observer of the local copy: silent unless it is the copy shown.
struct Gated<'a> {
    inner: &'a dyn Observer,
    voice: &'a Voice,
}

impl Observer for Gated<'_> {
    fn stdout(&self, data: &[u8]) {
        if self.voice.claim(LOCAL) {
            self.inner.stdout(data);
        }
    }
    fn stderr_line(&self, line: &[u8], live: Option<&dyn Live>) {
        if self.voice.claim(LOCAL) {
            self.inner.stderr_line(line, live);
        }
    }
}

/// The crate root a compiler command names: its first argument that is a
/// Rust source file, made absolute.
fn crate_root(argv: &[Vec<u8>], cwd: &[u8]) -> Option<Vec<u8>> {
    let file = argv.iter().skip(1).find(|a| a.ends_with(b".rs") && !a.starts_with(b"-"))?;
    Some(if file.starts_with(b"/") { file.clone() } else { vjoin(cwd, file) })
}

/// Whether a compiler command produces something that is linked.
fn links(argv: &[Vec<u8>]) -> bool {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == b"--test" {
            return true;
        }
        if a == b"--crate-type" {
            if let Some(t) = it.next() {
                if t.split(|&c| c == b',').any(|t| matches!(t, b"bin" | b"proc-macro" | b"dylib" | b"cdylib")) {
                    return true;
                }
            }
        }
    }
    false
}

/// The libraries of the toolchain `program` (`<sysroot>/bin/rustc`) belongs
/// to: everything in `<sysroot>/lib/rustlib/<target>/lib`.
fn sysroot_libs(view: &dyn View, program: &[u8]) -> Vec<Vec<u8>> {
    let Some(sysroot) = vsplit(program).and_then(|(bin, _)| vsplit(bin)).map(|(root, _)| root.to_vec()) else {
        return Vec::new();
    };
    let rustlib = vjoin(&vjoin(&sysroot, b"lib"), b"rustlib");
    let mut out = Vec::new();
    for (target, kind) in view.list(&rustlib).unwrap_or_default() {
        if kind != DType::Dir {
            continue;
        }
        let dir = vjoin(&vjoin(&rustlib, &target), b"lib");
        for (name, kind) in view.list(&dir).unwrap_or_default() {
            if kind == DType::File {
                out.push(vjoin(&dir, &name));
            }
        }
    }
    out
}

/// At most this many directories and files of a crate travel ahead.
const SOURCE_TREE_DIRS: usize = 300;
const SOURCE_TREE_FILES: usize = 3000;

/// The directories and `.rs` files below the directory of a crate root.
/// A crate root that sits beside its `Cargo.toml` (a build script, a crate
/// without `src/`) has the whole package below it, tests, vendored code and
/// all: then only the root's own directory is taken, not what is below.
fn source_tree(view: &dyn View, root: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let Some((top, _)) = vsplit(root) else { return (Vec::new(), Vec::new()) };
    let (mut dirs, mut files) = (Vec::new(), vec![root.to_vec()]);
    let mut queue = vec![top.to_vec()];
    let mut recurse = true;
    while let Some(dir) = queue.pop() {
        if dirs.len() >= SOURCE_TREE_DIRS || files.len() >= SOURCE_TREE_FILES {
            // Too large to be worth it; what was not listed is asked for.
            break;
        }
        let Ok(listing) = view.list(&dir) else { continue };
        if dir == top && listing.iter().any(|(n, _)| n == b"Cargo.toml") {
            recurse = false;
        }
        for (name, kind) in &listing {
            let path = vjoin(&dir, name);
            match kind {
                DType::Dir if recurse => queue.push(path),
                DType::File if name.ends_with(b".rs") && path != root => files.push(path),
                _ => {}
            }
        }
        dirs.push(dir);
    }
    (dirs, files)
}

/// The files a compiler command names with `--extern [opts:]name=path`.
fn extern_paths(argv: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let value: &[u8] = if a == b"--extern" {
            match it.next() {
                Some(v) => v,
                None => break,
            }
        } else if let Some(v) = a.strip_prefix(b"--extern=") {
            v
        } else {
            continue;
        };
        if let Some(eq) = value.iter().position(|&c| c == b'=') {
            let path = &value[eq + 1..];
            if path.starts_with(b"/") {
                out.push(path.to_vec());
            }
        }
    }
    out
}

/// Everything below `dirs` in `view`: what a lookup of each entry answers
/// (files with their content hash) and the listing of each directory.
#[allow(clippy::type_complexity)]
fn side_tree(
    view: &dyn View,
    content: &Content,
    dirs: &[Vec<u8>],
) -> (Vec<(Vec<u8>, Vec<u8>, WLooked)>, Vec<(Vec<u8>, Vec<(Vec<u8>, u8)>)>) {
    let mut prefill = Vec::new();
    let mut listings = Vec::new();
    let mut todo: Vec<Vec<u8>> = dirs.to_vec();
    while let Some(dir) = todo.pop() {
        let Ok(listing) = view.list(&dir) else { continue };
        for (name, _) in &listing {
            let looked = view.lookup(&dir, name);
            let mut w = to_wire(&looked, content);
            match &looked {
                Looked::Dir(_) => todo.push(vjoin(&dir, name)),
                Looked::File(f) => {
                    if let (Ok(h), WLooked::File { hash, .. }) = (content.hash(&f.src), &mut w) {
                        *hash = Some(h);
                    }
                }
                _ => {}
            }
            prefill.push((dir.clone(), name.clone(), w));
        }
        listings.push((dir, listing.into_iter().map(|(n, t)| (n, dtype_to_u8(t))).collect()));
    }
    (prefill, listings)
}

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
                assert_eq!(out.len(), z.len() + 9);
                return out;
            }
        }
    }
    let mut out = Vec::with_capacity(b.len() + 1);
    out.push(0);
    out.extend_from_slice(&b);
    assert_eq!(out.len(), b.len() + 1);
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

/// `RARGO_REMOTE_INCR=0` in rargod's environment: crates that are compiled
/// incrementally are never compiled on a worker (the rule of D55).
fn remote_incr() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RARGO_REMOTE_INCR").ok().as_deref() != Some("0"))
}

/// `RARGO_STATE_HOME_AFTER_MS` in rargod's environment: how long a state
/// lies on the worker that made it before it is fetched in the background
/// (default: not at all). A compile here that needs it fetches it at once
/// whatever this says.
fn state_home_after() -> Duration {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_millis(*MS.get_or_init(|| {
        std::env::var("RARGO_STATE_HOME_AFTER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
    }))
}

/// `RARGO_HEDGE=0` in rargod's environment: a compile given to a worker is
/// not also run on an idle local core. On unless set, so a core that
/// nothing else wants can finish the compile while the worker is still
/// fetching what it needs.
fn hedge() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RARGO_HEDGE").ok().as_deref() != Some("0"))
}

/// `RARGO_STATE_AHEAD=1` in rargod's environment: incremental states made
/// on the server are sent to workers ahead of need. Off unless asked for:
/// measured, the states' bytes cost the link more than the compiles that
/// could then leave the server gained (D65).
fn state_ahead() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RARGO_STATE_AHEAD").ok().as_deref() == Some("1"))
}

/// A state larger than this in what a worker lacks of it is not sent ahead.
const AHEAD_MAX: u64 = 256 << 20;

/// States made on the server that workers have not been sent yet: the
/// newest of each crate and worktree, newest first, a bounded number.
#[derive(Default)]
pub struct AheadQueue {
    waiting: std::collections::VecDeque<((Hash, String), Arc<Vec<(Hash, u64)>>)>,
}

impl AheadQueue {
    const KEPT: usize = 512;

    /// A newer state of a crate replaces the one still waiting.
    pub fn offer(&mut self, whose: (Hash, String), blobs: Arc<Vec<(Hash, u64)>>) {
        self.waiting.retain(|(w, _)| *w != whose);
        self.waiting.push_back((whose, blobs));
        if self.waiting.len() > Self::KEPT {
            self.waiting.pop_front();
        }
        assert!(self.waiting.len() <= Self::KEPT);
        debug_assert_eq!(self.waiting.iter().filter(|(w, _)| *w == self.waiting.back().unwrap().0).count(), 1, "one state per crate");
    }

    /// The newest state waiting.
    pub fn next(&mut self) -> Option<Arc<Vec<(Hash, u64)>>> {
        self.waiting.pop_back().map(|(_, b)| b)
    }

    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

/// How long after the link is reckoned to have carried a blob an action is
/// still told that it is coming.
const IN_FLIGHT_SLACK: Duration = Duration::from_secs(2);

fn still_in_flight(fl: &HashMap<Hash, std::time::Instant>, h: &Hash, now: std::time::Instant) -> bool {
    fl.get(h).is_some_and(|at| *at > now)
}

/// How much of an incremental state a worker may lack and still be given
/// the compile: that much is sent along; a state it does not have is not
/// (D31).
const SEED_GAP: u64 = 4 << 20;

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
    /// When what has been queued for the worker will have crossed the link.
    link_free_at: Mutex<std::time::Instant>,
    /// Contents on their way to the worker and when the link will have
    /// carried them: an action that needs one is told it is coming (it is
    /// sent ahead of contents and may arrive first).
    in_flight: Mutex<HashMap<Hash, std::time::Instant>>,
    /// The files every compile reads (the toolchain) are on their way.
    warmed: std::sync::atomic::AtomicBool,
    /// Store path names whose manifests have been sent to this worker.
    store_paths: Mutex<HashSet<Vec<u8>>>,
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
    /// Queued for both; a local core came free before a worker's.
    pub kept_local_first: AtomicU64,
    pub worker_failures: AtomicU64,
    /// Not offered to workers: no worker has the crate's incremental state.
    pub kept_local_incremental: AtomicU64,
    /// Dispatched with the incremental state the worker already held.
    pub seeded_in_place: AtomicU64,
    /// Incrementally compiled crates dispatched without any state (their
    /// first compile).
    pub unseeded_remote: AtomicU64,
    /// Files of incremental states left on workers, to come home later.
    pub state_left_blobs: AtomicU64,
    pub state_left_bytes: AtomicU64,
    /// Of those, fetched in the background on an idle core.
    pub state_homed_blobs: AtomicU64,
    pub state_homed_bytes: AtomicU64,
    /// Fetched because a compile here needed them.
    pub state_needed_blobs: AtomicU64,
    /// States made here and sent to workers ahead of need, and their size
    /// in what the workers lacked.
    pub states_sent_ahead: AtomicU64,
    pub state_ahead_bytes: AtomicU64,
    /// Dispatched, then run here because a local core fell idle first.
    pub hedged: AtomicU64,
    /// Hedges where the local copy finished first.
    pub hedge_won: AtomicU64,
    /// Hedges whose local copy was stopped because the worker's was ahead.
    pub hedge_stopped: AtomicU64,
    pub prefilled_entries: AtomicU64,
    pub lookups_served: AtomicU64,
    pub opens_served: AtomicU64,
    /// Questions whose directory is `/nix/store` or under it: lookups,
    /// listings and opens. A worker that holds the closure's trees asks none.
    pub nix_lookups: AtomicU64,
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
    /// Signalled whenever a worker core becomes free.
    freed: tokio::sync::Notify,
    /// Files of incremental states that are on a worker and not here, in
    /// the order they were made.
    home_queue: Mutex<std::collections::VecDeque<(Arc<Worker>, Hash, u64, std::time::Instant)>>,
    home_wake: tokio::sync::Notify,
    /// States made here, to be sent to workers when the link is idle.
    ahead: Mutex<AheadQueue>,
    ahead_wake: tokio::sync::Notify,
    /// Set under `ahead`'s lock before a state is taken off the queue, and
    /// cleared when that send has been handed to the links.
    ahead_busy: AtomicBool,
    /// Set under `home_queue`'s lock before a due file is taken off it, and
    /// cleared when that fetch has finished.
    home_fetching: AtomicBool,
    /// Woken when a transfer starts, finishes, or is given up.
    settle_wake: tokio::sync::Notify,
    /// Files of incremental states that compiles left on workers: which
    /// worker, and the action that made them (for diagnosis).
    left_on: DashMap<Hash, (u64, String)>,
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
            freed: tokio::sync::Notify::new(),
            home_queue: Mutex::new(std::collections::VecDeque::new()),
            home_wake: tokio::sync::Notify::new(),
            ahead: Mutex::new(AheadQueue::default()),
            ahead_wake: tokio::sync::Notify::new(),
            ahead_busy: AtomicBool::new(false),
            home_fetching: AtomicBool::new(false),
            settle_wake: tokio::sync::Notify::new(),
            left_on: DashMap::new(),
        });
        tokio::spawn(pool.clone().bring_states_home(engine.clone()));
        tokio::spawn(pool.clone().send_states_ahead(engine.clone()));
        let p = pool.clone();
        tokio::spawn(async move {
            while let Some(conn) = endpoint.accept().await {
                let p = p.clone();
                tokio::spawn(async move { p.serve_worker(conn).await });
            }
        });
        Ok(pool)
    }

    /// Measured cost of a remote action beyond running it, per worker.
    pub fn overhead_text(&self) -> String {
        let ws = self.workers.lock();
        let v: Vec<String> = ws.iter().filter_map(|w| w.overhead()).map(|d| format!("{:.2} s", d.as_secs_f64())).collect();
        if v.is_empty() { "not measured yet".into() } else { v.join(", ") }
    }

    /// Host files shown to actions, keyed by content hash, so a worker can
    /// be sent the bytes of a store path without the file being copied in.
    pub fn host_sources(&self) -> &Arc<DashMap<Hash, FileSrc>> {
        &self.host_srcs
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
                        link_free_at: Mutex::new(std::time::Instant::now()),
                        in_flight: Mutex::new(HashMap::new()),
                        warmed: std::sync::atomic::AtomicBool::new(false),
                        store_paths: Mutex::new(HashSet::new()),
                    });
                    eprintln!("rargod: worker {:x} joined from {} with {cores} cores", conn.id, conn.peer());
                    self.workers.lock().push(w.clone());
                    for _ in 0..cores {
                        self.freed.notify_one();
                    }
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
                Msg::Stored { blob } => {
                    if let Some(w) = &worker {
                        w.in_flight.lock().remove(&blob);
                        self.settle_wake.notify_waiters();
                    }
                }
                Msg::Bye => break,
                _ => {}
            }
        }
        if let Some(w) = worker {
            eprintln!("rargod: worker {:x} is gone", w.id);
            w.alive.store(false, Ordering::SeqCst);
            w.in_flight.lock().clear();
            self.settle_wake.notify_waiters();
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

    fn note_nix(&self, dir: &[u8]) {
        if dir == b"/nix/store" || dir.starts_with(b"/nix/store/") {
            self.stats.nix_lookups.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Answers a worker's request (runs on a blocking thread).
    fn answer(&self, w: &Worker, body: Req) -> Resp {
        match body {
            Req::Lookup { action, dir, name } => {
                let Some(a) = w.actions.get(&action).map(|a| a.clone()) else { return Resp::Err("no such action".into()) };
                self.note_nix(&dir);
                self.stats.lookups_served.fetch_add(1, Ordering::Relaxed);
                a.asked.fetch_add(1, Ordering::Relaxed);
                let looked = a.view.lookup(&dir, &name);
                if trace_remote() {
                    let kind = match &looked {
                        Looked::File(_) => "file",
                        Looked::Dir(_) => "dir",
                        Looked::Absent => "absent",
                        _ => "other",
                    };
                    eprintln!("rargod: asked lookup {kind} {}/{}", String::from_utf8_lossy(&dir), String::from_utf8_lossy(&name));
                }
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
                self.note_nix(&dir);
                Resp::Listing(
                    a.view
                        .list(&dir)
                        .map(|l| l.into_iter().map(|(n, t)| (n, dtype_to_u8(t))).collect())
                        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO)),
                )
            }
            Req::Open { action, dir, name } => {
                let Some(a) = w.actions.get(&action).map(|a| a.clone()) else { return Resp::Err("no such action".into()) };
                self.note_nix(&dir);
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
                let Some(r) = self.blobs.recipe(&blob) else {
                    let why = if self.blobs.store.has(&blob) {
                        "in the store, but unreadable".to_string()
                    } else {
                        match self.host_srcs.get(&blob).map(|s| s.clone()) {
                            Some(FileSrc::Host { path, sig }) => match std::fs::symlink_metadata(&path) {
                                Ok(m) if HostSig::of(&m) == sig => format!("{} is unchanged but could not be read", path.display()),
                                Ok(_) => format!("{} has changed since it was shown", path.display()),
                                Err(e) => format!("{}: {e}", path.display()),
                            },
                            Some(_) => "known, but not as a file on this machine".to_string(),
                            None => match self.left_on.get(&blob) {
                                Some(l) => format!(
                                    "part of the incremental state `{}` left on worker {:x}{}",
                                    l.1,
                                    l.0,
                                    if w.has_blobs.lock().contains(&blob) { " (believed to be on the asking worker)" } else { "" }
                                ),
                                None => "not in the store and never shown from a file".to_string(),
                            },
                        }
                    };
                    eprintln!("rargod: worker {:x} asked for content {blob}, which cannot be served: {why}", w.id);
                    return Resp::Blob(None);
                };
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

    /// Picks a worker with a free core; with `among`, only one of those.
    fn lease(&self, among: Option<&[u64]>) -> Option<Arc<Worker>> {
        let ws = self.workers.lock();
        let mut best: Option<&Arc<Worker>> = None;
        for w in ws.iter().filter(|w| w.alive.load(Ordering::Relaxed) && among.is_none_or(|ids| ids.contains(&w.id))) {
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

    fn give_back(&self, w: &Worker) {
        w.free.fetch_add(1, Ordering::Relaxed);
        // Whoever is woken may be waiting for another worker's core.
        self.freed.notify_waiters();
    }

    /// A worker core, as soon as there is one.
    async fn lease_wait(&self, among: Option<&[u64]>) -> Arc<Worker> {
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            // Registered before looking, so a core freed in between is not missed.
            freed.as_mut().enable();
            if let Some(w) = self.lease(among) {
                return w;
            }
            freed.await;
        }
    }

    /// The workers that hold the state `blobs` (all but `SEED_GAP` of it,
    /// and that remainder is here to be sent).
    fn seed_holders(&self, blobs: &[(Hash, u64)], store: &Store) -> Vec<u64> {
        let ws = self.workers.lock();
        ws.iter()
            .filter(|w| w.alive.load(Ordering::Relaxed))
            .filter(|w| {
                let has = w.has_blobs.lock();
                let mut gap = 0u64;
                for (h, size) in blobs {
                    if has.contains(h) {
                        continue;
                    }
                    gap += size;
                    if gap > SEED_GAP || !store.has(h) {
                        return false;
                    }
                }
                true
            })
            .map(|w| w.id)
            .collect()
    }

    /// A connected worker that holds `blob`.
    fn holder(&self, blob: &Hash) -> Option<Arc<Worker>> {
        let ws = self.workers.lock();
        ws.iter().find(|w| w.alive.load(Ordering::Relaxed) && w.has_blobs.lock().contains(blob)).cloned()
    }

    /// Sends workers the incremental states compiles made here, newest
    /// first, while their links have nothing else to carry. A compile
    /// that starts from a state is only given to a worker that holds it
    /// (D56), so a crate whose state is only here stays here; this is
    /// what lets its next compile go to a worker. Only what a worker
    /// lacks is sent, by chunk.
    async fn send_states_ahead(self: Arc<Self>, engine: Arc<Engine>) {
        loop {
            let next = {
                let mut q = self.ahead.lock();
                let next = q.next();
                if next.is_some() {
                    self.ahead_busy.store(true, Ordering::SeqCst);
                }
                next
            };
            let Some(blobs) = next else {
                self.ahead_wake.notified().await;
                continue;
            };
            let workers: Vec<Arc<Worker>> = self.workers.lock().iter().filter(|w| w.alive.load(Ordering::Relaxed)).cloned().collect();
            for w in workers {
                let missing: Vec<(Hash, u64)> = {
                    let has = w.has_blobs.lock();
                    blobs.iter().filter(|(h, _)| !has.contains(h) && engine.store.has(h)).copied().collect()
                };
                let bytes: u64 = missing.iter().map(|(_, s)| *s).sum();
                if missing.is_empty() || bytes > AHEAD_MAX {
                    continue;
                }
                // Behind whatever actions are waiting for on this link.
                for _ in 0..60 {
                    let busy = w.link_free_at.lock().saturating_duration_since(std::time::Instant::now());
                    if busy.is_zero() {
                        break;
                    }
                    tokio::time::sleep(busy.min(Duration::from_millis(500))).await;
                }
                if !w.alive.load(Ordering::Relaxed) {
                    continue;
                }
                // (Counted: what was sent, which is less than the files'
                // sizes where the worker held some of their chunks.)
                let sent: u64 = tokio::task::block_in_place(|| missing.iter().map(|(h, _)| self.push_blob(&w, *h)).sum());
                self.stats.states_sent_ahead.fetch_add(1, Ordering::Relaxed);
                self.stats.state_ahead_bytes.fetch_add(sent, Ordering::Relaxed);
            }
            self.ahead_busy.store(false, Ordering::SeqCst);
            self.settle_wake.notify_waiters();
        }
    }

    /// Fetches the files of incremental states that compiles left on
    /// workers, oldest first, one at a time and only on a core nothing else
    /// wants: putting a file together and checking it costs the CPU time
    /// the builds are short of. A compile that needs a state sooner fetches
    /// it itself (`bring_home`).
    async fn bring_states_home(self: Arc<Self>, engine: Arc<Engine>) {
        loop {
            let wake = self.home_wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            // A file stays queued while its delay has not elapsed, so a
            // caller that asks whether transfers are finished does not wait
            // out that delay. The flag is set under the same lock as the
            // pop, before anything awaits.
            let due = {
                let mut q = self.home_queue.lock();
                match q.front() {
                    Some((_, _, _, since)) => match state_home_after().checked_sub(since.elapsed()) {
                        Some(wait) if !wait.is_zero() => Err(wait),
                        _ => {
                            self.home_fetching.store(true, Ordering::SeqCst);
                            Ok(q.pop_front())
                        }
                    },
                    None => Ok(None),
                }
            };
            match due {
                Ok(None) => wake.await,
                Err(wait) => {
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = wake => {}
                    }
                }
                Ok(Some((w, h, size, _))) => {
                    if !engine.store.has(&h) && w.alive.load(Ordering::Relaxed) {
                        let permit = engine.sched.acquire(crate::sched::Priority::Idle, 0).await;
                        match self.blobs.fetch(&w.peer, &h).await {
                            Ok(()) => {
                                self.stats.state_homed_blobs.fetch_add(1, Ordering::Relaxed);
                                self.stats.state_homed_bytes.fetch_add(size, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("rargod: a file of an incremental state did not come home from worker {:x}: {e:#}", w.id)
                            }
                        }
                        drop(permit);
                    }
                    self.home_fetching.store(false, Ordering::SeqCst);
                    self.settle_wake.notify_waiters();
                }
            }
        }
    }

    /// Returns when every blob pushed to a worker has been acknowledged,
    /// nothing is queued or being sent ahead, and no incremental state that
    /// is already due has still to come home. A file whose delay has not
    /// elapsed is left where it is.
    pub async fn settle(&self) {
        loop {
            let wake = self.settle_wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.transfers_idle() {
                return;
            }
            wake.await;
        }
    }

    fn transfers_idle(&self) -> bool {
        {
            let q = self.ahead.lock();
            if self.ahead_busy.load(Ordering::SeqCst) || !q.is_empty() {
                return false;
            }
        }
        {
            let q = self.home_queue.lock();
            if self.home_fetching.load(Ordering::SeqCst) {
                return false;
            }
            let after = state_home_after();
            if q.iter().any(|(_, _, _, since)| since.elapsed() >= after) {
                return false;
            }
        }
        // After the queues: a send that took an item off a queue has either
        // not started (the queue or the flag still says so) or has put the
        // blob here.
        let ws = self.workers.lock();
        for w in ws.iter() {
            if w.alive.load(Ordering::Relaxed) && !w.in_flight.lock().is_empty() {
                return false;
            }
        }
        true
    }

    /// Sends a blob the worker is expected to need, leaving out the chunks
    /// it is known to hold.
    fn push_blob(&self, w: &Worker, h: Hash) -> u64 {
        // Another action's push may have sent it since this one was planned.
        // (Claimed before sending: pushes planned together run together.)
        if !w.has_blobs.lock().insert(h) {
            return 0;
        }
        let Some(recipe) = self.blobs.recipe(&h) else {
            w.has_blobs.lock().remove(&h);
            eprintln!("rargod: content {h} was to be sent ahead to worker {:x} but cannot be read here", w.id);
            return 0;
        };
        let missing: Vec<u32> = {
            let has = w.has_chunks.lock();
            (0..recipe.len() as u32).filter(|i| !has.contains(&recipe[*i as usize].hash)).collect()
        };
        let total: u64 = recipe.iter().map(|c| c.len as u64).sum();
        let need: u64 = missing.iter().map(|i| recipe[*i as usize].len as u64).sum();
        w.pushed_bytes.fetch_add(need, Ordering::Relaxed);
        w.skipped_bytes.fetch_add(total - need, Ordering::Relaxed);
        // When the link will have carried this, at the rate seen so far.
        {
            let secs = need as f64 / self.bandwidth.load(Ordering::Relaxed).max(1) as f64;
            let mut free_at = w.link_free_at.lock();
            *free_at = (*free_at).max(std::time::Instant::now()) + Duration::from_secs_f64(secs);
            let mut fl = w.in_flight.lock();
            let now = std::time::Instant::now();
            if fl.len() > 4096 {
                fl.retain(|_, at| *at > now);
            }
            fl.insert(h, *free_at + IN_FLIGHT_SLACK);
        }
        // Cut into messages of about a megabyte, counted first: the
        // worker has to know when it has them all.
        let mut batches: Vec<Vec<u32>> = vec![Vec::new()];
        let mut bytes = 0usize;
        for i in missing {
            if bytes + recipe[i as usize].len as usize > BATCH_BYTES && !batches.last().unwrap().is_empty() {
                batches.push(Vec::new());
                bytes = 0;
            }
            bytes += recipe[i as usize].len as usize;
            batches.last_mut().unwrap().push(i);
        }
        let of = batches.len() as u32;
        for (part, batch) in batches.iter().enumerate() {
            let chunks = match self.blobs.serve_chunks(&h, batch) {
                Ok(c) => batch.iter().copied().zip(c).collect(),
                Err(e) => {
                    eprintln!("rargod: content {h} could not be read for worker {:x}: {e:#}", w.id);
                    Vec::new()
                }
            };
            w.peer.post(&Msg::Push {
                blob: h,
                recipe: if part == 0 { (*recipe).clone() } else { Vec::new() },
                chunks,
                part: part as u32,
                of,
            });
        }
        w.has_chunks.lock().extend(recipe.iter().map(|c| c.hash));
        need
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
    /// Predicts what an action will ask, from its last trace (or, for one
    /// that has never run, from another run of the same program and its
    /// command line), and answers in advance with what the view says now.
    /// Returns the answers, the directory listings, and the contents the
    /// worker lacks.
    #[allow(clippy::type_complexity)]
    async fn predict(
        &self,
        w: &Arc<Worker>,
        engine: &Engine,
        spec: &ActionSpec,
        key: &Hash,
        view: &Arc<dyn View>,
        ra: &Arc<RemoteAction>,
    ) -> Result<(Vec<(Vec<u8>, Vec<u8>, WLooked)>, Vec<(Vec<u8>, Vec<(Vec<u8>, u8)>)>, Vec<Hash>, Vec<Vec<u8>>)> {
        let mut closed: Vec<Vec<u8>> = Vec::new();
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
                obs.extend(view.hints().into_iter().map(|p| (VPath::from(p), Obs::Dir)));
                // What the command line names as a dependency is read for
                // certain: sent ahead, it is there when the compiler opens
                // it, instead of one round trip per crate while it waits.
                obs.extend(
                    extern_paths(&spec.argv).into_iter().map(|p| (VPath::from(p), Obs::File { hash: Hash::ZERO, exec: false })),
                );
                // A compiler reads its crate's sources, and asks for many
                // names beside them that do not exist (`foo.rs` or
                // `foo/mod.rs`?). The source tree goes along whole: its
                // listings, which answer those questions, and its files.
                // A compile that links reads the standard library's object
                // code; until one such compile has run here there is no
                // history to say so.
                if spec.kind == ActionKind::Rustc && links(&spec.argv) && engine.profile(spec).is_none() {
                    if let Some(program) = spec.argv.first() {
                        let view = view.clone();
                        let program = program.clone();
                        let libs = tokio::task::spawn_blocking(move || sysroot_libs(&*view, &program)).await?;
                        obs.extend(libs.into_iter().map(|p| (VPath::from(p), Obs::File { hash: Hash::ZERO, exec: false })));
                    }
                }
                if spec.kind == ActionKind::Rustc {
                    if let Some(root) = crate_root(&spec.argv, &spec.cwd) {
                        let view = view.clone();
                        let (dirs, files) = tokio::task::spawn_blocking(move || source_tree(&*view, &root)).await?;
                        obs.extend(files.into_iter().map(|p| (VPath::from(p), Obs::File { hash: Hash::ZERO, exec: false })));
                        obs.extend(dirs.iter().map(|d| (VPath::from(d.clone()), Obs::Listing(Hash::ZERO))));
                        closed = dirs;
                    }
                }
                (!obs.is_empty()).then_some(obs)
            }
        };
        if let Some(obs) = predicted {
            let view = view.clone();
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
                        // An answer that names a content (one this server
                        // has hashed before) promises that the content can
                        // be asked for by that name. For a file on this
                        // machine's disk that means remembering where it
                        // is; without this the worker's request for it was
                        // refused and its compiler failed on the file.
                        if let (Looked::File(f), WLooked::File { hash: Some(h), .. }) = (&looked, &w) {
                            if let FileSrc::Host { .. } = &f.src {
                                host_srcs.entry(*h).or_insert_with(|| f.src.clone());
                                ra2.served.lock().push(f.src.clone());
                            }
                        }
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
            let fl = w.in_flight.lock();
            let now = std::time::Instant::now();
            incoming = files.into_iter().filter(|h| !has.contains(h) || still_in_flight(&fl, h, now)).collect();
            incoming.sort();
            incoming.dedup();
        }
        // Only what was really listed counts as complete.
        closed.retain(|d| listings.iter().any(|(l, _)| l == d));
        Ok((prefill, listings, incoming, closed))
    }

    async fn run_on(
        &self,
        w: &Arc<Worker>,
        engine: &Engine,
        spec: &ActionSpec,
        key: &Hash,
        req: &Request,
        voice: &Voice,
    ) -> RunResult {
        let action = self.next_action.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t0 = std::time::Instant::now();
        // The action's side directories (its incremental state) are shown
        // as they are: it is only sent to a worker that holds that state.
        let view: Arc<dyn View> = req.view.clone();
        let ra = Arc::new(RemoteAction {
            view: view.clone(),
            events: tx,
            served: Mutex::new(Vec::new()),
            asked: AtomicU64::new(0),
        });
        w.actions.insert(action, ra.clone());
        struct Cleanup<'a>(&'a Pool, &'a Worker, u64, std::cell::Cell<bool>);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                self.1.actions.remove(&self.2);
                if !self.3.get() {
                    // Given up before the worker finished (a local core ran
                    // it first, or the client left): the worker's core is
                    // only free once it has stopped.
                    self.1.peer.post(&Msg::Cancel { action: self.2 });
                }
                self.0.give_back(self.1);
            }
        }
        let cleanup = Cleanup(self, w, action, std::cell::Cell::new(false));

        let (mut prefill, mut listings, incoming, mut closed) = self.predict(w, engine, spec, key, &view, &ra).await?;
        if !req.seed_blobs.is_empty() {
            // The compiler walks its whole state before anything else: the
            // tree goes with the action instead of one question per file.
            let (view, content, dirs) = (view.clone(), self.content.clone(), spec.discard.clone());
            let (p, l) = tokio::task::spawn_blocking(move || side_tree(&*view, &content, &dirs)).await?;
            closed.extend(l.iter().map(|(d, _)| d.clone()));
            prefill.extend(p);
            listings.extend(l);
        }
        self.stats.prefilled_entries.fetch_add(prefill.len() as u64, Ordering::Relaxed);
        // The decision of which trees to send and the post that carries them
        // share one lock, so two actions cannot each believe the other is
        // sending a path. Serializing the manifests is synchronous work.
        tokio::task::block_in_place(|| {
            let mut held = w.store_paths.lock();
            let mut closure: Vec<(Vec<u8>, Hash)> = view.nix_closure().to_vec();
            closure.sort_by(|a, b| a.0.cmp(&b.0));
            let mut store_paths = Vec::new();
            for (name, root) in &closure {
                if held.contains(name) {
                    continue;
                }
                match crate::nixmanifest::load(engine.store.db(), name) {
                    Some(kind) if crate::nixmanifest::identity(&kind) == *root => {
                        held.insert(name.clone());
                        store_paths.push(crate::nixmanifest::WirePath { name: name.clone(), kind });
                    }
                    Some(_) => eprintln!(
                        "rargod: nix manifest for {} does not match the closure",
                        String::from_utf8_lossy(name)
                    ),
                    None => eprintln!("rargod: nix manifest for {} is missing", String::from_utf8_lossy(name)),
                }
            }
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
                closed,
                closure,
                store_paths,
            });
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
            if !matches!(ev, Event::Done(_)) && !voice.claim(REMOTE) {
                // The local copy spoke first and is the one being shown;
                // this run is about to be withdrawn.
                std::future::pending::<()>().await;
            }
            match ev {
                Event::Stdout(d) => {
                    req.observer.stdout(&d);
                    stdout.extend_from_slice(&d);
                }
                Event::Early(path, entry) => {
                    // The worker made this file: dependents sent to it do
                    // not need it pushed back.
                    if let MEntry::File { hash, .. } = &entry {
                        w.has_blobs.lock().insert(*hash);
                    }
                    early.lock().insert(path, entry);
                }
                Event::Line(line) => {
                    let live = RemoteLive { early: &early, pool: self, worker: w };
                    let obs = req.observer.clone();
                    tokio::task::block_in_place(|| obs.stderr_line(&line, Some(&live)));
                    stderr.extend_from_slice(&line);
                    stderr.push(b'\n');
                }
                Event::Done(r) => {
                    cleanup.3.set(true);
                    break r.map_err(|e| anyhow!("worker: {e}"))?;
                }
            }
        };

        let t_done = t0.elapsed();
        // Bring the outputs home. What the action left in the directories
        // that are kept on the side (its incremental state) stays on the
        // worker for now: the next compile of the crate there uses it in
        // place, and it comes home in the background or when a compile
        // here needs it.
        let mut wanted: Vec<Hash> = Vec::new();
        let mut wanted_bytes = 0u64;
        for (p, e) in &done.entries {
            if let MEntry::File { hash, size, .. } = e {
                if !spec.discard.iter().any(|d| vpath_starts_with(p, d)) && !engine.store.has(hash) && !wanted.contains(hash) {
                    wanted.push(*hash);
                    wanted_bytes += size;
                }
            }
        }
        self.blobs.fetch_all(&w.peer, &wanted).await.context("fetching an output from the worker")?;
        {
            let mut has = w.has_blobs.lock();
            for (_, e) in done.entries.iter().chain(done.side.iter()) {
                if let MEntry::File { hash, .. } = e {
                    has.insert(*hash);
                }
            }
        }
        {
            let mut left: Vec<(Hash, u64)> = done
                .side
                .iter()
                .filter_map(|(_, e)| match e {
                    MEntry::File { hash, size, .. } if !engine.store.has(hash) => Some((*hash, *size)),
                    _ => None,
                })
                .collect();
            left.sort();
            left.dedup();
            if !left.is_empty() {
                for (h, _) in &left {
                    self.left_on.insert(*h, (w.id, spec.label.clone()));
                }
                self.stats.state_left_blobs.fetch_add(left.len() as u64, Ordering::Relaxed);
                self.stats.state_left_bytes.fetch_add(left.iter().map(|(_, s)| s).sum::<u64>(), Ordering::Relaxed);
                let now = std::time::Instant::now();
                self.home_queue.lock().extend(left.into_iter().map(|(h, s)| (w.clone(), h, s, now)));
                self.home_wake.notify_one();
                self.settle_wake.notify_waiters();
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
    fn holds(&self, blob: &Hash) -> bool {
        self.holder(blob).is_some()
    }

    fn offer_state(&self, whose: (Hash, String), blobs: Arc<Vec<(Hash, u64)>>) {
        if !state_ahead() || self.worker_count() == 0 {
            return;
        }
        self.ahead.lock().offer(whose, blobs);
        self.ahead_wake.notify_one();
        self.settle_wake.notify_waiters();
    }

    fn bring_home<'a>(
        &'a self,
        blobs: &'a [Hash],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            // Grouped by the worker that holds them.
            let mut by_worker: Vec<(Arc<Worker>, Vec<Hash>)> = Vec::new();
            for h in blobs {
                if self.blobs.store.has(h) {
                    continue;
                }
                let Some(w) = self.holder(h) else { bail!("no connected worker holds {h}") };
                match by_worker.iter_mut().find(|(x, _)| x.id == w.id) {
                    Some((_, v)) => v.push(*h),
                    None => by_worker.push((w, vec![*h])),
                }
            }
            for (w, hashes) in by_worker {
                self.stats.state_needed_blobs.fetch_add(hashes.len() as u64, Ordering::Relaxed);
                self.blobs.fetch_all(&w.peer, &hashes).await?;
            }
            Ok(())
        })
    }

    fn try_run<'a>(
        &'a self,
        engine: &'a Engine,
        spec: &'a ActionSpec,
        key: &'a Hash,
        req: &'a Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RunResult>> + Send + 'a>> {
        Box::pin(async move {
            // Planning and nextest (installed on the server) run locally.
            // Host networking stays local too. Workers get the selected Nix
            // closure through RemoteView.
            if matches!(spec.kind, ActionKind::Plan | ActionKind::Nextest)
                || spec.net != NetMode::Loopback
            {
                return None;
            }
            // A crate that is compiled incrementally (a workspace member)
            // goes where its state is. A compile that starts from a state
            // is short only with that state at hand, and states are too
            // big to send along (D31): such a compile is offered only to
            // workers that hold the state it would start from, because
            // they made it or one like it. A first compile, with no state
            // anywhere, can run on any worker; what it leaves stays there
            // and comes home later.
            let seeded = !spec.discard.is_empty();
            let mut holders: Option<Vec<u64>> = None;
            let mut away = false;
            if seeded {
                if !remote_incr() {
                    self.stats.kept_local_incremental.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                if !req.seed_blobs.is_empty() {
                    let ids = self.seed_holders(&req.seed_blobs, &engine.store);
                    if ids.is_empty() {
                        self.stats.kept_local_incremental.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                    away = !engine.seed_away(req).is_empty();
                    holders = Some(ids);
                }
            }
            // A free local core is the better place, unless the state the
            // compile starts from would have to be fetched first and the
            // worker that holds it has a core free as well. A core held by
            // a compiler's helper thread is as busy as one held by an
            // action.
            let mem = engine.estimate_mem_kb(key, spec.kind);
            let (_, waiting, _, _) = engine.sched.snapshot();
            if trace_remote() && seeded {
                eprintln!(
                    "rargod: `{}`: state of {} files, {} of them not here, held by {:?}; {} local cores free, {} waiting",
                    spec.label,
                    req.seed_blobs.len(),
                    engine.seed_away(req).len(),
                    holders,
                    engine.sched.free_tokens(),
                    waiting
                );
            }
            let w = if engine.sched.free_tokens() > 0 && waiting == 0 {
                match if away { self.lease(holders.as_deref()) } else { None } {
                    Some(w) => w,
                    None => {
                        self.stats.kept_local_no_wait.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                }
            } else {
                // Every local core is busy: the action goes to whichever
                // comes free first, a core here or a core on a worker.
                tokio::select! {
                    biased;
                    w = self.lease_wait(holders.as_deref()) => w,
                    permit = engine.sched.acquire(req.prio, mem) => {
                        self.stats.kept_local_first.fetch_add(1, Ordering::Relaxed);
                        if let Err(e) = engine.seed_home(req).await {
                            return Some(Err(e));
                        }
                        engine.stats.runs.fetch_add(1, Ordering::Relaxed);
                        let started = std::time::Instant::now();
                        let r = engine.execute(spec, req.view.clone(), req.uid, req.gid, &*req.observer).await;
                        drop(permit);
                        engine.note_action_wall(started.elapsed());
                        return Some(r);
                    }
                }
            };
            // Not when its inputs would take longer to arrive than it takes
            // to run: the worker would sit waiting for bytes. That counts
            // what is already queued on the link and what this action is
            // known to read: from its last trace, or, if it has never run,
            // from another run of the same program (the toolchain).
            let own = engine.traces.traces(key).first().map(|(_, t)| t.clone());
            let known = own.clone().or_else(|| engine.profile(spec));
            if known.is_none() && !eager_dispatch() {
                // Nothing is known about what it reads (the first compile a
                // server ever runs, a build script's own run): the worker
                // would ask for everything one question at a time,
                // toolchain included. It runs here, and what it reads is
                // then known for the next of its kind.
                self.give_back(&w);
                self.stats.kept_local_transfer.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let missing: u64 = match &known {
                Some(t) => {
                    let has = w.has_blobs.lock();
                    t.obs
                        .iter()
                        .filter(|(p, _)| own.is_some() || !self.profile_exclude.iter().any(|x| vpath_starts_with(p, x)))
                        .filter_map(|(_, o)| match o {
                            Obs::File { hash, .. } if !has.contains(hash) => Some(engine.store.size_of(hash).unwrap_or(256 << 10)),
                            _ => None,
                        })
                        .sum()
                }
                None => 0,
            };
            let queued = w.link_free_at.lock().saturating_duration_since(std::time::Instant::now());
            let bytes_time = queued + Duration::from_secs_f64(missing as f64 / self.bandwidth.load(Ordering::Relaxed).max(1) as f64);
            if trace_remote() && bytes_time > engine.avg_action_wall() {
                eprintln!(
                    "rargod: `{}` not sent to {:x}: {:.1} MiB to send, link busy for {:.2}s, an action takes {:.2}s",
                    spec.label,
                    w.id,
                    missing as f64 / 1048576.0,
                    queued.as_secs_f64(),
                    engine.avg_action_wall().as_secs_f64()
                );
            }
            if bytes_time > engine.avg_action_wall() && !eager_dispatch() {
                // A worker that lacks the toolchain would never be given
                // anything; send it what every compile reads, once, so that
                // it is useful a few seconds from now.
                if own.is_none() && !w.warmed.swap(true, Ordering::SeqCst) {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    let ra = Arc::new(RemoteAction {
                        view: req.view.clone(),
                        events: tx,
                        served: Mutex::new(Vec::new()),
                        asked: AtomicU64::new(0),
                    });
                    if let Ok((_, _, incoming, _)) = self.predict(&w, engine, spec, key, &req.view, &ra).await {
                        tokio::task::block_in_place(|| {
                            for h in incoming {
                                self.push_blob(&w, h);
                            }
                        });
                    }
                }
                self.give_back(&w);
                self.stats.kept_local_transfer.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            self.stats.dispatched.fetch_add(1, Ordering::Relaxed);
            engine.stats.remote_runs.fetch_add(1, Ordering::Relaxed);
            if seeded {
                let which = if req.seed_blobs.is_empty() { &self.stats.unseeded_remote } else { &self.stats.seeded_in_place };
                which.fetch_add(1, Ordering::Relaxed);
            }
            // Hedge: should a local core that nothing else wants fall idle
            // while the worker has the action (it may be fetching a
            // toolchain, or simply be slower), the action also runs here,
            // and whichever copy finishes first is the result; the other is
            // stopped. Only one copy may be heard: the first to produce
            // output or announce metadata is kept and the other stopped at
            // once, because showing output twice, or announcing two
            // different metadata files, is worse than waiting.
            let voice = Voice::default();
            let gated = Gated { inner: &*req.observer, voice: &voice };
            let mut remote = Some(Box::pin(self.run_on(&w, engine, spec, key, req, &voice)));
            type Local<'f> = std::pin::Pin<Box<dyn std::future::Future<Output = RunResult> + Send + 'f>>;
            let mut local: Option<(Local<'_>, crate::sched::Permit, std::time::Instant)> = None;
            loop {
                match voice.owner() {
                    REMOTE if local.is_some() => {
                        self.stats.hedge_stopped.fetch_add(1, Ordering::Relaxed);
                        local = None;
                    }
                    LOCAL => remote = None,
                    _ => {}
                }
                tokio::select! {
                    r = async { remote.as_mut().expect("guarded").await }, if remote.is_some() => match r {
                        Ok(r) => {
                            if local.is_some() {
                                self.stats.hedge_stopped.fetch_add(1, Ordering::Relaxed);
                            }
                            return Some(Ok(r));
                        }
                        Err(e) => {
                            // The worker failed, not the action: run it here.
                            eprintln!("rargod: worker {:x} failed `{}`: {e:#}; running locally", w.id, spec.label);
                            self.stats.worker_failures.fetch_add(1, Ordering::Relaxed);
                            engine.stats.remote_failed.fetch_add(1, Ordering::Relaxed);
                            remote = None;
                            if local.is_none() {
                                return None;
                            }
                        }
                    },
                    r = async { (&mut local.as_mut().expect("guarded").0).await }, if local.is_some() => {
                        let (_, permit, started) = local.take().expect("guarded");
                        drop(permit);
                        engine.note_action_wall(started.elapsed());
                        self.stats.hedge_won.fetch_add(1, Ordering::Relaxed);
                        // Dropping `remote` withdraws the action from the worker.
                        return Some(r);
                    }
                    // (Not while the state the compile starts from is on
                    // the worker: fetching it would cost more than the
                    // idle core is worth.)
                    permit = engine.sched.acquire(crate::sched::Priority::Idle, mem),
                        if local.is_none() && remote.is_some() && !away && hedge() && voice.owner() == NOBODY =>
                    {
                        self.stats.hedged.fetch_add(1, Ordering::Relaxed);
                        engine.stats.runs.fetch_add(1, Ordering::Relaxed);
                        let run: Local<'_> = Box::pin(engine.execute(spec, req.view.clone(), req.uid, req.gid, &gated));
                        local = Some((run, permit, std::time::Instant::now()));
                    }
                    _ = voice.changed.notified() => {}
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn contents_go_behind_questions_and_actions() {
        use super::{is_bulk, Msg, Req, Resp};
        use crate::hash::Hash;
        let h = Hash([1; 32]);
        assert!(is_bulk(&Msg::Push { blob: h, recipe: Vec::new(), chunks: Vec::new(), part: 0, of: 1 }));
        assert!(is_bulk(&Msg::Resp { rid: 1, body: Resp::Chunks(Vec::new()) }));
        assert!(is_bulk(&Msg::Resp { rid: 1, body: Resp::Blob(None) }));
        assert!(!is_bulk(&Msg::Resp { rid: 1, body: Resp::Opened(Ok(h)) }));
        assert!(!is_bulk(&Msg::Resp { rid: 1, body: Resp::Recipe(None) }));
        assert!(!is_bulk(&Msg::Req { rid: 1, body: Req::Chunks { blob: h, idx: vec![0] } }));
        assert!(!is_bulk(&Msg::Cancel { action: 1 }));
        assert!(!is_bulk(&Msg::Stdout { action: 1, data: Vec::new() }));
        // An acknowledgement has to pass a blob that is still queued, or a
        // caller waiting on it would wait for the bytes twice.
        assert!(!is_bulk(&Msg::Stored { blob: h }));
        assert!(!is_bulk(&Msg::Bye));
    }

    #[test]
    fn a_blob_is_in_flight_until_the_link_has_carried_it() {
        use crate::hash::Hash;
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let mut fl = std::collections::HashMap::new();
        fl.insert(Hash([1; 32]), now + Duration::from_secs(1));
        fl.insert(Hash([2; 32]), now);
        assert!(super::still_in_flight(&fl, &Hash([1; 32]), now));
        assert!(!super::still_in_flight(&fl, &Hash([2; 32]), now));
        assert!(!super::still_in_flight(&fl, &Hash([3; 32]), now));
    }

    #[test]
    fn states_wait_newest_first_one_per_crate_and_worktree() {
        use crate::hash::Hash;
        use std::sync::Arc;
        let state = |n: u8| Arc::new(vec![(Hash([n; 32]), n as u64)]);
        let who = |c: u8, wt: &str| (Hash([c; 32]), wt.to_string());
        let mut q = super::AheadQueue::default();
        q.offer(who(1, "/a"), state(1));
        q.offer(who(2, "/a"), state(2));
        q.offer(who(1, "/b"), state(3));
        // The same crate in the same worktree again: only its newest waits.
        q.offer(who(1, "/a"), state(4));
        assert_eq!(q.len(), 3);
        let order: Vec<u64> = std::iter::from_fn(|| q.next()).map(|b| b[0].1).collect();
        assert_eq!(order, [4, 3, 2]);
        // Bounded: the oldest give way.
        for n in 0..(super::AheadQueue::KEPT + 10) {
            q.offer((Hash([9; 32]), format!("/wt{n}")), state(7));
        }
        assert_eq!(q.len(), super::AheadQueue::KEPT);
    }

    #[test]
    fn crate_roots_are_found_and_made_absolute() {
        let argv = |a: &[&str]| -> Vec<Vec<u8>> { a.iter().map(|s| s.as_bytes().to_vec()).collect() };
        assert_eq!(
            super::crate_root(&argv(&["rustc", "--crate-name", "x", "--edition=2021", "crates/x/src/lib.rs", "--out-dir", "/w/t"]), b"/w"),
            Some(b"/w/crates/x/src/lib.rs".to_vec())
        );
        assert_eq!(
            super::crate_root(&argv(&["rustc", "--crate-name", "y", "/c/registry/src/i/y-1.0/src/lib.rs"]), b"/c/registry/src/i/y-1.0"),
            Some(b"/c/registry/src/i/y-1.0/src/lib.rs".to_vec())
        );
        assert_eq!(super::crate_root(&argv(&["rustc", "-vV"]), b"/w"), None);
    }

    #[test]
    fn extern_files_are_read_from_the_command_line() {
        let argv: Vec<Vec<u8>> = [
            "rustc",
            "--crate-name",
            "app",
            "--extern",
            "util=/w/target/debug/deps/libutil-1.rmeta",
            "--extern",
            "noprelude:alloc=/w/t/liballoc.rlib",
            "--extern=mac=/w/t/libmac.so",
            "--extern",
            "proc_macro",
            "-L",
            "dependency=/w/target/debug/deps",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        let got: Vec<String> = super::extern_paths(&argv).into_iter().map(|p| String::from_utf8(p).unwrap()).collect();
        assert_eq!(got, ["/w/target/debug/deps/libutil-1.rmeta", "/w/t/liballoc.rlib", "/w/t/libmac.so"]);
    }
}
