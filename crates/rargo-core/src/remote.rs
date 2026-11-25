use crate::chunk::{self, ChunkIndex, ChunkRef, Recipe};
use crate::engine::{Engine, Live, Observer, RemoteRunner, Request};
use crate::hash::Hash;
use crate::net::{Conn, Endpoint};
use crate::sandbox::NetMode;
use crate::store::Store;
use crate::trace::*;
use crate::view::*;
use crate::pool_state::{self, Outcome, Settle, Who};
use crate::worker_state;
use anyhow::{Context, Result, anyhow, bail};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};


#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    side: Vec<(Vec<u8>, MEntry)>,
    pub tainted: bool,
}

#[derive(Serialize, Deserialize)]
pub enum Req {
    Lookup { action: u64, dir: Vec<u8>, name: Vec<u8> },
    List { action: u64, dir: Vec<u8> },
    Open { action: u64, dir: Vec<u8>, name: Vec<u8> },
    Recipe { blob: Hash },
    Chunks { blob: Hash, idx: Vec<u32> },
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
        prefill: Vec<(Vec<u8>, Vec<u8>, WLooked)>,
        listings: Vec<(Vec<u8>, Vec<(Vec<u8>, u8)>)>,
        untraced: Vec<Vec<u8>>,
        incoming: Vec<Hash>,
        closed: Vec<Vec<u8>>,
        closure: Vec<(Vec<u8>, Hash)>,
        store_paths: Vec<crate::nixmanifest::WirePath>,
    },
    Push { blob: Hash, recipe: Recipe, chunks: Vec<(u32, Vec<u8>)>, part: u32, of: u32 },
    Stdout { action: u64, data: Vec<u8> },
    StderrLine { action: u64, line: Vec<u8> },
    Early { action: u64, path: Vec<u8>, entry: MEntry },
    Done { action: u64, result: Result<WDone, String> },
    Cancel { action: u64 },
    Stored { blob: Hash },
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


pub struct Peer {
    pub conn: Arc<Conn>,
    calls: Mutex<pool_state::Calls<oneshot::Sender<Resp>>>,
    out: mpsc::UnboundedSender<Vec<u8>>,
    bulk: mpsc::UnboundedSender<Vec<u8>>,
}

fn is_bulk(m: &Msg) -> bool {
    matches!(m, Msg::Push { .. } | Msg::Resp { body: Resp::Chunks(_) | Resp::Blob(_), .. })
}

impl Peer {
    pub fn new(conn: Arc<Conn>) -> (Arc<Peer>, mpsc::UnboundedReceiver<Msg>) {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (bulk_tx, mut bulk_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let peer = Arc::new(Peer { conn: conn.clone(), calls: Mutex::new(pool_state::Calls::default()), out: out_tx, bulk: bulk_tx });
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
                        let waiter = p.calls.lock().answer(rid);
                        if let Some(tx) = waiter {
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
            let lost = p.calls.lock().lost();
            drop(lost);
        });
        (peer, in_rx)
    }

    pub fn post(&self, m: &Msg) {
        if let Ok(b) = bincode::serialize(m) {
            let _ = if is_bulk(m) { self.bulk.send(b) } else { self.out.send(b) };
        }
    }

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
        let (tx, rx) = oneshot::channel();
        let rid = self.calls.lock().start(tx);
        self.post(&Msg::Req { rid, body });
        rx
    }

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

    pub fn call_blocking(&self, body: Req) -> Result<Resp> {
        let rx = self.start(body);
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


pub trait BlobSource: Send + Sync {
    fn open(&self, h: &Hash) -> io::Result<std::fs::File>;
}

struct StoreSource(Arc<Store>);
impl BlobSource for StoreSource {
    fn open(&self, h: &Hash) -> io::Result<std::fs::File> {
        self.0.open_blob(h)
    }
}

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

pub struct Blobs {
    pub store: Arc<Store>,
    pub index: ChunkIndex,
    source: Box<dyn BlobSource>,
    pub fetched_bytes: AtomicU64,
    pub reused_bytes: AtomicU64,
    pub fetched_blobs: AtomicU64,
    outlines: dashmap::DashMap<Hash, Arc<Outline>>,
}

pub struct Outline {
    pub cvs: Vec<crate::proto::outline::Cv>,
    pub lazy: Vec<(u64, u64)>,
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
            outlines: dashmap::DashMap::new(),
        }
    }

    pub fn outline(&self, h: &Hash) -> Option<Arc<Outline>> {
        if let Some(o) = self.outlines.get(h) {
            return Some(o.clone());
        }
        let f = self.source.open(h).ok()?;
        let len = f.metadata().ok()?.len() as usize;
        if len == 0 {
            return Some(Arc::new(Outline { cvs: Vec::new(), lazy: Vec::new() }));
        }
        use std::os::fd::AsRawFd;
        let o = unsafe {
            let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0);
            if p == libc::MAP_FAILED {
                return None;
            }
            let data = std::slice::from_raw_parts(p as *const u8, len);
            let cvs = if len as u64 <= crate::proto::outline::GROUP {
                Vec::new()
            } else {
                use rayon::prelude::*;
                data.par_chunks(crate::proto::outline::GROUP as usize)
                    .enumerate()
                    .map(|(i, g)| crate::proto::outline::group_cv(i, g))
                    .collect()
            };
            let o = Outline { cvs, lazy: chunk::debug_ranges(data) };
            libc::munmap(p, len);
            o
        };
        assert!(o.cvs.is_empty() || o.cvs.len() == crate::proto::outline::groups(len as u64));
        let o = Arc::new(o);
        self.outlines.insert(*h, o.clone());
        Some(o)
    }

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

    pub async fn fetch(&self, peer: &Peer, h: &Hash) -> Result<()> {
        if self.store.has(h) {
            return Ok(());
        }
        let Resp::Blob(Some((recipe, chunks))) = peer.call(Req::Blob { blob: *h }).await? else {
            bail!("peer does not have blob {h}");
        };
        self.assemble(peer, h, &recipe, chunks.into_iter().collect()).await
    }

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

const FETCH_TOGETHER: usize = 8;


#[derive(Debug)]
struct Wire;

#[derive(Debug)]
struct RunWire {
    spec: WSpec,
    uid: u32,
    gid: u32,
    untraced: Vec<Vec<u8>>,
    closure: Vec<Vec<u8>>,
}

impl std::fmt::Debug for Req {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Req::Lookup { .. } => "Lookup",
            Req::List { .. } => "List",
            Req::Open { .. } => "Open",
            Req::Recipe { .. } => "Recipe",
            Req::Chunks { .. } => "Chunks",
            Req::Blob { .. } => "Blob",
        })
    }
}

impl worker_state::Carried for Wire {
    type Run = RunWire;
    type Part = (u32, Recipe, Vec<(u32, Vec<u8>)>);
    type Serve = Req;
}

type WEffect = worker_state::Effect<Wire>;

pub struct WorkerState {
    peer: Mutex<Option<Arc<Peer>>>,
    blobs: Arc<Blobs>,
    engine: Arc<Engine>,
    handle: tokio::runtime::Handle,
    core: Mutex<worker_state::Worker<Wire>>,
    woken: parking_lot::Condvar,
    tasks: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
    born: std::time::Instant,
    lookups_local: AtomicU64,
    lookups_remote: AtomicU64,
    starved: AtomicU64,
    nix: DashMap<Vec<u8>, Arc<crate::nixmanifest::Installed>>,
}

fn blocking<T>(f: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current().is_ok() { tokio::task::block_in_place(f) } else { f() }
}

impl WorkerState {
    fn peer(&self) -> Option<Arc<Peer>> {
        self.peer.lock().clone()
    }

    fn now_ms(&self) -> u64 {
        self.born.elapsed().as_millis() as u64
    }

    fn ensure_blob(self: &Arc<Self>, h: &Hash) -> io::Result<()> {
        blocking(|| {
            let mut core = self.core.lock();
            loop {
                let has = self.blobs.store.has(h);
                match core.need(*h, has) {
                    worker_state::Need::Have => return Ok(()),
                    worker_state::Need::Wait => {
                        self.woken.wait_for(&mut core, Duration::from_secs(1));
                    }
                    worker_state::Need::Gone => {
                        self.starved.fetch_add(1, Ordering::SeqCst);
                        return Err(io::Error::other(format!("content {h} is not here and the server is gone")));
                    }
                    worker_state::Need::Fetch => {
                        drop(core);
                        let got = match self.peer() {
                            Some(peer) => self.handle.block_on(self.blobs.fetch(&peer, h)),
                            None => Err(anyhow!("no link to the server")),
                        };
                        self.core.lock().fetched(*h);
                        self.woken.notify_all();
                        return got.map_err(|e| {
                            self.starved.fetch_add(1, Ordering::SeqCst);
                            eprintln!("rargo worker: content {h} could not be had from the server: {e:#}");
                            io::Error::other(format!("{e:#}"))
                        });
                    }
                }
            }
        })
    }

    fn ask(&self, token: u64, q: worker_state::Query) -> Option<worker_state::Reply> {
        use worker_state::{Asked, Query, Reply};
        blocking(|| {
            let mut core = self.core.lock();
            loop {
                match core.query(token, q.clone()) {
                    Asked::Known(r) => {
                        self.lookups_local.fetch_add(1, Ordering::Relaxed);
                        return Some(r);
                    }
                    Asked::Gone => return None,
                    Asked::Wait => {
                        self.woken.wait_for(&mut core, Duration::from_secs(1));
                    }
                    Asked::Ask { ticket, action } => {
                        drop(core);
                        self.lookups_remote.fetch_add(1, Ordering::Relaxed);
                        let req = match &q {
                            Query::Lookup { dir, name } => Req::Lookup { action, dir: dir.clone(), name: name.clone() },
                            Query::List { dir } => Req::List { action, dir: dir.clone() },
                            Query::Open { dir, name } => Req::Open { action, dir: dir.clone(), name: name.clone() },
                        };
                        let got = match self.peer() {
                            Some(peer) => peer.call_blocking(req),
                            None => Err(anyhow!("no link to the server")),
                        };
                        let reply = match got {
                            Ok(Resp::Looked(w)) => Some(Reply::Looked(w)),
                            Ok(Resp::Listing(l)) => Some(Reply::Listing(l)),
                            Ok(Resp::Opened(o)) => Some(Reply::Opened(o)),
                            other => {
                                if let Query::Lookup { dir, name } = &q {
                                    let why = match other {
                                        Err(e) => format!("{e:#}"),
                                        Ok(_) => "unexpected reply".to_string(),
                                    };
                                    eprintln!(
                                        "rargo worker: action {action:x}: no answer about {}/{}: {why}",
                                        String::from_utf8_lossy(dir),
                                        String::from_utf8_lossy(name)
                                    );
                                }
                                None
                            }
                        };
                        self.core.lock().answered(ticket, reply.clone());
                        self.woken.notify_all();
                        return reply;
                    }
                }
            }
        })
    }

    fn server_msg(self: &Arc<Self>, msg: Msg) {
        use worker_state::{FromServer, Run};
        let from = match msg {
            Msg::Welcome => FromServer::Welcome,
            Msg::Req { rid, body } => FromServer::Ask {
                rid,
                serve: matches!(body, Req::Recipe { .. } | Req::Blob { .. } | Req::Chunks { .. }).then_some(body),
            },
            Msg::Push { blob, recipe, chunks, part, of } => {
                FromServer::Push { blob, first: part == 0, of, part: (part, recipe, chunks) }
            }
            Msg::Run { action, spec, uid, gid, prefill, listings, untraced, incoming, closed, closure, store_paths } => {
                for path in &store_paths {
                    if self.nix.contains_key(&path.name) {
                        continue;
                    }
                    match crate::nixmanifest::Installed::from_kind(&path.kind) {
                        Ok(inst) => {
                            self.nix.insert(path.name.clone(), Arc::new(inst));
                        }
                        Err(e) => eprintln!(
                            "rargo worker: nix path {} is not a tree: {e}",
                            String::from_utf8_lossy(&path.name)
                        ),
                    }
                }
                let mut closure: Vec<Vec<u8>> = closure.into_iter().map(|(n, _)| n).collect();
                closure.sort();
                FromServer::Run(Run {
                    action,
                    incoming,
                    prefill,
                    listings,
                    closed,
                    run: RunWire { spec, uid, gid, untraced, closure },
                })
            }
            Msg::Cancel { action } => FromServer::Cancel { action },
            _ => FromServer::Other,
        };
        let now = self.now_ms();
        let fx = {
            let mut core = self.core.lock();
            let mut fx = core.server(from, &|h| self.blobs.store.has(h), now);
            fx.extend(core.tick(now));
            fx
        };
        self.woken.notify_all();
        self.carry(fx);
    }

    fn tick(self: &Arc<Self>) {
        let fx = self.core.lock().tick(self.now_ms());
        self.carry(fx);
    }

    fn carry(self: &Arc<Self>, fx: Vec<WEffect>) {
        for e in fx {
            match e {
                WEffect::Connect { .. } | WEffect::Quit | WEffect::Hello => {
                    unreachable!("the link's own effects are carried out by worker_main")
                }
                WEffect::Serve { rid, serve } => {
                    let Some(peer) = self.peer() else { continue };
                    let blobs = self.blobs.clone();
                    self.handle.spawn_blocking(move || {
                        let resp = match serve {
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
                            _ => unreachable!("the core serves only content requests"),
                        };
                        peer.post(&Msg::Resp { rid, body: resp });
                    });
                }
                WEffect::Refuse { rid } => {
                    if let Some(peer) = self.peer() {
                        peer.post(&Msg::Resp { rid, body: Resp::Err("not a worker request".into()) });
                    }
                }
                WEffect::Assemble { blob, parts } => {
                    let Some(peer) = self.peer() else { continue };
                    let state = self.clone();
                    self.handle.spawn(async move {
                        let mut recipe = None;
                        let mut chunks = HashMap::new();
                        for (part, r, c) in parts {
                            if part == 0 {
                                recipe = Some(r);
                            }
                            chunks.extend(c);
                        }
                        let recipe = recipe.unwrap_or_default();
                        if let Err(e) = state.blobs.assemble(&peer, &blob, &recipe, chunks).await {
                            eprintln!("rargo worker: pushed blob {blob} could not be assembled: {e:#}");
                        }
                        let fx = state.core.lock().assembled(blob);
                        state.woken.notify_all();
                        state.carry(fx);
                    });
                }
                WEffect::Stored { blob } => {
                    if let Some(peer) = self.peer() {
                        peer.post(&Msg::Stored { blob });
                    }
                }
                WEffect::Start { token, action, run } => self.start(token, action, run),
                WEffect::Abort { token } => {
                    if let Some(task) = self.tasks.lock().remove(&token) {
                        task.abort();
                    }
                }
                WEffect::Report { .. } => unreachable!("a run reports from its own task"),
            }
        }
    }

    fn start(self: &Arc<Self>, token: u64, action: u64, run: RunWire) {
        let Some(peer) = self.peer() else { return };
        let state = self.clone();
        let mut registry = self.tasks.lock();
        let task = self.handle.spawn(async move {
            let RunWire { spec, uid, gid, untraced, closure } = run;
            let view = Arc::new(RemoteView { state: state.clone(), token, untraced, closure });
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
            let permit = state.engine.sched.acquire(crate::sched::Priority::Agent, 0).await;
            let obs = WorkerObserver { peer: peer.clone(), action };
            let starved_before = state.starved.load(Ordering::SeqCst);
            let r = state.engine.execute(&aspec, view, uid, gid, &obs).await;
            drop(permit);
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
            state.tasks.lock().remove(&token);
            let fx = state.core.lock().ended(token);
            if fx.iter().any(|e| matches!(e, WEffect::Report { .. })) {
                peer.post(&Msg::Done { action, result });
            }
        });
        registry.insert(token, task.abort_handle());
    }
}

struct RemoteFile {
    state: Arc<WorkerState>,
    token: u64,
    dir: Vec<u8>,
    name: Vec<u8>,
}

impl LazyBlob for RemoteFile {
    fn materialize(&self, _store: &Store) -> io::Result<Hash> {
        let q = worker_state::Query::Open { dir: self.dir.clone(), name: self.name.clone() };
        let h = match self.state.ask(self.token, q) {
            Some(worker_state::Reply::Opened(Ok(h))) => h,
            Some(worker_state::Reply::Opened(Err(errno))) => return Err(io::Error::from_raw_os_error(errno)),
            _ => return Err(io::Error::from_raw_os_error(libc::EIO)),
        };
        self.state.ensure_blob(&h)?;
        Ok(h)
    }
}

pub struct RemoteView {
    state: Arc<WorkerState>,
    token: u64,
    untraced: Vec<Vec<u8>>,
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
                        token: self.token,
                        dir: dir.to_vec(),
                        name: name.to_vec(),
                    })),
                },
            }),
        }
    }
}

impl View for RemoteView {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
        if let Some(l) = crate::nixmanifest::lookup_at(|n| self.state.nix.get(n).map(|e| Arc::clone(e.value())), &self.closure, dir, name) {
            self.state.lookups_local.fetch_add(1, Ordering::Relaxed);
            return l;
        }
        let q = worker_state::Query::Lookup { dir: dir.to_vec(), name: name.to_vec() };
        match self.state.ask(self.token, q) {
            Some(worker_state::Reply::Looked(w)) => self.convert(dir, name, &w),
            _ => Looked::Absent,
        }
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        if let Some(l) = crate::nixmanifest::list_at(|n| self.state.nix.get(n).map(|e| Arc::clone(e.value())), &self.closure, dir) {
            return l;
        }
        match self.state.ask(self.token, worker_state::Query::List { dir: dir.to_vec() }) {
            Some(worker_state::Reply::Listing(Ok(l))) => Ok(l.into_iter().map(|(n, t)| (n, dtype_from_u8(t))).collect()),
            Some(worker_state::Reply::Listing(Err(errno))) => Err(io::Error::from_raw_os_error(errno)),
            _ => Err(io::Error::from_raw_os_error(libc::EIO)),
        }
    }

    fn traced(&self, dir: &[u8]) -> bool {
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

const BYE_WAIT: Duration = Duration::from_secs(5);

enum LinkEnd {
    Lost,
    Stop,
}

async fn serve_link(
    state: &Arc<WorkerState>,
    conn: &Arc<Conn>,
    inbox: &mut mpsc::UnboundedReceiver<Msg>,
    term: &mut tokio::signal::unix::Signal,
    intr: &mut tokio::signal::unix::Signal,
) -> LinkEnd {
    loop {
        let msg = tokio::select! {
            m = inbox.recv() => m,
            _ = term.recv() => return LinkEnd::Stop,
            _ = intr.recv() => return LinkEnd::Stop,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                if conn.is_closed() {
                    None
                } else {
                    state.tick();
                    continue;
                }
            }
        };
        let Some(msg) = msg else { return LinkEnd::Lost };
        state.server_msg(msg);
    }
}

pub async fn worker_main(engine: Arc<Engine>, server: SocketAddr, cores: u32, key: crate::net::LinkKey) -> Result<()> {
    let endpoint = Endpoint::bind_keyed("0.0.0.0:0".parse().unwrap(), false, key).await?;
    let blobs = Arc::new(Blobs::new(engine.store.clone(), None));
    let (core, first) = worker_state::Worker::new();
    let state = Arc::new(WorkerState {
        peer: Mutex::new(None),
        blobs,
        engine: engine.clone(),
        handle: tokio::runtime::Handle::current(),
        core: Mutex::new(core),
        woken: parking_lot::Condvar::new(),
        tasks: Mutex::new(HashMap::new()),
        born: std::time::Instant::now(),
        lookups_local: AtomicU64::new(0),
        lookups_remote: AtomicU64::new(0),
        starved: AtomicU64::new(0),
        nix: DashMap::new(),
    });
    let s2 = state.clone();
    engine.content.set_fetcher(Arc::new(move |h| s2.ensure_blob(h)));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
    let mut intr = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).expect("signal");
    let mut todo: std::collections::VecDeque<WEffect> = first.into();
    while let Some(effect) = todo.pop_front() {
        let after_ms = match effect {
            WEffect::Connect { after_ms } => after_ms,
            other => {
                state.carry(vec![other]);
                continue;
            }
        };
        let connected = tokio::select! {
            c = async {
                tokio::time::sleep(Duration::from_millis(after_ms)).await;
                endpoint.connect(server).await
            } => c,
            _ = term.recv() => return Ok(()),
            _ = intr.recv() => return Ok(()),
        };
        let conn = match connected {
            Ok(c) => c,
            Err(e) => {
                let timed_out = e.kind() == std::io::ErrorKind::TimedOut;
                eprintln!("rargo worker: {server}: {e}; trying again");
                let fx = state.core.lock().connect_failed(timed_out);
                if fx.iter().any(|e| matches!(e, WEffect::Quit)) {
                    return Err(e).with_context(|| format!("connecting to the server at {server}"));
                }
                todo.extend(fx);
                continue;
            }
        };
        let (peer, mut inbox) = Peer::new(conn.clone());
        *state.peer.lock() = Some(peer.clone());
        for e in state.core.lock().connected() {
            assert!(matches!(e, WEffect::Hello), "a new link says hello");
            peer.post(&Msg::Hello { cores, blobs: engine.store.all_hashes() });
        }
        eprintln!("rargo worker: connected to {server}, {cores} cores");
        match serve_link(&state, &conn, &mut inbox, &mut term, &mut intr).await {
            LinkEnd::Stop => {
                let _ = tokio::time::timeout(BYE_WAIT, peer.deliver(&Msg::Bye)).await;
                return Ok(());
            }
            LinkEnd::Lost => {
                eprintln!("rargo worker: the connection to {server} was lost; connecting again");
                *state.peer.lock() = None;
                let fx = state.core.lock().link_lost();
                state.woken.notify_all();
                for e in fx {
                    match e {
                        WEffect::Connect { .. } => todo.push_back(e),
                        other => state.carry(vec![other]),
                    }
                }
            }
        }
    }
    unreachable!("the worker either stops or connects again")
}


#[derive(Default)]
struct Voice {
    race: Mutex<pool_state::Race>,
    changed: tokio::sync::Notify,
}

impl Voice {
    fn claim(&self, who: Who) -> bool {
        let mut race = self.race.lock();
        let first = race.owner().is_none();
        let ok = race.claim(who);
        if ok && first {
            self.changed.notify_one();
        }
        ok
    }
}

struct Gated<'a> {
    inner: &'a dyn Observer,
    voice: &'a Voice,
}

impl Observer for Gated<'_> {
    fn stdout(&self, data: &[u8]) {
        if self.voice.claim(Who::Local) {
            self.inner.stdout(data);
        }
    }
    fn stderr_line(&self, line: &[u8], live: Option<&dyn Live>) {
        if self.voice.claim(Who::Local) {
            self.inner.stderr_line(line, live);
        }
    }
}

fn crate_root(argv: &[Vec<u8>], cwd: &[u8]) -> Option<Vec<u8>> {
    let file = argv.iter().skip(1).find(|a| a.ends_with(b".rs") && !a.starts_with(b"-"))?;
    Some(if file.starts_with(b"/") { file.clone() } else { vjoin(cwd, file) })
}

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

const SOURCE_TREE_DIRS: usize = 300;
const SOURCE_TREE_FILES: usize = 3000;

fn source_tree(view: &dyn View, root: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let Some((top, _)) = vsplit(root) else { return (Vec::new(), Vec::new()) };
    let (mut dirs, mut files) = (Vec::new(), vec![root.to_vec()]);
    let mut queue = vec![top.to_vec()];
    let mut recurse = true;
    while let Some(dir) = queue.pop() {
        if dirs.len() >= SOURCE_TREE_DIRS || files.len() >= SOURCE_TREE_FILES {
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
    served: Mutex<Vec<FileSrc>>,
    asked: AtomicU64,
}

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

fn eager_dispatch() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RARGO_DISPATCH").ok().as_deref() == Some("eager"))
}

fn remote_incr() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RARGO_REMOTE_INCR").ok().as_deref() != Some("0"))
}

fn state_home_after() -> Duration {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    Duration::from_millis(*MS.get_or_init(|| {
        std::env::var("RARGO_STATE_HOME_AFTER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
    }))
}

fn hedge() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RARGO_HEDGE").ok().as_deref() != Some("0"))
}

fn state_ahead() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RARGO_STATE_AHEAD").ok().as_deref() == Some("1"))
}

const AHEAD_MAX: u64 = 256 << 20;

#[derive(Default)]
pub struct AheadQueue {
    waiting: std::collections::VecDeque<((Hash, String), Arc<Vec<(Hash, u64)>>)>,
}

impl AheadQueue {
    const KEPT: usize = 512;

    pub fn offer(&mut self, whose: (Hash, String), blobs: Arc<Vec<(Hash, u64)>>) {
        self.waiting.retain(|(w, _)| *w != whose);
        self.waiting.push_back((whose, blobs));
        if self.waiting.len() > Self::KEPT {
            self.waiting.pop_front();
        }
        assert!(self.waiting.len() <= Self::KEPT);
        debug_assert_eq!(self.waiting.iter().filter(|(w, _)| *w == self.waiting.back().unwrap().0).count(), 1, "one state per crate");
    }

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

const IN_FLIGHT_SLACK: Duration = Duration::from_secs(2);

fn still_in_flight(fl: &HashMap<Hash, std::time::Instant>, h: &Hash, now: std::time::Instant) -> bool {
    fl.get(h).is_some_and(|at| *at > now)
}

const SEED_GAP: u64 = 4 << 20;

fn trace_remote() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RARGO_TRACE_REMOTE").is_some())
}

pub struct Worker {
    pub id: u64,
    peer: Arc<Peer>,
    pub cores: usize,
    has_blobs: Mutex<HashSet<Hash>>,
    has_chunks: Mutex<HashSet<Hash>>,
    actions: DashMap<u64, Arc<RemoteAction>>,
    alive: std::sync::atomic::AtomicBool,
    pub pushed_bytes: AtomicU64,
    pub skipped_bytes: AtomicU64,
    overhead_us: AtomicU64,
    overhead_samples: AtomicU64,
    link_free_at: Mutex<std::time::Instant>,
    in_flight: Mutex<HashMap<Hash, std::time::Instant>>,
    warmed: std::sync::atomic::AtomicBool,
    store_paths: Mutex<HashSet<Vec<u8>>>,
}

impl Worker {
    fn note_overhead(&self, d: Duration) {
        let us = d.as_micros() as u64;
        let n = self.overhead_samples.fetch_add(1, Ordering::Relaxed);
        let old = self.overhead_us.load(Ordering::Relaxed);
        let new = if n == 0 { us } else { (old * 3 + us) / 4 };
        self.overhead_us.store(new, Ordering::Relaxed);
    }

    fn overhead(&self) -> Option<Duration> {
        (self.overhead_samples.load(Ordering::Relaxed) >= 3)
            .then(|| Duration::from_micros(self.overhead_us.load(Ordering::Relaxed)))
    }
}

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
    pub kept_local_first: AtomicU64,
    pub worker_failures: AtomicU64,
    pub kept_local_incremental: AtomicU64,
    pub seeded_in_place: AtomicU64,
    pub unseeded_remote: AtomicU64,
    pub state_left_blobs: AtomicU64,
    pub state_left_bytes: AtomicU64,
    pub state_homed_blobs: AtomicU64,
    pub state_homed_bytes: AtomicU64,
    pub state_needed_blobs: AtomicU64,
    pub states_sent_ahead: AtomicU64,
    pub state_ahead_bytes: AtomicU64,
    pub hedged: AtomicU64,
    pub hedge_won: AtomicU64,
    pub hedge_stopped: AtomicU64,
    pub prefilled_entries: AtomicU64,
    pub lookups_served: AtomicU64,
    pub opens_served: AtomicU64,
    pub nix_lookups: AtomicU64,
}

pub struct Pool {
    workers: Mutex<Vec<Arc<Worker>>>,
    core: Mutex<pool_state::WorkerPool>,
    blobs: Arc<Blobs>,
    host_srcs: Arc<DashMap<Hash, FileSrc>>,
    content: Arc<Content>,
    next_action: AtomicU64,
    pub profile_exclude: Vec<Vec<u8>>,
    pub bandwidth: AtomicU64,
    pub stats: PoolStats,
    freed: tokio::sync::Notify,
    home_queue: Mutex<std::collections::VecDeque<(Arc<Worker>, Hash, u64, std::time::Instant)>>,
    home_wake: tokio::sync::Notify,
    ahead: Mutex<AheadQueue>,
    ahead_wake: tokio::sync::Notify,
    ahead_busy: AtomicBool,
    home_fetching: AtomicBool,
    settle_wake: tokio::sync::Notify,
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
            core: Mutex::new(pool_state::WorkerPool::new()),
            blobs,
            host_srcs,
            content: engine.content.clone(),
            next_action: AtomicU64::new({
                let mut b = [0u8; 8];
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

    pub fn overhead_text(&self) -> String {
        let ws = self.workers.lock();
        let v: Vec<String> = ws.iter().filter_map(|w| w.overhead()).map(|d| format!("{:.2} s", d.as_secs_f64())).collect();
        if v.is_empty() { "not measured yet".into() } else { v.join(", ") }
    }

    pub fn host_sources(&self) -> &Arc<DashMap<Hash, FileSrc>> {
        &self.host_srcs
    }

    pub fn worker_count(&self) -> usize {
        self.core.lock().count()
    }

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
                    let fx = self.core.lock().joined(w.id, cores as usize);
                    self.pool_effects(fx);
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
            let fx = self.core.lock().left(w.id);
            for e in fx {
                if let pool_state::Effect::Lost { action } = e
                    && let Some(a) = w.actions.get(&action)
                {
                    let _ = a.events.send(Event::Done(Err("worker connection lost".into())));
                }
            }
            self.workers.lock().retain(|x| x.id != w.id);
        }
    }

    fn pool_effects(&self, fx: Vec<pool_state::Effect>) {
        for e in fx {
            match e {
                pool_state::Effect::Wake => self.freed.notify_one(),
                pool_state::Effect::Lost { .. } => unreachable!("a lost action is told where its worker leaves"),
                pool_state::Effect::Cancel { worker, action } => {
                    if let Some(w) = self.workers.lock().iter().find(|w| w.id == worker) {
                        w.peer.post(&Msg::Cancel { action });
                    }
                }
            }
        }
    }

    fn event(&self, worker: &Option<Arc<Worker>>, action: u64, ev: Event) {
        let Some(w) = worker else { return };
        if !self.core.lock().route(w.id, action) {
            return;
        }
        if let Some(a) = w.actions.get(&action) {
            let _ = a.events.send(ev);
        }
    }

    fn note_nix(&self, dir: &[u8]) {
        if dir == b"/nix/store" || dir.starts_with(b"/nix/store/") {
            self.stats.nix_lookups.fetch_add(1, Ordering::Relaxed);
        }
    }

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

    fn lease(&self, among: Option<&[u64]>) -> Option<Arc<Worker>> {
        let ws = self.workers.lock();
        let id = self.core.lock().lease(among)?;
        Some(ws.iter().find(|w| w.id == id).expect("a worker in the pool is listed").clone())
    }

    fn give_back(&self, w: &Worker) {
        let fx = self.core.lock().give_back(w.id);
        if !fx.is_empty() {
            self.freed.notify_waiters();
        }
    }

    async fn lease_wait(&self, among: Option<&[u64]>) -> Arc<Worker> {
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if let Some(w) = self.lease(among) {
                return w;
            }
            freed.await;
        }
    }

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

    fn holder(&self, blob: &Hash) -> Option<Arc<Worker>> {
        let ws = self.workers.lock();
        ws.iter().find(|w| w.alive.load(Ordering::Relaxed) && w.has_blobs.lock().contains(blob)).cloned()
    }

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
                let sent: u64 = tokio::task::block_in_place(|| missing.iter().map(|(h, _)| self.push_blob(&w, *h)).sum());
                self.stats.states_sent_ahead.fetch_add(1, Ordering::Relaxed);
                self.stats.state_ahead_bytes.fetch_add(sent, Ordering::Relaxed);
            }
            self.ahead_busy.store(false, Ordering::SeqCst);
            self.settle_wake.notify_waiters();
        }
    }

    async fn bring_states_home(self: Arc<Self>, engine: Arc<Engine>) {
        loop {
            let wake = self.home_wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
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
        let ws = self.workers.lock();
        for w in ws.iter() {
            if w.alive.load(Ordering::Relaxed) && !w.in_flight.lock().is_empty() {
                return false;
            }
        }
        true
    }

    fn push_blob(&self, w: &Worker, h: Hash) -> u64 {
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

struct RemoteLive<'a> {
    early: &'a Mutex<HashMap<Vec<u8>, MEntry>>,
    pool: &'a Pool,
    worker: &'a Worker,
}

impl Live for RemoteLive<'_> {
    fn ingest(&self, path: &[u8]) -> Option<MEntry> {
        let entry = self.early.lock().get(path).cloned()?;
        if let MEntry::File { hash, .. } = &entry {
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
                obs.extend(view.hints().into_iter().map(|p| (VPath::from(p), Obs::Dir)));
                obs.extend(
                    extern_paths(&spec.argv).into_iter().map(|p| (VPath::from(p), Obs::File { hash: Hash::ZERO, exec: false })),
                );
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
                        if let (Looked::File(f), WLooked::File { hash: Some(h), .. }) = (&looked, &w) {
                            if let FileSrc::Host { .. } = &f.src {
                                host_srcs.entry(*h).or_insert_with(|| f.src.clone());
                                ra2.served.lock().push(f.src.clone());
                            }
                        }
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
                let fx = self.0.core.lock().finished(self.2, self.3.get());
                self.0.pool_effects(fx);
                self.0.give_back(self.1);
            }
        }
        let cleanup = Cleanup(self, w, action, std::cell::Cell::new(false));
        if !self.core.lock().sent(action, w.id) {
            bail!("worker connection lost");
        }

        let (mut prefill, mut listings, incoming, mut closed) = self.predict(w, engine, spec, key, &view, &ra).await?;
        if !req.seed_blobs.is_empty() {
            let (view, content, dirs) = (view.clone(), self.content.clone(), spec.discard.clone());
            let (p, l) = tokio::task::spawn_blocking(move || side_tree(&*view, &content, &dirs)).await?;
            closed.extend(l.iter().map(|(d, _)| d.clone()));
            prefill.extend(p);
            listings.extend(l);
        }
        self.stats.prefilled_entries.fetch_add(prefill.len() as u64, Ordering::Relaxed);
        tokio::task::block_in_place(|| {
            let mut held = w.store_paths.lock();
            let mut closure: Vec<(Vec<u8>, Hash)> = view.nix_closure().to_vec();
            closure.sort_by(|a, b| a.0.cmp(&b.0));
            let mut store_paths = Vec::new();
            for (name, root) in &mut closure {
                if held.contains(name) {
                    continue;
                }
                match crate::nixmanifest::load(engine.store.db(), name) {
                    Some(kind) => {
                        let id = crate::nixmanifest::identity(&kind);
                        if *root != Hash::ZERO && id != *root {
                            eprintln!(
                                "rargod: nix manifest for {} does not match the closure",
                                String::from_utf8_lossy(name)
                            );
                            continue;
                        }
                        *root = id;
                        held.insert(name.clone());
                        store_paths.push(crate::nixmanifest::WirePath { name: name.clone(), kind });
                    }
                    None => {
                        if *root != Hash::ZERO {
                            eprintln!(
                                "rargod: nix manifest for {} is missing",
                                String::from_utf8_lossy(name)
                            );
                        }
                    }
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
            let w2 = w.clone();
            let blobs = incoming;
            let this: &Pool = self;
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
            if !matches!(ev, Event::Done(_)) && !voice.claim(Who::Remote) {
                std::future::pending::<()>().await;
            }
            match ev {
                Event::Stdout(d) => {
                    req.observer.stdout(&d);
                    stdout.extend_from_slice(&d);
                }
                Event::Early(path, entry) => {
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
            if matches!(spec.kind, ActionKind::Plan | ActionKind::Nextest)
                || spec.net != NetMode::Loopback
            {
                return None;
            }
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
            let own = engine.traces.traces(key).first().map(|(_, t)| t.clone());
            let known = own.clone().or_else(|| engine.profile(spec));
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
            let send = pool_state::worth_sending(
                known.is_some(),
                bytes_time.as_micros() as u64,
                engine.avg_action_wall().as_micros() as u64,
                eager_dispatch(),
            );
            if !send {
                if known.is_some() && own.is_none() && !w.warmed.swap(true, Ordering::SeqCst) {
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
            let voice = Voice::default();
            let gated = Gated { inner: &*req.observer, voice: &voice };
            let mut remote = Some(Box::pin(self.run_on(&w, engine, spec, key, req, &voice)));
            type Local<'f> = std::pin::Pin<Box<dyn std::future::Future<Output = RunResult> + Send + 'f>>;
            let mut local: Option<(Local<'_>, crate::sched::Permit, std::time::Instant)> = None;
            loop {
                let settled = voice.race.lock().settle();
                match settled {
                    Some(Settle::StopLocal) => {
                        self.stats.hedge_stopped.fetch_add(1, Ordering::Relaxed);
                        local = None;
                    }
                    Some(Settle::DropRemote) => remote = None,
                    None => {}
                }
                let may_hedge = voice.race.lock().may_hedge(away, hedge());
                tokio::select! {
                    r = async { remote.as_mut().expect("guarded").await }, if remote.is_some() => {
                        let outcome = voice.race.lock().remote_ended(r.is_ok());
                        match r {
                            Ok(r) => {
                                if matches!(outcome, Outcome::Use { stopped_other: true, .. }) {
                                    self.stats.hedge_stopped.fetch_add(1, Ordering::Relaxed);
                                }
                                return Some(Ok(r));
                            }
                            Err(e) => {
                                eprintln!("rargod: worker {:x} failed `{}`: {e:#}; running locally", w.id, spec.label);
                                self.stats.worker_failures.fetch_add(1, Ordering::Relaxed);
                                engine.stats.remote_failed.fetch_add(1, Ordering::Relaxed);
                                remote = None;
                                if outcome == Outcome::RunElsewhere {
                                    return None;
                                }
                            }
                        }
                    },
                    r = async { (&mut local.as_mut().expect("guarded").0).await }, if local.is_some() => {
                        let (_, permit, started) = local.take().expect("guarded");
                        drop(permit);
                        engine.note_action_wall(started.elapsed());
                        voice.race.lock().local_ended();
                        self.stats.hedge_won.fetch_add(1, Ordering::Relaxed);
                        return Some(r);
                    }
                    permit = engine.sched.acquire(crate::sched::Priority::Idle, mem),
                        if local.is_none() && remote.is_some() && may_hedge =>
                    {
                        voice.race.lock().hedged();
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
        q.offer(who(1, "/a"), state(4));
        assert_eq!(q.len(), 3);
        let order: Vec<u64> = std::iter::from_fn(|| q.next()).map(|b| b[0].1).collect();
        assert_eq!(order, [4, 3, 2]);
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
