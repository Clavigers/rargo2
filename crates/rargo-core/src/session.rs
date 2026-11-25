use crate::hash::Hash;
use crate::session_state::{
    Accepted, Answer, ClientSet, Effect, Handshake, Next, Outgoing, Progress, Question, SessionState, Shake, Shook,
    bulk_size,
};
use crate::snapshot::{Uploader, futures::BoxFuture};
use crate::store::Store;
use anyhow::{Result, anyhow, bail};
use parking_lot::Mutex;
use rargo_proto::secure::{self, Opener, Sealer};
use rargo_proto::{ClientMsg, Delivery, Digest, ServerMsg, Write};
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
    parts: Mutex<BTreeMap<Digest, (PathBuf, std::fs::File)>>,
    totals: Arc<crate::build::DaemonStats>,
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
        self.state.lock().knows(&mut self.has.set.lock(), digests);
    }

    pub fn set_lazy(&self) {
        self.state.lock().set_lazy();
    }

    pub fn is_lazy(&self) -> bool {
        self.state.lock().is_lazy()
    }

    pub fn deliver_lazy(&self, files: Vec<Delivery>, writes: Vec<Write>, targets: Vec<String>) {
        let out = self.state.lock().deliver(&mut self.has.set.lock(), files, writes, targets);
        let Some(mut out) = out else { return };
        loop {
            match out.step() {
                Next::Read { hash, .. } => out.read(self.store.read(&Hash(hash)).ok()),
                Next::Send(m) => {
                    self.count_out(bulk_size(&m));
                    self.send(m);
                }
                Next::Done => return,
                n => unreachable!("a lazy delivery neither waits for the window nor fails: {n:?}"),
            }
        }
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
            if let Some((path, _)) = files.remove(&h) {
                let _ = std::fs::remove_file(&path);
            }
        }
        assert!(files.is_empty(), "every partial upload is one the session knows of");
    }

    pub fn incoming(&self, msg: ClientMsg) -> Result<()> {
        let step = self.state.lock().client(msg);
        self.totals.bytes_in.fetch_add(step.bytes_in, Ordering::Relaxed);
        tokio::task::block_in_place(|| -> Result<()> {
            for e in step.effects {
                self.carry_out(e)?;
            }
            Ok(())
        })?;
        match step.refused {
            Some(why) => bail!("{why}"),
            None => Ok(()),
        }
    }

    fn carry_out(&self, e: Effect) -> Result<()> {
        match e {
            Effect::Store { hash, bytes } => {
                self.store.put_bytes(&bytes)?;
                assert!(self.store.has(&Hash(hash)), "a stored blob is in the store");
                self.totals.files_in.fetch_add(1, Ordering::Relaxed);
            }
            Effect::PartOpen(hash) => {
                let path = self.store.tmp_path();
                match std::fs::File::create(&path) {
                    Ok(f) => {
                        self.parts.lock().insert(hash, (path, f));
                    }
                    Err(e) => {
                        self.state.lock().part_failed(&hash);
                        return Err(e.into());
                    }
                }
            }
            Effect::PartWrite(hash, data) => {
                let mut parts = self.parts.lock();
                let (_, file) = parts.get_mut(&hash).expect("the core opened this upload");
                file.write_all(&data)?;
            }
            Effect::PartInstall { hash, size } => {
                let (path, file) = self.parts.lock().remove(&hash).expect("the core opened this upload");
                drop(file);
                self.store.install_hashed(&path, Hash(hash), size)?;
                assert!(self.store.has(&Hash(hash)), "an installed blob is in the store");
                self.totals.files_in.fetch_add(1, Ordering::Relaxed);
            }
            Effect::PartDrop(hash) => {
                if let Some((path, _)) = self.parts.lock().remove(&hash) {
                    let _ = std::fs::remove_file(&path);
                }
            }
            Effect::Answer(Answer::Trees(t)) => {
                if let Some(tx) = self.trees_reply.lock().take() {
                    let _ = tx.send(t);
                }
            }
            Effect::Answer(Answer::Env(e)) => {
                if let Some(tx) = self.env_reply.lock().take() {
                    let _ = tx.send(e);
                }
            }
            Effect::Answer(Answer::Report(id)) => {
                if let Some(tx) = self.report_reply.lock().take() {
                    let _ = tx.send(id);
                }
            }
            Effect::Wake => self.arrivals.notify_waiters(),
        }
        Ok(())
    }

    async fn ask<T>(&self, q: Question, slot: &Mutex<Option<oneshot::Sender<T>>>) -> Result<T> {
        let _g = self.asking.lock().await;
        let (tx, rx) = oneshot::channel();
        *slot.lock() = Some(tx);
        let asked = self.state.lock().ask(q);
        match asked {
            Ok(m) => self.send(m),
            Err(_) => {
                slot.lock().take();
                return Err(self.gone());
            }
        }
        rx.await.map_err(|_| self.gone())
    }

    pub async fn want_env(&self, names: Vec<String>) -> Result<Vec<(String, Option<String>)>> {
        self.ask(Question::Env(names), &self.env_reply).await
    }

    pub async fn report(&self, report: rargo_proto::FutureReport) -> Result<u32> {
        self.ask(Question::Report(Box::new(report)), &self.report_reply).await
    }

    pub async fn deliver(&self, files: Vec<Delivery>, writes: Vec<Write>, targets: Vec<String>) -> Result<()> {
        let out = self.state.lock().deliver(&mut self.has.set.lock(), files, writes, targets);
        match out {
            Some(out) => self.send_out(out).await,
            None => Ok(()),
        }
    }

    pub fn count_out(&self, bytes: usize) {
        self.totals.bytes_out.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub async fn fetch(&self, id: u64, hashes: Vec<Digest>) -> Result<()> {
        let found: Vec<(Digest, Option<u64>)> = hashes.into_iter().map(|h| (h, self.store.size_of(&Hash(h)))).collect();
        let out = self.state.lock().fetch(&mut self.has.set.lock(), id, found);
        self.send_out(out).await
    }

    async fn send_out(&self, mut out: Outgoing) -> Result<()> {
        let mut open: Option<OpenBlob> = None;
        let failed = loop {
            match out.step() {
                Next::Read { hash, at, len, whole } => {
                    let store = self.store.clone();
                    let file = open.take().filter(|(h, _)| *h == hash);
                    let (o, read) = tokio::task::spawn_blocking(move || {
                        let read = read_blob(&store, hash, at, len, whole, file);
                        let file = match read {
                            Ok((bytes, file)) => {
                                out.read(Some(bytes));
                                Ok(file)
                            }
                            Err(e) => Err(e),
                        };
                        (out, file)
                    })
                    .await?;
                    out = o;
                    match read {
                        Ok(file) => open = file,
                        Err(e) => break Err(anyhow::Error::from(e)),
                    }
                }
                Next::Send(m) => {
                    self.count_out(bulk_size(&m));
                    self.send(m);
                }
                Next::Bulk(m) => {
                    if let Err(e) = self.send_bulk(m).await {
                        break Err(e);
                    }
                }
                Next::Unreadable(h) => break Err(anyhow!("blob {} is not readable", Hash(h))),
                Next::Gone => break Err(self.gone()),
                Next::Done => break Ok(()),
            }
        };
        if failed.is_err()
            && let Some(m) = out.abort()
        {
            self.send(m);
        }
        failed
    }
}

type OpenBlob = (Digest, std::fs::File);

fn read_blob(
    store: &Store,
    hash: Digest,
    at: u64,
    len: usize,
    whole: bool,
    file: Option<OpenBlob>,
) -> std::io::Result<(Vec<u8>, Option<OpenBlob>)> {
    use std::os::unix::fs::FileExt;
    if whole {
        return Ok((store.read(&Hash(hash))?, None));
    }
    let f = match file {
        Some((_, f)) => f,
        None => store.open_blob(&Hash(hash))?,
    };
    let mut data = vec![0u8; len];
    let mut n = 0;
    while n < len {
        let got = f.read_at(&mut data[n..], at + n as u64)?;
        if got == 0 {
            break;
        }
        n += got;
    }
    data.truncate(n);
    Ok((data, Some((hash, f))))
}

impl Uploader for Session {
    fn trees<'a>(&'a self, want: Vec<Hash>) -> BoxFuture<'a, Result<Vec<Vec<u8>>>> {
        Box::pin(async move { self.ask(Question::Trees(want.iter().map(|h| h.0).collect()), &self.trees_reply).await })
    }

    fn blobs<'a>(&'a self, want: Vec<Hash>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _g = self.asking.lock().await;
            let digests: Vec<Digest> = want.iter().map(|h| h.0).collect();
            let ask = self.state.lock().want(&digests, &|d| self.store.has(&Hash(*d)));
            let wanting = Wanting(self);
            self.send(ask);
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
    let (mut hs, mut next) = Handshake::new(secure::MAGIC);
    let mut keys_for = None;
    loop {
        next = match next {
            Shake::Read(n) => {
                let mut bytes = vec![0; n];
                stream.read_exact(&mut bytes).await?;
                hs.received(&bytes)
            }
            Shake::Check { user, msg1 } => {
                let accepted = keys
                    .lookup(&user)
                    .and_then(|(psk, admin)| secure::server_accept(&psk, &msg1).ok().map(|r| (r, admin)));
                hs.checked(accepted.map(|((msg2, early, sealer, opener), admin)| {
                    keys_for = Some((sealer, opener));
                    Accepted { msg2, early: !early.is_empty(), admin }
                }))
            }
            Shake::Reply { write, end } => {
                let wrote = if write.is_empty() { Ok(()) } else { stream.write_all(&write).await };
                return match end {
                    Shook::Accepted { user, admin } => {
                        wrote?;
                        let (sealer, opener) = keys_for.expect("an accepted link has its keys");
                        Ok((user, admin, sealer, opener))
                    }
                    Shook::Refused(why) => Err(anyhow!(why)),
                };
            }
        };
    }
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
