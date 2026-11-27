use crate::chunk::{self, ChunkIndex, ChunkRef, Recipe};
use crate::engine::{Engine, Live, Observer, RemoteRunner, Request};
use crate::hash::Hash;
use crate::net::{Conn, Endpoint};
use crate::sandbox::NetMode;
use crate::rng::Rng;
use crate::store::Store;
use crate::trace::*;
use crate::view::*;
use crate::pool_state::{self, Outcome, Settle, Who};
use crate::send_ahead::{self, AHEAD_MAX, BATCH_BYTES, inline_indices};
use crate::worker_state;
use anyhow::{Context, Result, anyhow, bail};
use dashmap::DashMap;
use std::cell::{Cell, RefCell};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;


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
    pub conn: Rc<Conn>,
    calls: RefCell<pool_state::Calls<crate::io::oneshot::Sender<Resp>>>,
    out: crate::io::UnboundedSender<Vec<u8>>,
    bulk: crate::io::UnboundedSender<Vec<u8>>,
}

fn is_bulk(m: &Msg) -> bool {
    matches!(m, Msg::Push { .. } | Msg::Resp { body: Resp::Chunks(_) | Resp::Blob(_), .. })
}

fn wait_reply(rx: crate::io::oneshot::Receiver<Resp>) -> Result<Resp> {
    match crate::io::timeout(Duration::from_secs(120), move || rx.recv()) {
        Ok(Some(Resp::Err(e))) => bail!("peer: {e}"),
        Ok(Some(r)) => Ok(r),
        Ok(None) => bail!("connection to peer lost"),
        Err(_) => bail!("peer did not answer"),
    }
}

impl Peer {
    pub fn new(conn: Rc<Conn>) -> (Arc<Peer>, crate::io::UnboundedReceiver<Msg>) {
        let (out_tx, mut out_rx) = crate::io::unbounded_channel::<Vec<u8>>();
        let (bulk_tx, mut bulk_rx) = crate::io::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = crate::io::unbounded_channel();
        let peer = Arc::new(Peer { conn: conn.clone(), calls: RefCell::new(pool_state::Calls::default()), out: out_tx, bulk: bulk_tx });
        let c = conn.clone();
        crate::io::spawn(move || {
            loop {
                enum Side {
                    Out(Option<Vec<u8>>),
                    Bulk(Option<Vec<u8>>),
                }
                let got = crate::io::race_biased(vec![
                    Box::new(|| Side::Out(out_rx.recv())),
                    Box::new(|| Side::Bulk(bulk_rx.recv())),
                ]);
                let m = match got {
                    Side::Out(m) | Side::Bulk(m) => m,
                };
                let Some(m) = m else { break };
                let m = if m.len() >= COMPRESS_FROM { crate::io::cpu(|| frame(m)) } else { frame(m) };
                if c.send(&m).is_err() {
                    break;
                }
            }
        });
        let p = peer.clone();
        crate::io::spawn(move || {
            while let Some(raw) = conn.recv() {
                let Some(raw) = unframe(&raw) else { continue };
                let Ok(msg) = bincode::deserialize::<Msg>(&raw) else { continue };
                match msg {
                    Msg::Resp { rid, body } => {
                        let waiter = p.calls.borrow_mut().answer(rid);
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
            let lost = p.calls.borrow_mut().lost();
            drop(lost);
        });
        (peer, in_rx)
    }

    pub fn post(&self, m: &Msg) {
        if let Ok(b) = bincode::serialize(m) {
            let _ = if is_bulk(m) { self.bulk.send(b) } else { self.out.send(b) };
        }
    }

    pub fn deliver(&self, m: &Msg) {
        let Ok(b) = bincode::serialize(m) else { return };
        let framed = if b.len() >= COMPRESS_FROM { crate::io::cpu(|| frame(b)) } else { frame(b) };
        if self.conn.send(&framed).is_err() {
            return;
        }
        loop {
            if self.conn.send_idle() {
                return;
            }
            self.conn.send_settled.notified();
        }
    }

    fn start(&self, body: Req) -> crate::io::oneshot::Receiver<Resp> {
        let (tx, rx) = crate::io::oneshot::channel();
        let rid = self.calls.borrow_mut().start(tx);
        self.post(&Msg::Req { rid, body });
        rx
    }

    pub fn call(&self, body: Req) -> Result<Resp> {
        wait_reply(self.start(body))
    }

    pub fn call_blocking(&self, body: Req) -> Result<Resp> {
        match self.start(body).recv() {
            Some(Resp::Err(e)) => bail!("peer: {e}"),
            Some(r) => Ok(r),
            None => bail!("connection to peer lost"),
        }
    }
}


pub trait BlobSource: Send + Sync {
    fn open(&self, h: &Hash) -> io::Result<crate::io::file::File>;
}

struct StoreSource(Arc<Store>);
impl BlobSource for StoreSource {
    fn open(&self, h: &Hash) -> io::Result<crate::io::file::File> {
        self.0.open_blob(h)
    }
}

pub struct Blobs {
    pub store: Arc<Store>,
    pub index: RefCell<ChunkIndex>,
    source: Arc<dyn BlobSource>,
    pub fetched_bytes: Cell<u64>,
    pub reused_bytes: Cell<u64>,
    pub fetched_blobs: Cell<u64>,
    outlines: RefCell<BTreeMap<Hash, Arc<Outline>>>,
}

pub struct Outline {
    pub cvs: Vec<crate::proto::outline::Cv>,
    pub lazy: Vec<(u64, u64)>,
}


/// The outline of blob `h` as `source` holds it, and whether it is worth remembering.
fn read_outline(source: &dyn BlobSource, h: &Hash) -> Option<(Outline, bool)> {
    let f = source.open(h).ok()?;
    let len = f.metadata().ok()?.len() as usize;
    if len == 0 {
        return Some((Outline { cvs: Vec::new(), lazy: Vec::new() }, false));
    }
    let o = f
        .map(len)
        .map(|data| {
            let data: &[u8] = &data;
            let cvs = if len as u64 <= crate::proto::outline::GROUP {
                Vec::new()
            } else {
                use rayon::prelude::*;
                data.par_chunks(crate::proto::outline::GROUP as usize)
                    .enumerate()
                    .map(|(i, g)| crate::proto::outline::group_cv(i, g))
                    .collect()
            };
            Outline { cvs, lazy: chunk::debug_ranges(data) }
        })
        .ok()?;
    assert!(o.cvs.is_empty() || o.cvs.len() == crate::proto::outline::groups(len as u64));
    Some((o, true))
}

impl Blobs {
    pub fn new(store: Arc<Store>, source: Option<Arc<dyn BlobSource>>) -> Blobs {
        let source = source.unwrap_or_else(|| Arc::new(StoreSource(store.clone())));
        Blobs {
            store,
            index: RefCell::new(ChunkIndex::default()),
            source,
            fetched_bytes: Cell::new(0),
            reused_bytes: Cell::new(0),
            fetched_blobs: Cell::new(0),
            outlines: RefCell::new(BTreeMap::new()),
        }
    }

    pub fn outline(&self, h: &Hash) -> Option<Arc<Outline>> {
        if let Some(o) = self.outlines.borrow().get(h) {
            return Some(o.clone());
        }
        let (source, blob) = (self.source.clone(), *h);
        let (o, keep) = crate::io::cpu(move || read_outline(&*source, &blob))?;
        let o = Arc::new(o);
        if keep {
            self.outlines.borrow_mut().insert(*h, o.clone());
        }
        Some(o)
    }

    pub fn recipe(&self, h: &Hash) -> Option<Arc<Recipe>> {
        let known = self.index.borrow().recipe(h);
        if let Some(r) = known {
            return Some(r);
        }
        let (source, blob) = (self.source.clone(), *h);
        let r = Arc::new(crate::io::cpu(move || chunk::chunk_file(&source.open(&blob).ok()?).ok())?);
        self.index.borrow_mut().step(chunk::Event::AddBlob { blob: *h, recipe: r.clone() });
        Some(r)
    }

    pub fn serve_chunks(&self, h: &Hash, idx: &[u32]) -> Result<Vec<Vec<u8>>> {
        let recipe = self.recipe(h).ok_or_else(|| anyhow!("unknown blob {h}"))?;
        let source = self.source.clone();
        crate::io::cpu(move || {
            let f = source.open(h)?;
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
        })
    }

    fn local_chunk(&self, c: &ChunkRef) -> Option<Vec<u8>> {
        let (blob, off, len) = self.index.borrow().locate(&c.hash)?;
        let f = self.source.open(&blob).ok()?;
        match chunk::read_chunk(&f, off, len) {
            Ok(b) if Hash::of(&b) == c.hash => Some(b),
            _ => {
                self.index.borrow_mut().step(chunk::Event::Forget(c.hash));
                None
            }
        }
    }

    pub fn assemble(
        &self,
        peer: &Peer,
        h: &Hash,
        recipe: &Recipe,
        mut have: BTreeMap<u32, Vec<u8>>,
    ) -> Result<()> {
        if self.store.has(h) {
            return Ok(());
        }
        let tmp = self.store.tmp_path();
        let file = crate::io::file::File::create(&tmp)?;
        let mut off = 0u64;
        let mut missing: Vec<(u32, u64, u32)> = Vec::new();
        for (i, c) in recipe.iter().enumerate() {
            let i = i as u32;
            if let Some(b) = have.remove(&i) {
                self.fetched_bytes.set(self.fetched_bytes.get() + b.len() as u64);
                file.write_all_at(&b, off)?;
            } else if let Some(b) = self.local_chunk(c) {
                self.reused_bytes.set(self.reused_bytes.get() + b.len() as u64);
                file.write_all_at(&b, off)?;
            } else {
                missing.push((i, off, c.len));
            }
            off += c.len as u64;
        }
        assert_eq!(off, recipe.iter().map(|c| c.len as u64).sum::<u64>());
        let missing_count = missing.len();
        let batches: Vec<Vec<(u32, u64, u32)>> = send_ahead::batches(missing, |m| m.2 as usize, BATCH_BYTES);
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), missing_count);
        assert!(batches.iter().all(|b| !b.is_empty()));
        for group in batches.chunks(8) {
            let calls: Vec<_> = group
                .iter()
                .map(|b| peer.start(Req::Chunks { blob: *h, idx: b.iter().map(|x| x.0).collect() }))
                .collect();
            for (batch, call) in group.iter().zip(calls) {
                let Resp::Chunks(chunks) = wait_reply(call)? else { bail!("unexpected reply to a chunk request") };
                if chunks.len() != batch.len() {
                    bail!("peer sent the wrong number of chunks");
                }
                for ((_, off, len), data) in batch.iter().zip(chunks) {
                    if data.len() != *len as usize {
                        bail!("peer sent a chunk of the wrong size");
                    }
                    self.fetched_bytes.set(self.fetched_bytes.get() + data.len() as u64);
                    file.write_all_at(&data, *off)?;
                }
            }
        }
        drop(file);
        let (got, size) = Hash::of_path(&tmp)?;
        if got != *h {
            let _ = crate::io::file::remove_file(&tmp);
            bail!("blob {h} arrived with the wrong content");
        }
        self.store.install_hashed(&tmp, got, size)?;
        assert!(self.store.has(h));
        self.index.borrow_mut().step(chunk::Event::AddBlob { blob: *h, recipe: Arc::new(recipe.clone()) });
        self.fetched_blobs.set(self.fetched_blobs.get() + 1);
        Ok(())
    }

    pub fn fetch(&self, peer: &Peer, h: &Hash) -> Result<()> {
        if self.store.has(h) {
            return Ok(());
        }
        let Resp::Blob(Some((recipe, chunks))) = peer.call(Req::Blob { blob: *h })? else {
            bail!("peer does not have blob {h}");
        };
        self.assemble(peer, h, &recipe, chunks.into_iter().collect())
    }

    pub fn fetch_all(self: &Arc<Self>, peer: &Arc<Peer>, blobs: &[Hash]) -> Result<()> {
        for group in blobs.chunks(FETCH_TOGETHER) {
            let mut tasks: Vec<_> = group
                .iter()
                .filter(|h| !self.store.has(h))
                .map(|h| {
                    let (this, peer, h) = (self.clone(), peer.clone(), *h);
                    crate::io::spawn(move || this.fetch(&peer, &h))
                })
                .collect();
            let mut first: Result<()> = Ok(());
            for t in &mut tasks {
                let r = t.join().map_err(|e| anyhow!("fetch task: {e}")).and_then(|r| r);
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
    asks: Asks,
    peer: RefCell<Option<Arc<Peer>>>,
    blobs: Arc<Blobs>,
    engine: Arc<Engine>,
    core: RefCell<worker_state::Worker<Wire>>,
    woken: crate::io::watch::Sender<()>,
    tasks: RefCell<BTreeMap<u64, crate::io::JoinHandle<()>>>,
    born: std::time::Instant,
    lookups_local: Cell<u64>,
    lookups_remote: Cell<u64>,
    starved: Cell<u64>,
    nix: RefCell<BTreeMap<Vec<u8>, Arc<crate::nixmanifest::Installed>>>,
}

/// What a view that is read off the loop asks the worker's loop for.
enum Ask {
    Query { token: u64, q: worker_state::Query, reply: crate::io::oneshot::Sender<Option<worker_state::Reply>> },
    Blob { blob: Hash, reply: crate::io::oneshot::Sender<io::Result<()>> },
}

/// The way from any thread to the worker's state, which the loop owns.
#[derive(Clone)]
struct Asks(crate::io::UnboundedSender<Ask>);

impl Asks {
    fn query(&self, token: u64, q: worker_state::Query) -> Option<worker_state::Reply> {
        let asks = self.0.clone();
        crate::io::call(move || {
            let (reply, answer) = crate::io::oneshot::channel();
            asks.send(Ask::Query { token, q, reply }).ok()?;
            answer.recv().flatten()
        })
    }

    fn blob(&self, blob: Hash) -> io::Result<()> {
        let asks = self.0.clone();
        let answer = crate::io::call(move || {
            let (reply, answer) = crate::io::oneshot::channel();
            asks.send(Ask::Blob { blob, reply }).ok()?;
            answer.recv()
        });
        answer.unwrap_or_else(|| Err(io::Error::other(format!("content {blob} is not here and the worker is stopping"))))
    }
}

impl WorkerState {
    /// Answers each question of `asks` in a fiber of its own until the last asker is gone.
    fn serve_asks(self: &Arc<Self>, mut asks: crate::io::UnboundedReceiver<Ask>) {
        while let Some(ask) = asks.recv() {
            let state = self.clone();
            crate::io::spawn(move || match ask {
                Ask::Query { token, q, reply } => {
                    let _ = reply.send(state.ask_here(token, q));
                }
                Ask::Blob { blob, reply } => {
                    let _ = reply.send(state.pull_blob(blob));
                }
            });
        }
    }

    fn peer(&self) -> Option<Arc<Peer>> {
        self.peer.borrow_mut().clone()
    }

    fn now_ms(&self) -> u64 {
        crate::io::now().saturating_duration_since(self.born).as_millis() as u64
    }

    fn pull_blob(&self, h: Hash) -> io::Result<()> {
        let mut wake = self.woken.subscribe();
        loop {
            wake.borrow_and_update();
            let has = self.blobs.store.has(&h);
            let need = self.core.borrow_mut().need(h, has);
            match need {
                worker_state::Need::Have => return Ok(()),
                worker_state::Need::Wait => {
                    let _ = wake.changed();
                }
                worker_state::Need::Gone => {
                    self.starved.set(self.starved.get() + 1);
                    return Err(io::Error::other(format!("content {h} is not here and the server is gone")));
                }
                worker_state::Need::Fetch => {
                    let got = match self.peer() {
                        Some(peer) => self.blobs.fetch(&peer, &h),
                        None => Err(anyhow!("no link to the server")),
                    };
                    self.core.borrow_mut().fetched(h);
                    let _ = self.woken.send_replace(());
                    return got.map_err(|e| {
                        self.starved.set(self.starved.get() + 1);
                        eprintln!("rargo worker: content {h} could not be had from the server: {e:#}");
                        io::Error::other(format!("{e:#}"))
                    });
                }
            }
        }
    }

    fn ask_here(&self, token: u64, q: worker_state::Query) -> Option<worker_state::Reply> {
        use worker_state::{Asked, Query, Reply};
        let mut wake = self.woken.subscribe();
        loop {
            wake.borrow_and_update();
            let asked = self.core.borrow_mut().query(token, q.clone());
            match asked {
                Asked::Known(r) => {
                    self.lookups_local.set(self.lookups_local.get() + 1);
                    return Some(r);
                }
                Asked::Gone => return None,
                Asked::Wait => {
                    let _ = wake.changed();
                }
                Asked::Ask { ticket, action } => {
                    self.lookups_remote.set(self.lookups_remote.get() + 1);
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
                    self.core.borrow_mut().answered(ticket, reply.clone());
                    let _ = self.woken.send_replace(());
                    return reply;
                }
            }
        }
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
                    if self.nix.borrow().contains_key(&path.name) {
                        continue;
                    }
                    match crate::nixmanifest::Installed::from_kind(&path.kind) {
                        Ok(inst) => {
                            self.nix.borrow_mut().insert(path.name.clone(), Arc::new(inst));
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
            let mut core = self.core.borrow_mut();
            let mut fx = core.server(from, &|h| self.blobs.store.has(h), now);
            fx.extend(core.tick(now));
            fx
        };
        let _ = self.woken.send_replace(());
        self.carry(fx);
    }

    fn tick(self: &Arc<Self>) {
        let fx = self.core.borrow_mut().tick(self.now_ms());
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
                    crate::io::spawn(move || {
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
                    crate::io::spawn(move || {
                        let mut recipe = None;
                        let mut chunks = BTreeMap::new();
                        for (part, r, c) in parts {
                            if part == 0 {
                                recipe = Some(r);
                            }
                            chunks.extend(c);
                        }
                        let recipe = recipe.unwrap_or_default();
                        if let Err(e) = state.blobs.assemble(&peer, &blob, &recipe, chunks) {
                            eprintln!("rargo worker: pushed blob {blob} could not be assembled: {e:#}");
                        }
                        let fx = state.core.borrow_mut().assembled(blob);
                        let _ = state.woken.send_replace(());
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
                    let task = self.tasks.borrow_mut().remove(&token);
                    if let Some(task) = task {
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
        let task = crate::io::spawn(move || {
            let RunWire { spec, uid, gid, untraced, closure } = run;
            let nix = {
                let known = state.nix.borrow();
                closure.iter().filter_map(|n| known.get(n).map(|i| (n.clone(), i.clone()))).collect()
            };
            let view = Arc::new(RemoteView { asks: state.asks.clone(), nix, token, untraced, closure });
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
            let permit = state.engine.sched.acquire(crate::sched::Priority::Agent, 0);
            let obs = WorkerObserver { peer: peer.clone(), action };
            let starved_before = state.starved.get();
            let r = state.engine.execute(&aspec, view, uid, gid, &obs);
            drop(permit);
            let r = if state.starved.get() != starved_before {
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
            state.tasks.borrow_mut().remove(&token);
            let fx = state.core.borrow_mut().ended(token);
            if fx.iter().any(|e| matches!(e, WEffect::Report { .. })) {
                peer.post(&Msg::Done { action, result });
            }
        });
        self.tasks.borrow_mut().insert(token, task);
    }
}

struct RemoteFile {
    asks: Asks,
    token: u64,
    dir: Vec<u8>,
    name: Vec<u8>,
}

impl LazyBlob for RemoteFile {
    fn materialize(&self, _store: &Store) -> io::Result<Hash> {
        let q = worker_state::Query::Open { dir: self.dir.clone(), name: self.name.clone() };
        let h = match self.asks.query(self.token, q) {
            Some(worker_state::Reply::Opened(Ok(h))) => h,
            Some(worker_state::Reply::Opened(Err(errno))) => return Err(io::Error::from_raw_os_error(errno)),
            _ => return Err(io::Error::from_raw_os_error(libc::EIO)),
        };
        self.asks.blob(h)?;
        Ok(h)
    }
}

pub struct RemoteView {
    asks: Asks,
    nix: BTreeMap<Vec<u8>, Arc<crate::nixmanifest::Installed>>,
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
                        asks: self.asks.clone(),
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
        if let Some(l) = crate::nixmanifest::lookup_at(|n| self.nix.get(n).cloned(), &self.closure, dir, name) {
            return l;
        }
        let q = worker_state::Query::Lookup { dir: dir.to_vec(), name: name.to_vec() };
        match self.asks.query(self.token, q) {
            Some(worker_state::Reply::Looked(w)) => self.convert(dir, name, &w),
            _ => Looked::Absent,
        }
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        if let Some(l) = crate::nixmanifest::list_at(|n| self.nix.get(n).cloned(), &self.closure, dir) {
            return l;
        }
        match self.asks.query(self.token, worker_state::Query::List { dir: dir.to_vec() }) {
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

enum LinkMsg {
    Inbox(Option<Msg>),
    Stop,
    Tick,
}

fn serve_link(
    state: &Arc<WorkerState>,
    conn: &Rc<Conn>,
    inbox: &mut crate::io::UnboundedReceiver<Msg>,
    term: &mut crate::io::Signal,
    intr: &mut crate::io::Signal,
) -> LinkEnd {
    loop {
        let msg = crate::io::race_biased(vec![
            Box::new(|| LinkMsg::Inbox(inbox.recv())),
            Box::new(|| {
                let _ = term.recv();
                LinkMsg::Stop
            }),
            Box::new(|| {
                let _ = intr.recv();
                LinkMsg::Stop
            }),
            Box::new(|| {
                crate::io::sleep(Duration::from_secs(5));
                LinkMsg::Tick
            }),
        ]);
        match msg {
            LinkMsg::Stop => return LinkEnd::Stop,
            LinkMsg::Tick => {
                if conn.is_closed() {
                    return LinkEnd::Lost;
                }
                state.tick();
            }
            LinkMsg::Inbox(Some(msg)) => state.server_msg(msg),
            LinkMsg::Inbox(None) => return LinkEnd::Lost,
        }
    }
}

enum Dial {
    Ready(std::io::Result<Rc<Conn>>),
    Stop,
}

pub fn worker_main(engine: Arc<Engine>, server: SocketAddr, cores: u32, key: crate::net::LinkKey) -> Result<()> {
    let endpoint = Endpoint::bind_keyed("0.0.0.0:0".parse().unwrap(), false, key)?;
    let blobs = Arc::new(Blobs::new(engine.store.clone(), None));
    let (core, first) = worker_state::Worker::new();
    let (asks, asked) = crate::io::unbounded_channel();
    let asks = Asks(asks);
    let state = Arc::new(WorkerState {
        asks: asks.clone(),
        peer: RefCell::new(None),
        blobs,
        engine: engine.clone(),
        core: RefCell::new(core),
        woken: crate::io::watch::channel(()).0,
        tasks: RefCell::new(BTreeMap::new()),
        born: crate::io::now(),
        lookups_local: Cell::new(0),
        lookups_remote: Cell::new(0),
        starved: Cell::new(0),
        nix: RefCell::new(BTreeMap::new()),
    });
    engine.content.set_fetcher(Arc::new(move |h| asks.blob(*h)));
    let serving = state.clone();
    let asks_served = crate::io::spawn(move || serving.serve_asks(asked));
    let ended = worker_links(&engine, &state, server, cores, endpoint, first);
    asks_served.abort();
    ended
}

/// Keeps a link to the server at `server` up and serves it until the worker is told to stop.
fn worker_links(
    engine: &Arc<Engine>,
    state: &Arc<WorkerState>,
    server: SocketAddr,
    cores: u32,
    endpoint: Rc<Endpoint>,
    first: Vec<WEffect>,
) -> Result<()> {
    let mut term = crate::io::signal(crate::io::SignalKind::terminate()).expect("signal");
    let mut intr = crate::io::signal(crate::io::SignalKind::interrupt()).expect("signal");
    let mut todo: std::collections::VecDeque<WEffect> = first.into();
    while let Some(effect) = todo.pop_front() {
        let after_ms = match effect {
            WEffect::Connect { after_ms } => after_ms,
            other => {
                state.carry(vec![other]);
                continue;
            }
        };
        let connected = crate::io::race_biased(vec![
            Box::new(|| {
                crate::io::sleep(Duration::from_millis(after_ms));
                Dial::Ready(endpoint.connect(server))
            }),
            Box::new(|| {
                let _ = term.recv();
                Dial::Stop
            }),
            Box::new(|| {
                let _ = intr.recv();
                Dial::Stop
            }),
        ]);
        let Dial::Ready(connected) = connected else { return Ok(()) };
        let conn = match connected {
            Ok(c) => c,
            Err(e) => {
                let timed_out = e.kind() == std::io::ErrorKind::TimedOut;
                eprintln!("rargo worker: {server}: {e}; trying again");
                let fx = state.core.borrow_mut().connect_failed(timed_out);
                if fx.iter().any(|e| matches!(e, WEffect::Quit)) {
                    return Err(e).with_context(|| format!("connecting to the server at {server}"));
                }
                todo.extend(fx);
                continue;
            }
        };
        let (peer, mut inbox) = Peer::new(conn.clone());
        *state.peer.borrow_mut() = Some(peer.clone());
        for e in state.core.borrow_mut().connected() {
            assert!(matches!(e, WEffect::Hello), "a new link says hello");
            peer.post(&Msg::Hello { cores, blobs: engine.store.all_hashes() });
        }
        eprintln!("rargo worker: connected to {server}, {cores} cores");
        match serve_link(&state, &conn, &mut inbox, &mut term, &mut intr) {
            LinkEnd::Stop => {
                let _ = crate::io::timeout(BYE_WAIT, || peer.deliver(&Msg::Bye));
                return Ok(());
            }
            LinkEnd::Lost => {
                eprintln!("rargo worker: the connection to {server} was lost; connecting again");
                *state.peer.borrow_mut() = None;
                let fx = state.core.borrow_mut().link_lost();
                let _ = state.woken.send_replace(());
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


struct Voice {
    race: RefCell<pool_state::Race>,
    changed: crate::io::watch::Sender<()>,
}

impl Voice {
    fn new() -> Voice {
        Voice { race: RefCell::new(pool_state::Race::default()), changed: crate::io::watch::channel(()).0 }
    }

    fn claim(&self, who: Who) -> bool {
        let mut race = self.race.borrow_mut();
        let first = race.owner().is_none();
        let ok = race.claim(who);
        if ok && first {
            let _ = self.changed.send_replace(());
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
    events: crate::io::UnboundedSender<Event>,
    served: RefCell<Vec<FileSrc>>,
    asked: Cell<u64>,
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

/// How the pool is told to behave by the environment it was started in.
struct Tuning {
    eager_dispatch: bool,
    remote_incr: bool,
    state_home_after: Duration,
    hedge: bool,
    state_ahead: bool,
    trace: bool,
}

impl Tuning {
    fn from_env() -> Tuning {
        let var = |name: &str| crate::io::env::var(name).ok();
        Tuning {
            eager_dispatch: var("RARGO_DISPATCH").as_deref() == Some("eager"),
            remote_incr: var("RARGO_REMOTE_INCR").as_deref() != Some("0"),
            state_home_after: Duration::from_millis(
                var("RARGO_STATE_HOME_AFTER_MS").and_then(|v| v.parse().ok()).unwrap_or(0),
            ),
            hedge: var("RARGO_HEDGE").as_deref() != Some("0"),
            state_ahead: var("RARGO_STATE_AHEAD").as_deref() == Some("1"),
            trace: crate::io::env::var_os("RARGO_TRACE_REMOTE").is_some(),
        }
    }
}


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

fn still_in_flight(fl: &BTreeMap<Hash, std::time::Instant>, h: &Hash, now: std::time::Instant) -> bool {
    fl.get(h).is_some_and(|at| *at > now)
}

const SEED_GAP: u64 = 4 << 20;

pub struct Worker {
    pub id: u64,
    peer: Arc<Peer>,
    pub cores: usize,
    has_blobs: RefCell<BTreeSet<Hash>>,
    has_chunks: RefCell<BTreeSet<Hash>>,
    actions: RefCell<BTreeMap<u64, Arc<RemoteAction>>>,
    alive: Cell<bool>,
    pub pushed_bytes: Cell<u64>,
    pub skipped_bytes: Cell<u64>,
    overhead_us: Cell<u64>,
    overhead_samples: Cell<u64>,
    link_free_at: RefCell<std::time::Instant>,
    in_flight: RefCell<BTreeMap<Hash, std::time::Instant>>,
    store_paths: RefCell<BTreeSet<Vec<u8>>>,
}

impl Worker {
    fn note_overhead(&self, d: Duration) {
        let us = d.as_micros() as u64;
        let n = self.overhead_samples.get();
        self.overhead_samples.set(n + 1);
        let old = self.overhead_us.get();
        let new = if n == 0 { us } else { (old * 3 + us) / 4 };
        self.overhead_us.set(new);
    }

    fn overhead(&self) -> Option<Duration> {
        (self.overhead_samples.get() >= 3)
            .then(|| Duration::from_micros(self.overhead_us.get()))
    }
}

struct ServerSource {
    store: Arc<Store>,
    host: Arc<DashMap<Hash, FileSrc>>,
    content: Arc<Content>,
}

impl BlobSource for ServerSource {
    fn open(&self, h: &Hash) -> io::Result<crate::io::file::File> {
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
    pub dispatched: Cell<u64>,
    pub kept_local_no_wait: Cell<u64>,
    pub kept_local_transfer: Cell<u64>,
    pub kept_local_first: Cell<u64>,
    pub worker_failures: Cell<u64>,
    pub kept_local_incremental: Cell<u64>,
    pub seeded_in_place: Cell<u64>,
    pub unseeded_remote: Cell<u64>,
    pub state_left_blobs: Cell<u64>,
    pub state_left_bytes: Cell<u64>,
    pub state_homed_blobs: Cell<u64>,
    pub state_homed_bytes: Cell<u64>,
    pub state_needed_blobs: Cell<u64>,
    pub states_sent_ahead: Cell<u64>,
    pub state_ahead_bytes: Cell<u64>,
    pub hedged: Cell<u64>,
    pub hedge_won: Cell<u64>,
    pub hedge_stopped: Cell<u64>,
    pub prefilled_entries: Cell<u64>,
    pub lookups_served: Cell<u64>,
    pub opens_served: Cell<u64>,
    pub nix_lookups: Cell<u64>,
}

/// Numbers the actions a pool sends out. The first is drawn from the
/// generator the pool was given, so a daemon restarted against the same
/// workers does not reuse the numbers of its last life, and a seeded run
/// numbers its actions the same way every time.
struct ActionIds(Cell<u64>);

impl ActionIds {
    fn new(rng: &mut Rng) -> ActionIds {
        ActionIds(Cell::new(rng.next_u64() >> 1))
    }

    fn next(&self) -> u64 {
        let id = self.0.get();
        self.0.set(id + 1);
        id
    }
}

pub struct Pool {
    workers: RefCell<Vec<Arc<Worker>>>,
    core: RefCell<pool_state::WorkerPool>,
    blobs: Arc<Blobs>,
    host_srcs: Arc<DashMap<Hash, FileSrc>>,
    content: Arc<Content>,
    next_action: ActionIds,
    pub profile_exclude: Vec<Vec<u8>>,
    pub bandwidth: Cell<u64>,
    pub stats: PoolStats,
    freed: crate::io::watch::Sender<()>,
    home_queue: RefCell<std::collections::VecDeque<(Arc<Worker>, Hash, u64, std::time::Instant)>>,
    home_wake: crate::io::watch::Sender<()>,
    ahead: RefCell<AheadQueue>,
    ahead_wake: crate::io::watch::Sender<()>,
    ahead_busy: Cell<bool>,
    home_fetching: Cell<bool>,
    settle_wake: crate::io::watch::Sender<()>,
    left_on: RefCell<BTreeMap<Hash, (u64, String)>>,
    tuning: Tuning,
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
    /// `rng` is the pool's only source of randomness: action IDs come from it.
    pub fn listen(engine: &Arc<Engine>, addr: SocketAddr, key: crate::net::LinkKey, rng: &mut Rng) -> Result<Arc<Pool>> {
        let endpoint = Endpoint::bind_keyed(addr, true, key).with_context(|| format!("binding {addr}"))?;
        let host_srcs: Arc<DashMap<Hash, FileSrc>> = Arc::new(DashMap::new());
        let blobs = Arc::new(Blobs::new(
            engine.store.clone(),
            Some(Arc::new(ServerSource {
                store: engine.store.clone(),
                host: host_srcs.clone(),
                content: engine.content.clone(),
            })),
        ));
        let pool = Arc::new(Pool {
            workers: RefCell::new(Vec::new()),
            core: RefCell::new(pool_state::WorkerPool::new()),
            blobs,
            host_srcs,
            content: engine.content.clone(),
            next_action: ActionIds::new(rng),
            profile_exclude: vec![
                crate::buildview::WORKTREE_AT.to_vec(),
                crate::buildview::CARGO_HOME_AT.to_vec(),
            ],
            bandwidth: Cell::new(100 << 20),
            stats: PoolStats::default(),
            freed: crate::io::watch::channel(()).0,
            home_queue: RefCell::new(std::collections::VecDeque::new()),
            home_wake: crate::io::watch::channel(()).0,
            ahead: RefCell::new(AheadQueue::default()),
            ahead_wake: crate::io::watch::channel(()).0,
            ahead_busy: Cell::new(false),
            home_fetching: Cell::new(false),
            settle_wake: crate::io::watch::channel(()).0,
            left_on: RefCell::new(BTreeMap::new()),
            tuning: Tuning::from_env(),
        });
        let home = pool.clone();
        let engine_home = engine.clone();
        crate::io::spawn(move || home.bring_states_home(engine_home));
        let ahead = pool.clone();
        let engine_ahead = engine.clone();
        crate::io::spawn(move || ahead.send_states_ahead(engine_ahead));
        let p = pool.clone();
        crate::io::spawn(move || {
            while let Some(conn) = endpoint.accept() {
                let p = p.clone();
                crate::io::spawn(move || p.serve_worker(conn));
            }
        });
        Ok(pool)
    }

    pub fn overhead_text(&self) -> String {
        let ws = self.workers.borrow_mut();
        let v: Vec<String> = ws.iter().filter_map(|w| w.overhead()).map(|d| format!("{:.2} s", d.as_secs_f64())).collect();
        if v.is_empty() { "not measured yet".into() } else { v.join(", ") }
    }

    pub fn host_sources(&self) -> &Arc<DashMap<Hash, FileSrc>> {
        &self.host_srcs
    }

    pub fn worker_count(&self) -> usize {
        self.core.borrow_mut().count()
    }

    pub fn transfer_totals(&self) -> (u64, u64, u64, u64) {
        let ws = self.workers.borrow_mut();
        (
            ws.iter().map(|w| w.pushed_bytes.get()).sum(),
            ws.iter().map(|w| w.skipped_bytes.get()).sum(),
            self.blobs.fetched_bytes.get(),
            self.blobs.reused_bytes.get(),
        )
    }

    fn serve_worker(self: Arc<Self>, conn: Rc<Conn>) {
        let (peer, mut inbox) = Peer::new(conn.clone());
        let mut worker: Option<Arc<Worker>> = None;
        loop {
            enum Incoming {
                Msg(Option<Msg>),
                Idle,
            }
            let msg = crate::io::race_biased(vec![
                Box::new(|| Incoming::Msg(inbox.recv())),
                Box::new(|| {
                    crate::io::sleep(Duration::from_secs(2));
                    Incoming::Idle
                }),
            ]);
            let msg = match msg {
                Incoming::Idle => {
                    let busy = worker.as_ref().is_some_and(|w| !w.actions.borrow().is_empty());
                    if busy && conn.idle() > Duration::from_secs(30) {
                        break;
                    }
                    continue;
                }
                Incoming::Msg(msg) => msg,
            };
            let Some(msg) = msg else { break };
            match msg {
                Msg::Hello { cores, blobs } => {
                    let w = Arc::new(Worker {
                        id: conn.id,
                        peer: peer.clone(),
                        cores: cores as usize,
                        has_blobs: RefCell::new(blobs.into_iter().collect()),
                        has_chunks: RefCell::new(BTreeSet::new()),
                        actions: RefCell::new(BTreeMap::new()),
                        alive: Cell::new(true),
                        pushed_bytes: Cell::new(0),
                        skipped_bytes: Cell::new(0),
                        overhead_us: Cell::new(0),
                        overhead_samples: Cell::new(0),
                        link_free_at: RefCell::new(crate::io::now()),
                        in_flight: RefCell::new(BTreeMap::new()),
                        store_paths: RefCell::new(BTreeSet::new()),
                    });
                    eprintln!("rargod: worker {:x} joined from {} with {cores} cores", conn.id, conn.peer());
                    self.workers.borrow_mut().push(w.clone());
                    let fx = self.core.borrow_mut().joined(w.id, cores as usize);
                    self.pool_effects(fx);
                    worker = Some(w);
                    peer.post(&Msg::Welcome);
                }
                Msg::Req { rid, body } => {
                    let Some(w) = worker.clone() else { continue };
                    let pool = self.clone();
                    let peer = peer.clone();
                    crate::io::spawn(move || {
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
                        w.in_flight.borrow_mut().remove(&blob);
                        let _ = self.settle_wake.send_replace(());
                    }
                }
                Msg::Bye => break,
                _ => {}
            }
        }
        if let Some(w) = worker {
            eprintln!("rargod: worker {:x} is gone", w.id);
            w.alive.set(false);
            w.in_flight.borrow_mut().clear();
            let _ = self.settle_wake.send_replace(());
            let fx = self.core.borrow_mut().left(w.id);
            for e in fx {
                if let pool_state::Effect::Lost { action } = e
                    && let Some(a) = w.actions.borrow().get(&action)
                {
                    let _ = a.events.send(Event::Done(Err("worker connection lost".into())));
                }
            }
            self.workers.borrow_mut().retain(|x| x.id != w.id);
        }
    }

    fn pool_effects(&self, fx: Vec<pool_state::Effect>) {
        for e in fx {
            match e {
                pool_state::Effect::Wake => {
                    let _ = self.freed.send_replace(());
                }
                pool_state::Effect::Lost { .. } => unreachable!("a lost action is told where its worker leaves"),
                pool_state::Effect::Cancel { worker, action } => {
                    if let Some(w) = self.workers.borrow_mut().iter().find(|w| w.id == worker) {
                        w.peer.post(&Msg::Cancel { action });
                    }
                }
            }
        }
    }

    fn event(&self, worker: &Option<Arc<Worker>>, action: u64, ev: Event) {
        let Some(w) = worker else { return };
        if !self.core.borrow_mut().route(w.id, action) {
            return;
        }
        if let Some(a) = w.actions.borrow().get(&action) {
            let _ = a.events.send(ev);
        }
    }

    fn note_nix(&self, dir: &[u8]) {
        if dir == b"/nix/store" || dir.starts_with(b"/nix/store/") {
            self.stats.nix_lookups.set(self.stats.nix_lookups.get() + 1);
        }
    }

    fn answer(&self, w: &Worker, body: Req) -> Resp {
        let acting = |action: u64| w.actions.borrow().get(&action).cloned();
        match body {
            Req::Lookup { action, dir, name } => {
                let Some(a) = acting(action) else { return Resp::Err("no such action".into()) };
                self.note_nix(&dir);
                self.stats.lookups_served.set(self.stats.lookups_served.get() + 1);
                a.asked.set(a.asked.get() + 1);
                let (view, content, trace) = (a.view.clone(), self.content.clone(), self.tuning.trace);
                let (looked, wire) = crate::io::cpu(move || {
                    let looked = view.lookup(&dir, &name);
                    if trace {
                        let kind = match &looked {
                            Looked::File(_) => "file",
                            Looked::Dir(_) => "dir",
                            Looked::Absent => "absent",
                            _ => "other",
                        };
                        eprintln!("rargod: asked lookup {kind} {}/{}", String::from_utf8_lossy(&dir), String::from_utf8_lossy(&name));
                    }
                    let wire = to_wire(&looked, &content);
                    (looked, wire)
                });
                if let (Looked::File(f), WLooked::File { hash: Some(h), .. }) = (&looked, &wire) {
                    self.remember_src(&a, *h, &f.src);
                }
                Resp::Looked(wire)
            }
            Req::List { action, dir } => {
                let Some(a) = acting(action) else { return Resp::Err("no such action".into()) };
                self.note_nix(&dir);
                let view = a.view.clone();
                Resp::Listing(crate::io::cpu(move || {
                    view.list(&dir)
                        .map(|l| l.into_iter().map(|(n, t)| (n, dtype_to_u8(t))).collect())
                        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
                }))
            }
            Req::Open { action, dir, name } => {
                let Some(a) = acting(action) else { return Resp::Err("no such action".into()) };
                self.note_nix(&dir);
                self.stats.opens_served.set(self.stats.opens_served.get() + 1);
                a.asked.set(a.asked.get() + 1);
                let (view, content) = (a.view.clone(), self.content.clone());
                let opened = crate::io::cpu(move || {
                    let Looked::File(f) = view.lookup(&dir, &name) else { return None };
                    let hashed = content.hash(&f.src);
                    Some((f.src, hashed))
                });
                match opened {
                    None => Resp::Opened(Err(libc::ENOENT)),
                    Some((src, Ok(h))) => {
                        self.remember_src(&a, h, &src);
                        Resp::Opened(Ok(h))
                    }
                    Some((_, Err(e))) => Resp::Opened(Err(e.raw_os_error().unwrap_or(libc::EIO))),
                }
            }
            Req::Recipe { blob } => {
                let r = self.blobs.recipe(&blob).map(|r| (*r).clone());
                if let Some(r) = &r {
                    w.has_chunks.borrow_mut().extend(r.iter().map(|c| c.hash));
                    w.has_blobs.borrow_mut().insert(blob);
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
                        let shown = self.host_srcs.get(&blob).map(|s| s.clone());
                        match shown {
                            Some(FileSrc::Host { path, sig }) => match crate::io::file::symlink_metadata(&path) {
                                Ok(m) if HostSig::of(&m) == sig => format!("{} is unchanged but could not be read", path.display()),
                                Ok(_) => format!("{} has changed since it was shown", path.display()),
                                Err(e) => format!("{}: {e}", path.display()),
                            },
                            Some(_) => "known, but not as a file on this machine".to_string(),
                            None => match self.left_on.borrow().get(&blob) {
                                Some(l) => format!(
                                    "part of the incremental state `{}` left on worker {:x}{}",
                                    l.1,
                                    l.0,
                                    if w.has_blobs.borrow_mut().contains(&blob) { " (believed to be on the asking worker)" } else { "" }
                                ),
                                None => "not in the store and never shown from a file".to_string(),
                            },
                        }
                    };
                    eprintln!("rargod: worker {:x} asked for content {blob}, which cannot be served: {why}", w.id);
                    return Resp::Blob(None);
                };
                let idx = {
                    let has = w.has_chunks.borrow_mut();
                    inline_indices(&r, &|h| has.contains(h))
                };
                let chunks = self.blobs.serve_chunks(&blob, &idx).unwrap_or_default();
                w.has_chunks.borrow_mut().extend(r.iter().map(|c| c.hash));
                w.has_blobs.borrow_mut().insert(blob);
                Resp::Blob(Some(((*r).clone(), idx.into_iter().zip(chunks).collect())))
            }
        }
    }

    fn remember_src(&self, a: &RemoteAction, h: Hash, src: &FileSrc) {
        if let FileSrc::Host { .. } = src {
            self.host_srcs.entry(h).or_insert_with(|| src.clone());
            a.served.borrow_mut().push(src.clone());
        }
    }

    fn lease(&self, among: Option<&[u64]>) -> Option<Arc<Worker>> {
        let ws = self.workers.borrow_mut();
        let id = self.core.borrow_mut().lease(among)?;
        Some(ws.iter().find(|w| w.id == id).expect("a worker in the pool is listed").clone())
    }

    fn give_back(&self, w: &Worker) {
        let fx = self.core.borrow_mut().give_back(w.id);
        if !fx.is_empty() {
            let _ = self.freed.send_replace(());
        }
    }

    fn lease_wait(&self, among: Option<&[u64]>) -> Arc<Worker> {
        let mut wake = self.freed.subscribe();
        loop {
            wake.borrow_and_update();
            if let Some(w) = self.lease(among) {
                return w;
            }
            let _ = wake.changed();
        }
    }

    fn seed_holders(&self, blobs: &[(Hash, u64)], store: &Store) -> Vec<u64> {
        let ws = self.workers.borrow_mut();
        ws.iter()
            .filter(|w| w.alive.get())
            .filter(|w| {
                let has = w.has_blobs.borrow_mut();
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
        let ws = self.workers.borrow_mut();
        ws.iter().find(|w| w.alive.get() && w.has_blobs.borrow_mut().contains(blob)).cloned()
    }

    fn send_states_ahead(self: Arc<Self>, engine: Arc<Engine>) {
        let mut wake = self.ahead_wake.subscribe();
        loop {
            wake.borrow_and_update();
            let next = {
                let mut q = self.ahead.borrow_mut();
                let next = q.next();
                if next.is_some() {
                    self.ahead_busy.set(true);
                }
                next
            };
            let Some(blobs) = next else {
                let _ = wake.changed();
                continue;
            };
            let workers: Vec<Arc<Worker>> = self.workers.borrow_mut().iter().filter(|w| w.alive.get()).cloned().collect();
            for w in workers {
                let missing = {
                    let has = w.has_blobs.borrow_mut();
                    send_ahead::state_to_send(&blobs, &|h| has.contains(h), &|h| engine.store.has(h), AHEAD_MAX)
                };
                let Some(missing) = missing else { continue };
                for _ in 0..60 {
                    let busy = w.link_free_at.borrow_mut().saturating_duration_since(crate::io::now());
                    if busy.is_zero() {
                        break;
                    }
                    crate::io::sleep(busy.min(Duration::from_millis(500)));
                }
                if !w.alive.get() {
                    continue;
                }
                let sent: u64 = missing.iter().map(|(h, _)| self.push_blob(&w, *h)).sum();
                self.stats.states_sent_ahead.set(self.stats.states_sent_ahead.get() + 1);
                self.stats.state_ahead_bytes.set(self.stats.state_ahead_bytes.get() + sent);
            }
            self.ahead_busy.set(false);
            let _ = self.settle_wake.send_replace(());
        }
    }

    fn bring_states_home(self: Arc<Self>, engine: Arc<Engine>) {
        let mut wake = self.home_wake.subscribe();
        loop {
            wake.borrow_and_update();
            let due = {
                let mut q = self.home_queue.borrow_mut();
                match q.front() {
                    Some((_, _, _, since)) => match self.tuning.state_home_after.checked_sub(crate::io::now().saturating_duration_since(*since)) {
                        Some(wait) if !wait.is_zero() => Err(wait),
                        _ => {
                            self.home_fetching.set(true);
                            Ok(q.pop_front())
                        }
                    },
                    None => Ok(None),
                }
            };
            match due {
                Ok(None) => {
                    let _ = wake.changed();
                }
                Err(wait) => {
                    crate::io::race_biased(vec![
                        Box::new(|| crate::io::sleep(wait)),
                        Box::new(|| {
                            let _ = wake.changed();
                        }),
                    ]);
                }
                Ok(Some((w, h, size, _))) => {
                    if !engine.store.has(&h) && w.alive.get() {
                        let permit = engine.sched.acquire(crate::sched::Priority::Idle, 0);
                        match self.blobs.fetch(&w.peer, &h) {
                            Ok(()) => {
                                self.stats.state_homed_blobs.set(self.stats.state_homed_blobs.get() + 1);
                                self.stats.state_homed_bytes.set(self.stats.state_homed_bytes.get() + size);
                            }
                            Err(e) => {
                                eprintln!("rargod: a file of an incremental state did not come home from worker {:x}: {e:#}", w.id)
                            }
                        }
                        drop(permit);
                    }
                    self.home_fetching.set(false);
                    let _ = self.settle_wake.send_replace(());
                }
            }
        }
    }

    pub fn settle(&self) {
        let mut wake = self.settle_wake.subscribe();
        loop {
            wake.borrow_and_update();
            if self.transfers_idle() {
                return;
            }
            let _ = wake.changed();
        }
    }

    fn transfers_idle(&self) -> bool {
        {
            let q = self.ahead.borrow_mut();
            if self.ahead_busy.get() || !q.is_empty() {
                return false;
            }
        }
        {
            let q = self.home_queue.borrow_mut();
            if self.home_fetching.get() {
                return false;
            }
            let after = self.tuning.state_home_after;
            if q.iter().any(|(_, _, _, since)| crate::io::now().saturating_duration_since(*since) >= after) {
                return false;
            }
        }
        let ws = self.workers.borrow_mut();
        for w in ws.iter() {
            if w.alive.get() && !w.in_flight.borrow_mut().is_empty() {
                return false;
            }
        }
        true
    }

    fn push_blob(&self, w: &Worker, h: Hash) -> u64 {
        if !w.has_blobs.borrow_mut().insert(h) {
            return 0;
        }
        let Some(recipe) = self.blobs.recipe(&h) else {
            w.has_blobs.borrow_mut().remove(&h);
            eprintln!("rargod: content {h} was to be sent ahead to worker {:x} but cannot be read here", w.id);
            return 0;
        };
        let batches: Vec<Vec<u32>> = {
            let has = w.has_chunks.borrow_mut();
            send_ahead::push_parts(&recipe, &|c| has.contains(c))
        };
        let total: u64 = recipe.iter().map(|c| c.len as u64).sum();
        let need: u64 = batches.iter().flatten().map(|i| recipe[*i as usize].len as u64).sum();
        w.pushed_bytes.set(w.pushed_bytes.get() + need);
        w.skipped_bytes.set(w.skipped_bytes.get() + total - need);
        {
            let secs = need as f64 / self.bandwidth.get().max(1) as f64;
            let mut free_at = w.link_free_at.borrow_mut();
            *free_at = (*free_at).max(crate::io::now()) + Duration::from_secs_f64(secs);
            let mut fl = w.in_flight.borrow_mut();
            let now = crate::io::now();
            if fl.len() > 4096 {
                fl.retain(|_, at| *at > now);
            }
            fl.insert(h, *free_at + IN_FLIGHT_SLACK);
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
        w.has_chunks.borrow_mut().extend(recipe.iter().map(|c| c.hash));
        need
    }
}

struct RemoteLive<'a> {
    early: &'a RefCell<BTreeMap<Vec<u8>, MEntry>>,
    pool: &'a Pool,
    worker: &'a Worker,
}

impl Live for RemoteLive<'_> {
    fn ingest(&self, path: &[u8]) -> Option<MEntry> {
        let entry = self.early.borrow_mut().get(path).cloned()?;
        if let MEntry::File { hash, .. } = &entry {
            let (pool, worker, hash) = (self.pool, self.worker, *hash);
            let ok = pool.blobs.fetch(&worker.peer, &hash).is_ok();
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
    fn predict(
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
                    .map(|t| send_ahead::profile_obs(&t.obs, &self.profile_exclude))
                    .unwrap_or_default();
                obs.extend(view.hints().into_iter().map(|p| (VPath::from(p), Obs::Dir)));
                obs.extend(
                    extern_paths(&spec.argv).into_iter().map(|p| (VPath::from(p), Obs::File { hash: Hash::ZERO, exec: false })),
                );
                if spec.kind == ActionKind::Rustc && links(&spec.argv) && engine.profile(spec).is_none() {
                    if let Some(program) = spec.argv.first() {
                        let view = view.clone();
                        let program = program.clone();
                        let libs = crate::io::cpu(move || sysroot_libs(&*view, &program));
                        obs.extend(libs.into_iter().map(|p| (VPath::from(p), Obs::File { hash: Hash::ZERO, exec: false })));
                    }
                }
                if spec.kind == ActionKind::Rustc {
                    if let Some(root) = crate_root(&spec.argv, &spec.cwd) {
                        let view = view.clone();
                        let (dirs, files) = crate::io::cpu(move || source_tree(&*view, &root));
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
            let host_srcs = self.host_srcs.clone();
            let (p, l, files, served) = crate::io::cpu(move || {
                let mut served = Vec::new();
                let mut prefill = Vec::new();
                let mut listings = Vec::new();
                let mut files = Vec::new();
                let mut seen_dirs: BTreeSet<Vec<u8>> = BTreeSet::new();
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
                                served.push(f.src.clone());
                            }
                        }
                        if let (Looked::File(f), true) = (&looked, matches!(o, Obs::File { .. }) && vjoin(&dir, &name) == path[..]) {
                            if let Ok(h) = content.hash(&f.src) {
                                if let FileSrc::Host { .. } = &f.src {
                                    host_srcs.entry(h).or_insert_with(|| f.src.clone());
                                    served.push(f.src.clone());
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
                (prefill, listings, files, served)
            });
            ra.served.borrow_mut().extend(served);
            prefill = p;
            listings = l;
            let has = w.has_blobs.borrow_mut();
            let fl = w.in_flight.borrow_mut();
            let now = crate::io::now();
            incoming = send_ahead::incoming(files, &|h| has.contains(h), &|h| still_in_flight(&fl, h, now));
        }
        closed.retain(|d| listings.iter().any(|(l, _)| l == d));
        Ok((prefill, listings, incoming, closed))
    }

    fn run_on(
        &self,
        w: &Arc<Worker>,
        engine: &Engine,
        spec: &ActionSpec,
        key: &Hash,
        req: &Request,
        voice: &Voice,
    ) -> RunResult {
        let action = self.next_action.next();
        let (tx, mut rx) = crate::io::unbounded_channel();
        let t0 = crate::io::now();
        let view: Arc<dyn View> = req.view.clone();
        let ra = Arc::new(RemoteAction {
            view: view.clone(),
            events: tx,
            served: RefCell::new(Vec::new()),
            asked: Cell::new(0),
        });
        w.actions.borrow_mut().insert(action, ra.clone());
        struct Cleanup<'a>(&'a Pool, &'a Worker, u64, std::cell::Cell<bool>);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                self.1.actions.borrow_mut().remove(&self.2);
                let fx = self.0.core.borrow_mut().finished(self.2, self.3.get());
                self.0.pool_effects(fx);
                self.0.give_back(self.1);
            }
        }
        let cleanup = Cleanup(self, w, action, std::cell::Cell::new(false));
        if !self.core.borrow_mut().sent(action, w.id) {
            bail!("worker connection lost");
        }

        let (mut prefill, mut listings, incoming, mut closed) = self.predict(w, engine, spec, key, &view, &ra)?;
        if !req.seed_blobs.is_empty() {
            let (view, content, dirs) = (view.clone(), self.content.clone(), spec.discard.clone());
            let (p, l) = crate::io::cpu(move || side_tree(&*view, &content, &dirs));
            closed.extend(l.iter().map(|(d, _)| d.clone()));
            prefill.extend(p);
            listings.extend(l);
        }
        self.stats.prefilled_entries.set(self.stats.prefilled_entries.get() + prefill.len() as u64);
        {
            let mut held = w.store_paths.borrow_mut();
            let mut closure: Vec<(Vec<u8>, Hash)> = view.nix_closure().to_vec();
            closure.sort_by(|a, b| a.0.cmp(&b.0));
            let mut store_paths = Vec::new();
            for (name, root) in &mut closure {
                if held.contains(name) {
                    continue;
                }
                match crate::nixmanifest::load(engine.store.db().as_ref(), name) {
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
        }
        if !incoming.is_empty() {
            for h in incoming {
                self.push_blob(w, h);
            }
        }

        let early: RefCell<BTreeMap<Vec<u8>, MEntry>> = RefCell::new(BTreeMap::new());
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let t_sent = crate::io::now().saturating_duration_since(t0);
        let done = loop {
            let Some(ev) = rx.recv() else { bail!("worker connection lost") };
            if !matches!(ev, Event::Done(_)) && !voice.claim(Who::Remote) {
                stop_here();
            }
            match ev {
                Event::Stdout(d) => {
                    req.observer.stdout(&d);
                    stdout.extend_from_slice(&d);
                }
                Event::Early(path, entry) => {
                    if let MEntry::File { hash, .. } = &entry {
                        w.has_blobs.borrow_mut().insert(*hash);
                    }
                    early.borrow_mut().insert(path, entry);
                }
                Event::Line(line) => {
                    let live = RemoteLive { early: &early, pool: self, worker: w };
                    req.observer.stderr_line(&line, Some(&live));
                    stderr.extend_from_slice(&line);
                    stderr.push(b'\n');
                }
                Event::Done(r) => {
                    cleanup.3.set(true);
                    break r.map_err(|e| anyhow!("worker: {e}"))?;
                }
            }
        };

        let t_done = crate::io::now().saturating_duration_since(t0);
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
        self.blobs.fetch_all(&w.peer, &wanted).context("fetching an output from the worker")?;
        {
            let mut has = w.has_blobs.borrow_mut();
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
                    self.left_on.borrow_mut().insert(*h, (w.id, spec.label.clone()));
                }
                self.stats.state_left_blobs.set(self.stats.state_left_blobs.get() + left.len() as u64);
                self.stats.state_left_bytes.set(self.stats.state_left_bytes.get() + left.iter().map(|(_, s)| s).sum::<u64>());
                let now = crate::io::now();
                self.home_queue.borrow_mut().extend(left.into_iter().map(|(h, s)| (w.clone(), h, s, now)));
                let _ = self.home_wake.send_replace(());
                let _ = self.settle_wake.send_replace(());
            }
        }
        let mut tainted = done.tainted;
        let served = ra.served.borrow().clone();
        for src in served.iter() {
            if let FileSrc::Host { path, sig } = src {
                match crate::io::file::symlink_metadata(path) {
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
        w.note_overhead(crate::io::now().saturating_duration_since(t0).saturating_sub(Duration::from_micros(result.stats.wall_us)));
        {
            let bytes: u64 = wanted_bytes;
            let secs = (crate::io::now().saturating_duration_since(t0) - t_done).as_secs_f64();
            if bytes > (4 << 20) && secs > 0.01 {
                let seen = (bytes as f64 / secs) as u64;
                let old = self.bandwidth.get();
                self.bandwidth.set((old * 3 + seen) / 4);
            }
        }
        if self.tuning.trace {
            eprintln!(
                "rargod: remote {:x} `{}`: {:.2}s total = {:.2}s to send, {:.2}s until done (worker ran it {:.2}s, {} questions), {:.2}s fetching outputs",
                w.id,
                spec.label,
                crate::io::now().saturating_duration_since(t0).as_secs_f64(),
                t_sent.as_secs_f64(),
                (t_done - t_sent).as_secs_f64(),
                result.stats.wall_us as f64 / 1e6,
                ra.asked.get(),
                (crate::io::now().saturating_duration_since(t0) - t_done).as_secs_f64(),
            );
        }
        Ok((result, done.obs, tainted, side))
    }
}

fn stop_here() -> ! {
    loop {
        crate::io::sleep(Duration::from_secs(3600));
    }
}

impl RemoteRunner for Pool {
    fn holds(&self, blob: &Hash) -> bool {
        self.holder(blob).is_some()
    }

    fn offer_state(&self, whose: (Hash, String), blobs: Arc<Vec<(Hash, u64)>>) {
        if !self.tuning.state_ahead || self.worker_count() == 0 {
            return;
        }
        self.ahead.borrow_mut().offer(whose, blobs);
        let _ = self.ahead_wake.send_replace(());
        let _ = self.settle_wake.send_replace(());
    }

    fn bring_home(&self, blobs: &[Hash]) -> Result<()> {
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
            self.stats.state_needed_blobs.set(self.stats.state_needed_blobs.get() + hashes.len() as u64);
            self.blobs.fetch_all(&w.peer, &hashes)?;
        }
        Ok(())
    }

    fn try_run(&self, engine: &Engine, spec: &ActionSpec, key: &Hash, req: &Request) -> Option<RunResult> {
        {
            if matches!(spec.kind, ActionKind::Plan | ActionKind::Nextest)
                || spec.net != NetMode::Loopback
            {
                return None;
            }
            let seeded = !spec.discard.is_empty();
            let mut holders: Option<Vec<u64>> = None;
            let mut away = false;
            if seeded {
                if !self.tuning.remote_incr {
                    self.stats.kept_local_incremental.set(self.stats.kept_local_incremental.get() + 1);
                    return None;
                }
                if !req.seed_blobs.is_empty() {
                    let ids = self.seed_holders(&req.seed_blobs, &engine.store);
                    if ids.is_empty() {
                        self.stats.kept_local_incremental.set(self.stats.kept_local_incremental.get() + 1);
                        return None;
                    }
                    away = !engine.seed_away(req).is_empty();
                    holders = Some(ids);
                }
            }
            let mem = engine.estimate_mem_kb(key, spec.kind);
            let (_, waiting, _, _) = engine.sched.snapshot();
            if self.tuning.trace && seeded {
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
            let placed = pool_state::placing(engine.sched.free_tokens() > 0, waiting > 0, away);
            let w = if placed != pool_state::Placing::RaceLocal {
                match if placed == pool_state::Placing::LeaseNow { self.lease(holders.as_deref()) } else { None } {
                    Some(w) => w,
                    None => {
                        self.stats.kept_local_no_wait.set(self.stats.kept_local_no_wait.get() + 1);
                        return None;
                    }
                }
            } else {
                enum First {
                    Leased(Arc<Worker>),
                    Local(crate::sched::Permit),
                }
                let arms: Vec<Box<dyn FnOnce() -> First + '_>> = vec![
                    Box::new(|| First::Leased(self.lease_wait(holders.as_deref()))),
                    Box::new(|| First::Local(engine.sched.acquire(req.prio, mem))),
                ];
                match crate::io::race_biased(arms) {
                    First::Leased(w) => w,
                    First::Local(permit) => {
                        self.stats.kept_local_first.set(self.stats.kept_local_first.get() + 1);
                        if let Err(e) = engine.seed_home(req) {
                            return Some(Err(e));
                        }
                        let r = engine.run_local(spec, req, &*req.observer);
                        drop(permit);
                        return Some(r);
                    }
                }
            };
            let own = engine.traces.traces(key).first().map(|(_, t)| t.clone());
            let known = own.clone().or_else(|| engine.profile(spec));
            let missing: u64 = match &known {
                Some(t) => {
                    let has = w.has_blobs.borrow_mut();
                    send_ahead::trace_bytes(
                        &t.obs,
                        own.is_some(),
                        &self.profile_exclude,
                        &|h| has.contains(h),
                        &|h| engine.store.size_of(h),
                    )
                }
                None => 0,
            };
            let queued = w.link_free_at.borrow_mut().saturating_duration_since(crate::io::now());
            let bytes_time = queued + Duration::from_secs_f64(missing as f64 / self.bandwidth.get().max(1) as f64);
            if self.tuning.trace && bytes_time > engine.avg_action_wall() {
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
                self.tuning.eager_dispatch,
            );
            if !send {
                if self.core.borrow_mut().warm(w.id, known.is_some(), own.is_some()) {
                    let (tx, _rx) = crate::io::unbounded_channel();
                    let ra = Arc::new(RemoteAction {
                        view: req.view.clone(),
                        events: tx,
                        served: RefCell::new(Vec::new()),
                        asked: Cell::new(0),
                    });
                    if let Ok((_, _, incoming, _)) = self.predict(&w, engine, spec, key, &req.view, &ra) {
                        for h in incoming {
                            self.push_blob(&w, h);
                        }
                    }
                }
                self.give_back(&w);
                self.stats.kept_local_transfer.set(self.stats.kept_local_transfer.get() + 1);
                return None;
            }
            self.stats.dispatched.set(self.stats.dispatched.get() + 1);
            engine.stats.remote_runs.set(engine.stats.remote_runs.get() + 1);
            if seeded {
                let which = if req.seed_blobs.is_empty() { &self.stats.unseeded_remote } else { &self.stats.seeded_in_place };
                which.set(which.get() + 1);
            }
            let voice = Voice::new();
            let gated = Gated { inner: &*req.observer, voice: &voice };
            enum Wake {
                Remote(RunResult),
                Local(RunResult),
                Permit(crate::sched::Permit),
                Voice,
            }
            let mut local: Option<(usize, crate::sched::Permit)> = None;
            let (tx, mut rx) = crate::io::unbounded_channel::<Wake>();
            let mut scope = Scope::default();
            let mut remote = Some({
                let (tx, w, voice) = (tx.clone(), &w, &voice);
                scope.spawn(move || {
                    let _ = tx.send(Wake::Remote(self.run_on(w, engine, spec, key, req, voice)));
                })
            });
            {
                let mut changed = voice.changed.subscribe();
                let tx = tx.clone();
                scope.spawn(move || {
                    while changed.changed().is_ok() && tx.send(Wake::Voice).is_ok() {}
                });
            }
            let mut acquiring: Option<usize> = None;
            loop {
                let settled = voice.race.borrow_mut().settle();
                match settled {
                    Some(Settle::StopLocal) => {
                        self.stats.hedge_stopped.set(self.stats.hedge_stopped.get() + 1);
                        if let Some((run, _)) = local.take() {
                            scope.abort(run);
                        }
                    }
                    Some(Settle::DropRemote) => {
                        if let Some(run) = remote.take() {
                            scope.abort(run);
                        }
                    }
                    None => {}
                }
                let may_hedge = voice.race.borrow_mut().may_hedge(away, self.tuning.hedge);
                let wants_permit = local.is_none() && remote.is_some() && may_hedge;
                match (wants_permit, acquiring) {
                    (true, None) => {
                        let tx = tx.clone();
                        acquiring = Some(scope.spawn(move || {
                            let _ = tx.send(Wake::Permit(engine.sched.acquire(crate::sched::Priority::Idle, mem)));
                        }));
                    }
                    (false, Some(waiting)) => {
                        scope.abort(waiting);
                        acquiring = None;
                    }
                    _ => {}
                }
                match rx.recv().expect("the try_run holds a sender") {
                    Wake::Remote(r) => {
                        if remote.take().is_none() {
                            continue;
                        }
                        let outcome = voice.race.borrow_mut().remote_ended(r.is_ok());
                        match r {
                            Ok(r) => {
                                if matches!(outcome, Outcome::Use { stopped_other: true, .. }) {
                                    self.stats.hedge_stopped.set(self.stats.hedge_stopped.get() + 1);
                                }
                                return Some(Ok(r));
                            }
                            Err(e) => {
                                eprintln!("rargod: worker {:x} failed `{}`: {e:#}; running locally", w.id, spec.label);
                                self.stats.worker_failures.set(self.stats.worker_failures.get() + 1);
                                engine.stats.remote_failed.set(engine.stats.remote_failed.get() + 1);
                                if outcome == Outcome::RunElsewhere {
                                    return None;
                                }
                            }
                        }
                    }
                    Wake::Local(r) => {
                        let Some((_, permit)) = local.take() else { continue };
                        drop(permit);
                        voice.race.borrow_mut().local_ended();
                        self.stats.hedge_won.set(self.stats.hedge_won.get() + 1);
                        return Some(r);
                    }
                    Wake::Permit(permit) => {
                        acquiring = None;
                        if local.is_some() || remote.is_none() {
                            continue;
                        }
                        voice.race.borrow_mut().hedged();
                        self.stats.hedged.set(self.stats.hedged.get() + 1);
                        let (tx, gated) = (tx.clone(), &gated);
                        let run = scope.spawn(move || {
                            let _ = tx.send(Wake::Local(engine.run_local(spec, req, gated)));
                        });
                        local = Some((run, permit));
                    }
                    Wake::Voice => {}
                }
            }
        }
    }
}

#[derive(Default)]
struct Scope<'a> {
    fibers: Vec<Option<crate::io::JoinHandle<()>>>,
    borrows: std::marker::PhantomData<&'a ()>,
}

impl<'a> Scope<'a> {
    fn spawn(&mut self, body: impl FnOnce() + 'a) -> usize {
        let body: Box<dyn FnOnce() + 'a> = Box::new(body);
        let body: Box<dyn FnOnce() + 'static> = unsafe { std::mem::transmute(body) };
        self.fibers.push(Some(crate::io::spawn(body)));
        self.fibers.len() - 1
    }

    fn abort(&mut self, fiber: usize) {
        if let Some(handle) = self.fibers[fiber].take() {
            handle.abort();
        }
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        for fiber in 0..self.fibers.len() {
            self.abort(fiber);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_seed_numbers_actions_the_same_way() {
        use super::ActionIds;
        use crate::rng::Rng;
        let ids = |seed| {
            let a = ActionIds::new(&mut Rng::new(seed));
            (0..5).map(|_| a.next()).collect::<Vec<_>>()
        };
        assert_eq!(ids(42), ids(42));
        assert_ne!(ids(42), ids(43));
        let first = ids(42)[0];
        assert_eq!(ids(42), (first..first + 5).collect::<Vec<_>>(), "actions are numbered in a row");
        assert!(first < 1 << 63, "the numbers have room to count up");
    }

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
        use std::time::Duration;
        crate::io::sim::test(|| {
            let now = crate::io::now();
            let mut fl = std::collections::BTreeMap::new();
            fl.insert(Hash([1; 32]), now + Duration::from_secs(1));
            fl.insert(Hash([2; 32]), now);
            assert!(super::still_in_flight(&fl, &Hash([1; 32]), now));
            assert!(!super::still_in_flight(&fl, &Hash([2; 32]), now));
            assert!(!super::still_in_flight(&fl, &Hash([3; 32]), now));
        });
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
