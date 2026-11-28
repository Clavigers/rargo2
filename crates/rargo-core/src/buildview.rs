use rargo_machine::StateMachine;
use crate::snapshot::Snapshot;
use crate::trace::{MEntry, Manifest};
use crate::view::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A host listing as the memo keeps it: shared, or the errno it failed with.
type Listed = Result<Arc<Listing>, i32>;

/// How many generations a cached host answer outlives its own.
const MEMO_SPAN: u64 = 64;

/// What the host was seen to hold, by generation, and the oldest generation
/// each live base view still accepts. The shell reads the host; this only
/// keeps what it was told.
pub(crate) struct MemoState {
    looked: BTreeMap<Vec<u8>, (u64, Looked)>,
    listed: BTreeMap<Vec<u8>, (u64, Listed)>,
    generation: u64,
    views: BTreeMap<u64, u64>,
    next_view: u64,
    sealed: bool,
}

impl Default for MemoState {
    fn default() -> Self {
        MemoState {
            looked: BTreeMap::new(),
            listed: BTreeMap::new(),
            generation: 1,
            views: BTreeMap::new(),
            next_view: 0,
            sealed: false,
        }
    }
}

impl MemoState {
    /// Starts a new generation, and every `MEMO_SPAN` generations forgets
    /// what no view can accept any more.
    fn advance(&mut self) -> u64 {
        self.generation += 1;
        let g = self.generation;
        if g.is_multiple_of(MEMO_SPAN) {
            self.looked.retain(|_, v| v.0 + MEMO_SPAN > g);
            self.listed.retain(|_, v| v.0 + MEMO_SPAN > g);
            assert!(self.looked.values().all(|v| v.0 + MEMO_SPAN > g), "pruning left a stale lookup");
            assert!(self.listed.values().all(|v| v.0 + MEMO_SPAN > g), "pruning left a stale listing");
        }
        g
    }

    fn open_view(&mut self, min_gen: u64) -> u64 {
        assert!(min_gen <= self.generation, "a view accepts generation {min_gen}, which has not started");
        let id = self.next_view;
        self.next_view += 1;
        assert!(self.views.insert(id, min_gen).is_none(), "view {id} opened twice");
        id
    }

    /// The view accepts nothing older than a generation started now.
    fn renew(&mut self, view: u64) {
        assert!(self.views.contains_key(&view), "renewing view {view}, which is closed");
        let g = self.advance();
        self.views.insert(view, g);
        assert_eq!(self.views[&view], self.generation);
    }

    fn close_view(&mut self, view: u64) {
        self.views.remove(&view);
        assert!(!self.views.contains_key(&view));
    }

    fn min_gen(&self, view: u64) -> u64 {
        *self.views.get(&view).unwrap_or_else(|| panic!("view {view} is closed"))
    }

    fn lookup_at(&self, min_gen: u64, key: &[u8]) -> Option<Looked> {
        let e = self.looked.get(key)?;
        (e.0 >= min_gen).then(|| e.1.clone())
    }

    fn list_at(&self, min_gen: u64, key: &[u8]) -> Option<Listed> {
        let e = self.listed.get(key)?;
        (e.0 >= min_gen).then(|| e.1.clone())
    }

    /// Keeps what the shell saw on the host, as of the current generation.
    fn observe(&mut self, key: Vec<u8>, answer: Observed) {
        assert!(!self.sealed, "a sealed memo was told what {} holds", String::from_utf8_lossy(&key));
        let generation = self.generation;
        match answer {
            Observed::Lookup(looked) => {
                self.looked.insert(key, (generation, looked));
            }
            Observed::List(listed) => {
                self.listed.insert(key, (generation, listed));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn store_lookup(&mut self, key: Vec<u8>, looked: Looked) {
        self.observe(key, Observed::Lookup(looked));
    }

    #[cfg(test)]
    pub(crate) fn store_list(&mut self, key: Vec<u8>, listed: Result<Listing, i32>) {
        self.observe(key, Observed::List(listed.map(Arc::new)));
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn seal(&mut self) {
        self.sealed = true;
        assert!(self.sealed);
    }
}

impl StateMachine for MemoState {
    fn invariants(&self) {
        assert!(self.generation >= 1, "generations start at 1");
        assert!(self.looked.values().all(|v| v.0 <= self.generation), "a lookup is from a generation not yet started");
        assert!(self.listed.values().all(|v| v.0 <= self.generation), "a listing is from a generation not yet started");
        assert!(self.views.values().all(|&g| g <= self.generation), "a view accepts a generation not yet started");
        assert!(self.views.keys().all(|&id| id < self.next_view), "a view id was not handed out");
    }
}

/// A host observation the shell delivers to the memo.
enum Observed {
    Lookup(Looked),
    List(Listed),
}

/// The host memo: owned by a shell loop while builds run, or a sealed
/// in-memory fixture that no one may tell anything new.
pub struct HostMemo {
    storage: MemoStorage,
}

enum MemoStorage {
    Live(StateLoop<MemoState>),
    /// A sealed in-memory fixture, for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    Frozen(MemoState),
}

impl Default for HostMemo {
    fn default() -> Self {
        HostMemo { storage: MemoStorage::Live(StateLoop::spawn("rargo-host-memo", MemoState::default())) }
    }
}

/// Which answers a base view accepts: those its memo's loop says are new
/// enough for it, or, over a sealed fixture, those from `min_gen` on.
enum MemoView {
    Live(u64),
    Fixed(u64),
}

impl HostMemo {
    #[cfg(test)]
    pub(crate) fn frozen(mut state: MemoState) -> HostMemo {
        state.seal();
        state.invariants();
        HostMemo { storage: MemoStorage::Frozen(state) }
    }

    pub fn advance(&self) -> u64 {
        match &self.storage {
            MemoStorage::Live(owner) => owner.call(|state, ()| state.advance(), ()),
            MemoStorage::Frozen(_) => panic!("a sealed memo does not advance"),
        }
    }

    fn open_view(&self, min_gen: u64) -> MemoView {
        match &self.storage {
            MemoStorage::Live(owner) => MemoView::Live(owner.call(|state, min_gen| state.open_view(min_gen), min_gen)),
            MemoStorage::Frozen(_) => MemoView::Fixed(min_gen),
        }
    }

    fn renew(&self, view: &MemoView) {
        match (&self.storage, view) {
            (MemoStorage::Live(owner), &MemoView::Live(id)) => owner.call(|state, id| state.renew(id), id),
            _ => panic!("a view over a sealed memo does not renew"),
        }
    }

    fn close_view(&self, view: &MemoView) {
        if let (MemoStorage::Live(owner), &MemoView::Live(id)) = (&self.storage, view) {
            owner.tell(move |state| state.close_view(id));
        }
    }

    fn lookup(&self, view: &MemoView, key: &[u8]) -> Option<Looked> {
        match (&self.storage, view) {
            (MemoStorage::Live(owner), &MemoView::Live(id)) => {
                owner.call(|state, (id, key)| state.lookup_at(state.min_gen(id), &key), (id, key.to_vec()))
            }
            (MemoStorage::Frozen(state), &MemoView::Fixed(min_gen)) => state.lookup_at(min_gen, key),
            _ => unreachable!("a view is opened by its own memo"),
        }
    }

    fn list(&self, view: &MemoView, key: &[u8]) -> Option<Listed> {
        match (&self.storage, view) {
            (MemoStorage::Live(owner), &MemoView::Live(id)) => {
                owner.call(|state, (id, key)| state.list_at(state.min_gen(id), &key), (id, key.to_vec()))
            }
            (MemoStorage::Frozen(state), &MemoView::Fixed(min_gen)) => state.list_at(min_gen, key),
            _ => unreachable!("a view is opened by its own memo"),
        }
    }

    fn sealed(&self) -> bool {
        match &self.storage {
            MemoStorage::Live(owner) => owner.call(|state, ()| state.sealed, ()),
            MemoStorage::Frozen(state) => state.sealed,
        }
    }

    fn observe(&self, key: Vec<u8>, answer: Observed) {
        match &self.storage {
            MemoStorage::Live(owner) => owner.call(|state, (key, answer)| state.observe(key, answer), (key, answer)),
            MemoStorage::Frozen(_) => panic!("a sealed memo was told what {} holds", String::from_utf8_lossy(&key)),
        }
    }
}

#[derive(Clone, Debug)]
pub enum SynthEnt {
    File { hash: crate::hash::Hash, size: u64 },
    Symlink(Vec<u8>),
}

pub enum Region {
    Host(HostRegion),
    Snap(Arc<Snapshot>),
    Empty,
    NixStore { names: Arc<BTreeSet<Vec<u8>>> },
    Synth(Arc<BTreeMap<Vec<u8>, SynthEnt>>),
    Link(Vec<u8>),
}

pub struct Mount {
    pub at: Vec<u8>,
    pub region: Region,
    pub traced: bool,
}

enum LookupPlan<'a> {
    Known(Looked),
    MountedHost(&'a Path),
    Host(&'a HostRegion, &'a [u8]),
    Nix(&'a [u8]),
}

pub struct BaseView {
    mounts: Vec<Mount>,
    memo: Arc<HostMemo>,
    accepts: MemoView,
    min_gen: u64,
    nix: Arc<Vec<(Vec<u8>, crate::hash::Hash)>>,
}

pub const NIX_STORE_AT: &[u8] = b"/nix/store";

const DIR_OWNER: Owner = Owner { uid: 0, gid: 0, mode: 0o755 };

impl BaseView {
    pub fn new(mut mounts: Vec<Mount>, memo: Arc<HostMemo>, min_gen: u64) -> BaseView {
        mounts.sort_by(|a, b| b.at.len().cmp(&a.at.len()));
        assert!(mounts.iter().any(|m| m.at == b"/"), "a root mount is required");
        assert!(mounts.windows(2).all(|w| w[0].at.len() >= w[1].at.len()));
        for (i, m) in mounts.iter().enumerate() {
            assert!(m.at.starts_with(b"/"), "mount points are absolute");
            assert!(mounts[i + 1..].iter().all(|o| o.at != m.at), "two mounts at {}", crate::view::show(&m.at));
        }
        let accepts = memo.open_view(min_gen);
        BaseView { mounts, memo, accepts, min_gen, nix: Arc::new(Vec::new()) }
    }

    fn nix_store_names(&self) -> Option<&Arc<BTreeSet<Vec<u8>>>> {
        self.mounts.iter().find_map(|m| match &m.region {
            Region::NixStore { names } if m.at == NIX_STORE_AT && !m.traced => Some(names),
            _ => None,
        })
    }

    pub fn nix_share_key(&self) -> Option<crate::hash::Hash> {
        let names = self.nix_store_names()?;
        let mut key = crate::hash::KeyHasher::new("rargo nix store share");
        for name in names.iter() {
            key.field(name);
        }
        Some(key.finish())
    }

    pub fn nix_share_view(&self) -> Option<Arc<dyn View>> {
        let names = self.nix_store_names()?.clone();
        let mounts = vec![
            Mount { at: b"/".to_vec(), region: Region::Empty, traced: false },
            Mount { at: NIX_STORE_AT.to_vec(), region: Region::NixStore { names }, traced: false },
        ];
        Some(Arc::new(Hosted::new(BaseView::new(mounts, self.memo.clone(), self.min_gen))))
    }

    pub fn with_nix(mut self, nix: Arc<Vec<(Vec<u8>, crate::hash::Hash)>>) -> BaseView {
        self.nix = nix;
        self
    }

    /// Asks the host afresh from now on: nothing cached before this call is accepted.
    pub fn renew(&self) {
        self.memo.renew(&self.accepts);
    }

    fn mount_of<'a>(&'a self, dir: &'a [u8]) -> (&'a Mount, &'a [u8]) {
        for m in &self.mounts {
            if vpath_starts_with(dir, &m.at) {
                let rel = if m.at == b"/" { dir } else { &dir[m.at.len()..] };
                let rel: &[u8] = if rel.is_empty() { b"/" } else { rel };
                assert!(rel.starts_with(b"/"));
                return (m, rel);
            }
        }
        unreachable!("root mount always matches")
    }

    fn mount_at(&self, path: &[u8]) -> Option<&Mount> {
        self.mounts.iter().find(|m| m.at == path)
    }

    fn cached_lookup(&self, key: &[u8]) -> Option<Looked> {
        self.memo.lookup(&self.accepts, key)
    }

    fn cached_list(&self, key: &[u8]) -> Option<Result<Arc<Listing>, i32>> {
        self.memo.list(&self.accepts, key)
    }

    fn deposit(&self, req: &HostReq, answer: HostAnswer) {
        let observed = match (&req.kind, answer) {
            (HostKind::Lookup, HostAnswer::Lookup(looked)) => Observed::Lookup(looked),
            (HostKind::List, HostAnswer::List(listed)) => Observed::List(match listed {
                Ok(list) => Ok(Arc::new(list)),
                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
            }),
            (HostKind::Lookup, HostAnswer::List(_)) | (HostKind::List, HostAnswer::Lookup(_)) => {
                panic!("a host answer matches the request");
            }
        };
        self.memo.observe(req.key.clone(), observed);
    }

    fn step_host(&self, region: &HostRegion, rel: &[u8], name: &[u8], infer_at: Option<&[u8]>) -> Step<Looked> {
        let key = lookup_key(region, rel, name);
        if let Some(raw) = self.cached_lookup(&key) {
            return Step::Done(match infer_at {
                Some(path) => self.infer_mount_parent(path, raw),
                None => raw,
            });
        }
        Step::Ask(HostReq { kind: HostKind::Lookup, key, path: region.real_path(rel, name) })
    }

    fn step_host_list(&self, region: &HostRegion, rel: &[u8]) -> Step<io::Result<Arc<Listing>>> {
        let key = list_key(region, rel);
        if let Some(listed) = self.cached_list(&key) {
            return Step::Done(match listed {
                Ok(list) => Ok(list),
                Err(err) => Err(io::Error::from_raw_os_error(err)),
            });
        }
        Step::Ask(HostReq { kind: HostKind::List, key, path: region.real_path(rel, b"") })
    }

    fn step_mounted(&self, real: &Path) -> Step<Looked> {
        let (Some(parent), Some(name)) = (real.parent(), real.file_name()) else {
            return Step::Done(Looked::Dir(DIR_OWNER));
        };
        let region = HostRegion::new(parent);
        self.step_host(&region, b"/", name.as_encoded_bytes(), None)
    }

    fn step_is_real_dir(&self, dir: &[u8]) -> Step<bool> {
        match vsplit(dir) {
            None => Step::Done(true),
            Some((parent, name)) => match self.step_lookup(parent, name) {
                Step::Ask(ask) => Step::Ask(ask),
                Step::Done(looked) => {
                    if !matches!(looked, Looked::Dir(_)) {
                        Step::Done(false)
                    } else {
                        self.step_is_real_dir(parent)
                    }
                }
            },
        }
    }

    fn plan_lookup<'a>(&'a self, dir: &'a [u8], name: &[u8], path: &[u8]) -> LookupPlan<'a> {
        if let Some(mount) = self.mount_at(path) {
            return match &mount.region {
                Region::Link(target) => LookupPlan::Known(Looked::Symlink(target.clone())),
                Region::Host(host) => LookupPlan::MountedHost(&host.real),
                _ => LookupPlan::Known(Looked::Dir(DIR_OWNER)),
            };
        }
        let (mount, rel) = self.mount_of(dir);
        match &mount.region {
            Region::Empty | Region::Link(_) => LookupPlan::Known(Looked::Absent),
            Region::Snap(snapshot) => LookupPlan::Known(snapshot.lookup(rel, name)),
            Region::Host(host) => LookupPlan::Host(host, rel),
            Region::NixStore { names } if rel == b"/" && !names.contains(name) => LookupPlan::Known(Looked::Absent),
            Region::NixStore { .. } => LookupPlan::Nix(rel),
            Region::Synth(entries) => LookupPlan::Known(match (rel, entries.get(name)) {
                (b"/", Some(SynthEnt::File { hash, size })) => Looked::File(FileInfo {
                    size: *size,
                    owner: Owner { uid: 0, gid: 0, mode: 0o644 },
                    src: FileSrc::Blob(*hash),
                    sem: None,
                }),
                (b"/", Some(SynthEnt::Symlink(target))) => Looked::Symlink(target.clone()),
                _ => Looked::Absent,
            }),
        }
    }

    fn infer_mount_parent(&self, path: &[u8], observed: Looked) -> Looked {
        if matches!(observed, Looked::Absent) && self.mounts.iter().any(|mount| is_strictly_under(&mount.at, path)) {
            Looked::Dir(DIR_OWNER)
        } else {
            observed
        }
    }

    fn step_lookup(&self, dir: &[u8], name: &[u8]) -> Step<Looked> {
        let path = vjoin(dir, name);
        match self.plan_lookup(dir, name, &path) {
            LookupPlan::Known(observed) => Step::Done(self.infer_mount_parent(&path, observed)),
            LookupPlan::MountedHost(real) => self.step_mounted(real),
            LookupPlan::Host(host, rel) => self.step_host(host, rel, name, Some(&path)),
            LookupPlan::Nix(rel) => {
                let region = nix_store_region();
                self.step_host(&region, rel, name, Some(&path))
            }
        }
    }

    fn step_list(&self, dir: &[u8]) -> Step<io::Result<Listing>> {
        let (mount, rel) = self.mount_of(dir);
        let listed: io::Result<Listing> = match &mount.region {
            Region::Empty | Region::Link(_) => Ok(Vec::new()),
            Region::Snap(snapshot) => snapshot.list(rel),
            Region::Host(host) => match self.step_host_list(host, rel) {
                Step::Ask(ask) => return Step::Ask(ask),
                Step::Done(result) => result.map(|list| list.as_ref().clone()),
            },
            Region::NixStore { names } if rel == b"/" => Ok(names.iter().map(|n| (n.clone(), DType::Dir)).collect()),
            Region::NixStore { .. } => {
                let region = nix_store_region();
                match self.step_host_list(&region, rel) {
                    Step::Ask(ask) => return Step::Ask(ask),
                    Step::Done(result) => result.map(|list| list.as_ref().clone()),
                }
            }
            Region::Synth(entries) => Ok(entries
                .iter()
                .map(|(n, e)| (n.clone(), if matches!(e, SynthEnt::File { .. }) { DType::File } else { DType::Symlink }))
                .collect()),
        };
        let mut out = match listed {
            Ok(list) => list,
            Err(_) if self.mounts.iter().any(|other| is_strictly_under(&other.at, dir)) => Vec::new(),
            Err(e) => return Step::Done(Err(e)),
        };
        for other in &self.mounts {
            if !is_strictly_under(&other.at, dir) {
                continue;
            }
            let rest = &other.at[if dir == b"/" { 1 } else { dir.len() + 1 }..];
            let name = rest.split(|&c| c == b'/').next().unwrap_or(rest);
            if name.is_empty() || out.iter().any(|(n, _)| n == name) {
                continue;
            }
            let kind = match self.step_lookup(dir, name) {
                Step::Ask(ask) => return Step::Ask(ask),
                Step::Done(Looked::File(_)) => DType::File,
                Step::Done(Looked::Symlink(_)) => DType::Symlink,
                Step::Done(_) => DType::Dir,
            };
            out.push((name.to_vec(), kind));
        }
        debug_assert!(
            out.iter().enumerate().all(|(i, (n, _))| out[i + 1..].iter().all(|(m, _)| m != n)),
            "listing names are unique"
        );
        Step::Done(Ok(out))
    }

    fn step_stat(&self, path: &[u8]) -> Step<Looked> {
        match vsplit(path) {
            None => Step::Done(Looked::Dir(DIR_OWNER)),
            Some((dir, name)) => {
                let (mount, _) = self.mount_of(dir);
                if matches!(mount.region, Region::Host(_) | Region::NixStore { .. }) {
                    match self.step_is_real_dir(dir) {
                        Step::Ask(ask) => return Step::Ask(ask),
                        Step::Done(false) => return Step::Done(Looked::Absent),
                        Step::Done(true) => {}
                    }
                }
                self.step_lookup(dir, name)
            }
        }
    }
}

impl Drop for BaseView {
    fn drop(&mut self) {
        self.memo.close_view(&self.accepts);
    }
}

impl StateMachine for BaseView {
    fn invariants(&self) {
        assert!(self.mounts.iter().any(|m| m.at == b"/"), "something is mounted at the root");
        assert!(self.mounts.windows(2).all(|w| w[0].at.len() >= w[1].at.len()), "the longest mount points come first");
        assert!(self.mounts.iter().all(|m| m.at.starts_with(b"/")), "every mount point is absolute");
        for (i, m) in self.mounts.iter().enumerate() {
            assert!(self.mounts[i + 1..].iter().all(|o| o.at != m.at), "one mount at {}", String::from_utf8_lossy(&m.at));
        }
    }
}

fn host_answer(view: &BaseView, req: &HostReq) {
    assert!(!view.memo.sealed(), "sealed view asked to read {}", req.path.display());
    let answer = match req.kind {
        HostKind::Lookup => HostAnswer::Lookup(read_host_lookup(&req.path)),
        HostKind::List => HostAnswer::List(read_host_list(&req.path)),
    };
    view.deposit(req, answer);
}

fn nix_store_region() -> HostRegion {
    HostRegion::new("/nix/store")
}

fn is_strictly_under(path: &[u8], dir: &[u8]) -> bool {
    path.len() > dir.len() && vpath_starts_with(path, dir)
}

fn given<T>(view: &BaseView, step: Step<T>) -> T {
    view.invariants();
    match step {
        Step::Done(value) => value,
        Step::Ask(_) => panic!("a base view is given its contents; the shell answers a host request"),
    }
}

impl View for BaseView {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
        given(self, self.step_lookup(dir, name))
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        given(self, self.step_list(dir))
    }

    fn traced(&self, dir: &[u8]) -> bool {
        self.mount_of(dir).0.traced
    }

    fn nix_closure(&self) -> &[(Vec<u8>, crate::hash::Hash)] {
        &self.nix
    }

    fn nix_share_key(&self) -> Option<crate::hash::Hash> {
        BaseView::nix_share_key(self)
    }

    fn nix_share_view(&self) -> Option<Arc<dyn View>> {
        BaseView::nix_share_view(self)
    }

    fn stat(&self, path: &[u8]) -> Looked {
        given(self, self.step_stat(path))
    }
}

/// The shell around a base view. It is the only caller that turns a host request into a filesystem read.
pub struct Hosted {
    view: BaseView,
}

impl Hosted {
    pub fn new(view: BaseView) -> Hosted {
        Hosted { view }
    }
}

impl View for Hosted {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
        drive(&self.view, |view| view.step_lookup(dir, name))
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        drive(&self.view, |view| view.step_list(dir))
    }

    fn traced(&self, dir: &[u8]) -> bool {
        self.view.mount_of(dir).0.traced
    }

    fn nix_closure(&self) -> &[(Vec<u8>, crate::hash::Hash)] {
        &self.view.nix
    }

    fn nix_share_key(&self) -> Option<crate::hash::Hash> {
        self.view.nix_share_key()
    }

    fn nix_share_view(&self) -> Option<Arc<dyn View>> {
        self.view.nix_share_view()
    }

    fn stat(&self, path: &[u8]) -> Looked {
        drive(&self.view, |view| view.step_stat(path))
    }
}

enum HostKind {
    Lookup,
    List,
}

struct HostReq {
    kind: HostKind,
    key: Vec<u8>,
    path: PathBuf,
}

enum HostAnswer {
    Lookup(Looked),
    List(io::Result<Listing>),
}

enum Step<T> {
    Done(T),
    Ask(HostReq),
}

fn drive<T>(view: &BaseView, mut step: impl FnMut(&BaseView) -> Step<T>) -> T {
    for _ in 0..10_000 {
        match step(view) {
            Step::Done(value) => {
                view.invariants();
                return value;
            }
            Step::Ask(req) => {
                host_answer(view, &req);
                view.invariants();
            }
        }
    }
    panic!("host lookup did not settle");
}

fn host_key(region: &HostRegion, rel_dir: &[u8]) -> Vec<u8> {
    let mut key = region.real.as_os_str().as_encoded_bytes().to_vec();
    if rel_dir != b"/" {
        key.extend_from_slice(rel_dir);
    }
    key
}

fn lookup_key(region: &HostRegion, rel_dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut key = host_key(region, rel_dir);
    key.push(b'/');
    key.extend_from_slice(name);
    key
}

fn list_key(region: &HostRegion, rel_dir: &[u8]) -> Vec<u8> {
    host_key(region, rel_dir)
}

pub fn read_host_lookup(path: &Path) -> Looked {
    let Ok(m) = crate::io::file::symlink_metadata(path) else { return Looked::Absent };
    let owner = Owner { uid: m.uid(), gid: m.gid(), mode: m.mode() & 0o777 };
    let ft = m.file_type();
    if ft.is_dir() {
        Looked::Dir(owner)
    } else if ft.is_file() {
        Looked::File(FileInfo {
            size: m.len(),
            owner,
            src: FileSrc::Host { path: Arc::from(path.to_path_buf()), sig: HostSig::of(&m) },
            sem: None,
        })
    } else if ft.is_symlink() {
        match crate::io::file::read_link(path) {
            Ok(t) => Looked::Symlink(t.as_os_str().as_bytes().to_vec()),
            Err(_) => Looked::Absent,
        }
    } else {
        Looked::Other
    }
}

pub fn read_host_list(path: &Path) -> io::Result<Listing> {
    let mut out = Vec::new();
    for e in crate::io::file::read_dir(path)? {
        let e = e?;
        let ft = e.file_type()?;
        let kind = if ft.is_dir() {
            DType::Dir
        } else if ft.is_file() {
            DType::File
        } else if ft.is_symlink() {
            DType::Symlink
        } else {
            DType::Other
        };
        out.push((e.file_name().as_bytes().to_vec(), kind));
    }
    Ok(out)
}

pub const LEVEL_NONE: u8 = 0;
pub const LEVEL_META: u8 = 1;
pub const LEVEL_FULL: u8 = 2;

#[derive(Clone)]
enum LayerEnt {
    Entry(MEntry),
    Implied,
}

#[derive(Default)]
struct LayerDir {
    children: BTreeMap<Vec<u8>, Vec<(u32, u8, LayerEnt)>>,
}

#[derive(Default)]
pub(crate) struct OutputState {
    dirs: BTreeMap<Vec<u8>, LayerDir>,
    ranks: BTreeMap<u32, u64>,
}

impl OutputState {
    pub(crate) fn add(&mut self, unit: u32, stage: u8, entries: &[(Vec<u8>, MEntry)]) {
        for (path, e) in entries {
            let Some((parent, name)) = vsplit(path) else { continue };
            {
                let d = self.dirs.entry(parent.to_vec()).or_default();
                let list = d.children.entry(name.to_vec()).or_default();
                list.retain(|(u, s, ent)| !(*u == unit && *s >= stage && matches!(ent, LayerEnt::Implied)));
                if !list.iter().any(|(u, s, _)| *u == unit && *s == stage) {
                    list.push((unit, stage, LayerEnt::Entry(e.clone())));
                }
                assert_eq!(list.iter().filter(|(u, s, _)| *u == unit && *s == stage).count(), 1);
            }
            let mut cur = parent;
            while let Some((pp, pn)) = vsplit(cur) {
                let d = self.dirs.entry(pp.to_vec()).or_default();
                let list = d.children.entry(pn.to_vec()).or_default();
                if list.iter().any(|(u, s, _)| *u == unit && *s <= stage) {
                    break;
                }
                list.push((unit, stage, LayerEnt::Implied));
                assert!(pp.len() < cur.len());
                cur = pp;
            }
        }
    }

    pub(crate) fn add_manifest(&mut self, unit: u32, stage: u8, m: &Manifest) {
        self.add(unit, stage, &m.entries);
        assert!(m.entries.iter().filter_map(|(path, _)| vsplit(path)).all(|(dir, _)| self.dirs.contains_key(dir)));
    }

    fn set_rank(&mut self, unit: u32, rank: u64) {
        self.ranks.insert(unit, rank);
        assert_eq!(self.rank(unit), rank);
    }

    fn rank(&self, unit: u32) -> u64 {
        self.ranks.get(&unit).copied().unwrap_or(unit as u64)
    }

    fn pick(&self, list: &[(u32, u8, LayerEnt)], levels: &[u8]) -> Option<LayerEnt> {
        let mut best: Option<(u32, &LayerEnt)> = None;
        for (u, stage, ent) in list {
            if levels.get(*u as usize).copied().unwrap_or(LEVEL_NONE) < *stage {
                continue;
            }
            let better = match (&best, ent) {
                (None, _) => true,
                (Some((_, LayerEnt::Implied)), LayerEnt::Entry(_)) => true,
                (Some((_, LayerEnt::Entry(_))), LayerEnt::Implied) => false,
                (Some((bu, _)), _) => self.rank(*u) > self.rank(*bu),
            };
            if better {
                best = Some((*u, ent));
            }
        }
        assert!(best.is_some() || list.iter().all(|(u, s, _)| levels.get(*u as usize).copied().unwrap_or(LEVEL_NONE) < *s));
        best.map(|(_, e)| e.clone())
    }

    fn get(&self, dir: &[u8], name: &[u8], levels: &[u8]) -> Option<LayerEnt> {
        let d = self.dirs.get(dir)?;
        self.pick(d.children.get(name)?, levels)
    }

    fn visible_paths(&self, levels: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for (dir, d) in &self.dirs {
            for (name, list) in &d.children {
                if let Some(LayerEnt::Entry(MEntry::File { .. } | MEntry::Symlink(_))) = self.pick(list, levels) {
                    out.push(vjoin(dir, name));
                }
            }
        }
        out
    }

    fn names(&self, dir: &[u8], levels: &[u8]) -> Vec<(Vec<u8>, LayerEnt)> {
        match self.dirs.get(dir) {
            None => Vec::new(),
            Some(d) => d
                .children
                .iter()
                .filter_map(|(name, list)| self.pick(list, levels).map(|e| (name.clone(), e)))
                .collect(),
        }
    }
}

impl StateMachine for OutputState {
    fn invariants(&self) {
        for (dir, d) in &self.dirs {
            for (name, list) in &d.children {
                for (i, (u, s, _)) in list.iter().enumerate() {
                    assert!(
                        list[i + 1..].iter().all(|(v, t, _)| (v, t) != (u, s)),
                        "unit {u} stage {s} has two entries at {}",
                        String::from_utf8_lossy(&vjoin(dir, name))
                    );
                }
            }
        }
    }
}

/// An output overlay is either owned by the shell loop or published as an immutable snapshot.
pub struct OutputsLayer {
    storage: OutputStorage,
}

enum OutputStorage {
    Live(StateLoop<OutputState>),
    Frozen(OutputState),
}

impl Default for OutputsLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputsLayer {
    pub fn new() -> Self {
        Self { storage: OutputStorage::Live(StateLoop::spawn("rargo-output-layer", OutputState::default())) }
    }

    pub(crate) fn frozen(state: OutputState) -> Self {
        Self { storage: OutputStorage::Frozen(state) }
    }

    pub fn add(&self, unit: u32, stage: u8, entries: &[(Vec<u8>, MEntry)]) {
        self.live().call(move |state, entries| state.add(unit, stage, &entries), entries.to_vec());
    }

    pub fn add_manifest(&self, unit: u32, stage: u8, m: &Manifest) {
        self.add(unit, stage, &m.entries);
    }

    pub fn set_rank(&self, unit: u32, rank: u64) {
        self.live().call(move |state, ()| state.set_rank(unit, rank), ());
    }

    fn live(&self) -> &StateLoop<OutputState> {
        match &self.storage {
            OutputStorage::Live(owner) => owner,
            OutputStorage::Frozen(_) => panic!("a published output layer is immutable"),
        }
    }

    fn get(&self, dir: &[u8], name: &[u8], levels: Levels<'_>) -> Option<LayerEnt> {
        match &self.storage {
            OutputStorage::Live(owner) => owner.call(
                |state, (dir, name, levels)| state.get(&dir, &name, &levels),
                (dir.to_vec(), name.to_vec(), levels.shared()),
            ),
            OutputStorage::Frozen(state) => state.get(dir, name, levels.slice()),
        }
    }

    fn visible_paths(&self, levels: Levels<'_>) -> Vec<Vec<u8>> {
        match &self.storage {
            OutputStorage::Live(owner) => owner.call(|state, levels| state.visible_paths(&levels), levels.shared()),
            OutputStorage::Frozen(state) => state.visible_paths(levels.slice()),
        }
    }

    fn names(&self, dir: &[u8], levels: Levels<'_>) -> Vec<(Vec<u8>, LayerEnt)> {
        match &self.storage {
            OutputStorage::Live(owner) => owner.call(
                |state, (dir, levels)| state.names(&dir, &levels),
                (dir.to_vec(), levels.shared()),
            ),
            OutputStorage::Frozen(state) => state.names(dir, levels.slice()),
        }
    }
}

/// The level of each unit a lookup sees. A view's own levels are shared with
/// the layer's loop by reference count, so a lookup does not copy them.
#[derive(Clone, Copy)]
enum Levels<'a> {
    Shared(&'a Arc<Vec<u8>>),
    Fixed(&'static [u8]),
}

impl Levels<'_> {
    fn slice(&self) -> &[u8] {
        match self {
            Levels::Shared(levels) => levels,
            Levels::Fixed(levels) => levels,
        }
    }

    fn shared(&self) -> Arc<Vec<u8>> {
        match self {
            Levels::Shared(levels) => Arc::clone(levels),
            Levels::Fixed(levels) => Arc::new(levels.to_vec()),
        }
    }
}

const OWN_FILE: Owner = Owner::own(0o644);
const OWN_EXEC: Owner = Owner::own(0o755);
const OWN_DIR: Owner = Owner::own(0o755);

pub struct BuildView {
    pub base: Arc<BaseView>,
    pub layer: Arc<OutputsLayer>,
    pub levels: Arc<Vec<u8>>,
    pub untraced: Vec<Vec<u8>>,
    pub seed: Option<Arc<OutputsLayer>>,
    pub content: Option<Arc<Content>>,
}

const SEED_LEVELS: [u8; 1] = [LEVEL_FULL];

impl BuildView {
    fn layered(&self, dir: &[u8], name: &[u8]) -> Option<LayerEnt> {
        if let Some(seed) = &self.seed {
            if let Some(e) = seed.get(dir, name, Levels::Fixed(&SEED_LEVELS)) {
                return Some(e);
            }
        }
        self.layer.get(dir, name, Levels::Shared(&self.levels))
    }
}

impl View for BuildView {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
        match self.layered(dir, name) {
            Some(LayerEnt::Entry(MEntry::File { hash, exec, size })) => Looked::File(FileInfo {
                size,
                owner: if exec { OWN_EXEC } else { OWN_FILE },
                src: FileSrc::Blob(hash),
                sem: match (&self.content, name.ends_with(b".rmeta")) {
                    (Some(c), true) => c.metadata_identity(&hash),
                    _ => None,
                },
            }),
            Some(LayerEnt::Entry(MEntry::Symlink(t))) => Looked::Symlink(t),
            Some(LayerEnt::Entry(MEntry::Whiteout)) => Looked::Absent,
            Some(LayerEnt::Entry(MEntry::Dir)) | Some(LayerEnt::Implied) => {
                match drive(&self.base, |view| view.step_lookup(dir, name)) {
                    Looked::Dir(o) => Looked::Dir(o),
                    _ => Looked::Dir(OWN_DIR),
                }
            }
            None => drive(&self.base, |view| view.step_lookup(dir, name)),
        }
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        let mut layered = self.layer.names(dir, Levels::Shared(&self.levels));
        if let Some(seed) = &self.seed {
            layered.extend(seed.names(dir, Levels::Fixed(&SEED_LEVELS)));
        }
        let base = if self.base_has_dir(dir) {
            drive(&self.base, |view| view.step_list(dir))?
        } else {
            Listing::new()
        };
        Ok(crate::view::merge_listings(base, layered.into_iter().map(|(name, entry)| {
            let dtype = match entry {
                LayerEnt::Entry(MEntry::Whiteout) => None,
                LayerEnt::Entry(MEntry::File { .. }) => Some(DType::File),
                LayerEnt::Entry(MEntry::Symlink(_)) => Some(DType::Symlink),
                LayerEnt::Entry(MEntry::Dir) | LayerEnt::Implied => Some(DType::Dir),
            };
            (name, dtype)
        })))
    }

    fn traced(&self, dir: &[u8]) -> bool {
        if self.untraced.iter().any(|u| vpath_starts_with(dir, u)) {
            return false;
        }
        self.base.traced(dir)
    }

    fn hints(&self) -> Vec<Vec<u8>> {
        self.layer.visible_paths(Levels::Shared(&self.levels))
    }

    fn nix_closure(&self) -> &[(Vec<u8>, crate::hash::Hash)] {
        self.base.nix_closure()
    }

    fn nix_share_key(&self) -> Option<crate::hash::Hash> {
        self.base.nix_share_key()
    }

    fn nix_share_view(&self) -> Option<Arc<dyn View>> {
        self.base.nix_share_view()
    }

    fn stat(&self, path: &[u8]) -> Looked {
        let Some((dir, name)) = vsplit(path) else { return Looked::Dir(DIR_OWNER) };
        match self.layered(dir, name) {
            Some(_) => self.lookup(dir, name),
            None => match drive(&self.base, |view| view.step_stat(path)) {
                Looked::Absent if self.layer_has_dir(dir) => drive(&self.base, |view| view.step_lookup(dir, name)),
                other => other,
            },
        }
    }
}

impl BuildView {
    fn base_has_dir(&self, dir: &[u8]) -> bool {
        matches!(drive(&self.base, |view| view.step_stat(dir)), Looked::Dir(_))
    }

    fn layer_has_dir(&self, dir: &[u8]) -> bool {
        match vsplit(dir) {
            None => true,
            Some((p, n)) => {
                matches!(self.layered(p, n), Some(LayerEnt::Implied) | Some(LayerEnt::Entry(MEntry::Dir)))
            }
        }
    }
}

#[derive(Clone)]
pub struct ViewParts {
    pub snapshot: Arc<Snapshot>,
    pub cargo_home: PathBuf,
    pub home_files: Vec<(Vec<u8>, PathBuf)>,
    pub masks: Vec<Vec<u8>>,
    pub hidden: Vec<Vec<u8>>,
    pub planner: PathBuf,
    pub tools: Vec<(Vec<u8>, PathBuf)>,
    pub restricted: Option<Restricted>,
}

#[derive(Clone)]
pub struct Restricted {
    pub store_names: Arc<BTreeSet<Vec<u8>>>,
    pub store_paths: Arc<Vec<(Vec<u8>, crate::hash::Hash)>>,
    pub etc: Arc<BTreeMap<Vec<u8>, SynthEnt>>,
    pub links: Vec<(Vec<u8>, Vec<u8>)>,
}

pub const WORKTREE_AT: &[u8] = b"/w";
pub const CARGO_HOME_AT: &[u8] = b"/c";
pub const PLANNER_AT: &[u8] = b"/rargo/cargo";

fn cargo_home_mounts(cargo_home: &Path) -> [Mount; 2] {
    [
        Mount { at: CARGO_HOME_AT.to_vec(), region: Region::Host(HostRegion::new(cargo_home)), traced: true },
        Mount { at: vjoin(CARGO_HOME_AT, b"bin"), region: Region::Empty, traced: true },
    ]
}

impl ViewParts {
    pub fn build(&self, memo: Arc<HostMemo>, min_gen: u64, host_root: bool) -> BaseView {
        let mut mounts = Vec::new();
        match (&self.restricted, host_root) {
            (Some(r), false) => {
                mounts.push(Mount { at: b"/".to_vec(), region: Region::Empty, traced: true });
                mounts.push(Mount {
                    at: NIX_STORE_AT.to_vec(),
                    region: Region::NixStore { names: r.store_names.clone() },
                    traced: false,
                });
                mounts.push(Mount { at: b"/etc".to_vec(), region: Region::Synth(r.etc.clone()), traced: true });
                for (at, target) in &r.links {
                    mounts.push(Mount { at: at.clone(), region: Region::Link(target.clone()), traced: true });
                }
            }
            _ => mounts.push(Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/")), traced: true }),
        }
        let this = self.clone();
        let this = &this;
        mounts.push(Mount { at: WORKTREE_AT.to_vec(), region: Region::Snap(this.snapshot.clone()), traced: true });
        mounts.extend(cargo_home_mounts(&this.cargo_home));
        mounts.push(Mount {
            at: PLANNER_AT.to_vec(),
            region: Region::Host(HostRegion::new(this.planner.clone())),
            traced: true,
        });
        for (at, real) in this.home_files.iter().chain(&this.tools) {
            mounts.push(Mount { at: at.clone(), region: Region::Host(HostRegion::new(real.clone())), traced: true });
        }
        for m in &this.masks {
            mounts.push(Mount { at: m.clone(), region: Region::Empty, traced: true });
        }
        if this.restricted.is_none() || host_root {
            for m in &this.hidden {
                mounts.push(Mount { at: m.clone(), region: Region::Empty, traced: true });
            }
        }
        let nix = match (&self.restricted, host_root) {
            (Some(r), false) => r.store_paths.clone(),
            _ => Arc::new(Vec::new()),
        };
        BaseView::new(mounts, memo, min_gen).with_nix(nix)
    }
}

#[cfg(test)]
fn host_file(path: &Path, size: u64) -> Looked {
    Looked::File(FileInfo {
        size,
        owner: Owner { uid: 0, gid: 0, mode: 0o644 },
        src: FileSrc::Host {
            path: Arc::from(path.to_path_buf()),
            sig: HostSig { dev: 1, ino: 1, size, mtime_ns: 0, ctime_ns: 0 },
        },
        sem: None,
    })
}

#[cfg(test)]
fn host_dir() -> Looked {
    Looked::Dir(Owner { uid: 0, gid: 0, mode: 0o755 })
}

#[cfg(test)]
fn rooted(real: &str, seed: impl FnOnce(&HostRegion, &mut MemoState)) -> Arc<BaseView> {
    let region = HostRegion::new(real);
    let mut memo = MemoState::default();
    seed(&region, &mut memo);
    let memo = Arc::new(HostMemo::frozen(memo));
    Arc::new(BaseView::new(
        vec![
            Mount { at: b"/".to_vec(), region: Region::Host(region), traced: true },
            Mount { at: b"/t".to_vec(), region: Region::Empty, traced: true },
        ],
        memo,
        0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Transcript;
    use expect_test::expect;

    fn file(n: u8) -> MEntry {
        MEntry::File { hash: crate::hash::Hash([n; 32]), exec: false, size: n as u64 }
    }

    fn layer_base() -> Arc<BaseView> {
        rooted("/view/layer", |region, memo| {
            let path = region.real.join("hostfile");
            memo.store_lookup(lookup_key(region, b"/", b"hostfile"), host_file(&path, 1));
            memo.store_list(list_key(region, b"/"), Ok(vec![(b"hostfile".to_vec(), DType::File)]));
        })
    }

    #[test]
    fn layer_visibility_by_level() {
        let mut layer = OutputState::default();
        layer.add(0, LEVEL_META, &[(b"/t/debug/a/liba.rmeta".to_vec(), file(1))]);
        layer.add(0, LEVEL_FULL, &[(b"/t/debug/a/liba.rlib".to_vec(), file(2))]);
        layer.add(1, LEVEL_FULL, &[(b"/t/debug/b/libb.rlib".to_vec(), file(3))]);

        layer.add(1, LEVEL_FULL, &[(b"/t/debug/a/liba.rlib".to_vec(), file(4))]);
        layer.set_rank(0, 10);
        let layer = Arc::new(OutputsLayer::frozen(layer));
        let view = |levels: Vec<u8>| BuildView {
            base: layer_base(),
            layer: layer.clone(),
            levels: Arc::new(levels),
            untraced: vec![],
            seed: None,
            content: None,
        };
        let v = view(vec![LEVEL_META, LEVEL_NONE]);
        assert!(matches!(v.stat(b"/t/debug/a/liba.rmeta"), Looked::File(_)));
        assert!(matches!(v.stat(b"/t/debug/a/liba.rlib"), Looked::Absent));
        assert!(matches!(v.stat(b"/t/debug/b"), Looked::Absent));
        assert!(matches!(v.stat(b"/t/debug"), Looked::Dir(_)));
        let mut names: Vec<_> = v.list(b"/t/debug/a").unwrap();
        names.sort();
        assert_eq!(names, vec![(b"liba.rmeta".to_vec(), DType::File)]);
        assert_eq!(v.list(b"/t/debug").unwrap(), vec![(b"a".to_vec(), DType::Dir)]);
        let v = view(vec![LEVEL_FULL, LEVEL_FULL]);
        assert!(matches!(v.stat(b"/t/debug/a/liba.rlib"), Looked::File(f) if f.size == 2));
        assert_eq!(v.hints(), [
            b"/t/debug/a/liba.rlib".to_vec(),
            b"/t/debug/a/liba.rmeta".to_vec(),
            b"/t/debug/b/libb.rlib".to_vec(),
        ]);
        assert_eq!(v.list(b"/t/debug").unwrap().len(), 2);
        let v = view(vec![LEVEL_NONE, LEVEL_NONE]);
        assert!(matches!(v.stat(b"/t"), Looked::Dir(_)));
        assert!(v.list(b"/t").unwrap().is_empty());
        assert!(matches!(v.stat(b"/hostfile"), Looked::File(_)));
        let mut root: Vec<_> = v.list(b"/").unwrap().into_iter().map(|e| e.0).collect();
        root.sort();
        assert_eq!(root, vec![b"hostfile".to_vec(), b"t".to_vec()]);
    }

    #[test]
    fn live_layer_steps_its_loop() {
        let layer = Arc::new(OutputsLayer::new());
        let view = |levels: Vec<u8>| BuildView {
            base: layer_base(),
            layer: layer.clone(),
            levels: Arc::new(levels),
            untraced: vec![],
            seed: None,
            content: None,
        };
        let full = view(vec![LEVEL_FULL, LEVEL_FULL]);
        assert!(matches!(full.stat(b"/t/debug/a/liba.rlib"), Looked::Absent));

        layer.add(0, LEVEL_META, &[(b"/t/debug/a/liba.rmeta".to_vec(), file(1))]);
        layer.add(0, LEVEL_FULL, &[(b"/t/debug/a/liba.rlib".to_vec(), file(2))]);
        layer.add(1, LEVEL_FULL, &[(b"/t/debug/a/liba.rlib".to_vec(), file(4))]);
        assert!(matches!(full.stat(b"/t/debug/a/liba.rlib"), Looked::File(f) if f.size == 4));
        layer.set_rank(0, 10);
        assert!(matches!(full.stat(b"/t/debug/a/liba.rlib"), Looked::File(f) if f.size == 2));

        let meta = view(vec![LEVEL_META, LEVEL_NONE]);
        assert!(matches!(meta.stat(b"/t/debug/a/liba.rlib"), Looked::Absent));
        assert_eq!(meta.list(b"/t/debug/a").unwrap(), vec![(b"liba.rmeta".to_vec(), DType::File)]);
        assert_eq!(full.hints(), [b"/t/debug/a/liba.rlib".to_vec(), b"/t/debug/a/liba.rmeta".to_vec()]);
    }

    #[test]
    fn live_layer_panic_reaches_the_caller_and_the_loop_goes_on() {
        let layer = OutputsLayer::new();
        let OutputStorage::Live(owner) = &layer.storage else { unreachable!() };
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.call(|_, ()| -> () { panic!("a step failed") }, ())
        }));
        let cause = failed.unwrap_err();
        assert_eq!(cause.downcast_ref::<&str>(), Some(&"a step failed"));
        layer.add(0, LEVEL_FULL, &[(b"/t/x".to_vec(), file(1))]);
        assert!(layer.get(b"/t", b"x", Levels::Fixed(&[LEVEL_FULL])).is_some());
    }

    #[test]
    fn explicit_directory_does_not_hide_an_earlier_stage() {
        let mut layer = OutputState::default();
        layer.add(0, LEVEL_META, &[(b"/t/debug/a/out/liba.rmeta".to_vec(), file(1))]);
        layer.add(
            0,
            LEVEL_FULL,
            &[
                (b"/t/debug".to_vec(), MEntry::Dir),
                (b"/t/debug/a".to_vec(), MEntry::Dir),
                (b"/t/debug/a/out".to_vec(), MEntry::Dir),
                (b"/t/debug/a/out/liba.rlib".to_vec(), file(2)),
            ],
        );
        let v = BuildView {
            base: rooted("/view/empty", |_, _| {}),
            layer: Arc::new(OutputsLayer::frozen(layer)),
            levels: Arc::new(vec![LEVEL_META]),
            untraced: vec![],
            seed: None,
            content: None,
        };
        assert!(matches!(v.stat(b"/t/debug/a/out"), Looked::Dir(_)));
        assert!(matches!(v.stat(b"/t/debug/a/out/liba.rmeta"), Looked::File(_)));
        assert!(matches!(v.stat(b"/t/debug/a/out/liba.rlib"), Looked::Absent));
        assert_eq!(v.list(b"/t/debug/a/out").unwrap(), vec![(b"liba.rmeta".to_vec(), DType::File)]);
    }

    #[test]
    fn symlinked_host_parent_is_not_a_literal_path() {
        let b = rooted("/view/link", |region, memo| {
            let file = region.real.join("real/f");
            memo.store_lookup(lookup_key(region, b"/", b"real"), host_dir());
            memo.store_lookup(lookup_key(region, b"/real", b"f"), host_file(&file, 1));
            memo.store_lookup(lookup_key(region, b"/", b"link"), Looked::Symlink(b"real".to_vec()));
        });
        assert!(matches!(b.stat(b"/real/f"), Looked::File(_)));
        assert!(matches!(b.stat(b"/link/f"), Looked::Absent));
        assert!(matches!(b.stat(b"/link"), Looked::Symlink(_)));
    }

    #[test]
    fn whiteout_hides_base_entry() {
        let base = rooted("/view/white", |region, memo| {
            let kept = region.real.join("kept");
            memo.store_lookup(lookup_key(region, b"/", b"kept"), host_file(&kept, 1));
            memo.store_list(
                list_key(region, b"/"),
                Ok(vec![(b"gone".to_vec(), DType::File), (b"kept".to_vec(), DType::File)]),
            );
        });
        let mut layer = OutputState::default();
        layer.add(0, LEVEL_FULL, &[(b"/gone".to_vec(), MEntry::Whiteout)]);
        let v = BuildView { base, layer: Arc::new(OutputsLayer::frozen(layer)), levels: Arc::new(vec![LEVEL_FULL]), untraced: vec![], seed: None, content: None };
        assert!(matches!(v.stat(b"/gone"), Looked::Absent));
        assert!(matches!(v.stat(b"/kept"), Looked::File(_)));
        let names: Vec<_> = v.list(b"/").unwrap().into_iter().map(|e| e.0).collect();
        assert!(!names.contains(&b"gone".to_vec()) && names.contains(&b"kept".to_vec()));
    }

    #[test]
    fn a_base_view_is_given_its_contents() {
        let mut outcomes = Transcript::default();
        let memo = Arc::new(HostMemo::default());
        let view = BaseView::new(
            vec![Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/no/such/host-view")), traced: true }],
            memo,
            0,
        );
        let Step::Ask(req) = view.step_lookup(b"/", b"missing") else {
            panic!("a miss asks the shell");
        };
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = View::lookup(&view, b"/", b"missing");
        }));
        assert!(refused.is_err(), "the view does not read the host");
        view.deposit(&req, HostAnswer::Lookup(host_dir()));
        view.invariants();
        let Step::Done(found) = view.step_lookup(b"/", b"missing") else {
            panic!("the deposit answers the lookup");
        };
        outcomes.record("retry lookup", found);
        view.invariants();
        expect![[r#"
            retry lookup: Dir(Owner { uid: 0, gid: 0, mode: 493 })
        "#]].assert_eq(&outcomes.0);
    }
}

#[cfg(test)]
mod host_listing_tests {
    use super::*;

    #[test]
    fn build_view_lists_host_directories() {
        let region = HostRegion::new("/view/host");
        let mut memo = MemoState::default();
        memo.store_lookup(lookup_key(&region, b"/", b"usr"), host_dir());
        memo.store_lookup(lookup_key(&region, b"/", b"etc"), host_dir());
        memo.store_lookup(lookup_key(&region, b"/usr", b"lib"), host_dir());
        memo.store_lookup(lookup_key(&region, b"/usr/lib", b"sample"), host_file(&region.real.join("usr/lib/sample"), 7));
        memo.store_lookup(lookup_key(&region, b"/etc", b"settings"), host_file(&region.real.join("etc/settings"), 6));
        memo.store_list(list_key(&region, b"/"), Ok(vec![(b"etc".to_vec(), DType::Dir), (b"usr".to_vec(), DType::Dir)]));
        memo.store_list(list_key(&region, b"/usr/lib"), Ok(vec![(b"sample".to_vec(), DType::File)]));
        memo.store_list(list_key(&region, b"/etc"), Ok(vec![(b"settings".to_vec(), DType::File)]));
        let memo = Arc::new(HostMemo::frozen(memo));
        let base = Arc::new(BaseView::new(
            vec![Mount { at: b"/".to_vec(), region: Region::Host(region), traced: true }],
            memo,
            0,
        ));
        let v = BuildView { base, layer: Arc::new(OutputsLayer::frozen(OutputState::default())), levels: Arc::new(vec![]), untraced: vec![], seed: None, content: None };
        assert_eq!(v.list(b"/usr/lib").unwrap(), vec![(b"sample".to_vec(), DType::File)]);
        assert_eq!(v.list(b"/etc").unwrap(), vec![(b"settings".to_vec(), DType::File)]);
        let mut root: Vec<_> = v.list(b"/").unwrap();
        root.sort();
        assert_eq!(root, vec![(b"etc".to_vec(), DType::Dir), (b"usr".to_vec(), DType::Dir)]);
        for dir in [b"/usr/lib".as_slice(), b"/etc".as_slice(), b"/".as_slice()] {
            assert!(matches!(v.stat(dir), Looked::Dir(_)));
        }
    }
}

#[cfg(test)]
mod mount_tests {
    use super::*;

    #[test]
    fn cargo_home_without_its_programs() {
        let home = PathBuf::from("/mem/cargo");
        let region = HostRegion::new(&home);
        let mounted = HostRegion::new("/mem");
        let mut memo = MemoState::default();
        memo.store_lookup(lookup_key(&mounted, b"/", b"cargo"), host_dir());
        memo.store_lookup(lookup_key(&region, b"/", b"registry"), host_dir());
        memo.store_lookup(lookup_key(&region, b"/registry", b"index"), host_dir());
        memo.store_lookup(lookup_key(&region, b"/", b"config.toml"), host_file(&home.join("config.toml"), 0));
        let memo = Arc::new(HostMemo::frozen(memo));
        let mut mounts = vec![Mount { at: b"/".to_vec(), region: Region::Empty, traced: true }];
        mounts.extend(cargo_home_mounts(&home));
        let v = BaseView::new(mounts, memo, 0);
        assert!(matches!(v.stat(b"/c/registry/index"), Looked::Dir(_)));
        assert!(matches!(v.stat(b"/c/config.toml"), Looked::File(_)));
        assert!(matches!(v.stat(b"/c/bin"), Looked::Dir(_)));
        assert!(v.list(b"/c/bin").unwrap().is_empty());
        assert!(matches!(v.stat(b"/c/bin/cargo-clippy"), Looked::Absent));
    }

    #[test]
    fn file_mounted_below_a_directory_that_does_not_exist() {
        let root = HostRegion::new("/mem/root");
        let tool_parent = HostRegion::new("/mem");
        let mut memo = MemoState::default();
        memo.store_lookup(lookup_key(&root, b"/", b"opt"), Looked::Absent);
        memo.store_lookup(lookup_key(&root, b"/opt", b"x"), Looked::Absent);
        memo.store_lookup(lookup_key(&root, b"/opt", b"y"), Looked::Absent);
        memo.store_lookup(lookup_key(&tool_parent, b"/", b"tool"), host_file(&PathBuf::from("/mem/tool"), 6));
        memo.store_list(list_key(&root, b"/"), Ok(Vec::new()));
        memo.store_list(list_key(&root, b"/opt"), Err(libc::ENOENT));
        memo.store_list(list_key(&root, b"/opt/x"), Err(libc::ENOENT));
        let memo = Arc::new(HostMemo::frozen(memo));
        let v = BaseView::new(
            vec![
                Mount { at: b"/".to_vec(), region: Region::Host(root), traced: true },
                Mount { at: b"/opt/x/tool".to_vec(), region: Region::Host(HostRegion::new("/mem/tool")), traced: true },
            ],
            memo,
            0,
        );
        assert!(matches!(v.stat(b"/opt"), Looked::Dir(_)));
        assert!(matches!(v.stat(b"/opt/x"), Looked::Dir(_)));
        assert!(matches!(v.stat(b"/opt/x/tool"), Looked::File(f) if f.size == 6));
        assert!(matches!(v.stat(b"/opt/y"), Looked::Absent));
        assert_eq!(v.list(b"/").unwrap(), vec![(b"opt".to_vec(), DType::Dir)]);
        assert_eq!(v.list(b"/opt").unwrap(), vec![(b"x".to_vec(), DType::Dir)]);
        assert_eq!(v.list(b"/opt/x").unwrap(), vec![(b"tool".to_vec(), DType::File)]);
    }
}

#[cfg(test)]
mod memo_tests {
    use super::*;
    use rargo_machine::check;

    fn step(memo: &mut MemoState, at: &str, f: impl FnOnce(&mut MemoState)) {
        f(memo);
        check(&*memo, at);
    }

    #[test]
    fn a_renewed_view_asks_again_and_others_keep_their_answers() {
        let mut memo = MemoState::default();
        let (mut old, mut new) = (0, 0);
        step(&mut memo, "open", |m| {
            old = m.open_view(1);
            new = m.open_view(1);
        });
        step(&mut memo, "observe", |m| m.observe(b"/k".to_vec(), Observed::Lookup(host_dir())));
        assert!(memo.lookup_at(memo.min_gen(old), b"/k").is_some());
        step(&mut memo, "renew", |m| m.renew(new));
        assert_eq!(memo.min_gen(new), 2);
        assert!(memo.lookup_at(memo.min_gen(new), b"/k").is_none(), "a renewed view does not accept what came before");
        assert!(memo.lookup_at(memo.min_gen(old), b"/k").is_some(), "the other view still does");
        step(&mut memo, "observe again", |m| m.observe(b"/k".to_vec(), Observed::Lookup(Looked::Absent)));
        assert!(matches!(memo.lookup_at(memo.min_gen(new), b"/k"), Some(Looked::Absent)));
        step(&mut memo, "close", |m| m.close_view(old));
        assert!(!memo.views.contains_key(&old));
    }

    #[test]
    fn every_span_of_generations_forgets_what_is_too_old() {
        let mut memo = MemoState::default();
        step(&mut memo, "observe", |m| m.observe(b"/old".to_vec(), Observed::List(Err(libc::ENOENT))));
        let advance_to = |memo: &mut MemoState, g: u64| {
            while memo.generation < g {
                step(memo, "advance", |m| {
                    m.advance();
                });
            }
        };
        advance_to(&mut memo, MEMO_SPAN);
        assert!(memo.listed.contains_key(b"/old".as_slice()), "an answer outlives its generation by a whole span");
        advance_to(&mut memo, MEMO_SPAN + 1);
        step(&mut memo, "observe late", |m| m.observe(b"/late".to_vec(), Observed::List(Ok(Arc::new(Vec::new())))));
        advance_to(&mut memo, 2 * MEMO_SPAN - 1);
        assert!(memo.listed.contains_key(b"/old".as_slice()), "pruned only when a span ends");
        advance_to(&mut memo, 2 * MEMO_SPAN);
        assert!(!memo.listed.contains_key(b"/old".as_slice()), "generation 1 is forgotten two spans on");
        assert!(memo.listed.contains_key(b"/late".as_slice()), "a recent listing is kept");
    }

    #[test]
    fn a_state_loop_checks_invariants_after_a_step_only_with_debug_assertions() {
        let owner = StateLoop::spawn("test", MemoState::default());
        let broke = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.tell(|m| m.generation = 0)));
        assert_eq!(broke.is_err(), cfg!(debug_assertions), "a release server does not scan its state after every step (#788)");
    }

    #[test]
    fn a_sealed_memo_refuses_observations() {
        let memo = HostMemo::frozen(MemoState::default());
        assert!(memo.sealed());
        let told = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            memo.observe(b"/k".to_vec(), Observed::Lookup(host_dir()))
        }));
        assert!(told.is_err());
    }

    #[test]
    fn a_live_memo_tracks_its_views_until_they_drop() {
        let memo = Arc::new(HostMemo::default());
        let g = memo.advance();
        let view = BaseView::new(
            vec![Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/no/such/memo-view")), traced: true }],
            memo.clone(),
            g,
        );
        let MemoView::Live(id) = view.accepts else { panic!("a live memo opens live views") };
        let MemoStorage::Live(owner) = &memo.storage else { unreachable!() };
        assert_eq!(owner.call(move |m, ()| m.min_gen(id), ()), g);
        view.renew();
        assert_eq!(owner.call(move |m, ()| m.min_gen(id), ()), g + 1);
        drop(view);
        assert!(!owner.call(move |m, ()| m.views.contains_key(&id), ()), "a dropped view is closed");
    }
}

/// The owner of a build view's live state on the loop thread. Each step runs
/// where it is asked for. With debug assertions (tests, the simulator) the
/// state's invariants are checked after it; a release server does not, since
/// they scan every lookup and output a build has made, after every FUSE
/// request, which made a cached build of a large workspace ten times slower
/// (#788).
struct StateLoop<S> {
    state: std::cell::RefCell<S>,
}

impl<S: StateMachine> StateLoop<S> {
    fn spawn(_name: &str, state: S) -> Self {
        Self { state: std::cell::RefCell::new(state) }
    }

    fn call<A, T>(&self, step: impl FnOnce(&mut S, A) -> T, args: A) -> T {
        let mut state = self.state.borrow_mut();
        let answer = step(&mut state, args);
        if cfg!(debug_assertions) {
            state.invariants();
        }
        answer
    }

    fn tell(&self, step: impl FnOnce(&mut S)) {
        self.call(|state, ()| step(state), ());
    }
}
