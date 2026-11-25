use crate::scan::hash_file;
use rargo_proto::{Delivery, Digest, Write};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

fn hex(d: &Digest) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

type Record = (u64, i64, u64, Digest);

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Placed {
    files: BTreeMap<String, Record>,
}

impl Placed {
    pub fn load(path: &Path) -> Placed {
        std::fs::read(path).ok().and_then(|b| bincode::deserialize(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::write_atomic(path, &bincode::serialize(self).map_err(io::Error::other)?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stat {
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub links: u64,
    pub mtime: i64,
    pub ino: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Look {
    Stat(PathBuf),
    Hash(PathBuf, u64),
    Read(PathBuf),
    List(PathBuf),
    Process(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Sight {
    Stat(Option<Stat>),
    Hash(Option<Digest>),
    Read(Option<Vec<u8>>),
    List(Vec<String>),
    Process(bool),
}

impl Look {
    fn answered_by(&self, sight: &Sight) -> bool {
        matches!(
            (self, sight),
            (Look::Stat(_), Sight::Stat(_))
                | (Look::Hash(..), Sight::Hash(_))
                | (Look::Read(_), Sight::Read(_))
                | (Look::List(_), Sight::List(_))
                | (Look::Process(_), Sight::Process(_))
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum How {
    Reflink,
    Link,
    Copy,
    Move,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Way {
    pub from: PathBuf,
    pub how: How,
    pub mode: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Remove(PathBuf),
    Unlink(PathBuf),
    Dir(PathBuf),
    Symlink(PathBuf, Vec<u8>),
    Text(PathBuf, String),
    Install { to: PathBuf, ways: Vec<Way>, required: bool },
    Record(PathBuf, Digest),
}

impl Op {
    #[cfg(test)]
    fn path(&self) -> &Path {
        match self {
            Op::Remove(p) | Op::Unlink(p) | Op::Dir(p) | Op::Symlink(p, _) | Op::Text(p, _) | Op::Record(p, _) => p,
            Op::Install { to, .. } => to,
        }
    }
}

fn mode(exec: bool, writable: bool) -> u32 {
    match (exec, writable) {
        (true, true) => 0o755,
        (false, true) => 0o644,
        (true, false) => 0o555,
        (false, false) => 0o444,
    }
}

fn cache_path(blobs: &Path, hash: &Digest, exec: bool) -> PathBuf {
    let h = hex(hash);
    assert_eq!(h.len(), 2 * hash.len());
    blobs.join(&h[..2]).join(if exec { format!("{}.x", &h[2..]) } else { h[2..].to_string() })
}

pub(crate) fn within(root: &Path, path: &Path) -> bool {
    root.is_absolute()
        && path.strip_prefix(root).is_ok_and(|rest| {
            rest.components().next().is_some() && rest.components().all(|c| matches!(c, Component::Normal(_)))
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bound {
    root: PathBuf,
    targets: Vec<PathBuf>,
}

impl Bound {
    pub(crate) fn new(root: &Path, targets: &[String]) -> Bound {
        let mut targets: Vec<PathBuf> = targets.iter().map(PathBuf::from).filter(|t| within(Path::new("/"), t)).collect();
        targets.sort();
        targets.dedup();
        Bound { root: root.into(), targets }
    }

    pub(crate) fn holds(&self, path: &Path) -> bool {
        within(&self.root, path) || self.targets.iter().any(|t| t == path || within(t, path))
    }
}

impl std::fmt::Display for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the worktree `{}`", self.root.display())?;
        for t in &self.targets {
            write!(f, " or the target directory `{}`", t.display())?;
        }
        Ok(())
    }
}

fn write_path(w: &Write) -> &str {
    match w {
        Write::Dir { path } | Write::Remove { path } | Write::Symlink { path, .. } | Write::File { path, .. } | Write::Text { path, .. } => {
            path
        }
    }
}

pub(crate) fn matches_hash(hash: &Digest, bytes: &[u8]) -> bool {
    blake3::hash(bytes).as_bytes() == hash
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub path: PathBuf,
    pub hash: Digest,
    pub size: u64,
    pub exec: bool,
    pub own: bool,
    pub clear: bool,
}

impl Target {
    fn fits(&self, s: &Stat) -> bool {
        s.kind == Kind::File
            && s.size == self.size
            && (s.mode & 0o111 != 0) == self.exec
            && (!self.own || (s.links == 1 && s.mode & 0o200 != 0))
    }

    fn ways(&self, from: &Path, link: bool) -> Vec<Way> {
        let mode = Some(mode(self.exec, true));
        let mut ways = vec![Way { from: from.into(), how: How::Reflink, mode }];
        if link && !self.own {
            ways.push(Way { from: from.into(), how: How::Link, mode: None });
        }
        ways.push(Way { from: from.into(), how: How::Copy, mode });
        ways
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Choice {
    Keep,
    Install(PathBuf, bool),
    Fetch,
}

pub(crate) struct Placement {
    bound: Bound,
    blobs: PathBuf,
    writes: Vec<Write>,
    targets: BTreeMap<PathBuf, Target>,
    records: BTreeMap<PathBuf, Record>,
    seen: BTreeMap<Look, Sight>,
}

#[derive(Debug)]
pub(crate) struct Plan {
    pub ops: Vec<Op>,
    pub fetch: Vec<Target>,
    pub reused: u64,
}

impl Placement {
    pub(crate) fn new(bound: &Bound, blobs: &Path, placed: &Placed, files: Vec<Delivery>, writes: Vec<Write>) -> Result<Self, PathBuf> {
        let mut targets = BTreeMap::new();
        let mut rest = Vec::new();
        let mut add = |path: String, hash: Digest, size: u64, exec: bool, own: bool| {
            let path = PathBuf::from(path);
            targets.insert(path.clone(), Target { path, hash, size, exec, own, clear: false });
        };
        for w in writes {
            if !bound.holds(Path::new(write_path(&w))) {
                return Err(write_path(&w).into());
            }
            match w {
                Write::File { path, hash, size, exec } => add(path, hash, size, exec, true),
                other => rest.push(other),
            }
        }
        for f in files {
            if !bound.holds(Path::new(&f.path)) {
                return Err(f.path.into());
            }
            add(f.path, f.hash, f.size, f.exec, f.own);
        }
        let records = targets
            .keys()
            .filter_map(|p| Some((p.clone(), *placed.files.get(p.to_str()?)?)))
            .collect();
        Ok(Placement { bound: bound.clone(), blobs: blobs.into(), writes: rest, targets, records, seen: BTreeMap::new() })
    }

    pub(crate) fn saw(&mut self, look: Look, sight: Sight) {
        assert!(look.answered_by(&sight), "{look:?} is answered by its own kind of sight, not {sight:?}");
        self.seen.insert(look, sight);
    }

    fn overwritten(&self, path: &Path, before: usize) -> bool {
        self.writes[..before].iter().any(|w| !matches!(w, Write::Dir { .. }) && path.starts_with(write_path(w)))
    }

    fn sight(&self, look: Look, want: &mut BTreeSet<Look>) -> Option<&Sight> {
        let sight = self.seen.get(&look);
        if sight.is_none() {
            want.insert(look);
        }
        sight
    }

    fn stat(&self, path: &Path, want: &mut BTreeSet<Look>) -> Option<Option<Stat>> {
        match self.sight(Look::Stat(path.into()), want)? {
            Sight::Stat(s) => Some(*s),
            _ => Some(None),
        }
    }

    fn hash(&self, path: &Path, size: u64, want: &mut BTreeSet<Look>) -> Option<Option<Digest>> {
        match self.sight(Look::Hash(path.into(), size), want)? {
            Sight::Hash(h) => Some(*h),
            _ => Some(None),
        }
    }

    fn choose(&self, t: &Target, want: &mut BTreeSet<Look>) -> Option<(Choice, Option<Stat>)> {
        let existing = if self.overwritten(&t.path, self.writes.len()) { None } else { self.stat(&t.path, want)? };
        if let Some(s) = existing.filter(|s| t.fits(s)) {
            let known = match self.records.get(&t.path) {
                Some(&(size, mtime, ino, hash)) if (size, mtime, ino) == (s.size, s.mtime, s.ino) => Some(hash),
                _ => self.hash(&t.path, t.size, want)?,
            };
            if known == Some(t.hash) {
                return Some((Choice::Keep, existing));
            }
        }
        for exec in [t.exec, !t.exec] {
            let from = cache_path(&self.blobs, &t.hash, exec);
            let fits = self.stat(&from, want)?.is_some_and(|s| s.kind == Kind::File && s.size == t.size);
            if fits && self.hash(&from, t.size, want)? == Some(t.hash) {
                return Some((Choice::Install(from, exec == t.exec), existing));
            }
        }
        Some((Choice::Fetch, existing))
    }

    fn stale(&self, dir: &Path, want: &mut BTreeSet<Look>) -> Option<Vec<PathBuf>> {
        let Sight::List(names) = self.sight(Look::List(dir.into()), want)? else { return Some(Vec::new()) };
        let mut gone = Vec::new();
        let mut complete = true;
        for name in names {
            let Some(pid) = crate::tmp::owner(name, crate::tmp::BESIDE) else { continue };
            match self.sight(Look::Process(pid), want) {
                Some(Sight::Process(false)) => gone.push(dir.join(name)),
                Some(_) => {}
                None => complete = false,
            }
        }
        complete.then_some(gone)
    }

    pub(crate) fn wants(&self) -> Vec<Look> {
        let mut want = BTreeSet::new();
        for (i, w) in self.writes.iter().enumerate() {
            if let Write::Text { path, text: Some(_) } = w
                && !self.overwritten(Path::new(path), i)
            {
                self.sight(Look::Read(path.into()), &mut want);
            }
        }
        for t in self.targets.values() {
            if let Some((choice, _)) = self.choose(t, &mut want)
                && choice != Choice::Keep
                && let Some(dir) = t.path.parent()
            {
                self.stale(dir, &mut want);
            }
        }
        want.into_iter().collect()
    }

    pub(crate) fn plan(self) -> Plan {
        let mut want = BTreeSet::new();
        let decided: Vec<(&Target, Choice, Option<Stat>)> = self
            .targets
            .values()
            .map(|t| {
                let (choice, existing) = self.choose(t, &mut want).expect("a plan is made once everything it needs has been seen");
                (t, choice, existing)
            })
            .collect();
        let mut ops = Vec::new();
        let mut swept = BTreeSet::new();
        for (t, choice, _) in &decided {
            if let (false, Some(dir)) = (*choice == Choice::Keep, t.path.parent())
                && swept.insert(dir)
            {
                let gone = self.stale(dir, &mut want).expect("a plan is made once everything it needs has been seen");
                ops.extend(gone.into_iter().map(Op::Remove));
            }
        }
        assert!(want.is_empty(), "a plan is made once everything it needs has been seen: {want:?}");
        for (i, w) in self.writes.iter().enumerate() {
            ops.push(match w {
                Write::Dir { path } => Op::Dir(path.into()),
                Write::Remove { path } => Op::Remove(path.into()),
                Write::Symlink { path, target } => Op::Symlink(path.into(), target.clone()),
                Write::Text { path, text: None } => Op::Unlink(path.into()),
                Write::Text { path, text: Some(text) } => {
                    let read = self.seen.get(&Look::Read(path.into()));
                    let same = matches!(read, Some(Sight::Read(Some(b))) if b == text.as_bytes());
                    if same && !self.overwritten(Path::new(path), i) {
                        continue;
                    }
                    Op::Text(path.into(), text.clone())
                }
                Write::File { .. } => unreachable!("a written file is a target"),
            });
        }
        let mut fetch = Vec::new();
        let mut reused = 0;
        let mut dirs = BTreeSet::new();
        for (t, choice, existing) in decided {
            let clear = existing.is_some_and(|s| s.kind == Kind::Dir);
            match choice {
                Choice::Keep => {
                    reused += 1;
                    ops.push(Op::Record(t.path.clone(), t.hash));
                }
                Choice::Install(from, matching) => {
                    reused += 1;
                    if let Some(dir) = t.path.parent().filter(|d| self.bound.holds(d) && dirs.insert(d.to_path_buf())) {
                        ops.push(Op::Dir(dir.into()));
                    }
                    if clear {
                        ops.push(Op::Remove(t.path.clone()));
                    }
                    ops.push(Op::Install { to: t.path.clone(), ways: t.ways(&from, matching), required: true });
                    ops.push(Op::Record(t.path.clone(), t.hash));
                }
                Choice::Fetch => fetch.push(Target { clear, ..t.clone() }),
            }
        }
        Plan { ops, fetch, reused }
    }

    #[cfg(test)]
    fn invariants(&self) -> bool {
        let ours = |p: &Path| p == self.bound.root || self.bound.holds(p) || within(&self.blobs, p);
        let named = |pid: u32| {
            self.seen.values().any(|s| matches!(s, Sight::List(names) if names.iter().any(|n| crate::tmp::owner(n, crate::tmp::BESIDE) == Some(pid))))
        };
        let wants = self.wants();
        self.targets.iter().all(|(p, t)| *p == t.path && self.bound.holds(p) && !t.clear)
            && self.writes.iter().all(|w| self.bound.holds(Path::new(write_path(w))) && !matches!(w, Write::File { .. }))
            && self.records.keys().all(|p| self.targets.contains_key(p))
            && self.seen.iter().all(|(look, sight)| {
                look.answered_by(sight)
                    && match look {
                        Look::Stat(p) | Look::Hash(p, _) | Look::Read(p) | Look::List(p) => ours(p),
                        Look::Process(pid) => named(*pid),
                    }
            })
            && wants.iter().all(|l| !self.seen.contains_key(l))
            && wants.windows(2).all(|w| w[0] < w[1])
    }
}

#[derive(Default)]
pub(crate) struct Waiting {
    targets: BTreeMap<Digest, Vec<Target>>,
}

impl Waiting {
    pub(crate) fn add(&mut self, t: Target) -> bool {
        let waiting = self.targets.entry(t.hash).or_default();
        waiting.push(t);
        waiting.len() == 1
    }

    pub(crate) fn wanting(&self) -> Vec<Digest> {
        self.targets.keys().copied().collect()
    }

    pub(crate) fn waits_for(&self, hash: &Digest) -> bool {
        self.targets.contains_key(hash)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub(crate) fn arrived(&mut self, blobs: &Path, hash: Digest, tmp: &Path) -> Vec<Op> {
        let targets = self.targets.remove(&hash).unwrap_or_default();
        let mut ops = Vec::new();
        let mut dirs = BTreeSet::new();
        let mut dir = |ops: &mut Vec<Op>, path: &Path| {
            if let Some(d) = path.parent().filter(|d| dirs.insert(d.to_path_buf())) {
                ops.push(Op::Dir(d.into()));
            }
        };
        if targets.is_empty() {
            let to = cache_path(blobs, &hash, false);
            dir(&mut ops, &to);
            let ways = vec![Way { from: tmp.into(), how: How::Move, mode: Some(mode(false, false)) }];
            ops.push(Op::Install { to, ways, required: true });
            return ops;
        }
        let mut cached = BTreeSet::new();
        for t in targets.iter().filter(|t| !t.own) {
            if cached.insert(t.exec) {
                let to = cache_path(blobs, &hash, t.exec);
                let mode = Some(mode(t.exec, false));
                dir(&mut ops, &to);
                let hows: &[How] = if cached.len() == 1 { &[How::Reflink, How::Link, How::Copy] } else { &[How::Reflink, How::Copy] };
                let ways = hows.iter().map(|&how| Way { from: tmp.into(), how, mode }).collect();
                ops.push(Op::Install { to, ways, required: false });
            }
        }
        for t in &targets {
            dir(&mut ops, &t.path);
            if t.clear {
                ops.push(Op::Remove(t.path.clone()));
            }
            let mut ways = if t.own { Vec::new() } else { t.ways(&cache_path(blobs, &hash, t.exec), true) };
            ways.extend(t.ways(tmp, false));
            ops.push(Op::Install { to: t.path.clone(), ways, required: true });
            ops.push(Op::Record(t.path.clone(), hash));
        }
        ops
    }

    #[cfg(test)]
    fn invariants(&self) -> bool {
        self.targets.iter().all(|(hash, ts)| !ts.is_empty() && ts.iter().all(|t| t.hash == *hash && t.path.is_absolute()))
    }
}

fn mtime_ns(m: &std::fs::Metadata) -> i64 {
    m.mtime() * 1_000_000_000 + m.mtime_nsec()
}

fn look_at(look: &Look) -> Sight {
    match look {
        Look::Stat(p) => Sight::Stat(std::fs::symlink_metadata(p).ok().map(|md| Stat {
            kind: if md.is_file() {
                Kind::File
            } else if md.is_dir() {
                Kind::Dir
            } else if md.file_type().is_symlink() {
                Kind::Symlink
            } else {
                Kind::Other
            },
            size: md.len(),
            mode: md.mode() & 0o7777,
            links: md.nlink(),
            mtime: mtime_ns(&md),
            ino: md.ino(),
        })),
        Look::Hash(p, size) => Sight::Hash(hash_file(p, *size).ok()),
        Look::Read(p) => Sight::Read(std::fs::read(p).ok()),
        Look::List(dir) => {
            let mut names: Vec<String> =
                std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
            names.sort();
            Sight::List(names)
        }
        Look::Process(pid) => Sight::Process(crate::tmp::alive(*pid)),
    }
}

#[cfg(target_os = "linux")]
fn reflink(src: &Path, dst: &Path) -> bool {
    use std::os::fd::AsRawFd;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let (Ok(s), Ok(d)) = (std::fs::File::open(src), std::fs::OpenOptions::new().write(true).create_new(true).open(dst))
    else {
        return false;
    };
    let ok = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as libc::Ioctl, s.as_raw_fd()) } == 0;
    if !ok {
        let _ = std::fs::remove_file(dst);
    }
    ok
}

#[cfg(not(target_os = "linux"))]
fn reflink(_src: &Path, _dst: &Path) -> bool {
    false
}

fn place_beside(to: &Path, way: &Way) -> io::Result<()> {
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = to.with_file_name(format!("{}{}-{}-{name}", crate::tmp::BESIDE, std::process::id(), crate::tmp::seq()));
    assert_eq!(tmp.parent(), to.parent());
    assert_ne!(tmp, to);
    let _watched = crate::tmp::watch(&tmp);
    let done = (|| -> io::Result<()> {
        match way.how {
            How::Reflink => {
                if !reflink(&way.from, &tmp) {
                    return Err(io::Error::other("the file system cannot share this file's blocks"));
                }
            }
            How::Link => std::fs::hard_link(&way.from, &tmp)?,
            How::Copy => {
                std::fs::copy(&way.from, &tmp)?;
            }
            How::Move => unreachable!("a move renames its source"),
        }
        if let Some(m) = way.mode {
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m))?;
        }
        std::fs::rename(&tmp, to)
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

fn install(to: &Path, ways: &[Way]) -> io::Result<()> {
    let mut last = io::Error::other("no way to place the file");
    for way in ways {
        let done = match way.how {
            How::Move => way
                .mode
                .map_or(Ok(()), |m| std::fs::set_permissions(&way.from, std::fs::Permissions::from_mode(m)))
                .and_then(|()| std::fs::rename(&way.from, to)),
            _ => place_beside(to, way),
        };
        match done {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

pub fn cached(cache_dir: &Path, hash: &Digest, size: u64) -> Option<PathBuf> {
    let blobs = cache_dir.join("blobs");
    [cache_path(&blobs, hash, false), cache_path(&blobs, hash, true)]
        .into_iter()
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && m.len() == size).unwrap_or(false))
}

pub fn cache_slot(cache_dir: &Path, hash: &Digest) -> PathBuf {
    cache_path(&cache_dir.join("blobs"), hash, false)
}

struct Part {
    tmp: PathBuf,
    file: std::fs::File,
    _watched: crate::tmp::Temp,
}

pub struct Placer {
    cache: PathBuf,
    pub placed: Placed,
    pub changed: bool,
    waiting: Waiting,
    parts: HashMap<Digest, Part>,
    pub fetched_bytes: u64,
    pub reused_files: u64,
}

impl Drop for Placer {
    fn drop(&mut self) {
        for (_, part) in self.parts.drain() {
            let _ = std::fs::remove_file(&part.tmp);
        }
    }
}

impl Placer {
    pub fn new(cache_dir: &Path, placed: Placed) -> Placer {
        crate::tmp::sweep(&cache_dir.join("blobs"), ".incoming-");
        Placer {
            cache: cache_dir.join("blobs"),
            placed,
            changed: false,
            waiting: Waiting::default(),
            parts: HashMap::new(),
            fetched_bytes: 0,
            reused_files: 0,
        }
    }

    fn run(&mut self, ops: Vec<Op>) -> io::Result<()> {
        for op in ops {
            match op {
                Op::Remove(path) => {
                    if let Ok(md) = std::fs::symlink_metadata(&path) {
                        let _ = if md.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
                    }
                }
                Op::Unlink(path) => {
                    let _ = std::fs::remove_file(path);
                }
                Op::Dir(path) => std::fs::create_dir_all(path)?,
                Op::Symlink(path, target) => {
                    use std::os::unix::ffi::OsStrExt;
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let _ = std::fs::remove_file(&path);
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), path)?;
                }
                Op::Text(path, text) => crate::write_atomic(&path, text.as_bytes())?,
                Op::Install { to, ways, required } => {
                    let done = install(&to, &ways);
                    if required {
                        done?;
                    }
                }
                Op::Record(path, hash) => {
                    if let Ok(md) = std::fs::symlink_metadata(&path) {
                        self.placed.files.insert(path.to_string_lossy().into_owned(), (md.len(), mtime_ns(&md), md.ino(), hash));
                        self.changed = true;
                    }
                }
            }
        }
        Ok(())
    }

    fn arrived(&mut self, hash: Digest, tmp: &Path) -> io::Result<()> {
        assert!(!self.parts.contains_key(&hash), "a finished download is no longer a part");
        let ops = self.waiting.arrived(&self.cache, hash, tmp);
        let done = self.run(ops);
        let _ = std::fs::remove_file(tmp);
        done
    }

    fn scratch(&mut self) -> io::Result<PathBuf> {
        std::fs::create_dir_all(&self.cache)?;
        Ok(self.cache.join(format!(".incoming-{}-{}", std::process::id(), crate::tmp::seq())))
    }

    pub fn blob(&mut self, hash: Digest, bytes: &[u8]) -> io::Result<()> {
        if !matches_hash(&hash, bytes) {
            return Err(io::Error::other("the server sent content that does not match its hash"));
        }
        self.fetched_bytes += bytes.len() as u64;
        let tmp = self.scratch()?;
        let _watched = crate::tmp::watch(&tmp);
        std::fs::write(&tmp, bytes)?;
        self.arrived(hash, &tmp)
    }

    pub fn part(&mut self, hash: Digest, data: &[u8], last: bool) -> io::Result<()> {
        self.fetched_bytes += data.len() as u64;
        if !self.parts.contains_key(&hash) {
            let tmp = self.scratch()?;
            let watched = crate::tmp::watch(&tmp);
            let file = std::fs::File::create(&tmp)?;
            self.parts.insert(hash, Part { tmp, file, _watched: watched });
        }
        let part = self.parts.get_mut(&hash).unwrap();
        part.file.write_all(data)?;
        if !last {
            return Ok(());
        }
        let part = self.parts.remove(&hash).unwrap();
        assert!(!self.parts.contains_key(&hash));
        drop(part.file);
        let size = std::fs::metadata(&part.tmp)?.len();
        if hash_file(&part.tmp, size)? != hash {
            let _ = std::fs::remove_file(&part.tmp);
            return Err(io::Error::other("the server sent content that does not match its hash"));
        }
        self.arrived(hash, &part.tmp)
    }

    pub fn deliver(&mut self, root: &Path, targets: &[String], files: Vec<Delivery>, writes: Vec<Write>) -> io::Result<Vec<Digest>> {
        let bound = Bound::new(root, targets);
        let mut placement = Placement::new(&bound, &self.cache, &self.placed, files, writes)
            .map_err(|path| io::Error::other(format!("the server asked to place `{}`, which is not inside {bound}", path.display())))?;
        loop {
            let wants = placement.wants();
            if wants.is_empty() {
                break;
            }
            for look in wants {
                let sight = look_at(&look);
                placement.saw(look, sight);
            }
        }
        let plan = placement.plan();
        self.reused_files += plan.reused;
        self.run(plan.ops)?;
        Ok(plan.fetch.into_iter().filter_map(|t| { let hash = t.hash; self.waiting.add(t).then_some(hash) }).collect())
    }

    pub fn wanting(&self) -> Vec<Digest> {
        self.waiting.wanting()
    }

    pub fn awaiting(&self, hash: &Digest) -> bool {
        self.waiting.waits_for(hash) || self.parts.contains_key(hash)
    }

    pub fn settled(&self) -> bool {
        self.waiting.is_empty() && self.parts.is_empty()
    }

    pub fn prune(&self) {
        let marker = self.cache.join(".pruned");
        let age = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if age(&marker).is_some_and(|a| a.as_secs() < 3600) || !self.cache.is_dir() {
            return;
        }
        let _ = std::fs::write(&marker, b"");
        for shard in std::fs::read_dir(&self.cache).into_iter().flatten().flatten() {
            let p = shard.path();
            if !p.is_dir() {
                if age(&p).is_some_and(|a| a.as_secs() > 86400) && p != marker {
                    let _ = std::fs::remove_file(&p);
                }
                continue;
            }
            for f in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                let fp = f.path();
                let Ok(md) = std::fs::metadata(&fp) else { continue };
                if md.nlink() == 1 && age(&fp).is_some_and(|a| a.as_secs() > 3 * 86400) {
                    let _ = std::fs::remove_file(&fp);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn chance(&mut self, n: u64) -> bool {
            self.below(n) == 0
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Entry {
        File(u64),
        Dir,
        Link(Vec<u8>),
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Inode {
        data: Vec<u8>,
        mode: u32,
        mtime: i64,
    }

    #[derive(Clone)]
    struct World {
        tree: BTreeMap<PathBuf, Entry>,
        inodes: BTreeMap<u64, Inode>,
        next: u64,
        now: i64,
        dead: BTreeSet<u32>,
        reflinks: bool,
        placed: Placed,
    }

    impl World {
        fn new(reflinks: bool) -> World {
            World { tree: BTreeMap::new(), inodes: BTreeMap::new(), next: 1, now: 100, dead: BTreeSet::new(), reflinks, placed: Placed::default() }
        }

        fn inode(&mut self, data: Vec<u8>, mode: u32) -> u64 {
            let ino = self.next;
            self.next += 1;
            self.inodes.insert(ino, Inode { data, mode, mtime: self.now });
            ino
        }

        fn links(&self, ino: u64) -> u64 {
            self.tree.values().filter(|e| **e == Entry::File(ino)).count() as u64
        }

        fn mkdirs(&mut self, path: &Path) -> Result<(), ()> {
            let mut chain: Vec<&Path> = path.ancestors().filter(|a| a.parent().is_some()).collect();
            chain.reverse();
            for a in chain {
                match self.tree.get(a) {
                    Some(Entry::Dir) => {}
                    Some(_) => return Err(()),
                    None => {
                        self.tree.insert(a.into(), Entry::Dir);
                    }
                }
            }
            Ok(())
        }

        fn file(&mut self, path: &str, data: &[u8], mode: u32) -> u64 {
            let path = Path::new(path);
            self.mkdirs(path.parent().unwrap()).unwrap();
            let ino = self.inode(data.to_vec(), mode);
            self.tree.insert(path.into(), Entry::File(ino));
            ino
        }

        fn remove_tree(&mut self, path: &Path) {
            self.tree.retain(|p, _| !p.starts_with(path));
        }

        fn data(&self, path: &Path) -> Option<&Inode> {
            match self.tree.get(path)? {
                Entry::File(ino) => self.inodes.get(ino),
                _ => None,
            }
        }

        fn look(&self, look: &Look) -> Sight {
            match look {
                Look::Stat(p) => Sight::Stat(self.tree.get(p).map(|e| match e {
                    Entry::File(ino) => {
                        let i = &self.inodes[ino];
                        Stat { kind: Kind::File, size: i.data.len() as u64, mode: i.mode, links: self.links(*ino), mtime: i.mtime, ino: *ino }
                    }
                    Entry::Dir => Stat { kind: Kind::Dir, size: 0, mode: 0o755, links: 2, mtime: 0, ino: 0 },
                    Entry::Link(t) => Stat { kind: Kind::Symlink, size: t.len() as u64, mode: 0o777, links: 1, mtime: 0, ino: 0 },
                })),
                Look::Hash(p, size) => Sight::Hash(
                    self.data(p).filter(|i| i.data.len() as u64 == *size).map(|i| *blake3::hash(&i.data).as_bytes()),
                ),
                Look::Read(p) => Sight::Read(self.data(p).map(|i| i.data.clone())),
                Look::List(d) => Sight::List(
                    self.tree.keys().filter(|p| p.parent() == Some(d)).map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect(),
                ),
                Look::Process(pid) => Sight::Process(!self.dead.contains(pid)),
            }
        }

        fn place(&mut self, to: &Path, way: &Way) -> Result<(), ()> {
            if self.tree.get(to.parent().unwrap()) != Some(&Entry::Dir) || self.tree.get(to) == Some(&Entry::Dir) {
                return Err(());
            }
            let Some(Entry::File(src)) = self.tree.get(&way.from).cloned() else { return Err(()) };
            let ino = match way.how {
                How::Reflink if !self.reflinks => return Err(()),
                How::Reflink | How::Copy => {
                    let data = self.inodes[&src].data.clone();
                    self.inode(data, 0o644)
                }
                How::Link | How::Move => src,
            };
            if let Some(m) = way.mode {
                self.inodes.get_mut(&ino).unwrap().mode = m;
            }
            if way.how == How::Move {
                self.tree.remove(&way.from);
            }
            self.tree.insert(to.into(), Entry::File(ino));
            Ok(())
        }

        fn run(&mut self, op: &Op) -> Result<(), ()> {
            self.now += 1;
            match op {
                Op::Remove(p) => self.remove_tree(p),
                Op::Unlink(p) => {
                    if self.tree.get(p) != Some(&Entry::Dir) {
                        self.tree.remove(p);
                    }
                }
                Op::Dir(p) => self.mkdirs(p)?,
                Op::Symlink(p, t) => {
                    self.mkdirs(p.parent().unwrap())?;
                    if self.tree.get(p) == Some(&Entry::Dir) {
                        return Err(());
                    }
                    self.tree.insert(p.clone(), Entry::Link(t.clone()));
                }
                Op::Text(p, t) => {
                    self.mkdirs(p.parent().unwrap())?;
                    if self.tree.get(p) == Some(&Entry::Dir) {
                        return Err(());
                    }
                    let ino = self.inode(t.as_bytes().to_vec(), 0o644);
                    self.tree.insert(p.clone(), Entry::File(ino));
                }
                Op::Install { to, ways, required } => {
                    let placed = ways.iter().any(|w| self.place(to, w).is_ok());
                    if !placed && *required {
                        return Err(());
                    }
                }
                Op::Record(p, h) => {
                    if let Some(Entry::File(ino)) = self.tree.get(p) {
                        let i = &self.inodes[ino];
                        let record = (i.data.len() as u64, i.mtime, *ino, *h);
                        self.placed.files.insert(p.to_string_lossy().into_owned(), record);
                    }
                }
            }
            Ok(())
        }

        fn deliver(&mut self, waiting: &mut Waiting, bound: &Bound, blobs: &Path, files: Vec<Delivery>, writes: Vec<Write>) -> (Vec<Op>, Vec<Digest>) {
            let mut placement = Placement::new(bound, blobs, &self.placed, files, writes).unwrap();
            assert!(placement.invariants());
            loop {
                let wants = placement.wants();
                if wants.is_empty() {
                    break;
                }
                for look in wants {
                    let sight = self.look(&look);
                    placement.saw(look, sight);
                    assert!(placement.invariants());
                }
            }
            let plan = placement.plan();
            for op in &plan.ops {
                let root_dir = matches!(op, Op::Dir(p) if *p == bound.root);
                assert!(root_dir || bound.holds(op.path()) || within(blobs, op.path()), "{op:?} stays in the worktree, the target directories or the cache");
                if let Op::Install { ways, .. } = op {
                    assert!(ways.iter().all(|w| within(blobs, &w.from)), "a placement copies only from the cache: {ways:?}");
                }
                self.run(op).unwrap_or_else(|()| panic!("{op:?} failed"));
            }
            let need = plan.fetch.into_iter().filter_map(|t| { let h = t.hash; waiting.add(t).then_some(h) }).collect();
            assert!(waiting.invariants());
            (plan.ops, need)
        }

        fn arrive(&mut self, waiting: &mut Waiting, blobs: &Path, content: &[u8]) {
            let hash = *blake3::hash(content).as_bytes();
            assert!(matches_hash(&hash, content));
            let tmp = blobs.join(format!(".incoming-1-{}", self.next));
            self.mkdirs(blobs).unwrap();
            let ino = self.inode(content.to_vec(), 0o644);
            self.tree.insert(tmp.clone(), Entry::File(ino));
            for op in waiting.arrived(blobs, hash, &tmp) {
                assert!(within(blobs, op.path()) || op.path().starts_with("/w") || op.path().starts_with("/o/tgt"), "{op:?}");
                self.run(&op).unwrap_or_else(|()| panic!("{op:?} failed"));
                assert!(waiting.invariants());
            }
            self.tree.remove(&tmp);
        }
    }

    fn delivery(path: &str, content: &[u8], exec: bool, own: bool) -> Delivery {
        Delivery { path: path.into(), hash: *blake3::hash(content).as_bytes(), size: content.len() as u64, exec, own }
    }

    #[test]
    fn generated_placements_leave_the_delivered_tree_keep_what_is_unchanged_and_touch_nothing_else() {
        let blobs = Path::new("/home/u/.cache/rargo/blobs");
        let inside = Bound::new(Path::new("/w"), &["/w/target".into()]);
        let outside = Bound::new(Path::new("/w"), &["/o/tgt".into(), "/o/tgt/build".into()]);
        let pool = [
            "/w/target/debug/app",
            "/w/target/debug/deps/libx.rlib",
            "/w/target/release/app",
            "/w/target/release/tool",
            "/w/target/doc/x/index.html",
            "/w/target/debug/examples/ex",
            "/o/tgt/debug/app",
            "/o/tgt/build/debug/deps/liby.rlib",
        ];
        let contents: Vec<Vec<u8>> = (0..6u8).map(|i| vec![i; 1 + (i as usize % 2)]).collect();
        let (dead_tmp, live_tmp) = ("/w/target/debug/.rargo-tmp-777-1-app", "/w/target/debug/.rargo-tmp-888-2-app");
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let elsewhere = rng.chance(2);
            let bound = if elsewhere { &outside } else { &inside };
            let mut w = World::new(rng.chance(2));
            w.dead.insert(777);
            w.file("/etc/passwd", b"root", 0o644);
            w.file("/w/src/main.rs", b"fn main() {}", 0o644);
            w.file("/w/target/debug/stale", b"old", 0o755);
            w.file("/w/target/release/stale", b"old", 0o755);
            w.file(dead_tmp, b"half", 0o644);
            w.file(live_tmp, b"half", 0o644);
            for (i, c) in contents.iter().enumerate() {
                let hash = *blake3::hash(c).as_bytes();
                match rng.below(4) {
                    1 => drop(w.file(cache_path(blobs, &hash, true).to_str().unwrap(), c, 0o555)),
                    2 => drop(w.file(cache_path(blobs, &hash, false).to_str().unwrap(), c, 0o444)),
                    3 => drop(w.file(cache_path(blobs, &hash, false).to_str().unwrap(), &vec![0xee; c.len()], 0o444)),
                    _ => {}
                }
                assert!(i < contents.len());
            }
            let mut files = Vec::new();
            let mut kept = BTreeSet::new();
            for path in pool {
                if rng.chance(3) || !bound.holds(Path::new(path)) {
                    continue;
                }
                let content = &contents[rng.below(contents.len() as u64) as usize];
                let (exec, own) = (rng.chance(2), rng.chance(2));
                let d = delivery(path, content, exec, own);
                let fitting = mode(exec, true);
                match rng.below(8) {
                    1 => {
                        let ino = w.file(path, content, fitting);
                        let i = &w.inodes[&ino];
                        w.placed.files.insert(path.into(), (i.data.len() as u64, i.mtime, ino, d.hash));
                        kept.insert(path);
                    }
                    2 => {
                        w.file(path, content, fitting);
                        kept.insert(path);
                    }
                    3 => {
                        let ino = w.file(path, &vec![0xab; content.len()], fitting);
                        w.placed.files.insert(path.into(), (content.len() as u64, -1, ino, d.hash));
                    }
                    4 => drop(w.file(path, &vec![0xcd; content.len()], fitting)),
                    5 => drop(w.file(&format!("{path}/inner"), b"in the way", 0o644)),
                    6 => {
                        w.mkdirs(Path::new(path).parent().unwrap()).unwrap();
                        w.tree.insert(path.into(), Entry::Link(b"elsewhere".to_vec()));
                    }
                    7 => {
                        let read_only = rng.chance(2);
                        let ino = w.file(path, content, if read_only { mode(exec, false) } else { mode(!exec, true) });
                        if own {
                            w.tree.insert("/w/target/other-link".into(), Entry::File(ino));
                        } else if read_only {
                            kept.insert(path);
                        }
                    }
                    _ => {}
                }
                files.push(d);
            }
            let mut writes = Vec::new();
            let dep_info = "app: src/main.rs\n";
            if rng.chance(2) {
                if rng.chance(2) {
                    w.file("/w/target/debug/app.d", dep_info.as_bytes(), 0o644);
                }
                writes.push(Write::Text { path: "/w/target/debug/app.d".into(), text: Some(dep_info.into()) });
            }
            if rng.chance(3) {
                w.file("/w/target/debug/old.d", b"old", 0o644);
                writes.push(Write::Text { path: "/w/target/debug/old.d".into(), text: None });
            }
            if rng.chance(3) {
                writes.push(Write::Dir { path: "/w/gen/deep".into() });
            }
            if elsewhere && rng.chance(2) {
                writes.push(Write::Dir { path: "/o/tgt".into() });
                writes.push(Write::Text { path: "/o/tgt/CACHEDIR.TAG".into(), text: Some("Signature: 8a477f597d28d172789f06886806bc55\n".into()) });
            }
            if rng.chance(3) {
                writes.push(Write::Symlink { path: "/w/gen/link".into(), target: b"deep".to_vec() });
            }
            let cleared = rng.chance(4);
            if cleared {
                writes.push(Write::Remove { path: "/w/target/release".into() });
                kept.retain(|p| !p.starts_with("/w/target/release/"));
            }
            if rng.chance(3) {
                let lock = &contents[rng.below(contents.len() as u64) as usize];
                writes.push(Write::File { path: "/w/Cargo.lock".into(), hash: *blake3::hash(lock).as_bytes(), size: lock.len() as u64, exec: false });
            }

            let before = w.clone();
            let mut waiting = Waiting::default();
            let (_, need) = w.deliver(&mut waiting, bound, blobs, files.clone(), writes.clone());
            assert_eq!(need.iter().collect::<BTreeSet<_>>().len(), need.len(), "each content is fetched once");
            for hash in &need {
                let content = contents.iter().find(|c| blake3::hash(c).as_bytes() == hash).unwrap();
                w.arrive(&mut waiting, blobs, content);
            }
            assert!(waiting.is_empty());

            let mut targets: BTreeMap<PathBuf, (Digest, u64, bool, bool)> = BTreeMap::new();
            for wr in &writes {
                if let Write::File { path, hash, size, exec } = wr {
                    targets.insert(path.into(), (*hash, *size, *exec, true));
                }
            }
            for d in &files {
                targets.insert(d.path.clone().into(), (d.hash, d.size, d.exec, d.own));
            }
            let cache_inodes: BTreeSet<u64> =
                w.tree.iter().filter(|(p, _)| p.starts_with(blobs)).filter_map(|(_, e)| if let Entry::File(i) = e { Some(*i) } else { None }).collect();
            for (path, (hash, _, exec, own)) in &targets {
                let Some(Entry::File(ino)) = w.tree.get(path) else { panic!("seed {seed}: {path:?} is delivered") };
                let i = &w.inodes[ino];
                assert_eq!(blake3::hash(&i.data).as_bytes(), hash, "seed {seed}: {path:?} holds what was delivered");
                assert_eq!(i.mode & 0o111 != 0, *exec, "seed {seed}: {path:?}'s exec bit");
                if *own {
                    assert!(w.links(*ino) == 1 && i.mode & 0o200 != 0, "seed {seed}: {path:?} is the worktree's own writable file");
                    assert!(!cache_inodes.contains(ino), "seed {seed}: {path:?} does not share the cache's inode");
                } else if !w.reflinks && !kept.contains(path.to_str().unwrap()) {
                    let slot = cache_path(blobs, hash, *exec);
                    if w.data(&slot).is_some_and(|c| blake3::hash(&c.data).as_bytes() == hash) {
                        assert_eq!(w.tree.get(&slot), Some(&Entry::File(*ino)), "seed {seed}: {path:?} is linked from the shared cache");
                    }
                }
                if kept.contains(path.to_str().unwrap()) {
                    let was = before.data(path).unwrap();
                    assert_eq!((before.tree.get(path), was.mtime), (w.tree.get(path), i.mtime), "seed {seed}: {path:?} was already right and is kept");
                }
            }
            for wr in &writes {
                let at = |p: &str| w.tree.get(Path::new(p));
                match wr {
                    Write::Text { path, text: Some(t) } => {
                        assert_eq!(w.data(Path::new(path)).unwrap().data, t.as_bytes());
                        let was = before.data(Path::new(path));
                        if was.is_some_and(|i| i.data == t.as_bytes()) {
                            assert_eq!(was.unwrap().mtime, w.data(Path::new(path)).unwrap().mtime, "seed {seed}: unchanged text keeps its mtime");
                        }
                    }
                    Write::Text { path, text: None } => assert_eq!(at(path), None),
                    Write::Dir { path } => assert_eq!(at(path), Some(&Entry::Dir)),
                    Write::Symlink { path, target } => assert_eq!(at(path), Some(&Entry::Link(target.clone()))),
                    Write::Remove { path } => assert!(w.tree.keys().filter(|p| p.starts_with(path)).all(|p| p == Path::new(path)
                        || targets.keys().any(|t| t.starts_with(p))), "seed {seed}: only delivered files are back under {path}"),
                    Write::File { .. } => {}
                }
            }
            let installed_in_debug = targets.keys().any(|t| t.parent() == Some(Path::new("/w/target/debug")) && !kept.contains(t.to_str().unwrap()));
            assert_eq!(w.tree.contains_key(Path::new(dead_tmp)), !installed_in_debug, "seed {seed}: a dead process's temp is swept beside a placement");
            assert!(w.tree.contains_key(Path::new(live_tmp)), "seed {seed}: a live process's temp stays");

            let written: Vec<PathBuf> = writes.iter().map(|wr| PathBuf::from(write_path(wr))).chain(targets.keys().cloned()).collect();
            for (path, entry) in &before.tree {
                if path.starts_with(blobs) || written.iter().any(|p| path.starts_with(p)) || path == Path::new(dead_tmp) {
                    continue;
                }
                assert_eq!(w.tree.get(path), Some(entry), "seed {seed}: {path:?} is not part of the delivery and is untouched");
                if let Entry::File(ino) = entry {
                    assert_eq!(w.inodes.get(ino), before.inodes.get(ino), "seed {seed}: {path:?}'s content, mode and mtime are untouched");
                }
            }
            for path in w.tree.keys().filter(|p| !before.tree.contains_key(*p)) {
                assert!(
                    path.starts_with(blobs) || blobs.starts_with(path) || written.iter().any(|p| p.starts_with(path) || path.starts_with(p)),
                    "seed {seed}: {path:?} is new but belongs to no delivered path"
                );
            }

            let settled = w.clone();
            let mut waiting = Waiting::default();
            let (ops, need) = w.deliver(&mut waiting, bound, blobs, files.clone(), Vec::new());
            assert!(need.is_empty(), "seed {seed}: a second delivery of the same files fetches nothing");
            assert!(ops.iter().all(|op| matches!(op, Op::Record(..))), "seed {seed}: and changes nothing: {ops:?}");
            assert_eq!((w.tree, w.inodes), (settled.tree, settled.inodes), "seed {seed}: every mtime is kept");
        }
    }

    #[test]
    fn only_paths_inside_the_worktree_or_the_commands_target_directories_are_placed() {
        let root = Path::new("/w");
        let named = ["/w/target".to_string(), "/srv/out".into(), "/srv/out".into(), "/".into(), "rel/out".into(), "/x/../etc".into(), "".into()];
        let bound = Bound::new(root, &named);
        assert_eq!(bound.targets, [PathBuf::from("/srv/out"), PathBuf::from("/w/target")], "only plain absolute directories bound a delivery");
        assert_eq!(bound.to_string(), "the worktree `/w` or the target directory `/srv/out` or the target directory `/w/target`");
        for (path, worktree, with_targets) in [
            ("/w/target/debug/app", true, true),
            ("/w/Cargo.lock", true, true),
            ("/w", false, false),
            ("/srv/out", false, true),
            ("/srv/out/CACHEDIR.TAG", false, true),
            ("/srv/out/debug/app", false, true),
            ("/srv/out/../../etc/passwd", false, false),
            ("/srv/outer/app", false, false),
            ("/srv", false, false),
            ("/etc/passwd", false, false),
            ("/x/../etc/passwd", false, false),
            ("/w/../etc/passwd", false, false),
            ("/w/target/../../etc/passwd", false, false),
            ("/wx/target/app", false, false),
            ("target/debug/app", false, false),
            ("", false, false),
        ] {
            assert_eq!(within(root, Path::new(path)), worktree, "{path}");
            for (b, inside) in [(Bound::new(root, &[]), worktree), (bound.clone(), with_targets)] {
                assert_eq!(b.holds(Path::new(path)), inside, "{path} in {b}");
                let file = Placement::new(&b, Path::new("/c"), &Placed::default(), vec![delivery(path, b"x", false, true)], vec![]);
                assert_eq!(file.is_ok(), inside, "{path} in {b}");
                let write = Placement::new(&b, Path::new("/c"), &Placed::default(), vec![], vec![Write::Remove { path: path.into() }]);
                assert_eq!(write.err(), (!inside).then(|| PathBuf::from(path)), "{path} in {b}");
            }
        }
        assert!(!within(Path::new("w"), Path::new("w/a")), "the root is absolute");
    }

    #[test]
    fn forged_content_is_refused_and_shared_content_comes_from_the_cache() {
        let content = b"fn main() {}";
        let hash = *blake3::hash(content).as_bytes();
        assert!(matches_hash(&hash, content));
        assert!(!matches_hash(&hash, b"forged"));
        let blobs = Path::new("/c/blobs");
        let mut waiting = Waiting::default();
        let shared = Target { path: "/w/target/debug/app".into(), hash, size: content.len() as u64, exec: true, own: false, clear: false };
        let own = Target { path: "/v/target/debug/app".into(), own: true, ..shared.clone() };
        let plain = Target { path: "/w/target/debug/app.txt".into(), exec: false, ..shared.clone() };
        assert!(waiting.add(shared.clone()));
        assert!(!waiting.add(own.clone()), "the same content is fetched once for every place it goes");
        assert!(!waiting.add(plain.clone()));
        assert!(waiting.invariants());
        let tmp = Path::new("/c/blobs/.incoming-1-1");
        let ops = waiting.arrived(blobs, hash, tmp);
        assert!(waiting.is_empty() && waiting.invariants());
        let cache = cache_path(blobs, &hash, true);
        let install = |to: &Path| ops.iter().find_map(|op| match op { Op::Install { to: t, ways, required } if t == to => Some((ways.clone(), *required)), _ => None }).unwrap();
        let (ways, required) = install(&cache);
        assert!(!required, "a cache that cannot be filled does not stop the placement");
        assert!(ways.iter().all(|w| w.from == tmp && w.mode == Some(0o555)), "the cache holds a read-only copy: {ways:?}");
        let (ways, _) = install(&cache_path(blobs, &hash, false));
        assert!(ways.iter().all(|w| w.how != How::Link), "a second mode is not a link of the first, or one chmod would change both: {ways:?}");
        let (ways, _) = install(&shared.path);
        assert_eq!(ways[1], Way { from: cache.clone(), how: How::Link, mode: None }, "a shared output is linked from the cache");
        assert!(ways.iter().any(|w| w.from == tmp), "and falls back to the download");
        let (ways, _) = install(&own.path);
        assert!(ways.iter().all(|w| w.from == tmp && w.how != How::Link && w.mode == Some(0o755)), "an own output is a writable copy: {ways:?}");
    }

    #[test]
    fn large_content_arrives_in_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("t/big");
        let content: Vec<u8> = (0..300_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let h = *blake3::hash(&content).as_bytes();
        let mut p = Placer::new(&tmp.path().join("cache"), Placed::default());
        let d = delivery(dest.to_str().unwrap(), &content, false, true);
        assert_eq!(p.deliver(tmp.path(), &[], vec![d], vec![]).unwrap(), vec![h]);
        let (x, y) = content.split_at(700_000);
        p.part(h, x, false).unwrap();
        p.part(h, y, true).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        assert!(p.settled());
    }

    #[test]
    fn writes() {
        let tmp = tempfile::tempdir().unwrap();
        let s = |n: &str| tmp.path().join(n).to_string_lossy().into_owned();
        let mut p = Placer::new(&tmp.path().join("cache"), Placed::default());
        let src = b"fn generated() {}";
        let h = *blake3::hash(src).as_bytes();
        let need = p
            .deliver(
                tmp.path(),
                &[],
                vec![],
                vec![
                    Write::Dir { path: s("gen/deep") },
                    Write::Text { path: s("target/debug/app.d"), text: Some("app: src/main.rs\n".into()) },
                    Write::Symlink { path: s("gen/link"), target: b"deep".to_vec() },
                    Write::File { path: s("src/generated.rs"), hash: h, size: src.len() as u64, exec: false },
                ],
            )
            .unwrap();
        assert_eq!(need, vec![h]);
        assert!(p.blob(h, b"forged").is_err(), "content that does not match its hash is refused");
        p.blob(h, src).unwrap();
        assert!(tmp.path().join("gen/deep").is_dir());
        assert_eq!(std::fs::read_link(tmp.path().join("gen/link")).unwrap(), Path::new("deep"));
        assert_eq!(std::fs::read(tmp.path().join("src/generated.rs")).unwrap(), src);
        let md = std::fs::metadata(tmp.path().join("src/generated.rs")).unwrap();
        assert!(md.mode() & 0o200 != 0 && md.nlink() == 1, "a source file is an ordinary writable file");
        p.deliver(tmp.path(), &[], vec![], vec![Write::Remove { path: s("gen") }, Write::Text { path: s("target/debug/app.d"), text: None }]).unwrap();
        assert!(!tmp.path().join("gen").exists() && !tmp.path().join("target/debug/app.d").exists());
        assert!(p.deliver(tmp.path(), &[], vec![], vec![Write::Remove { path: "/etc".into() }]).is_err());
    }

    #[test]
    fn a_target_directory_outside_the_worktree_is_placed_when_the_server_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, outside) = (tmp.path().join("proj"), tmp.path().join("outside"));
        std::fs::create_dir_all(&root).unwrap();
        let named = [outside.to_string_lossy().into_owned(), root.join("target").to_string_lossy().into_owned()];
        let content = b"\x7fELF hi";
        let h = *blake3::hash(content).as_bytes();
        let app = outside.join("debug/p");
        let tag = Write::Text { path: outside.join("CACHEDIR.TAG").to_string_lossy().into_owned(), text: Some("Signature: 8a477f597d28d172789f06886806bc55\n".into()) };
        let files = vec![delivery(app.to_str().unwrap(), content, true, false)];
        let mut p = Placer::new(&tmp.path().join("cache"), Placed::default());
        let refused = p.deliver(&root, &named[1..], files.clone(), vec![tag.clone()]).unwrap_err().to_string();
        assert_eq!(
            refused,
            format!(
                "the server asked to place `{}`, which is not inside the worktree `{}` or the target directory `{}`",
                outside.join("CACHEDIR.TAG").display(),
                root.display(),
                root.join("target").display()
            )
        );
        assert!(!outside.exists(), "nothing is placed when the delivery is refused");
        assert_eq!(p.deliver(&root, &named, files, vec![Write::Dir { path: named[0].clone() }, tag]).unwrap(), vec![h]);
        p.blob(h, content).unwrap();
        assert_eq!(std::fs::read(&app).unwrap(), content);
        assert!(outside.join("CACHEDIR.TAG").is_file());
        let passwd = vec![delivery("/etc/passwd", b"x", false, true)];
        assert!(p.deliver(&root, &named, passwd, vec![]).is_err(), "and nothing else outside is");
    }
}
