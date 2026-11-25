use crate::db::{Db, Table};
use crate::hash::{Hash, KeyHasher};
use crate::sandbox::NetMode;
use crate::store::Store;
use crate::view::{Content, Obs, VPath, View, obs_holds};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Plan,
    Rustc,
    Rustdoc,
    BuildScript,
    Test,
    Doctest,
    Other,
    Nextest,
}

impl ActionKind {
    pub const ALL: [ActionKind; 8] = [
        ActionKind::Plan,
        ActionKind::Rustc,
        ActionKind::Rustdoc,
        ActionKind::BuildScript,
        ActionKind::Test,
        ActionKind::Doctest,
        ActionKind::Other,
        ActionKind::Nextest,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ActionKind::Plan => "planning",
            ActionKind::Rustc => "rustc",
            ActionKind::Rustdoc => "rustdoc",
            ActionKind::BuildScript => "build scripts",
            ActionKind::Test => "tests",
            ActionKind::Doctest => "doctests",
            ActionKind::Other => "other",
            ActionKind::Nextest => "nextest",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ActionSpec {
    pub kind: ActionKind,
    pub argv: Vec<Vec<u8>>,
    pub env: BTreeMap<Vec<u8>, Vec<u8>>,
    pub cwd: Vec<u8>,
    pub net: NetMode,
    pub mkdirs: Vec<Vec<u8>>,
    pub root_id: Hash,
    pub discard: Vec<Vec<u8>>,
    pub label: String,
}

impl ActionSpec {
    pub fn weak_key(&self) -> Hash {
        let mut h = KeyHasher::new("action-v1");
        h.u64(self.argv.len() as u64);
        for a in &self.argv {
            h.field(a);
        }
        h.u64(self.env.len() as u64);
        for (k, v) in &self.env {
            h.field(k).field(v);
        }
        h.field(&self.cwd);
        h.u64(match self.net {
            NetMode::Loopback => 0,
            NetMode::Host => 1,
        });
        h.u64(self.mkdirs.len() as u64);
        for d in &self.mkdirs {
            h.field(d);
        }
        h.field(&self.root_id.0);
        h.u64(self.discard.len() as u64);
        for d in &self.discard {
            h.field(d);
        }
        h.finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MEntry {
    File { hash: Hash, exec: bool, size: u64 },
    Symlink(Vec<u8>),
    Dir,
    Whiteout,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: Vec<(Vec<u8>, MEntry)>,
}

impl Manifest {
    pub fn get(&self, path: &[u8]) -> Option<&MEntry> {
        self.entries.binary_search_by(|(p, _)| p.as_slice().cmp(path)).ok().map(|i| &self.entries[i].1)
    }

    pub fn total_size(&self) -> u64 {
        self.entries
            .iter()
            .map(|(_, e)| match e {
                MEntry::File { size, .. } => *size,
                _ => 0,
            })
            .sum()
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub wall_us: u64,
    pub cpu_us: u64,
    pub max_rss_kb: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionResult {
    pub status: i32,
    pub stdout: Hash,
    pub stderr: Hash,
    pub outputs: Arc<Manifest>,
    pub stats: Stats,
}

impl ActionResult {
    pub fn code(&self) -> Option<i32> {
        libc::WIFEXITED(self.status).then(|| libc::WEXITSTATUS(self.status))
    }
    pub fn signal(&self) -> Option<i32> {
        libc::WIFSIGNALED(self.status).then(|| libc::WTERMSIG(self.status))
    }
    pub fn success(&self) -> bool {
        self.code() == Some(0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub obs: Vec<(VPath, Obs)>,
    pub result: ActionResult,
    pub created: u64,
}

impl Trace {
    pub fn holds(&self, view: &dyn View, content: &Content) -> bool {
        self.obs.iter().all(|(p, o)| obs_holds(view, content, p, o))
    }

    pub fn first_mismatch(&self, view: &dyn View, content: &Content) -> Option<&(VPath, Obs)> {
        self.obs.iter().find(|(p, o)| !obs_holds(view, content, p, o))
    }

    pub fn complete(&self, store: &Store) -> bool {
        store.has(&self.result.stdout)
            && store.has(&self.result.stderr)
            && self.result.outputs.entries.iter().all(|(_, e)| match e {
                MEntry::File { hash, .. } => store.has(hash),
                _ => true,
            })
    }

    pub fn touch(&self, store: &Store) {
        store.touch(&self.result.stdout);
        store.touch(&self.result.stderr);
        for (_, e) in &self.result.outputs.entries {
            if let MEntry::File { hash, .. } = e {
                store.touch(hash);
            }
        }
    }
}

const MAX_TRACES_PER_KEY: usize = 24;

#[derive(Default)]
pub struct CacheStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub stored: AtomicU64,
}

pub struct TraceCache {
    db: Arc<Db>,
    mem: DashMap<Hash, Arc<Vec<(u64, Arc<Trace>)>>>,
    next_id: AtomicU64,
    env_names: parking_lot::Mutex<Option<std::collections::BTreeSet<String>>>,
    pub stats: CacheStats,
}

fn trace_key(weak: &Hash, id: u64) -> Vec<u8> {
    let mut k = weak.0.to_vec();
    k.extend_from_slice(&id.to_be_bytes());
    k
}

impl TraceCache {
    pub fn new(db: Arc<Db>, seed: u64) -> TraceCache {
        TraceCache { db, mem: DashMap::new(), env_names: parking_lot::Mutex::new(None), next_id: AtomicU64::new(seed), stats: CacheStats::default() }
    }

    fn load(&self, weak: &Hash) -> Arc<Vec<(u64, Arc<Trace>)>> {
        if let Some(v) = self.mem.get(weak) {
            return v.clone();
        }
        let mut list = Vec::new();
        for (k, v) in self.db.scan_prefix(Table::Traces, &weak.0) {
            if k.len() != 40 {
                continue;
            }
            let id = u64::from_be_bytes(k[32..40].try_into().unwrap());
            let Ok(raw) = zstd::decode_all(&v[..]) else { continue };
            let Ok(t) = bincode::deserialize::<Trace>(&raw) else { continue };
            list.push((id, Arc::new(t)));
        }
        list.sort_by(|a, b| b.0.cmp(&a.0));
        let list = Arc::new(list);
        self.mem.insert(*weak, list.clone());
        list
    }

    pub fn traces(&self, weak: &Hash) -> Arc<Vec<(u64, Arc<Trace>)>> {
        self.load(weak)
    }

    pub fn lookup(&self, weak: &Hash, view: &dyn View, content: &Content) -> Option<Arc<Trace>> {
        let list = self.load(weak);
        for (id, t) in list.iter() {
            if !t.holds(view, content) {
                continue;
            }
            if !t.complete(&content.store) {
                self.remove(weak, *id);
                continue;
            }
            t.touch(&content.store);
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            return Some(t.clone());
        }
        self.stats.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub fn insert(&self, weak: &Hash, trace: Trace) -> Arc<Trace> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let raw = bincode::serialize(&trace).expect("trace serialises");
        let packed = zstd::encode_all(&raw[..], 3).expect("zstd");
        self.db.put(Table::Traces, &trace_key(weak, id), &packed);
        let trace = Arc::new(trace);
        let mut list: Vec<(u64, Arc<Trace>)> = self.load(weak).as_ref().clone();
        list.insert(0, (id, trace.clone()));
        while list.len() > MAX_TRACES_PER_KEY {
            let (old, _) = list.pop().unwrap();
            self.db.delete(Table::Traces, &trace_key(weak, old));
        }
        self.mem.insert(*weak, Arc::new(list));
        self.stats.stored.fetch_add(1, Ordering::Relaxed);
        trace
    }

    pub fn remove(&self, weak: &Hash, id: u64) {
        self.db.delete(Table::Traces, &trace_key(weak, id));
        let mut list: Vec<(u64, Arc<Trace>)> = self.load(weak).as_ref().clone();
        list.retain(|(i, _)| *i != id);
        self.mem.insert(*weak, Arc::new(list));
    }

    pub fn needs_env(&self, weak: &Hash) -> Vec<String> {
        let mut k = b"env:".to_vec();
        k.extend_from_slice(&weak.0);
        self.db
            .get(Table::Meta, &k)
            .and_then(|v| bincode::deserialize::<Vec<String>>(&v).ok())
            .unwrap_or_default()
    }

    pub fn add_needs_env(&self, weak: &Hash, vars: &[String]) -> Vec<String> {
        let mut cur = self.needs_env(weak);
        let mut added = Vec::new();
        for v in vars {
            if !cur.contains(v) {
                cur.push(v.clone());
                added.push(v.clone());
            }
        }
        if !added.is_empty() {
            cur.sort();
            let mut k = b"env:".to_vec();
            k.extend_from_slice(&weak.0);
            self.db.put(Table::Meta, &k, &bincode::serialize(&cur).unwrap());
            if let Some(names) = &mut *self.env_names.lock() {
                names.extend(added.iter().cloned());
            }
        }
        added
    }

    pub fn every_env_name(&self) -> Vec<String> {
        let mut names = self.env_names.lock();
        names
            .get_or_insert_with(|| self.all_needs_env().into_iter().flat_map(|(_, v)| v).collect())
            .iter()
            .cloned()
            .collect()
    }

    pub fn all(&self) -> Vec<(Hash, Trace)> {
        let mut out = Vec::new();
        self.db.for_each(Table::Traces, |k, v| {
            if k.len() != 40 {
                return;
            }
            let Ok(raw) = zstd::decode_all(v) else { return };
            let Ok(t) = bincode::deserialize::<Trace>(&raw) else { return };
            out.push((Hash(k[..32].try_into().unwrap()), t));
        });
        out
    }

    pub fn all_needs_env(&self) -> Vec<(Hash, Vec<String>)> {
        self.db
            .scan_prefix(Table::Meta, b"env:")
            .into_iter()
            .filter_map(|(k, v)| {
                let key = Hash(k.get(4..36)?.try_into().ok()?);
                Some((key, bincode::deserialize::<Vec<String>>(&v).ok()?))
            })
            .collect()
    }

    pub fn mark(&self, live: &mut std::collections::HashSet<Hash>) {
        self.db.for_each(Table::Traces, |_, v| {
            let Ok(raw) = zstd::decode_all(v) else { return };
            let Ok(t) = bincode::deserialize::<Trace>(&raw) else { return };
            live.insert(t.result.stdout);
            live.insert(t.result.stderr);
            for (_, e) in &t.result.outputs.entries {
                if let MEntry::File { hash, .. } = e {
                    live.insert(*hash);
                }
            }
        });
    }
}
