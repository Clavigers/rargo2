use crate::db_writer::Batch;
#[cfg(doc)]
use crate::db_writer::DbWriter;
use crate::io::db::{Disk, Open, Owner, Reopenable};
use anyhow::{Context, Result};
use redb::{Database, TableDefinition};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

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
    db: Reopenable,
    born: Instant,
}

impl Disk for Shared {
    fn now_ms(&self) -> u64 {
        crate::io::now().saturating_duration_since(self.born).as_millis() as u64
    }

    fn commit(&self, batch: &Batch) -> Result<()> {
        self.db.commit(|db| commit(db, batch))
    }
}

/// The tables on disk, and the writes not yet committed to them. The
/// pending writes belong to a [`DbWriter`] that one owner steps: a fiber on
/// the I/O loop while one runs, which commits them in the background.
/// A regular database without a loop commits on flush or drop. A database
/// opened over the simulated filesystem commits only on explicit flush.
pub struct Db {
    shared: Arc<Shared>,
    writer: Owner,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let owned = path.to_path_buf();
        Db::over(Box::new(move || Database::create(&owned)), true)
            .with_context(|| format!("opening {}", path.display()))
    }

    /// A database that lives in memory and is gone when it is dropped.
    pub fn open_in_memory() -> Result<Db> {
        Db::on_backend(Memory::default(), true)
    }

    /// Real redb using the simulated filesystem. The caller controls commits.
    pub fn open_simulated(path: &Path) -> Result<Db> {
        assert!(crate::io::sim::active());
        let owned = path.to_path_buf();
        Db::over(
            Box::new(move || {
                let file = crate::io::file::OpenOptions::new().read(true).write(true).create(true).open(&owned)?;
                redb::Builder::new().create_with_backend(SimFile(file))
            }),
            false,
        )
    }

    /// A database on `disk`, opened again on the same disk after a failed commit.
    fn on_backend(disk: impl redb::StorageBackend + Clone, background: bool) -> Result<Db> {
        Db::over(Box::new(move || redb::Builder::new().create_with_backend(disk.clone())), background)
    }

    /// A database on `disk` that never starts a background writer, so a test's writes wait for
    /// its own `flush` and never reach another test's loop.
    #[cfg(test)]
    fn on(disk: impl redb::StorageBackend + Clone) -> Result<Db> {
        Db::on_backend(disk, false)
    }

    fn over(open: Open, background: bool) -> Result<Db> {
        let db = open()?;
        {
            let tx = db.begin_write()?;
            for t in ALL {
                tx.open_table(t.def())?;
            }
            tx.commit()?;
        }
        let shared = Arc::new(Shared { db: Reopenable::new(db, open), born: crate::io::now() });
        let writer = Owner::new(shared.clone(), background);
        Ok(Db { shared, writer })
    }

    pub fn get(&self, table: Table, key: &[u8]) -> Option<Arc<[u8]>> {
        let key_owned = key.to_vec();
        if let Some(v) = self.writer.read(move |w| w.get(table, &key_owned)) {
            return v;
        }
        self.shared.db.read(|db| {
            let tx = db?.begin_read().ok()?;
            let t = tx.open_table(table.def()).ok()?;
            let v = t.get(key).ok()??;
            Some(Arc::from(v.value()))
        })
    }

    pub fn put(&self, table: Table, key: &[u8], val: &[u8]) {
        let now = self.shared.now_ms();
        let (key, val) = (key.to_vec(), val.to_vec());
        self.writer.write(move |w| w.put(table, &key, &val, now));
    }

    pub fn delete(&self, table: Table, key: &[u8]) {
        let now = self.shared.now_ms();
        let key = key.to_vec();
        self.writer.write(move |w| w.delete(table, &key, now));
    }

    pub fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Vec<(Vec<u8>, Arc<[u8]>)> {
        let mut out: std::collections::BTreeMap<Vec<u8>, Option<Arc<[u8]>>> = Default::default();
        self.shared.db.read(|db| {
            if let Some(Ok(tx)) = db.map(Database::begin_read) {
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
        });
        let prefix = prefix.to_vec();
        for (k, v) in self.writer.read(move |w| w.pending_with_prefix(table, &prefix)) {
            out.insert(k, v);
        }
        out.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()
    }

    pub fn for_each(&self, table: Table, mut f: impl FnMut(&[u8], &[u8])) {
        for (k, v) in self.scan_prefix(table, &[]) {
            f(&k, &v);
        }
    }

    /// Commits every write made before this call, and returns once that
    /// commit has ended.
    pub fn flush(&self) {
        let _ = self.flush_checked();
    }

    pub fn flush_checked(&self) -> Result<()> {
        self.writer.flush()
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        self.writer.stop();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_writer::DELAY_MS;
    use crate::io::{sim, sleep};
    use std::time::Duration;

    fn committed(db: &Db, key: &[u8]) -> bool {
        on_disk(&db.shared.db, key)
    }

    fn on_disk(db: &Reopenable, key: &[u8]) -> bool {
        db.read(|db| {
            let tx = db.unwrap().begin_read().unwrap();
            let table = tx.open_table(Table::Meta.def()).unwrap();
            table.get(key).unwrap().is_some()
        })
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

    fn scan(db: &Db, prefix: &[u8]) -> Vec<(String, String)> {
        db.scan_prefix(Table::Meta, prefix)
            .into_iter()
            .map(|(k, v)| (String::from_utf8_lossy(&k).into_owned(), String::from_utf8_lossy(&v).into_owned()))
            .collect()
    }

    #[test]
    fn the_loop_owner_answers_reads_with_the_pending_writes() {
        sim::test(|| {
            let db = Db::open_in_memory().unwrap();
            db.put(Table::Meta, b"a1", b"old");
            db.put(Table::Meta, b"a2", b"two");
            db.flush();
            assert!(committed(&db, b"a1") && committed(&db, b"a2"), "an explicit flush commits at once");

            db.put(Table::Meta, b"a1", b"new");
            db.delete(Table::Meta, b"a2");
            db.put(Table::Meta, b"a3", b"three");
            db.put(Table::Meta, b"b1", b"other");
            assert_eq!(db.get(Table::Meta, b"a1").as_deref(), Some(&b"new"[..]));
            assert_eq!(db.get(Table::Meta, b"a2"), None, "a pending delete hides the committed value");
            assert_eq!(scan(&db, b"a"), [("a1".into(), "new".into()), ("a3".into(), "three".into())]);
            assert!(committed(&db, b"a2"), "nothing is committed before the delay");

            sleep(Duration::from_millis(DELAY_MS + 1));
            assert!(!committed(&db, b"a2") && committed(&db, b"a3"), "the owner commits after the delay");
            assert_eq!(scan(&db, b"a"), [("a1".into(), "new".into()), ("a3".into(), "three".into())]);
        });
    }

    #[test]
    fn without_a_loop_writes_wait_for_a_flush() {
        let db = Db::open_in_memory().unwrap();
        db.put(Table::Meta, b"k", b"v");
        db.delete(Table::Meta, b"gone");
        assert_eq!(db.get(Table::Meta, b"k").as_deref(), Some(&b"v"[..]));
        assert!(!committed(&db, b"k"));
        db.flush();
        assert!(committed(&db, b"k"));
        assert_eq!(scan(&db, b""), [("k".into(), "v".into())]);
    }

    #[test]
    fn a_loop_that_ends_hands_the_writes_back() {
        let db = Db::open_in_memory().unwrap();
        db.put(Table::Meta, b"before", b"1");
        let db = sim::test(move || {
            db.put(Table::Meta, b"during", b"2");
            assert_eq!(db.get(Table::Meta, b"before").as_deref(), Some(&b"1"[..]));
            db
        });
        assert!(!committed(&db, b"during"), "the loop ended before the delay");
        assert_eq!(scan(&db, b""), [("before".into(), "1".into()), ("during".into(), "2".into())]);
        db.put(Table::Meta, b"after", b"3");
        db.flush();
        assert!(committed(&db, b"before") && committed(&db, b"during") && committed(&db, b"after"));
    }

    #[test]
    fn dropping_commits_what_is_pending() {
        sim::test(|| {
            let db = Db::open_in_memory().unwrap();
            db.put(Table::Meta, b"k", b"v");
            let shared = Arc::clone(&db.shared);
            drop(db);
            assert!(on_disk(&shared.db, b"k"), "dropping the database waits for its last commit");
        });
        let db = Db::open_in_memory().unwrap();
        db.put(Table::Meta, b"k", b"v");
        let shared = Arc::clone(&db.shared);
        drop(db);
        assert!(on_disk(&shared.db, b"k"), "without a loop too");
    }

    #[test]
    fn a_database_dropped_as_its_loop_ends_commits_what_is_pending() {
        let shared = sim::test(|| {
            let db = Db::open_in_memory().unwrap();
            let shared = Arc::clone(&db.shared);
            // The loop ends with this fiber still holding the database. It
            // starts before the writer's owner, so the loop drops it first,
            // outside any fiber, where nothing can wait for the owner.
            crate::io::background(move || {
                db.put(Table::Meta, b"k", b"v");
                sleep(Duration::from_secs(3600));
            });
            sleep(Duration::from_millis(1));
            shared
        });
        assert!(on_disk(&shared.db, b"k"));
    }
}

#[cfg(test)]
mod disk_tests;
