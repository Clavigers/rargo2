//! The composite view an action sees.
//!
//! [`BaseView`] maps sandbox paths onto regions: the host root, the worktree
//! snapshot at `/w`, cargo home at its canonical path, and masks. On top of
//! it, [`BuildView`] layers the outputs of the units an action depends on
//! ([`OutputsLayer`]), each visible at the level that unit is needed
//! (metadata only, or everything).

use crate::snapshot::Snapshot;
use crate::trace::{MEntry, Manifest};
use crate::view::*;
use dashmap::DashMap;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Remembers host lookups for the duration of a request generation, so that
/// checking thousands of traces stats each host path once.
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
    /// Starts a new generation. Lookups made by views created afterwards do
    /// not trust anything remembered before this call.
    pub fn advance(&self) -> u64 {
        let g = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // Bound memory: drop entries more than a few generations old.
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

/// One entry of a synthetic directory.
#[derive(Clone, Debug)]
pub enum SynthEnt {
    File { hash: crate::hash::Hash, size: u64 },
    Symlink(Vec<u8>),
}

pub enum Region {
    Host(HostRegion),
    Snap(Arc<Snapshot>),
    /// An empty directory.
    Empty,
    /// `/nix/store`, restricted to the store paths of one closure. Store
    /// paths never change once they exist, so nothing below is traced; the
    /// closure's identity is part of the action key instead.
    NixStore { names: Arc<std::collections::HashSet<Vec<u8>>> },
    /// A flat directory of fixed files and symlinks (a minimal `/etc`).
    Synth(Arc<HashMap<Vec<u8>, SynthEnt>>),
    /// The mount point itself is a symlink.
    Link(Vec<u8>),
}

pub struct Mount {
    pub at: Vec<u8>,
    pub region: Region,
    /// Lookups below this mount are recorded in traces.
    pub traced: bool,
}

/// The mount table. Immutable once built.
pub struct BaseView {
    mounts: Vec<Mount>,
    memo: Arc<HostMemo>,
    /// Host lookups remembered at or after this generation are trusted.
    min_gen: u64,
}

const DIR_OWNER: Owner = Owner { uid: 0, gid: 0, mode: 0o755 };

impl BaseView {
    /// `mounts` may be given in any order; one of them must be at `/`.
    pub fn new(mut mounts: Vec<Mount>, memo: Arc<HostMemo>, min_gen: u64) -> BaseView {
        // Longest prefix first.
        mounts.sort_by(|a, b| b.at.len().cmp(&a.at.len()));
        assert!(mounts.iter().any(|m| m.at == b"/"), "a root mount is required");
        BaseView { mounts, memo, min_gen }
    }

    /// The mount containing directory `dir`, and `dir` relative to it.
    fn mount_of<'a>(&'a self, dir: &'a [u8]) -> (&'a Mount, &'a [u8]) {
        for m in &self.mounts {
            if vpath_starts_with(dir, &m.at) {
                let rel = if m.at == b"/" { dir } else { &dir[m.at.len()..] };
                return (m, if rel.is_empty() { b"/" } else { rel });
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
            if e.0 >= self.min_gen {
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
            if e.0 >= self.min_gen {
                return Ok(e.1.clone());
            }
        }
        let l = Arc::new(region.list(rel_dir)?);
        self.memo.listed.insert(key, (self.memo.current(), l.clone()));
        Ok(l)
    }

    /// Whether `dir` is a real directory all the way down (no symlinked
    /// component). The sandbox walks paths component by component and would
    /// have followed a symlink elsewhere; a recorded path is only valid if
    /// it can still be walked literally.
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
                    // The mount root itself may be a file (a worktree's
                    // `.git`), so look at what is really there.
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
                // Only the closure's own store paths exist at the top.
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
                }),
                (b"/", Some(SynthEnt::Symlink(t))) => Looked::Symlink(t.clone()),
                _ => Looked::Absent,
            },
        };
        // A directory that exists only because something is mounted below.
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
            // A directory that exists only because of a mount below it.
            Err(_) if self.mounts.iter().any(|o| is_strictly_under(&o.at, dir)) => Vec::new(),
            Err(e) => return Err(e),
        };
        // Mount points below this directory exist too, as do the
        // directories leading to them.
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
        Ok(out)
    }

    fn traced(&self, dir: &[u8]) -> bool {
        self.mount_of(dir).0.traced
    }

    fn stat(&self, path: &[u8]) -> Looked {
        match vsplit(path) {
            None => Looked::Dir(DIR_OWNER),
            Some((dir, name)) => {
                // Only host regions can have symlinked parents that the
                // literal path hides; snapshots and masks resolve literally.
                let (m, _) = self.mount_of(dir);
                if matches!(m.region, Region::Host(_) | Region::NixStore { .. }) && !self.is_real_dir(dir) {
                    return Looked::Absent;
                }
                self.lookup_unchecked(dir, name)
            }
        }
    }
}

/// How much of a unit's output an action may see.
pub const LEVEL_NONE: u8 = 0;
/// Only what the unit announced early (its `.rmeta`).
pub const LEVEL_META: u8 = 1;
pub const LEVEL_FULL: u8 = 2;

#[derive(Clone)]
enum LayerEnt {
    Entry(MEntry),
    /// A directory that exists because something below it was written.
    Implied,
}

#[derive(Default)]
struct LayerDir {
    /// name -> providers `(unit, stage, entry)`
    children: HashMap<Vec<u8>, Vec<(u32, u8, LayerEnt)>>,
}

/// Outputs of the units of one build, indexed by directory. Grows as units
/// finish; an action only ever sees units that finished before it started.
#[derive(Default)]
pub struct OutputsLayer {
    dirs: DashMap<Vec<u8>, LayerDir>,
}

impl OutputsLayer {
    pub fn new() -> OutputsLayer {
        OutputsLayer::default()
    }

    /// Adds entries produced by `unit`, visible from `stage` on.
    pub fn add(&self, unit: u32, stage: u8, entries: &[(Vec<u8>, MEntry)]) {
        for (path, e) in entries {
            let Some((parent, name)) = vsplit(path) else { continue };
            {
                let mut d = self.dirs.entry(parent.to_vec()).or_default();
                let list = d.children.entry(name.to_vec()).or_default();
                // An implied directory of this unit at the same or a later
                // stage is superseded. One from an earlier stage must stay:
                // viewers at that level still need the directory to exist.
                list.retain(|(u, s, ent)| !(*u == unit && *s >= stage && matches!(ent, LayerEnt::Implied)));
                if !list.iter().any(|(u, s, _)| *u == unit && *s == stage) {
                    list.push((unit, stage, LayerEnt::Entry(e.clone())));
                }
            }
            // Every ancestor directory exists by implication.
            let mut cur = parent;
            while let Some((pp, pn)) = vsplit(cur) {
                let mut d = self.dirs.entry(pp.to_vec()).or_default();
                let list = d.children.entry(pn.to_vec()).or_default();
                if list.iter().any(|(u, s, _)| *u == unit && *s <= stage) {
                    break;
                }
                list.push((unit, stage, LayerEnt::Implied));
                cur = pp;
            }
        }
    }

    pub fn add_manifest(&self, unit: u32, stage: u8, m: &Manifest) {
        self.add(unit, stage, &m.entries);
    }

    fn pick(list: &[(u32, u8, LayerEnt)], levels: &[u8]) -> Option<LayerEnt> {
        let mut best: Option<(u32, &LayerEnt)> = None;
        for (u, stage, ent) in list {
            if levels.get(*u as usize).copied().unwrap_or(LEVEL_NONE) < *stage {
                continue;
            }
            let better = match (&best, ent) {
                (None, _) => true,
                // A real entry beats an implied directory; among real
                // entries the higher unit index wins, deterministically.
                (Some((_, LayerEnt::Implied)), LayerEnt::Entry(_)) => true,
                (Some((_, LayerEnt::Entry(_))), LayerEnt::Implied) => false,
                (Some((bu, _)), _) => *u > *bu,
            };
            if better {
                best = Some((*u, ent));
            }
        }
        best.map(|(_, e)| e.clone())
    }

    fn get(&self, dir: &[u8], name: &[u8], levels: &[u8]) -> Option<LayerEnt> {
        let d = self.dirs.get(dir)?;
        Self::pick(d.children.get(name)?, levels)
    }

    fn names(&self, dir: &[u8], levels: &[u8]) -> Vec<(Vec<u8>, LayerEnt)> {
        match self.dirs.get(dir) {
            None => Vec::new(),
            Some(d) => d
                .children
                .iter()
                .filter_map(|(name, list)| Self::pick(list, levels).map(|e| (name.clone(), e)))
                .collect(),
        }
    }
}

const OWN_FILE: Owner = Owner::own(0o644);
const OWN_EXEC: Owner = Owner::own(0o755);
const OWN_DIR: Owner = Owner::own(0o755);

/// A base view with the outputs of other units layered on top.
pub struct BuildView {
    pub base: Arc<BaseView>,
    pub layer: Arc<OutputsLayer>,
    /// Visibility level per unit index.
    pub levels: Arc<Vec<u8>>,
    /// Directories whose content is not traced (incremental state).
    pub untraced: Vec<Vec<u8>>,
    /// A private extra layer for this one action (its incremental seed).
    pub seed: Option<Arc<OutputsLayer>>,
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
            }),
            Some(LayerEnt::Entry(MEntry::Symlink(t))) => Looked::Symlink(t),
            Some(LayerEnt::Entry(MEntry::Whiteout)) => Looked::Absent,
            Some(LayerEnt::Entry(MEntry::Dir)) | Some(LayerEnt::Implied) => {
                // A directory in the layer merges with the base directory of
                // the same name, if there is one.
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
        // The directory may exist only in the layer.
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

    fn stat(&self, path: &[u8]) -> Looked {
        let Some((dir, name)) = vsplit(path) else { return Looked::Dir(DIR_OWNER) };
        match self.layered(dir, name) {
            Some(_) => self.lookup(dir, name),
            None => {
                // The base checks that host parents are real directories;
                // a parent that exists only in the layer is real by
                // construction.
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

/// Convenience for building the standard mount table.
#[derive(Clone)]
pub struct ViewParts {
    pub snapshot: Arc<Snapshot>,
    /// Real path of the worktree's `.git` (file or directory).
    pub git_path: PathBuf,
    pub cargo_home: PathBuf,
    /// Sandbox paths shown as empty directories (the target directory).
    pub masks: Vec<Vec<u8>>,
    /// Real host paths hidden from the sandbox.
    pub hidden: Vec<Vec<u8>>,
    /// The forked cargo, shown at [`PLANNER_AT`].
    pub planner: PathBuf,
    /// Restrict the root to a Nix closure instead of showing the host.
    pub restricted: Option<Restricted>,
}

/// What replaces the host root in a flake-restricted sandbox.
#[derive(Clone)]
pub struct Restricted {
    pub store_names: Arc<std::collections::HashSet<Vec<u8>>>,
    pub etc: Arc<HashMap<Vec<u8>, SynthEnt>>,
    /// Absolute symlinks to provide (`/bin/sh`, `/usr/bin/env`).
    pub links: Vec<(Vec<u8>, Vec<u8>)>,
}

pub const WORKTREE_AT: &[u8] = b"/w";
pub const CARGO_HOME_AT: &[u8] = b"/c";
/// Where the forked cargo appears. Cargo puts its own path into every
/// process's `CARGO` variable, so it has to be the same everywhere.
pub const PLANNER_AT: &[u8] = b"/rargo/cargo";

impl ViewParts {
    /// Builds the mount table. With `host_root`, the host is the root even
    /// if a restricted root is configured (the planner needs host libraries).
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
        mounts.extend([
            Mount { at: WORKTREE_AT.to_vec(), region: Region::Snap(this.snapshot.clone()), traced: true },
            Mount {
                at: vjoin(WORKTREE_AT, b".git"),
                region: Region::Host(HostRegion::new(this.git_path.clone())),
                traced: true,
            },
            Mount {
                at: CARGO_HOME_AT.to_vec(),
                region: Region::Host(HostRegion::new(this.cargo_home.clone())),
                traced: true,
            },
            Mount { at: PLANNER_AT.to_vec(), region: Region::Host(HostRegion::new(this.planner.clone())), traced: true },
        ]);
        for m in this.masks.iter().chain(this.hidden.iter()) {
            // Hidden host paths only matter when the host is visible.
            mounts.push(Mount { at: m.clone(), region: Region::Empty, traced: true });
        }
        BaseView::new(mounts, memo, min_gen)
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
        // Unit 0 announces an rmeta early (stage 1) and an rlib at the end.
        layer.add(0, LEVEL_META, &[(b"/t/debug/a/liba.rmeta".to_vec(), file(1))]);
        layer.add(0, LEVEL_FULL, &[(b"/t/debug/a/liba.rlib".to_vec(), file(2))]);
        layer.add(1, LEVEL_FULL, &[(b"/t/debug/b/libb.rlib".to_vec(), file(3))]);

        let view = |levels: Vec<u8>| BuildView {
            base: base(tmp.path()),
            layer: layer.clone(),
            levels: Arc::new(levels),
            untraced: vec![],
            seed: None,
        };
        // Sees only unit 0's metadata.
        let v = view(vec![LEVEL_META, LEVEL_NONE]);
        assert!(matches!(v.stat(b"/t/debug/a/liba.rmeta"), Looked::File(_)));
        assert!(matches!(v.stat(b"/t/debug/a/liba.rlib"), Looked::Absent));
        assert!(matches!(v.stat(b"/t/debug/b"), Looked::Absent));
        assert!(matches!(v.stat(b"/t/debug"), Looked::Dir(_)));
        let mut names: Vec<_> = v.list(b"/t/debug/a").unwrap();
        names.sort();
        assert_eq!(names, vec![(b"liba.rmeta".to_vec(), DType::File)]);
        assert_eq!(v.list(b"/t/debug").unwrap(), vec![(b"a".to_vec(), DType::Dir)]);
        // Sees everything.
        let v = view(vec![LEVEL_FULL, LEVEL_FULL]);
        assert!(matches!(v.stat(b"/t/debug/a/liba.rlib"), Looked::File(_)));
        assert_eq!(v.list(b"/t/debug").unwrap().len(), 2);
        // Sees nothing: the mask is an empty directory.
        let v = view(vec![LEVEL_NONE, LEVEL_NONE]);
        assert!(matches!(v.stat(b"/t"), Looked::Dir(_)));
        assert!(v.list(b"/t").unwrap().is_empty());
        assert!(matches!(v.stat(b"/hostfile"), Looked::File(_)));
        // The root listing shows the mask next to host entries.
        let mut root: Vec<_> = v.list(b"/").unwrap().into_iter().map(|e| e.0).collect();
        root.sort();
        assert_eq!(root, vec![b"hostfile".to_vec(), b"t".to_vec()]);
    }

    #[test]
    fn explicit_directory_does_not_hide_an_earlier_stage() {
        // A pipelined unit announces its rmeta early; its final manifest also
        // lists the output directory itself (it was created for the unit).
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
        let v = BuildView { base: base(tmp.path()), layer, levels: Arc::new(vec![LEVEL_META]), untraced: vec![], seed: None };
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
        // The file is reachable through the symlink on the host, but an
        // action would have observed the symlink, not this literal path.
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
        let v = BuildView { base: base(tmp.path()), layer, levels: Arc::new(vec![LEVEL_FULL]), untraced: vec![], seed: None };
        assert!(matches!(v.stat(b"/gone"), Looked::Absent));
        assert!(matches!(v.stat(b"/kept"), Looked::File(_)));
        let names: Vec<_> = v.list(b"/").unwrap().into_iter().map(|e| e.0).collect();
        assert!(!names.contains(&b"gone".to_vec()) && names.contains(&b"kept".to_vec()));
    }
}

#[cfg(test)]
mod host_listing_tests {
    use super::*;

    /// A build view over the real root lists a real directory completely.
    #[test]
    fn build_view_lists_host_directories() {
        let memo = Arc::new(HostMemo::default());
        let g = memo.advance();
        let base = Arc::new(BaseView::new(
            vec![Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/")), traced: true }],
            memo,
            g,
        ));
        let v = BuildView { base, layer: Arc::new(OutputsLayer::new()), levels: Arc::new(vec![]), untraced: vec![], seed: None };
        for dir in ["/usr/lib", "/etc", "/"] {
            let real = std::fs::read_dir(dir).unwrap().count();
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
