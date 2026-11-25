use anyhow::{Context, Result};
use parking_lot::{Condvar, Mutex};
use redb::{Database, TableDefinition};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Table {
    Blobs,
    Traces,
    HostSigs,
    Meta,
    Incr,
    Chunks,
    Cold,
}

const ALL: [Table; 7] = [
    Table::Blobs,
    Table::Traces,
    Table::HostSigs,
    Table::Meta,
    Table::Incr,
    Table::Chunks,
    Table::Cold,
];

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

type Pending = HashMap<(Table, Vec<u8>), Option<Arc<[u8]>>>;

struct Shared {
    db: Database,
    pending: Mutex<Pending>,
    wake: Condvar,
    stop: Mutex<bool>,
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
            pending: Mutex::new(HashMap::new()),
            wake: Condvar::new(),
            stop: Mutex::new(false),
        });
        let s2 = shared.clone();
        let writer = std::thread::Builder::new()
            .name("db-writer".into())
            .spawn(move || writer_loop(s2))?;
        Ok(Db { shared, writer: Mutex::new(Some(writer)) })
    }

    pub fn get(&self, table: Table, key: &[u8]) -> Option<Arc<[u8]>> {
        if let Some(v) = self.shared.pending.lock().get(&(table, key.to_vec())) {
            return v.clone();
        }
        let tx = self.shared.db.begin_read().ok()?;
        let t = tx.open_table(table.def()).ok()?;
        let v = t.get(key).ok()??;
        Some(Arc::from(v.value()))
    }

    pub fn put(&self, table: Table, key: &[u8], val: &[u8]) {
        self.shared.pending.lock().insert((table, key.to_vec()), Some(Arc::from(val)));
        self.shared.wake.notify_one();
    }

    pub fn delete(&self, table: Table, key: &[u8]) {
        self.shared.pending.lock().insert((table, key.to_vec()), None);
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
        for ((t, k), v) in self.shared.pending.lock().iter() {
            if *t == table && k.starts_with(prefix) {
                out.insert(k.clone(), v.clone());
            }
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

fn flush_once(shared: &Shared) {
    let batch: Vec<((Table, Vec<u8>), Option<Arc<[u8]>>)> = {
        let p = shared.pending.lock();
        p.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    if batch.is_empty() {
        return;
    }
    let commit = || -> Result<()> {
        let tx = shared.db.begin_write()?;
        for t in ALL {
            let mut table = tx.open_table(t.def())?;
            for ((bt, k), v) in &batch {
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
    };
    if let Err(e) = commit() {
        eprintln!("rargod: db commit failed: {e:#}");
        return;
    }
    let mut p = shared.pending.lock();
    for (k, v) in batch {
        if let Some(cur) = p.get(&k) {
            let same = match (cur, &v) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if same {
                p.remove(&k);
            }
        }
    }
}

fn writer_loop(shared: Arc<Shared>) {
    loop {
        {
            let mut p = shared.pending.lock();
            if p.is_empty() {
                if *shared.stop.lock() {
                    return;
                }
                shared.wake.wait_for(&mut p, Duration::from_millis(500));
            }
        }
        if *shared.stop.lock() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
        flush_once(&shared);
    }
}
