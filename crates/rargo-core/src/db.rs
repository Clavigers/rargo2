use crate::db_writer::{Batch, DbWriter, Due, IDLE_MS};
use anyhow::{Context, Result};
use parking_lot::Mutex;
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

/// The tables a test writes to: a database held in memory.
#[cfg(test)]
pub struct MemTables(Db);

#[cfg(test)]
impl Default for MemTables {
    fn default() -> MemTables {
        MemTables(Db::open_in_memory().expect("a database in memory opens"))
    }
}

#[cfg(test)]
impl MemTables {
    /// Empties every table, which costs less than opening another database.
    pub fn clear(&self) {
        for table in ALL {
            for (key, _) in self.0.scan_prefix(table, &[]) {
                self.0.delete(table, &key);
            }
        }
        self.0.flush();
    }
}

#[cfg(test)]
impl Tables for MemTables {
    fn get(&self, table: Table, key: &[u8]) -> Option<Arc<[u8]>> {
        self.0.get(table, key)
    }

    fn put(&self, table: Table, key: &[u8], val: &[u8]) {
        self.0.put(table, key, val)
    }

    fn delete(&self, table: Table, key: &[u8]) {
        self.0.delete(table, key)
    }

    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Vec<(Vec<u8>, Arc<[u8]>)> {
        self.0.scan_prefix(table, prefix)
    }
}

struct Shared {
    db: Database,
    writer: Mutex<DbWriter>,
    flushing: Mutex<()>,
    wake: std::sync::OnceLock<crate::io::Notify>,
    stop: Mutex<bool>,
    born: Instant,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        crate::io::now().saturating_duration_since(self.born).as_millis() as u64
    }
}

pub struct Db {
    shared: Arc<Shared>,
    writing: std::sync::Once,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        Db::over(Database::create(path).with_context(|| format!("opening {}", path.display()))?)
    }

    /// A database that lives in memory and is gone when it is dropped.
    pub fn open_in_memory() -> Result<Db> {
        Db::over(redb::Builder::new().create_with_backend(redb::backends::InMemoryBackend::new())?)
    }

    fn over(db: Database) -> Result<Db> {
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
            wake: std::sync::OnceLock::new(),
            stop: Mutex::new(false),
            born: crate::io::now(),
        });
        Ok(Db { shared, writing: std::sync::Once::new() })
    }

    fn written(&self) {
        if let Some(wake) = self.shared.wake.get() {
            wake.notify_one();
            return;
        }
        if !crate::io::loop_running() {
            return;
        }
        self.writing.call_once(|| {
            let shared = self.shared.clone();
            crate::io::background(move || {
                let _ = shared.wake.set(crate::io::Notify::new());
                writer_loop(shared)
            });
        });
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
        self.written();
    }

    pub fn delete(&self, table: Table, key: &[u8]) {
        let now = self.shared.now_ms();
        self.shared.writer.lock().delete(table, key, now);
        self.written();
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
        flush_off_the_loop(&self.shared);
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        *self.shared.stop.lock() = true;
        if let Some(wake) = self.shared.wake.get() {
            wake.notify_one();
        }
        flush_off_the_loop(&self.shared);
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

fn flush_off_the_loop(shared: &Shared) {
    if crate::io::on_fiber() {
        crate::io::cpu(|| flush_once(shared));
    } else {
        flush_once(shared);
    }
}

fn writer_loop(shared: Arc<Shared>) {
    let wake = shared.wake.get().expect("the writer starts with its wake");
    loop {
        if *shared.stop.lock() {
            return;
        }
        let wait = {
            let w = shared.writer.lock();
            let now = shared.now_ms();
            match w.due(now) {
                Due::Now => None,
                Due::Idle => Some(IDLE_MS),
                Due::At(t) => Some(t.saturating_sub(now).max(1)),
            }
        };
        match wait {
            None => crate::io::cpu(|| flush_once(&shared)),
            Some(ms) => {
                let _ = crate::io::timeout(Duration::from_millis(ms), || wake.notified());
            }
        }
    }
}
