use crate::snapshot::Snapshot;
use crate::trace::{MEntry, Manifest};
use crate::view::*;
use dashmap::DashMap;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct HostMemo {
    looked: DashMap<Vec<u8>, (u64, Looked)>,
    listed: DashMap<Vec<u8>, (u64, Arc<Listing>)>,
    generation: AtomicU64,
}

impl Default for HostMemo {
    fn default() -> Self {
        HostMemo { looked: DashMap::new(), listed: DashMap::new(), generation: AtomicU64::new(1) }
    }
}

impl HostMemo {
    pub fn advance(&self) -> u64 {
        let g = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if g % 64 == 0 {
            self.looked.retain(|_, v| v.0 + 64 > g);
            self.listed.retain(|_, v| v.0 + 64 > g);
        }
        g
    }

    pub fn current(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
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
    NixStore { names: Arc<std::collections::HashSet<Vec<u8>>> },
    Synth(Arc<HashMap<Vec<u8>, SynthEnt>>),
    Link(Vec<u8>),
}

pub struct Mount {
    pub at: Vec<u8>,
    pub region: Region,
    pub traced: bool,
}

pub struct BaseView {
    mounts: Vec<Mount>,
    memo: Arc<HostMemo>,
    min_gen: AtomicU64,
    nix: Arc<Vec<(Vec<u8>, crate::hash::Hash)>>,
}

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
        BaseView { mounts, memo, min_gen: AtomicU64::new(min_gen), nix: Arc::new(Vec::new()) }
    }

    pub fn with_nix(mut self, nix: Arc<Vec<(Vec<u8>, crate::hash::Hash)>>) -> BaseView {
        self.nix = nix;
        self
    }

    pub fn renew(&self) {
        self.min_gen.store(self.memo.advance(), Ordering::SeqCst);
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

    fn host_lookup(&self, region: &HostRegion, rel_dir: &[u8], name: &[u8]) -> Looked {
        let mut key = region.real.as_os_str().as_encoded_bytes().to_vec();
        if rel_dir != b"/" {
            key.extend_from_slice(rel_dir);
        }
        key.push(b'/');
        key.extend_from_slice(name);
        if let Some(e) = self.memo.looked.get(&key) {
            if e.0 >= self.min_gen.load(Ordering::Relaxed) {
                return e.1.clone();
            }
        }
        let l = region.lookup(rel_dir, name);
        self.memo.looked.insert(key, (self.memo.current(), l.clone()));
        l
    }

    fn host_list(&self, region: &HostRegion, rel_dir: &[u8]) -> io::Result<Arc<Listing>> {
        let mut key = region.real.as_os_str().as_encoded_bytes().to_vec();
        if rel_dir != b"/" {
            key.extend_from_slice(rel_dir);
        }
        if let Some(e) = self.memo.listed.get(&key) {
            if e.0 >= self.min_gen.load(Ordering::Relaxed) {
                return Ok(e.1.clone());
            }
        }
        let l = Arc::new(region.list(rel_dir)?);
        self.memo.listed.insert(key, (self.memo.current(), l.clone()));
        Ok(l)
    }

    fn is_real_dir(&self, dir: &[u8]) -> bool {
        match vsplit(dir) {
            None => true,
            Some((parent, name)) => {
                matches!(self.lookup_unchecked(parent, name), Looked::Dir(_)) && self.is_real_dir(parent)
            }
        }
    }

    fn lookup_unchecked(&self, dir: &[u8], name: &[u8]) -> Looked {
        let path = vjoin(dir, name);
        if let Some(m) = self.mount_at(&path) {
            return match &m.region {
                Region::Link(target) => Looked::Symlink(target.clone()),
                Region::Empty | Region::Snap(_) | Region::NixStore { .. } | Region::Synth(_) => {
                    Looked::Dir(DIR_OWNER)
                }
                Region::Host(h) => {
                    let real = &h.real;
                    match (real.parent(), real.file_name()) {
                        (Some(p), Some(n)) => {
                            HostRegion::new(p).lookup(b"/", n.as_encoded_bytes())
                        }
                        _ => Looked::Dir(DIR_OWNER),
                    }
                }
            };
        }
        let (m, rel) = self.mount_of(dir);
        let looked = match &m.region {
            Region::Empty | Region::Link(_) => Looked::Absent,
            Region::Snap(s) => s.lookup(rel, name),
            Region::Host(h) => self.host_lookup(h, rel, name),
            Region::NixStore { names } => {
                if rel == b"/" && !names.contains(name) {
                    Looked::Absent
                } else {
                    self.host_lookup(&nix_store_region(), rel, name)
                }
            }
            Region::Synth(entries) => match (rel, entries.get(name)) {
                (b"/", Some(SynthEnt::File { hash, size })) => Looked::File(FileInfo {
                    size: *size,
                    owner: Owner { uid: 0, gid: 0, mode: 0o644 },
                    src: FileSrc::Blob(*hash),
                    sem: None,
                }),
                (b"/", Some(SynthEnt::Symlink(t))) => Looked::Symlink(t.clone()),
                _ => Looked::Absent,
            },
        };
        if matches!(looked, Looked::Absent) && self.mounts.iter().any(|o| is_strictly_under(&o.at, &path)) {
            return Looked::Dir(DIR_OWNER);
        }
        looked
    }
}

fn nix_store_region() -> HostRegion {
    HostRegion::new("/nix/store")
}

fn is_strictly_under(path: &[u8], dir: &[u8]) -> bool {
    path.len() > dir.len() && vpath_starts_with(path, dir)
}

impl View for BaseView {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
        self.lookup_unchecked(dir, name)
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        let (m, rel) = self.mount_of(dir);
        let listed: io::Result<Listing> = match &m.region {
            Region::Empty | Region::Link(_) => Ok(Vec::new()),
            Region::Snap(s) => s.list(rel),
            Region::Host(h) => self.host_list(h, rel).map(|l| l.as_ref().clone()),
            Region::NixStore { names } if rel == b"/" => {
                Ok(names.iter().map(|n| (n.clone(), DType::Dir)).collect())
            }
            Region::NixStore { .. } => self.host_list(&nix_store_region(), rel).map(|l| l.as_ref().clone()),
            Region::Synth(entries) => Ok(entries
                .iter()
                .map(|(n, e)| {
                    (n.clone(), if matches!(e, SynthEnt::File { .. }) { DType::File } else { DType::Symlink })
                })
                .collect()),
        };
        let mut out = match listed {
            Ok(l) => l,
            Err(_) if self.mounts.iter().any(|o| is_strictly_under(&o.at, dir)) => Vec::new(),
            Err(e) => return Err(e),
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
            let t = match self.lookup_unchecked(dir, name) {
                Looked::File(_) => DType::File,
                Looked::Symlink(_) => DType::Symlink,
                _ => DType::Dir,
            };
            out.push((name.to_vec(), t));
        }
        debug_assert!(out.iter().enumerate().all(|(i, (n, _))| out[i + 1..].iter().all(|(m, _)| m != n)), "listing names are unique");
        Ok(out)
    }

    fn traced(&self, dir: &[u8]) -> bool {
        self.mount_of(dir).0.traced
    }

    fn nix_closure(&self) -> &[(Vec<u8>, crate::hash::Hash)] {
        &self.nix
    }

    fn stat(&self, path: &[u8]) -> Looked {
        match vsplit(path) {
            None => Looked::Dir(DIR_OWNER),
            Some((dir, name)) => {
                let (m, _) = self.mount_of(dir);
                if matches!(m.region, Region::Host(_) | Region::NixStore { .. }) && !self.is_real_dir(dir) {
                    return Looked::Absent;
                }
                self.lookup_unchecked(dir, name)
            }
        }
    }
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
    children: HashMap<Vec<u8>, Vec<(u32, u8, LayerEnt)>>,
}

#[derive(Default)]
pub struct OutputsLayer {
    dirs: DashMap<Vec<u8>, LayerDir>,
    ranks: DashMap<u32, u64>,
}

impl OutputsLayer {
    pub fn new() -> OutputsLayer {
        OutputsLayer::default()
    }

    pub fn add(&self, unit: u32, stage: u8, entries: &[(Vec<u8>, MEntry)]) {
        for (path, e) in entries {
            let Some((parent, name)) = vsplit(path) else { continue };
            {
                let mut d = self.dirs.entry(parent.to_vec()).or_default();
                let list = d.children.entry(name.to_vec()).or_default();
                list.retain(|(u, s, ent)| !(*u == unit && *s >= stage && matches!(ent, LayerEnt::Implied)));
                if !list.iter().any(|(u, s, _)| *u == unit && *s == stage) {
                    list.push((unit, stage, LayerEnt::Entry(e.clone())));
                }
                assert_eq!(list.iter().filter(|(u, s, _)| *u == unit && *s == stage).count(), 1);
            }
            let mut cur = parent;
            while let Some((pp, pn)) = vsplit(cur) {
                let mut d = self.dirs.entry(pp.to_vec()).or_default();
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

    pub fn add_manifest(&self, unit: u32, stage: u8, m: &Manifest) {
        self.add(unit, stage, &m.entries);
    }

    pub fn set_rank(&self, unit: u32, rank: u64) {
        self.ranks.insert(unit, rank);
    }

    fn rank(&self, unit: u32) -> u64 {
        self.ranks.get(&unit).map(|r| *r).unwrap_or(unit as u64)
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
        for d in self.dirs.iter() {
            for (name, list) in &d.value().children {
                if let Some(LayerEnt::Entry(MEntry::File { .. } | MEntry::Symlink(_))) = self.pick(list, levels) {
                    out.push(vjoin(d.key(), name));
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
            if let Some(e) = seed.get(dir, name, &SEED_LEVELS) {
                return Some(e);
            }
        }
        self.layer.get(dir, name, &self.levels)
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
                match self.base.lookup(dir, name) {
                    Looked::Dir(o) => Looked::Dir(o),
                    _ => Looked::Dir(OWN_DIR),
                }
            }
            None => self.base.lookup(dir, name),
        }
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        let mut layered = self.layer.names(dir, &self.levels);
        if let Some(seed) = &self.seed {
            layered.extend(seed.names(dir, &SEED_LEVELS));
        }
        let mut map: HashMap<Vec<u8>, DType> = match self.base_has_dir(dir) {
            true => self.base.list(dir)?.into_iter().collect(),
            false => HashMap::new(),
        };
        for (name, e) in layered {
            match e {
                LayerEnt::Entry(MEntry::Whiteout) => {
                    map.remove(&name);
                }
                LayerEnt::Entry(MEntry::File { .. }) => {
                    map.insert(name, DType::File);
                }
                LayerEnt::Entry(MEntry::Symlink(_)) => {
                    map.insert(name, DType::Symlink);
                }
                LayerEnt::Entry(MEntry::Dir) | LayerEnt::Implied => {
                    map.insert(name, DType::Dir);
                }
            }
        }
        Ok(map.into_iter().collect())
    }

    fn traced(&self, dir: &[u8]) -> bool {
        if self.untraced.iter().any(|u| vpath_starts_with(dir, u)) {
            return false;
        }
        self.base.traced(dir)
    }

    fn hints(&self) -> Vec<Vec<u8>> {
        self.layer.visible_paths(&self.levels)
    }

    fn nix_closure(&self) -> &[(Vec<u8>, crate::hash::Hash)] {
        self.base.nix_closure()
    }

    fn stat(&self, path: &[u8]) -> Looked {
        let Some((dir, name)) = vsplit(path) else { return Looked::Dir(DIR_OWNER) };
        match self.layered(dir, name) {
            Some(_) => self.lookup(dir, name),
            None => {
                match self.base.stat(path) {
                    Looked::Absent if self.layer_has_dir(dir) => self.base.lookup(dir, name),
                    other => other,
                }
            }
        }
    }
}

impl BuildView {
    fn base_has_dir(&self, dir: &[u8]) -> bool {
        matches!(self.base.stat(dir), Looked::Dir(_))
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
    pub store_names: Arc<std::collections::HashSet<Vec<u8>>>,
    pub store_paths: Arc<Vec<(Vec<u8>, crate::hash::Hash)>>,
    pub etc: Arc<HashMap<Vec<u8>, SynthEnt>>,
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
                    at: b"/nix/store".to_vec(),
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
mod tests {
    use super::*;

    fn file(n: u8) -> MEntry {
        MEntry::File { hash: crate::hash::Hash([n; 32]), exec: false, size: n as u64 }
    }

    fn base(dir: &std::path::Path) -> Arc<BaseView> {
        let memo = Arc::new(HostMemo::default());
        Arc::new(BaseView::new(
            vec![
                Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new(dir)), traced: true },
                Mount { at: b"/t".to_vec(), region: Region::Empty, traced: true },
            ],
            memo,
            0,
        ))
    }

    #[test]
    fn layer_visibility_by_level() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hostfile"), "x").unwrap();
        let layer = Arc::new(OutputsLayer::new());
        layer.add(0, LEVEL_META, &[(b"/t/debug/a/liba.rmeta".to_vec(), file(1))]);
        layer.add(0, LEVEL_FULL, &[(b"/t/debug/a/liba.rlib".to_vec(), file(2))]);
        layer.add(1, LEVEL_FULL, &[(b"/t/debug/b/libb.rlib".to_vec(), file(3))]);

        let view = |levels: Vec<u8>| BuildView {
            base: base(tmp.path()),
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
        assert!(matches!(v.stat(b"/t/debug/a/liba.rlib"), Looked::File(_)));
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
    fn explicit_directory_does_not_hide_an_earlier_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let layer = Arc::new(OutputsLayer::new());
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
        let v = BuildView { base: base(tmp.path()), layer, levels: Arc::new(vec![LEVEL_META]), untraced: vec![], seed: None, content: None };
        assert!(matches!(v.stat(b"/t/debug/a/out"), Looked::Dir(_)));
        assert!(matches!(v.stat(b"/t/debug/a/out/liba.rmeta"), Looked::File(_)));
        assert!(matches!(v.stat(b"/t/debug/a/out/liba.rlib"), Looked::Absent));
        assert_eq!(v.list(b"/t/debug/a/out").unwrap(), vec![(b"liba.rmeta".to_vec(), DType::File)]);
    }

    #[test]
    fn symlinked_host_parent_is_not_a_literal_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        std::fs::write(tmp.path().join("real/f"), "x").unwrap();
        std::os::unix::fs::symlink("real", tmp.path().join("link")).unwrap();
        let b = base(tmp.path());
        assert!(matches!(b.stat(b"/real/f"), Looked::File(_)));
        assert!(matches!(b.stat(b"/link/f"), Looked::Absent));
        assert!(matches!(b.stat(b"/link"), Looked::Symlink(_)));
    }

    #[test]
    fn whiteout_hides_base_entry() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("gone"), "x").unwrap();
        std::fs::write(tmp.path().join("kept"), "x").unwrap();
        let layer = Arc::new(OutputsLayer::new());
        layer.add(0, LEVEL_FULL, &[(b"/gone".to_vec(), MEntry::Whiteout)]);
        let v = BuildView { base: base(tmp.path()), layer, levels: Arc::new(vec![LEVEL_FULL]), untraced: vec![], seed: None, content: None };
        assert!(matches!(v.stat(b"/gone"), Looked::Absent));
        assert!(matches!(v.stat(b"/kept"), Looked::File(_)));
        let names: Vec<_> = v.list(b"/").unwrap().into_iter().map(|e| e.0).collect();
        assert!(!names.contains(&b"gone".to_vec()) && names.contains(&b"kept".to_vec()));
    }
}

#[cfg(test)]
mod host_listing_tests {
    use super::*;

    #[test]
    fn build_view_lists_host_directories() {
        let host = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host.path().join("usr/lib")).unwrap();
        std::fs::create_dir(host.path().join("etc")).unwrap();
        std::fs::write(host.path().join("usr/lib/sample"), b"library").unwrap();
        std::fs::write(host.path().join("etc/settings"), b"config").unwrap();
        let memo = Arc::new(HostMemo::default());
        let g = memo.advance();
        let base = Arc::new(BaseView::new(
            vec![Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new(host.path())), traced: true }],
            memo,
            g,
        ));
        let v = BuildView { base, layer: Arc::new(OutputsLayer::new()), levels: Arc::new(vec![]), untraced: vec![], seed: None, content: None };
        for dir in ["/usr/lib", "/etc", "/"] {
            let real = std::fs::read_dir(host.path().join(dir.trim_start_matches('/'))).unwrap().count();
            let seen = v.list(dir.as_bytes()).unwrap().len();
            assert_eq!(seen, real, "listing of {dir}");
            assert!(matches!(v.stat(dir.as_bytes()), Looked::Dir(_)));
        }
    }
}

#[cfg(test)]
mod mount_tests {
    use super::*;

    #[test]
    fn cargo_home_without_its_programs() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("registry/index")).unwrap();
        std::fs::create_dir(home.path().join("bin")).unwrap();
        std::fs::write(home.path().join("bin/cargo-clippy"), "rustup").unwrap();
        std::fs::write(home.path().join("config.toml"), "").unwrap();
        let mut mounts = vec![Mount { at: b"/".to_vec(), region: Region::Empty, traced: true }];
        mounts.extend(cargo_home_mounts(home.path()));
        let v = BaseView::new(mounts, Arc::new(HostMemo::default()), 0);
        assert!(matches!(v.stat(b"/c/registry/index"), Looked::Dir(_)));
        assert!(matches!(v.stat(b"/c/config.toml"), Looked::File(_)));
        assert!(matches!(v.stat(b"/c/bin"), Looked::Dir(_)));
        assert!(v.list(b"/c/bin").unwrap().is_empty());
        assert!(matches!(v.stat(b"/c/bin/cargo-clippy"), Looked::Absent));
    }

    #[test]
    fn file_mounted_below_a_directory_that_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("root")).unwrap();
        std::fs::write(tmp.path().join("tool"), "binary").unwrap();
        let memo = Arc::new(HostMemo::default());
        let v = BaseView::new(
            vec![
                Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new(tmp.path().join("root"))), traced: true },
                Mount { at: b"/opt/x/tool".to_vec(), region: Region::Host(HostRegion::new(tmp.path().join("tool"))), traced: true },
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
