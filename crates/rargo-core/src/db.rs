use crate::db_writer::{Batch, DbWriter, Due, IDLE_MS};
use crate::io::db::{WriterTask, WriterWake};
use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
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

type Open = Box<dyn Fn() -> Result<Database, redb::DatabaseError> + Send + Sync>;

struct Shared {
    db: RwLock<Option<Database>>,
    open: Open,
    writer: Mutex<DbWriter>,
    born: Instant,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        crate::io::now().saturating_duration_since(self.born).as_millis() as u64
    }
}

pub struct Db {
    shared: Arc<Shared>,
    writing: Option<WriterTask>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let owned = path.to_path_buf();
        Db::over(Box::new(move || Database::create(&owned))).with_context(|| format!("opening {}", path.display()))
    }

    /// A database that lives in memory and is gone when it is dropped.
    pub fn open_in_memory() -> Result<Db> {
        Db::on_backend(Memory::default())
    }

    /// Real redb using the simulated filesystem. The caller controls commits.
    pub fn open_simulated(path: &Path) -> Result<Db> {
        assert!(crate::io::sim::active());
        let owned = path.to_path_buf();
        let mut db = Db::over(Box::new(move || {
            let file = crate::io::file::OpenOptions::new().read(true).write(true).create(true).open(&owned)?;
            redb::Builder::new().create_with_backend(SimFile(file))
        }))?;
        db.writing = None;
        Ok(db)
    }

    /// A database on `disk`, opened again on the same disk after a failed commit.
    fn on_backend(disk: impl redb::StorageBackend + Clone) -> Result<Db> {
        Db::over(Box::new(move || redb::Builder::new().create_with_backend(disk.clone())))
    }

    /// A database on `disk` that never starts a background writer, so a test's writes wait for
    /// its own `flush` and never reach another test's loop.
    #[cfg(test)]
    fn on(disk: impl redb::StorageBackend + Clone) -> Result<Db> {
        let mut db = Db::on_backend(disk)?;
        db.writing = None;
        Ok(db)
    }

    fn over(open: Open) -> Result<Db> {
        let db = open()?;
        {
            let tx = db.begin_write()?;
            for t in ALL {
                tx.open_table(t.def())?;
            }
            tx.commit()?;
        }
        let shared = Arc::new(Shared {
            db: RwLock::new(Some(db)),
            open,
            writer: Mutex::new(DbWriter::new()),
            born: crate::io::now(),
        });
        Ok(Db { shared, writing: Some(WriterTask::default()) })
    }

    fn written(&self) {
        if let Some(writing) = &self.writing {
            let shared = Arc::clone(&self.shared);
            writing.written(move |wake| writer_loop(shared, wake));
        }
    }

    pub fn get(&self, table: Table, key: &[u8]) -> Option<Arc<[u8]>> {
        if let Some(v) = self.shared.writer.lock().get(table, key) {
            return v;
        }
        let db = self.shared.db.read();
        let tx = db.as_ref()?.begin_read().ok()?;
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
        if let Some(Ok(tx)) = self.shared.db.read().as_ref().map(Database::begin_read) {
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
        let _ = self.flush_checked();
    }

    pub fn flush_checked(&self) -> Result<()> {
        flush_off_the_loop(&self.shared)
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Some(writing) = &mut self.writing {
            writing.stop();
            self.flush();
        }
    }
}

fn commit(db: Option<&Database>, batch: &Batch) -> Result<()> {
    let tx = db.context("the database could not be opened again")?.begin_write()?;
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

fn flush_once(shared: &Shared) -> Result<()> {
    // Serialize flushes while allowing readers to use the database during commit.
    let mut db = shared.db.upgradable_read();
    let batch = shared.writer.lock().take();
    if batch.is_empty() {
        return Ok(());
    }
    if db.is_none() {
        db.with_upgraded(|db| reopen(db, &shared.open));
    }
    let done = commit(db.as_ref(), &batch);
    let now = shared.now_ms();
    match &done {
        Ok(()) => shared.writer.lock().flushed(&batch, now),
        Err(e) => {
            eprintln!("rargod: db commit failed: {e:#}");
            shared.writer.lock().failed(now);
            db.with_upgraded(|db| reopen(db, &shared.open));
        }
    }
    done
}

// After an I/O error redb refuses every later transaction until the database is opened again.
// The old handle goes first: a file holds one open database at a time.
fn reopen(db: &mut Option<Database>, open: &Open) {
    drop(db.take());
    match open() {
        Ok(fresh) => *db = Some(fresh),
        Err(e) => eprintln!("rargod: db reopen failed: {e:#}"),
    }
}

#[derive(Debug)]
struct SimFile(crate::io::file::File);

impl redb::StorageBackend for SimFile {
    fn len(&self) -> std::io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn read(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let mut bytes = vec![0; len];
        self.0.read_exact_at(&mut bytes, offset)?;
        Ok(bytes)
    }

    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.0.set_len(len)
    }

    fn sync_data(&self, _eventual: bool) -> std::io::Result<()> {
        self.0.sync_data()
    }

    fn write(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_all_at(bytes, offset)
    }
}

/// Memory that a reopened database finds as the failed one left it.
#[derive(Clone, Debug, Default)]
struct Memory(Arc<redb::backends::InMemoryBackend>);

impl redb::StorageBackend for Memory {
    fn len(&self) -> std::io::Result<u64> {
        self.0.len()
    }

    fn read(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.0.read(offset, len)
    }

    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.0.set_len(len)
    }

    fn sync_data(&self, eventual: bool) -> std::io::Result<()> {
        self.0.sync_data(eventual)
    }

    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        self.0.write(offset, data)
    }
}

fn flush_off_the_loop(shared: &Shared) -> Result<()> {
    if crate::io::on_fiber() {
        crate::io::cpu(|| flush_once(shared))
    } else {
        flush_once(shared)
    }
}

fn writer_loop(shared: Arc<Shared>, wake: WriterWake) {
    while wake.running() {
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
            None => {
                let _ = crate::io::cpu(|| flush_once(&shared));
            }
            Some(ms) => wake.wait(Duration::from_millis(ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_writer::DELAY_MS;
    use crate::io::{sim, sleep};

    fn committed(db: &Db, key: &[u8]) -> bool {
        let tx = db.shared.db.read().as_ref().unwrap().begin_read().unwrap();
        let table = tx.open_table(Table::Meta.def()).unwrap();
        table.get(key).unwrap().is_some()
    }

    #[test]
    fn background_flush_wakes_for_writes_and_stops_on_drop() {
        sim::test(|| {
            let db = Db::open_in_memory().unwrap();
            db.put(Table::Meta, b"first", b"value");
            sleep(Duration::from_millis(DELAY_MS + 1));
            assert!(committed(&db, b"first"));

            sleep(Duration::from_millis(1));
            db.put(Table::Meta, b"second", b"value");
            sleep(Duration::from_millis(DELAY_MS + 1));
            assert!(committed(&db, b"second"));

            let shared = Arc::downgrade(&db.shared);
            drop(db);
            sleep(Duration::from_millis(1));
            assert!(shared.upgrade().is_none());
        });
    }
}

#[cfg(test)]
mod disk_tests;
