use crate::hash::Hash;
use crate::session_state::{Ask, ClientSet, Hello, Progress, SessionState};
use crate::snapshot::{Uploader, futures::BoxFuture};
use crate::store::Store;
use anyhow::{Result, anyhow, bail};
use parking_lot::Mutex;
use rargo_proto::secure::{self, Opener, Sealer};
use rargo_proto::{Blob, ClientMsg, Delivery, Digest, PART_SIZE, ServerMsg, Write};
use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, oneshot};

pub struct Keys {
    open: bool,
    tokens: Option<Arc<crate::tokens::TokenStore>>,
}

impl Keys {
    pub fn open() -> Keys {
        Keys { open: true, tokens: None }
    }

    pub fn with_tokens(store: Arc<crate::tokens::TokenStore>) -> Keys {
        Keys { open: false, tokens: Some(store) }
    }

    pub fn tokens(&self) -> Option<&Arc<crate::tokens::TokenStore>> {
        self.tokens.as_ref()
    }

    fn lookup(&self, user: &str) -> Option<([u8; 32], bool)> {
        if self.open {
            return Some((secure::open_psk(), true));
        }
        self.tokens.as_ref()?.lookup(user).map(|(key, admin, _)| (key, admin))
    }

    pub fn still_valid(&self, user: &str) -> bool {
        self.lookup(user).is_some()
    }

    pub fn name_of(&self, user: &str) -> String {
        if self.open {
            return user.to_string();
        }
        self.tokens.as_ref().and_then(|t| t.lookup(user)).map(|(_, _, name)| name).unwrap_or_else(|| user.to_string())
    }

    pub fn psk(&self, user: &str) -> Option<[u8; 32]> {
        self.lookup(user).map(|(k, _)| k)
    }

    pub fn admin(&self, user: &str) -> bool {
        self.lookup(user).is_some_and(|(_, admin)| admin)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
}

fn bulk_size(m: &ServerMsg) -> usize {
    match m {
        ServerMsg::Blobs(v) => v.iter().map(|b| b.data.len()).sum(),
        ServerMsg::BlobPart { part, .. } => part.data.len(),
        _ => 0,
    }
}

struct Part {
    path: PathBuf,
    file: std::fs::File,
    hasher: blake3::Hasher,
}

#[derive(Default)]
pub struct ClientHas {
    set: Mutex<ClientSet>,
}

impl ClientHas {
    pub fn extend(&self, digests: impl IntoIterator<Item = Digest>) {
        self.set.lock().extend(digests);
    }
}

pub struct Session {
    tx: UnboundedSender<ServerMsg>,
    has: Arc<ClientHas>,
    state: Mutex<SessionState>,
    store: Arc<Store>,
    trees_reply: Mutex<Option<oneshot::Sender<Vec<Vec<u8>>>>>,
    env_reply: Mutex<Option<oneshot::Sender<Vec<(String, Option<String>)>>>>,
    report_reply: Mutex<Option<oneshot::Sender<u32>>>,
    asking: tokio::sync::Mutex<()>,
    arrivals: Notify,
    window: Notify,
    parts: Mutex<BTreeMap<Digest, Part>>,
    totals: Arc<crate::build::DaemonStats>,
}

fn sizes_of(files: &[Delivery], writes: &[Write]) -> BTreeMap<Digest, u64> {
    let mut sizes: BTreeMap<Digest, u64> = files.iter().map(|f| (f.hash, f.size)).collect();
    for w in writes {
        if let Write::File { hash, size, .. } = w {
            sizes.insert(*hash, *size);
        }
    }
    sizes
}

impl Session {
    pub fn new(store: Arc<Store>) -> (Arc<Session>, UnboundedReceiver<ServerMsg>) {
        Session::on_link(store, Arc::new(ClientHas::default()), Arc::new(crate::build::DaemonStats::default()))
    }

    pub fn on_link(
        store: Arc<Store>,
        has: Arc<ClientHas>,
        totals: Arc<crate::build::DaemonStats>,
    ) -> (Arc<Session>, UnboundedReceiver<ServerMsg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let s = Arc::new(Session {
            tx,
            has,
            state: Mutex::new(SessionState::new()),
            store,
            trees_reply: Mutex::new(None),
            env_reply: Mutex::new(None),
            report_reply: Mutex::new(None),
            asking: tokio::sync::Mutex::new(()),
            arrivals: Notify::new(),
            window: Notify::new(),
            parts: Mutex::new(BTreeMap::new()),
            totals,
        });
        (s, rx)
    }

    pub fn knows(&self, digests: impl IntoIterator<Item = Digest>) {
        self.has.extend(digests);
        self.state.lock().set_knows();
    }

    pub fn set_lazy(&self) {
        self.state.lock().set_lazy();
    }

    pub fn is_lazy(&self) -> bool {
        self.state.lock().is_lazy()
    }

    pub fn deliver_lazy(&self, files: Vec<Delivery>, writes: Vec<Write>) {
        if files.is_empty() && writes.is_empty() {
            return;
        }
        let sizes = sizes_of(&files, &writes);
        let id = self.state.lock().delivery(&sizes);
        let small = self.has.set.lock().push_small(&sizes);
        self.send(ServerMsg::Deliver { id, files, writes });
        let mut batch: Vec<Blob> = Vec::new();
        let mut bytes = 0usize;
        for h in small {
            let Ok(content) = self.store.read(&Hash(h)) else { continue };
            let b = Blob::pack(h, &content);
            bytes += b.data.len();
            batch.push(b);
            if bytes >= 1 << 20 {
                self.totals.bytes_out.fetch_add(bytes as u64, Ordering::Relaxed);
                self.send(ServerMsg::Blobs(std::mem::take(&mut batch)));
                bytes = 0;
            }
        }
        if !batch.is_empty() {
            self.totals.bytes_out.fetch_add(bytes as u64, Ordering::Relaxed);
            self.send(ServerMsg::Blobs(batch));
        }
        self.send(ServerMsg::Delivered { id });
    }

    pub fn delivered(&self) -> HashSet<Digest> {
        self.state.lock().delivered().iter().copied().collect()
    }

    pub fn sent_bulk(&self, bytes: usize) {
        self.state.lock().sent_bulk(bytes);
        self.window.notify_waiters();
    }

    pub fn close_window(&self) {
        self.state.lock().close_window();
        self.window.notify_waiters();
    }

    pub fn sender(&self) -> UnboundedSender<ServerMsg> {
        self.tx.clone()
    }

    pub fn send(&self, m: ServerMsg) {
        let _ = self.tx.send(m);
    }

    fn gone(&self) -> anyhow::Error {
        if self.state.lock().has_changed() {
            anyhow!("the worktree changed while it was being synced")
        } else {
            anyhow!("the client went away")
        }
    }

    async fn send_bulk(&self, m: ServerMsg) -> Result<()> {
        let bytes = bulk_size(&m);
        loop {
            let freed = self.window.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            match self.state.lock().take_bulk(bytes) {
                Err(_) => return Err(self.gone()),
                Ok(true) => break,
                Ok(false) => {}
            }
            freed.await;
        }
        self.totals.bytes_out.fetch_add(crate::session_state::bulk_cost(bytes) as u64, Ordering::Relaxed);
        self.tx.send(m).map_err(|_| self.gone())
    }

    pub fn close(&self) {
        let parts = self.state.lock().close();
        self.trees_reply.lock().take();
        self.env_reply.lock().take();
        self.report_reply.lock().take();
        self.window.notify_waiters();
        self.arrivals.notify_waiters();
        let mut files = self.parts.lock();
        for h in parts {
            if let Some(p) = files.remove(&h) {
                let _ = std::fs::remove_file(&p.path);
            }
        }
        assert!(files.is_empty(), "every partial upload is one the session knows of");
    }

    fn store_blob(&self, claimed: Digest, bytes: &[u8]) -> Result<()> {
        self.totals.bytes_in.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if Hash::of(bytes).0 != claimed {
            bail!("the client sent content that does not match its hash");
        }
        self.store.put_bytes(bytes)?;
        assert!(self.store.has(&Hash(claimed)), "a stored blob is in the store");
        self.totals.files_in.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn incoming(&self, msg: ClientMsg) -> Result<()> {
        match msg {
            ClientMsg::Run(_)
            | ClientMsg::Packed(_)
            | ClientMsg::Fetch { .. }
            | ClientMsg::Recipe { .. }
            | ClientMsg::Chunks { .. }
            | ClientMsg::Offer(_)
            | ClientMsg::Cancel
            | ClientMsg::Ping(_) => bail!("unexpected message from the client"),
            ClientMsg::Trees(t) => {
                if self.state.lock().answer(Ask::Trees)
                    && let Some(tx) = self.trees_reply.lock().take()
                {
                    let _ = tx.send(t);
                }
            }
            ClientMsg::Env(e) => {
                if self.state.lock().answer(Ask::Env)
                    && let Some(tx) = self.env_reply.lock().take()
                {
                    let _ = tx.send(e);
                }
            }
            ClientMsg::ReportId(id) => {
                if self.state.lock().answer(Ask::Report)
                    && let Some(tx) = self.report_reply.lock().take()
                {
                    let _ = tx.send(id);
                }
            }
            ClientMsg::Changed => {
                self.state.lock().changed();
                self.arrivals.notify_waiters();
            }
            ClientMsg::Blobs(blobs) => {
                tokio::task::block_in_place(|| -> Result<()> {
                    for b in blobs {
                        let hash = b.hash;
                        self.store_blob(hash, &b.unpack()?)?;
                    }
                    Ok(())
                })?;
                self.arrivals.notify_waiters();
            }
            ClientMsg::BlobPart { hash, part, last } => {
                let done = tokio::task::block_in_place(|| -> Result<bool> {
                    self.totals.bytes_in.fetch_add(part.data.len() as u64, Ordering::Relaxed);
                    let data = part.unpack()?;
                    let step = self.state.lock().part(hash, data.len() as u64, last);
                    let mut parts = self.parts.lock();
                    if step.begin {
                        let path = self.store.tmp_path();
                        let file = match std::fs::File::create(&path) {
                            Ok(f) => f,
                            Err(e) => {
                                self.state.lock().part_failed(&hash);
                                return Err(e.into());
                            }
                        };
                        parts.insert(hash, Part { path, file, hasher: blake3::Hasher::new() });
                    }
                    let Some(p) = parts.get_mut(&hash) else { bail!("a part arrived for an upload that was closed") };
                    p.file.write_all(&data)?;
                    p.hasher.update(&data);
                    let Some(size) = step.finish else { return Ok(false) };
                    let p = parts.remove(&hash).expect("the finished part is held");
                    drop(parts);
                    drop(p.file);
                    if *p.hasher.finalize().as_bytes() != hash {
                        let _ = std::fs::remove_file(&p.path);
                        bail!("the client sent content that does not match its hash");
                    }
                    self.store.install_hashed(&p.path, Hash(hash), size)?;
                    assert!(self.store.has(&Hash(hash)), "an installed blob is in the store");
                    self.totals.files_in.fetch_add(1, Ordering::Relaxed);
                    Ok(true)
                })?;
                if done {
                    self.arrivals.notify_waiters();
                }
            }
        }
        Ok(())
    }

    pub async fn want_env(&self, names: Vec<String>) -> Result<Vec<(String, Option<String>)>> {
        let _g = self.asking.lock().await;
        let (tx, rx) = oneshot::channel();
        *self.env_reply.lock() = Some(tx);
        if self.state.lock().ask(Ask::Env).is_err() {
            self.env_reply.lock().take();
            return Err(self.gone());
        }
        self.send(ServerMsg::WantEnv(names));
        rx.await.map_err(|_| self.gone())
    }

    pub async fn report(&self, report: rargo_proto::FutureReport) -> Result<u32> {
        let _g = self.asking.lock().await;
        let (tx, rx) = oneshot::channel();
        *self.report_reply.lock() = Some(tx);
        if self.state.lock().ask(Ask::Report).is_err() {
            self.report_reply.lock().take();
            return Err(self.gone());
        }
        self.send(ServerMsg::Report(Box::new(report)));
        rx.await.map_err(|_| self.gone())
    }

    pub async fn deliver(&self, files: Vec<Delivery>, writes: Vec<Write>) -> Result<()> {
        if files.is_empty() && writes.is_empty() {
            return Ok(());
        }
        let sizes = sizes_of(&files, &writes);
        let (id, knows, closed) = {
            let mut st = self.state.lock();
            (st.delivery(&sizes), st.knows(), st.is_closed())
        };
        let push = self.has.set.lock().push(&sizes, knows);
        self.send(ServerMsg::Deliver { id, files, writes });
        if closed {
            return Err(self.gone());
        }
        self.send_blobs(push).await?;
        self.send(ServerMsg::Delivered { id });
        Ok(())
    }

    pub fn count_out(&self, bytes: usize) {
        self.totals.bytes_out.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub async fn fetch(&self, id: u64, hashes: Vec<Digest>) -> Result<()> {
        let wanted: Vec<(Digest, u64)> =
            hashes.into_iter().filter_map(|h| self.store.size_of(&Hash(h)).map(|size| (h, size))).collect();
        self.has.extend(wanted.iter().map(|(h, _)| *h));
        let sent = self.send_blobs(wanted).await;
        self.send(ServerMsg::Fetched { id });
        sent
    }

    async fn send_blobs(&self, blobs: Vec<(Digest, u64)>) -> Result<()> {
        let mut batch: Vec<Blob> = Vec::new();
        let mut batch_bytes = 0usize;
        for (h, size) in blobs {
            let hash = Hash(h);
            if size as usize <= PART_SIZE {
                let store = self.store.clone();
                let bytes = tokio::task::spawn_blocking(move || store.read(&hash)).await??;
                let b = Blob::pack(h, &bytes);
                batch_bytes += b.data.len();
                batch.push(b);
                if batch_bytes >= rargo_proto::BATCH_SIZE {
                    batch_bytes = 0;
                    self.send_bulk(ServerMsg::Blobs(std::mem::take(&mut batch))).await?;
                }
                assert!(batch_bytes < rargo_proto::BATCH_SIZE);
                continue;
            }
            let store = self.store.clone();
            let mut file =
                tokio::fs::File::from_std(tokio::task::spawn_blocking(move || store.open_blob(&hash)).await??);
            let mut sent = 0u64;
            loop {
                let mut data = vec![0u8; PART_SIZE];
                let mut n = 0;
                while n < data.len() {
                    let got = file.read(&mut data[n..]).await?;
                    if got == 0 {
                        break;
                    }
                    n += got;
                }
                data.truncate(n);
                sent += n as u64;
                let last = sent >= size || n == 0;
                let part = tokio::task::spawn_blocking(move || rargo_proto::Part::pack(data)).await?;
                self.send_bulk(ServerMsg::BlobPart { hash: h, part, last }).await?;
                if last {
                    break;
                }
            }
        }
        if !batch.is_empty() {
            self.send_bulk(ServerMsg::Blobs(batch)).await?;
        }
        Ok(())
    }
}

impl Uploader for Session {
    fn trees<'a>(&'a self, want: Vec<Hash>) -> BoxFuture<'a, Result<Vec<Vec<u8>>>> {
        Box::pin(async move {
            let _g = self.asking.lock().await;
            let (tx, rx) = oneshot::channel();
            *self.trees_reply.lock() = Some(tx);
            if self.state.lock().ask(Ask::Trees).is_err() {
                self.trees_reply.lock().take();
                return Err(self.gone());
            }
            self.send(ServerMsg::WantTrees(want.iter().map(|h| h.0).collect()));
            rx.await.map_err(|_| self.gone())
        })
    }

    fn blobs<'a>(&'a self, want: Vec<Hash>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _g = self.asking.lock().await;
            let digests: Vec<Digest> = want.iter().map(|h| h.0).collect();
            self.state.lock().want(&digests, &|d| self.store.has(&Hash(*d)));
            let wanting = Wanting(self);
            self.send(ServerMsg::WantBlobs(digests));
            loop {
                let arrived = self.arrivals.notified();
                tokio::pin!(arrived);
                arrived.as_mut().enable();
                match self.state.lock().progress(&|d| self.store.has(&Hash(*d))) {
                    Progress::Done => break,
                    Progress::Gone => return Err(self.gone()),
                    Progress::Wait => {}
                }
                arrived.await;
            }
            drop(wanting);
            Ok(())
        })
    }
}

struct Wanting<'a>(&'a Session);

impl Drop for Wanting<'_> {
    fn drop(&mut self) {
        self.0.state.lock().abandon_want();
    }
}

pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    keys: &Keys,
) -> Result<(String, bool, Sealer, Opener)> {
    let mut buf = Vec::new();
    let (user, msg1) = loop {
        match crate::session_state::hello(&buf, secure::MAGIC) {
            Hello::Need(n) => {
                let have = buf.len();
                buf.resize(n, 0);
                stream.read_exact(&mut buf[have..]).await?;
            }
            Hello::Foreign(reply) => {
                let _ = stream.write_all(&reply).await;
                bail!("not a rargo client (or a different protocol version)");
            }
            Hello::BadUser => bail!("user name is not UTF-8"),
            Hello::Hello { user, msg1 } => break (user, msg1),
        }
    };
    let accepted =
        keys.lookup(&user).and_then(|(psk, admin)| secure::server_accept(&psk, &msg1).ok().map(|r| (r, admin)));
    let Some(((msg2, early, sealer, opener), admin)) = accepted else {
        let _ = stream.write_all(&crate::session_state::refusal(crate::proto::REFUSED)).await;
        bail!("refused `{user}`: unknown user or wrong key");
    };
    stream.write_all(&crate::session_state::welcome(&msg2)).await?;
    if !early.is_empty() {
        bail!("`{user}` sent a request with the handshake, which this protocol version does not do");
    }
    Ok((user, admin, sealer, opener))
}

pub fn bulk_of(m: &ServerMsg) -> usize {
    bulk_size(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_server_admits_anyone_and_one_with_tokens_their_holders() {
        assert!(Keys::open().lookup("anyone").unwrap().1);
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::tokens::TokenStore::new(dir.path().join("tokens.json")));
        let (ann, secret) = store.create("ann", true, None, "test").unwrap();
        let (bob, _) = store.create("bob", false, None, "test").unwrap();
        let k = Keys::with_tokens(store.clone());
        assert_eq!(k.lookup(&ann.id).unwrap(), (secure::psk_from_secret(secret.as_bytes()), true));
        assert!(!k.lookup(&bob.id).unwrap().1);
        assert!(k.lookup("ann").is_none(), "a name is not a token");
        assert_eq!(k.name_of(&bob.id), "bob");
        store.revoke(&bob.id).unwrap();
        assert!(!k.still_valid(&bob.id));
    }
}
