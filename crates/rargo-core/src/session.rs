//! One client connection: the encrypted channel, uploads coming in,
//! artifacts going out.
//!
//! A [`Session`] is the server's handle on a client for the duration of
//! one command: one stream of the client daemon's link (`serve::link`). Output is pushed through an ordered queue; bulk
//! data (file contents) is additionally held back by a byte window so that
//! a slow link applies back-pressure instead of filling memory.

use crate::hash::Hash;
use crate::snapshot::{Uploader, futures::BoxFuture};
use crate::store::Store;
use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use rargo_proto::secure::{self, Opener, Sealer};
use rargo_proto::{Blob, ClientMsg, Delivery, Digest, PART_SIZE, ServerMsg, Write};
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, Semaphore, oneshot};

/// Who may connect, and with which key.
pub struct Keys {
    users: HashMap<String, ([u8; 32], bool)>,
    /// No key file: anyone who can reach the (loopback) port is accepted,
    /// and is an administrator.
    open: bool,
}

impl Keys {
    pub fn open() -> Keys {
        Keys { users: HashMap::new(), open: true }
    }

    /// One user per line: `name secret [admin]`. `#` starts a comment.
    pub fn parse(text: &str) -> Result<Keys> {
        let mut users = HashMap::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(name), Some(secret)) = (it.next(), it.next()) else {
                bail!("key file line {}: expected `name secret [admin]`", i + 1);
            };
            if secret.len() < 16 {
                bail!("key file line {}: the secret of `{name}` needs at least 16 characters", i + 1);
            }
            let admin = match it.next() {
                None => false,
                Some("admin") => true,
                Some(other) => bail!("key file line {}: unknown word `{other}`", i + 1),
            };
            users.insert(name.to_string(), (secure::psk_from_secret(secret.as_bytes()), admin));
        }
        if users.is_empty() {
            bail!("the key file names no users");
        }
        Ok(Keys { users, open: false })
    }

    fn lookup(&self, user: &str) -> Option<([u8; 32], bool)> {
        if self.open { Some((secure::open_psk(), true)) } else { self.users.get(user).copied() }
    }

    /// The user's key, for the UDP transport's key agreement.
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

/// Bytes of bulk data that may sit in the outgoing queue.
const WINDOW: usize = 32 << 20;

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
    size: u64,
}

/// What the daemon at the other end of a link is known to hold: the
/// contents this server sent it or listed in a delivery it placed.
#[derive(Default)]
pub struct ClientHas {
    set: Mutex<HashSet<Digest>>,
}

impl ClientHas {
    pub fn extend(&self, digests: impl IntoIterator<Item = Digest>) {
        self.set.lock().extend(digests);
    }
}

pub struct Session {
    tx: UnboundedSender<ServerMsg>,
    has: Arc<ClientHas>,
    /// `has` covers what this command's worktree holds: what is not in it
    /// is sent without being asked for. Otherwise nothing is sent blind
    /// and the client fetches what it lacks.
    knows: AtomicBool,
    /// Everything this command delivered.
    delivered: Mutex<HashSet<Digest>>,
    window: Arc<Semaphore>,
    store: Arc<Store>,
    trees_reply: Mutex<Option<oneshot::Sender<Vec<Vec<u8>>>>>,
    env_reply: Mutex<Option<oneshot::Sender<Vec<(String, Option<String>)>>>>,
    report_reply: Mutex<Option<oneshot::Sender<u32>>>,
    /// One question to the client at a time.
    asking: tokio::sync::Mutex<()>,
    arrivals: Notify,
    changed: AtomicBool,
    closed: AtomicBool,
    parts: Mutex<HashMap<Digest, Part>>,
    next_id: AtomicU64,
    /// The server's totals, which this session adds to as it goes.
    totals: Arc<crate::build::DaemonStats>,
}

impl Session {
    pub fn new(store: Arc<Store>) -> (Arc<Session>, UnboundedReceiver<ServerMsg>) {
        Session::on_link(store, Arc::new(ClientHas::default()), Arc::new(crate::build::DaemonStats::default()))
    }

    /// A session on a link whose other end is known to hold `has`.
    pub fn on_link(
        store: Arc<Store>,
        has: Arc<ClientHas>,
        totals: Arc<crate::build::DaemonStats>,
    ) -> (Arc<Session>, UnboundedReceiver<ServerMsg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let s = Arc::new(Session {
            tx,
            has,
            knows: AtomicBool::new(false),
            delivered: Mutex::new(HashSet::new()),
            window: Arc::new(Semaphore::new(WINDOW)),
            store,
            trees_reply: Mutex::new(None),
            env_reply: Mutex::new(None),
            report_reply: Mutex::new(None),
            asking: tokio::sync::Mutex::new(()),
            arrivals: Notify::new(),
            changed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            parts: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            totals,
        });
        (s, rx)
    }

    /// The client holds `digests`, and with them the link's knowledge
    /// covers this command's worktree.
    pub fn knows(&self, digests: impl IntoIterator<Item = Digest>) {
        self.has.extend(digests);
        self.knows.store(true, Ordering::SeqCst);
    }

    /// Everything this command has delivered so far.
    pub fn delivered(&self) -> HashSet<Digest> {
        self.delivered.lock().clone()
    }

    /// Bulk data sent was handed to the link: room for more.
    pub fn sent_bulk(&self, bytes: usize) {
        self.window.add_permits(bytes.clamp(1, WINDOW));
    }

    pub fn close_window(&self) {
        self.window.close();
    }

    /// The queue everything sent to the client goes through.
    pub fn sender(&self) -> UnboundedSender<ServerMsg> {
        self.tx.clone()
    }

    pub fn send(&self, m: ServerMsg) {
        let _ = self.tx.send(m);
    }

    fn gone(&self) -> anyhow::Error {
        if self.changed.load(Ordering::SeqCst) {
            anyhow!("the worktree changed while it was being synced")
        } else {
            anyhow!("the client went away")
        }
    }

    /// Sends bulk data, waiting while too much is queued.
    async fn send_bulk(&self, m: ServerMsg) -> Result<()> {
        let n = bulk_size(&m).clamp(1, WINDOW) as u32;
        let permit = self.window.acquire_many(n).await.map_err(|_| self.gone())?;
        permit.forget();
        self.totals.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
        self.tx.send(m).map_err(|_| self.gone())
    }

    /// The connection is over: wake everyone who is waiting for the client.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.trees_reply.lock().take();
        self.env_reply.lock().take();
        self.report_reply.lock().take();
        self.window.close();
        self.arrivals.notify_waiters();
        for (_, p) in self.parts.lock().drain() {
            let _ = std::fs::remove_file(&p.path);
        }
    }

    fn store_blob(&self, claimed: Digest, bytes: &[u8]) -> Result<()> {
        self.totals.bytes_in.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if Hash::of(bytes).0 != claimed {
            bail!("the client sent content that does not match its hash");
        }
        self.store.put_bytes(bytes)?;
        self.totals.files_in.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Handles one message from the client. Runs on the connection's reader
    /// task; file system work is done in place.
    pub fn incoming(&self, msg: ClientMsg) -> Result<()> {
        match msg {
            ClientMsg::Run(_)
            | ClientMsg::Packed(_)
            | ClientMsg::Fetch { .. }
            | ClientMsg::Offer(_)
            | ClientMsg::Cancel
            | ClientMsg::Ping(_) => bail!("unexpected message from the client"),
            ClientMsg::Trees(t) => {
                if let Some(tx) = self.trees_reply.lock().take() {
                    let _ = tx.send(t);
                }
            }
            ClientMsg::Env(e) => {
                if let Some(tx) = self.env_reply.lock().take() {
                    let _ = tx.send(e);
                }
            }
            ClientMsg::ReportId(id) => {
                if let Some(tx) = self.report_reply.lock().take() {
                    let _ = tx.send(id);
                }
            }
            ClientMsg::Changed => {
                self.changed.store(true, Ordering::SeqCst);
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
                    let mut parts = self.parts.lock();
                    let part = match parts.entry(hash) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(v) => {
                            let path = self.store.tmp_path();
                            let file = std::fs::File::create(&path)?;
                            v.insert(Part { path, file, hasher: blake3::Hasher::new(), size: 0 })
                        }
                    };
                    part.file.write_all(&data)?;
                    part.hasher.update(&data);
                    part.size += data.len() as u64;
                    if !last {
                        return Ok(false);
                    }
                    let part = parts.remove(&hash).unwrap();
                    drop(parts);
                    drop(part.file);
                    if *part.hasher.finalize().as_bytes() != hash {
                        let _ = std::fs::remove_file(&part.path);
                        bail!("the client sent content that does not match its hash");
                    }
                    self.store.install_hashed(&part.path, Hash(hash), part.size)?;
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

    /// Asks the client for the values of environment variables.
    pub async fn want_env(&self, names: Vec<String>) -> Result<Vec<(String, Option<String>)>> {
        let _g = self.asking.lock().await;
        let (tx, rx) = oneshot::channel();
        *self.env_reply.lock() = Some(tx);
        self.send(ServerMsg::WantEnv(names));
        if self.closed.load(Ordering::SeqCst) {
            return Err(self.gone());
        }
        rx.await.map_err(|_| self.gone())
    }

    /// Has the client add a future-incompatibility report to its report
    /// file. Returns the report's number there.
    pub async fn report(&self, report: rargo_proto::FutureReport) -> Result<u32> {
        let _g = self.asking.lock().await;
        let (tx, rx) = oneshot::channel();
        *self.report_reply.lock() = Some(tx);
        self.send(ServerMsg::Report(Box::new(report)));
        if self.closed.load(Ordering::SeqCst) {
            return Err(self.gone());
        }
        rx.await.map_err(|_| self.gone())
    }

    /// Places files on the client. The contents the client is known to
    /// lack are sent along; nothing is asked, so a delivery costs no round
    /// trip. Returns once everything has been queued; the stream is
    /// ordered, so whatever is sent afterwards arrives after it.
    pub async fn deliver(&self, files: Vec<Delivery>, writes: Vec<Write>) -> Result<()> {
        if files.is_empty() && writes.is_empty() {
            return Ok(());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut sizes: HashMap<Digest, u64> = files.iter().map(|f| (f.hash, f.size)).collect();
        for w in &writes {
            if let Write::File { hash, size, .. } = w {
                sizes.insert(*hash, *size);
            }
        }
        let push: Vec<(Digest, u64)> = {
            let mut has = self.has.set.lock();
            let knows = self.knows.load(Ordering::SeqCst);
            let mut push = Vec::new();
            for (h, size) in &sizes {
                if has.insert(*h) && knows {
                    push.push((*h, *size));
                }
            }
            push
        };
        self.delivered.lock().extend(sizes.keys().copied());
        self.send(ServerMsg::Deliver { id, files, writes });
        if self.closed.load(Ordering::SeqCst) {
            return Err(self.gone());
        }
        self.send_blobs(push).await?;
        self.send(ServerMsg::Delivered { id });
        Ok(())
    }

    /// Sends contents the client asked for, then says that that was all.
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
                continue;
            }
            let store = self.store.clone();
            let mut file = tokio::fs::File::from_std(tokio::task::spawn_blocking(move || store.open_blob(&hash)).await??);
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
            self.send(ServerMsg::WantTrees(want.iter().map(|h| h.0).collect()));
            if self.closed.load(Ordering::SeqCst) {
                return Err(self.gone());
            }
            rx.await.map_err(|_| self.gone())
        })
    }

    fn blobs<'a>(&'a self, want: Vec<Hash>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _g = self.asking.lock().await;
            // Largest batches first would starve small files; keep the
            // client's own order by asking for everything at once.
            self.send(ServerMsg::WantBlobs(want.iter().map(|h| h.0).collect()));
            let mut left: Vec<Hash> = want;
            loop {
                let arrived = self.arrivals.notified();
                tokio::pin!(arrived);
                arrived.as_mut().enable();
                left.retain(|h| !self.store.has(h));
                if left.is_empty() {
                    return Ok(());
                }
                if self.closed.load(Ordering::SeqCst) || self.changed.load(Ordering::SeqCst) {
                    return Err(self.gone());
                }
                arrived.await;
            }
        })
    }
}

/// Performs the server side of the handshake of a TCP link. Returns the
/// user's name, whether they administer the server, and the channel.
pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    keys: &Keys,
) -> Result<(String, bool, Sealer, Opener)> {
    let mut magic = [0u8; 9];
    stream.read_exact(&mut magic).await?;
    if &magic[..8] != secure::MAGIC {
        let text = b"this is a rargo build server; the client speaks a different protocol version\n";
        let mut reply = vec![1u8];
        reply.extend_from_slice(&(text.len() as u16).to_le_bytes());
        reply.extend_from_slice(text);
        let _ = stream.write_all(&reply).await;
        bail!("not a rargo client (or a different protocol version)");
    }
    let mut user = vec![0u8; magic[8] as usize];
    stream.read_exact(&mut user).await?;
    let user = String::from_utf8(user).context("user name is not UTF-8")?;
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await?;
    let mut msg1 = vec![0u8; u16::from_le_bytes(len) as usize];
    stream.read_exact(&mut msg1).await?;
    let refuse = "the server does not know this user or key\n";
    let accepted = keys.lookup(&user).and_then(|(psk, admin)| secure::server_accept(&psk, &msg1).ok().map(|r| (r, admin)));
    let Some(((msg2, early, sealer, opener), admin)) = accepted else {
        let mut reply = vec![1u8];
        reply.extend_from_slice(&(refuse.len() as u16).to_le_bytes());
        reply.extend_from_slice(refuse.as_bytes());
        let _ = stream.write_all(&reply).await;
        bail!("refused `{user}`: unknown user or wrong key");
    };
    let mut reply = vec![0u8];
    reply.extend_from_slice(&(msg2.len() as u16).to_le_bytes());
    reply.extend_from_slice(&msg2);
    stream.write_all(&reply).await?;
    if !early.is_empty() {
        bail!("`{user}` sent a request with the handshake, which this protocol version does not do");
    }
    Ok((user, admin, sealer, opener))
}

/// The size of a message that counts against the bulk window.
pub fn bulk_of(m: &ServerMsg) -> usize {
    bulk_size(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file() {
        let k = Keys::parse("# team\nann 0123456789abcdef0123 admin\nbob  another-long-secret-here\n\n").unwrap();
        assert!(k.lookup("ann").unwrap().1);
        assert!(!k.lookup("bob").unwrap().1);
        assert!(k.lookup("eve").is_none());
        assert_ne!(k.lookup("ann").unwrap().0, k.lookup("bob").unwrap().0);
        assert!(Keys::parse("ann short").is_err());
        assert!(Keys::parse("ann 0123456789abcdef0123 root").is_err());
        assert!(Keys::parse("# nothing\n").is_err());
        assert!(Keys::open().lookup("anyone").unwrap().1);
    }
}
