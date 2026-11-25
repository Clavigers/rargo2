use crate::db_writer::{Batch, DbWriter, Due, IDLE_MS};
use anyhow::{Context, Result};
use parking_lot::{Condvar, Mutex};
use redb::{Database, TableDefinition};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Table {
    Blobs,
    Traces,
    HostSigs,
    Meta,
    Incr,
    Chunks,
    Cold,
}

const ALL: [Table; 7] =
    [Table::Blobs, Table::Traces, Table::HostSigs, Table::Meta, Table::Incr, Table::Chunks, Table::Cold];

impl Table {
    fn def(self) -> TableDefinition<'static, &'static [u8], &'static [u8]> {
        TableDefinition::new(match self {
            Table::Blobs => "blobs",
            Table::Traces => "traces",
            Table::HostSigs => "hostsigs",
            Table::Meta => "meta",
            Table::Incr => "incr",
            Table::Chunks => "chunks",
            Table::Cold => "cold",
        })
    }
}

pub trait Tables: Send + Sync {
    fn get(&self, table: Table, key: &[u8]) -> Option<Arc<[u8]>>;
    fn put(&self, table: Table, key: &[u8], val: &[u8]);
    fn delete(&self, table: Table, key: &[u8]);
    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Vec<(Vec<u8>, Arc<[u8]>)>;
}

impl Tables for Db {
    fn get(&self, table: Table, key: &[u8]) -> Option<Arc<[u8]>> {
        Db::get(self, table, key)
    }

    fn put(&self, table: Table, key: &[u8], val: &[u8]) {
        Db::put(self, table, key, val)
    }

    fn delete(&self, table: Table, key: &[u8]) {
        Db::delete(self, table, key)
    }

    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Vec<(Vec<u8>, Arc<[u8]>)> {
        Db::scan_prefix(self, table, prefix)
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct MemTables(Mutex<std::collections::BTreeMap<(Table, Vec<u8>), Arc<[u8]>>>);

#[cfg(test)]
impl Tables for MemTables {
    fn get(&self, table: Table, key: &[u8]) -> Option<Arc<[u8]>> {
        self.0.lock().get(&(table, key.to_vec())).cloned()
    }

    fn put(&self, table: Table, key: &[u8], val: &[u8]) {
        self.0.lock().insert((table, key.to_vec()), Arc::from(val));
    }

    fn delete(&self, table: Table, key: &[u8]) {
        self.0.lock().remove(&(table, key.to_vec()));
    }

    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Vec<(Vec<u8>, Arc<[u8]>)> {
        self.0.lock().range((table, prefix.to_vec())..).take_while(|((t, k), _)| *t == table && k.starts_with(prefix)).map(|((_, k), v)| (k.clone(), v.clone())).collect()
    }
}

struct Shared {
    db: Database,
    writer: Mutex<DbWriter>,
    flushing: Mutex<()>,
    wake: Condvar,
    stop: Mutex<bool>,
    born: Instant,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.born.elapsed().as_millis() as u64
    }
}

pub struct Db {
    shared: Arc<Shared>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let db = Database::create(path).with_context(|| format!("opening {}", path.display()))?;
        {
            let tx = db.begin_write()?;
            for t in ALL {
                tx.open_table(t.def())?;
            }
            tx.commit()?;
        }
        let shared = Arc::new(Shared {
            db,
            writer: Mutex::new(DbWriter::new()),
            flushing: Mutex::new(()),
            wake: Condvar::new(),
            stop: Mutex::new(false),
            born: Instant::now(),
        });
        let s2 = shared.clone();
        let writer = std::thread::Builder::new().name("db-writer".into()).spawn(move || writer_loop(s2))?;
        Ok(Db { shared, writer: Mutex::new(Some(writer)) })
    }

    pub fn get(&self, table: Table, key: &[u8]) -> Option<Arc<[u8]>> {
        if let Some(v) = self.shared.writer.lock().get(table, key) {
            return v;
        }
        let tx = self.shared.db.begin_read().ok()?;
        let t = tx.open_table(table.def()).ok()?;
        let v = t.get(key).ok()??;
        Some(Arc::from(v.value()))
    }

    pub fn put(&self, table: Table, key: &[u8], val: &[u8]) {
        let now = self.shared.now_ms();
        self.shared.writer.lock().put(table, key, val, now);
        self.shared.wake.notify_one();
    }

    pub fn delete(&self, table: Table, key: &[u8]) {
        let now = self.shared.now_ms();
        self.shared.writer.lock().delete(table, key, now);
        self.shared.wake.notify_one();
    }

    pub fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Vec<(Vec<u8>, Arc<[u8]>)> {
        let mut out: std::collections::BTreeMap<Vec<u8>, Option<Arc<[u8]>>> = Default::default();
        if let Ok(tx) = self.shared.db.begin_read() {
            if let Ok(t) = tx.open_table(table.def()) {
                if let Ok(range) = t.range::<&[u8]>(prefix..) {
                    for item in range {
                        let Ok((k, v)) = item else { break };
                        if !k.value().starts_with(prefix) {
                            break;
                        }
                        out.insert(k.value().to_vec(), Some(Arc::from(v.value())));
                    }
                }
            }
        }
        for (k, v) in self.shared.writer.lock().pending_with_prefix(table, prefix) {
            out.insert(k, v);
        }
        out.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()
    }

    pub fn for_each(&self, table: Table, mut f: impl FnMut(&[u8], &[u8])) {
        for (k, v) in self.scan_prefix(table, &[]) {
            f(&k, &v);
        }
    }

    pub fn flush(&self) {
        flush_once(&self.shared);
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        *self.shared.stop.lock() = true;
        self.shared.wake.notify_all();
        if let Some(h) = self.writer.lock().take() {
            let _ = h.join();
        }
        flush_once(&self.shared);
    }
}

fn commit(db: &Database, batch: &Batch) -> Result<()> {
    let tx = db.begin_write()?;
    for t in ALL {
        let mut table = tx.open_table(t.def())?;
        for (bt, k, v) in &batch.writes {
            if *bt != t {
                continue;
            }
            match v {
                Some(v) => {
                    table.insert(k.as_slice(), &v[..])?;
                }
                None => {
                    table.remove(k.as_slice())?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

fn flush_once(shared: &Shared) {
    let _one = shared.flushing.lock();
    let batch = shared.writer.lock().take();
    if batch.is_empty() {
        return;
    }
    let done = commit(&shared.db, &batch);
    let now = shared.now_ms();
    match done {
        Ok(()) => shared.writer.lock().flushed(&batch, now),
        Err(e) => {
            eprintln!("rargod: db commit failed: {e:#}");
            shared.writer.lock().failed(now);
        }
    }
}

fn writer_loop(shared: Arc<Shared>) {
    loop {
        {
            let mut w = shared.writer.lock();
            let now = shared.now_ms();
            match w.due(now) {
                Due::Now => {}
                Due::Idle => {
                    if *shared.stop.lock() {
                        return;
                    }
                    shared.wake.wait_for(&mut w, Duration::from_millis(IDLE_MS));
                    continue;
                }
                Due::At(t) => {
                    if *shared.stop.lock() {
                        return;
                    }
                    shared.wake.wait_for(&mut w, Duration::from_millis(t.saturating_sub(now).max(1)));
                    continue;
                }
            }
        }
        if *shared.stop.lock() {
            return;
        }
        flush_once(&shared);
    }
}
