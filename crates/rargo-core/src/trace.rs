#[cfg(test)]
use rargo_machine::StateMachine;
use crate::db::{Table, Tables};
use crate::hash::{Hash, KeyHasher};
use crate::sandbox::NetMode;
use crate::store::Store;
use crate::view::{Content, FileSrc, Obs, VPath, View, obs_holds};
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

fn observed_hash(content: &Content, src: &FileSrc) -> Option<Hash> {
    content.known_hash(src).or_else(|| content.hash(src).ok())
}

pub fn code_verdict(view: &dyn View, content: &Content, path: &[u8], executed: &Hash, rest: &Hash) -> crate::fn_code::Verdict {
    match view.stat(path) {
        crate::view::Looked::File(f) if f.exec() => match observed_hash(content, &f.src) {
            Some(binary) => content.code_verdict(&binary, executed, rest),
            None => crate::fn_code::Verdict::NoTable,
        },
        _ => crate::fn_code::Verdict::NotAFile,
    }
}

fn holds_one(view: &dyn View, content: &Content, path: &[u8], obs: &Obs) -> bool {
    match obs {
        Obs::Code { executed, rest } => code_verdict(view, content, path, executed, rest) == crate::fn_code::Verdict::Holds,
        _ => obs_holds(view, &|src| observed_hash(content, src), path, obs),
    }
}

impl Trace {
    pub fn holds(&self, view: &dyn View, content: &Content) -> bool {
        self.obs.iter().all(|(p, o)| holds_one(view, content, p, o))
    }

    pub fn first_mismatch(&self, view: &dyn View, content: &Content) -> Option<&(VPath, Obs)> {
        self.obs.iter().find(|(p, o)| !holds_one(view, content, p, o))
    }

    fn code_blobs(&self) -> impl Iterator<Item = &Hash> {
        self.obs.iter().filter_map(|(_, o)| match o {
            Obs::Code { executed, .. } => Some(executed),
            _ => None,
        })
    }

    pub fn by_code(&self) -> bool {
        self.code_blobs().next().is_some()
    }

    pub fn complete(&self, store: &Store) -> bool {
        store.has(&self.result.stdout)
            && store.has(&self.result.stderr)
            && self.code_blobs().all(|h| store.has(h))
            && self.result.outputs.entries.iter().all(|(_, e)| match e {
                MEntry::File { hash, .. } => store.has(hash),
                _ => true,
            })
    }

    pub fn touch(&self, store: &Store) {
        store.touch(&self.result.stdout);
        store.touch(&self.result.stderr);
        for h in self.code_blobs() {
            store.touch(h);
        }
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
    db: Arc<dyn Tables>,
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
    pub fn new(db: Arc<dyn Tables>, seed: u64) -> TraceCache {
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
        let mut list: Vec<(u64, Arc<Trace>)> = self.load(weak).as_ref().clone();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let raw = bincode::serialize(&trace).expect("trace serialises");
        let packed = zstd::encode_all(&raw[..], 3).expect("zstd");
        self.db.put(Table::Traces, &trace_key(weak, id), &packed);
        let trace = Arc::new(trace);
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
        for (k, v) in self.db.scan_prefix(Table::Traces, &[]) {
            if k.len() != 40 {
                continue;
            }
            let Ok(raw) = zstd::decode_all(&v[..]) else { continue };
            let Ok(t) = bincode::deserialize::<Trace>(&raw) else { continue };
            out.push((Hash(k[..32].try_into().unwrap()), t));
        }
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

    pub fn mark(&self, live: &mut std::collections::BTreeSet<Hash>) {
        for (_, v) in self.db.scan_prefix(Table::Traces, &[]) {
            let Ok(raw) = zstd::decode_all(&v[..]) else { continue };
            let Ok(t) = bincode::deserialize::<Trace>(&raw) else { continue };
            live.insert(t.result.stdout);
            live.insert(t.result.stderr);
            for (_, e) in &t.result.outputs.entries {
                if let MEntry::File { hash, .. } = e {
                    live.insert(*hash);
                }
            }
        }
    }
}

#[cfg(test)]
impl StateMachine for TraceCache {
    fn invariants(&self) {
        let next = self.next_id.load(Ordering::Relaxed);
        let stored = |weak: &Hash| -> Vec<u64> {
            let mut ids: Vec<u64> = self.db.scan_prefix(Table::Traces, &weak.0).into_iter().filter(|(k, _)| k.len() == 40).map(|(k, _)| u64::from_be_bytes(k[32..40].try_into().unwrap())).collect();
            ids.sort_by(|a, b| b.cmp(a));
            ids
        };
        let names: Option<std::collections::BTreeSet<String>> = self.env_names.lock().clone();
        for e in self.mem.iter() {
            let ids: Vec<u64> = e.value().iter().map(|(id, _)| *id).collect();
            assert!(ids.len() <= MAX_TRACES_PER_KEY, "{} traces are kept for one key, over {MAX_TRACES_PER_KEY}", ids.len());
            assert!(ids.windows(2).all(|w| w[0] > w[1]), "a key's traces are newest first: {ids:?}");
            assert!(ids.iter().all(|id| *id < next), "every trace id {ids:?} was handed out, below {next}");
            assert_eq!(ids, stored(e.key()), "the traces in memory are the traces in the database");
        }
        if let Some(n) = names {
            let needed: std::collections::BTreeSet<String> = self.all_needs_env().into_iter().flat_map(|(_, v)| v).collect();
            assert_eq!(n, needed, "the cached environment names are the names the traces need");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::MemTables;

    fn trace(n: u8, created: u64) -> Trace {
        let file = MEntry::File { hash: Hash([n; 32]), exec: false, size: n as u64 };
        let result = ActionResult {
            status: 0,
            stdout: Hash([n.wrapping_add(1); 32]),
            stderr: Hash([n.wrapping_add(2); 32]),
            outputs: Arc::new(Manifest { entries: vec![(format!("/o/{n}").into_bytes(), file)] }),
            stats: Stats { wall_us: 10, cpu_us: 5, max_rss_kb: 1 },
        };
        Trace { obs: Vec::new(), result, created }
    }

    fn ids(c: &TraceCache, weak: &Hash) -> Vec<u64> {
        c.traces(weak).iter().map(|(id, _)| *id).collect()
    }

    #[test]
    fn trace_ids_follow_the_given_seed_and_each_key_keeps_its_newest() {
        let (a, b) = (Hash::of(b"action a"), Hash::of(b"action b"));
        let run = |seed: u64| {
            let db = Arc::new(MemTables::default());
            let c = TraceCache::new(db.clone(), seed);
            c.invariants();
            for n in 0..30u8 {
                c.insert(if n % 3 == 0 { &b } else { &a }, trace(n, 1000 + n as u64));
                rargo_machine::check(&c, format_args!("after insert {n}"));
            }
            let keys: Vec<Vec<u8>> = db.scan_prefix(Table::Traces, &[]).into_iter().map(|(k, _)| k).collect();
            (db, c, keys)
        };
        let (db, c, keys) = run(500);
        assert_eq!(run(500).2, keys, "the same seed gives the same ids");
        assert_ne!(run(7).2, keys);
        let of_a: Vec<u64> = (500..530).filter(|i| (i - 500) % 3 != 0).rev().collect();
        assert_eq!(ids(&c, &a), of_a, "20 traces of a, newest first");
        let of_b: Vec<u64> = (500..530).filter(|i| (i - 500) % 3 == 0).rev().collect();
        assert_eq!(ids(&c, &b), of_b);
        for n in 0..30u8 {
            c.insert(&a, trace(n, 2000));
            c.invariants();
        }
        assert_eq!(ids(&c, &a), (530..560).rev().take(MAX_TRACES_PER_KEY).collect::<Vec<u64>>(), "only the newest 24 are kept");
        assert_eq!(db.scan_prefix(Table::Traces, &a.0).len(), MAX_TRACES_PER_KEY, "and the rest are deleted");
        assert_eq!(c.traces(&a)[0].1.created, 2000, "a trace keeps the time it was given");
        c.remove(&a, 545);
        c.invariants();
        assert!(!ids(&c, &a).contains(&545));

        let again = TraceCache::new(db.clone(), 900);
        assert_eq!(ids(&again, &a), ids(&c, &a), "another cache reads the same traces back");
        again.invariants();
        let created: Vec<u64> = again.traces(&b).iter().map(|(_, t)| t.created).collect();
        assert_eq!(created, (0..30u64).filter(|n| n % 3 == 0).rev().map(|n| 1000 + n).collect::<Vec<u64>>());
        assert_eq!(again.all().len(), MAX_TRACES_PER_KEY - 1 + 10);
        let mut live = std::collections::BTreeSet::new();
        again.mark(&mut live);
        let expected: std::collections::BTreeSet<Hash> = again
            .all()
            .iter()
            .flat_map(|(_, t)| {
                let files = t.result.outputs.entries.iter().filter_map(|(_, e)| if let MEntry::File { hash, .. } = e { Some(*hash) } else { None });
                [t.result.stdout, t.result.stderr].into_iter().chain(files).collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(live, expected, "every kept trace's outputs are live");
        assert!(live.contains(&Hash([0; 32])), "including the oldest trace of b");
    }

    #[test]
    fn the_environment_an_action_needs_is_merged_and_remembered() {
        let db = Arc::new(MemTables::default());
        let c = TraceCache::new(db.clone(), 1);
        let (a, b) = (Hash::of(b"a"), Hash::of(b"b"));
        assert!(c.needs_env(&a).is_empty() && c.every_env_name().is_empty());
        c.invariants();
        assert_eq!(c.add_needs_env(&a, &["PATH".into(), "HOME".into()]), ["PATH", "HOME"]);
        c.invariants();
        assert_eq!(c.add_needs_env(&a, &["HOME".into(), "CC".into()]), ["CC"]);
        assert!(c.add_needs_env(&a, &["CC".into()]).is_empty());
        assert_eq!(c.add_needs_env(&b, &["RUSTFLAGS".into()]), ["RUSTFLAGS"]);
        c.invariants();
        assert_eq!(c.needs_env(&a), ["CC", "HOME", "PATH"]);
        assert_eq!(c.every_env_name(), ["CC", "HOME", "PATH", "RUSTFLAGS"]);
        let again = TraceCache::new(db, 1);
        assert_eq!(again.all_needs_env().len(), 2);
        assert_eq!(again.every_env_name(), ["CC", "HOME", "PATH", "RUSTFLAGS"]);
    }
}
