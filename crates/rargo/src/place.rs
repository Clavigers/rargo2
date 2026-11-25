use crate::scan::hash_file;
use rargo_proto::{Delivery, Digest, Write};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn hex(d: &Digest) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Default, Serialize, Deserialize)]
pub struct Placed {
    files: HashMap<String, (u64, i64, u64, Digest)>,
}

impl Placed {
    pub fn load(path: &Path) -> Placed {
        std::fs::read(path).ok().and_then(|b| bincode::deserialize(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::write_atomic(path, &bincode::serialize(self).map_err(io::Error::other)?)
    }
}

struct Dest {
    path: PathBuf,
    exec: bool,
    own: bool,
}

enum WriteOp {
    Dir(PathBuf),
    Remove(PathBuf),
    Symlink(PathBuf, Vec<u8>),
    RemoveText(PathBuf),
    Text(PathBuf, String),
}

struct PlacementPlan {
    writes: Vec<WriteOp>,
    files: Vec<(Dest, Digest, u64)>,
}

#[derive(Debug, PartialEq, Eq)]
enum PlaceFile {
    Keep,
    Install { source: PathBuf, matching_mode: bool },
    Fetch,
}

#[derive(Clone, Copy)]
struct Existing {
    size: u64,
    exec: bool,
    writable: bool,
    links: u64,
    hash: Option<Digest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Materialize {
    Reflink,
    Link,
    Copy,
}

impl Materialize {
    fn try_install(self, src: &Path, tmp: &Path) -> io::Result<bool> {
        match self {
            Self::Reflink => Ok(reflink(src, tmp)),
            Self::Link => Ok(std::fs::hard_link(src, tmp).is_ok()),
            Self::Copy => std::fs::copy(src, tmp).map(|_| true),
        }
    }
}

impl Dest {
    fn materialize(&self, matching_cache_mode: bool) -> &'static [Materialize] {
        use Materialize::*;
        if !self.own && matching_cache_mode { &[Reflink, Link, Copy] } else { &[Reflink, Copy] }
    }

    fn fits(&self, info: Existing, size: u64) -> bool {
        info.size == size && info.exec == self.exec && (!self.own || (info.links == 1 && info.writable))
    }

    fn plan(&self, hash: &Digest, size: u64, existing: Option<Existing>, cache: Option<(PathBuf, bool)>) -> PlaceFile {
        if existing.is_some_and(|info| self.fits(info, size) && info.hash.as_ref() == Some(hash)) {
            PlaceFile::Keep
        } else if let Some((source, matching_mode)) = cache {
            PlaceFile::Install { source, matching_mode }
        } else {
            PlaceFile::Fetch
        }
    }
}

impl PlacementPlan {
    fn new(files: Vec<Delivery>, writes: Vec<Write>) -> Self {
        let mut plan = Self { writes: Vec::new(), files: Vec::new() };
        for write in writes {
            let op = match write {
                Write::Dir { path } => WriteOp::Dir(path.into()),
                Write::Remove { path } => WriteOp::Remove(path.into()),
                Write::Symlink { path, target } => WriteOp::Symlink(path.into(), target),
                Write::Text { path, text: None } => WriteOp::RemoveText(path.into()),
                Write::Text { path, text: Some(text) } => WriteOp::Text(path.into(), text),
                Write::File { path, hash, size, exec } => {
                    plan.files.push((Dest { path: path.into(), exec, own: true }, hash, size));
                    continue;
                }
            };
            plan.writes.push(op);
        }
        plan.files.extend(files.into_iter().map(|f| (Dest { path: f.path.into(), exec: f.exec, own: f.own }, f.hash, f.size)));
        plan
    }

    #[cfg(test)]
    fn invariants(&self) -> bool {
        self.writes.iter().all(|op| {
            let path = match op {
                WriteOp::Dir(path) | WriteOp::Remove(path) | WriteOp::Symlink(path, _) | WriteOp::RemoveText(path)
                | WriteOp::Text(path, _) => path,
            };
            !path.as_os_str().is_empty()
        }) && self.files.iter().all(|(dest, _, _)| !dest.path.as_os_str().is_empty())
    }
}

impl WriteOp {
    fn execute(self) -> io::Result<()> {
        match self {
            Self::Dir(path) => std::fs::create_dir_all(path)?,
            Self::Remove(path) => {
                if let Ok(md) = std::fs::symlink_metadata(&path) {
                    let _ = if md.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
                }
            }
            Self::Symlink(path, target) => {
                use std::os::unix::ffi::OsStrExt;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let _ = std::fs::remove_file(&path);
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), path)?;
            }
            Self::RemoveText(path) => {
                let _ = std::fs::remove_file(path);
            }
            Self::Text(path, text) => {
                if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                    crate::write_atomic(&path, text.as_bytes())?;
                }
            }
        }
        Ok(())
    }
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
    pending: BTreeMap<Digest, Vec<Dest>>,
    parts: HashMap<Digest, Part>,
    swept: std::collections::HashSet<PathBuf>,
    pub fetched_bytes: u64,
    pub reused_files: u64,
}

fn mtime_ns(m: &std::fs::Metadata) -> i64 {
    m.mtime() * 1_000_000_000 + m.mtime_nsec()
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

fn mode(exec: bool, writable: bool) -> std::fs::Permissions {
    std::fs::Permissions::from_mode(match (exec, writable) {
        (true, true) => 0o755,
        (false, true) => 0o644,
        (true, false) => 0o555,
        (false, false) => 0o444,
    })
}

pub fn cached(cache_dir: &Path, hash: &Digest, size: u64) -> Option<PathBuf> {
    let h = hex(hash);
    let dir = cache_dir.join("blobs").join(&h[..2]);
    [dir.join(&h[2..]), dir.join(format!("{}.x", &h[2..]))]
        .into_iter()
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && m.len() == size).unwrap_or(false))
}

pub fn cache_slot(cache_dir: &Path, hash: &Digest) -> PathBuf {
    let h = hex(hash);
    assert_eq!(h.len(), 64);
    cache_dir.join("blobs").join(&h[..2]).join(&h[2..])
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
            swept: Default::default(),
            cache: cache_dir.join("blobs"),
            placed,
            changed: false,
            pending: BTreeMap::new(),
            parts: HashMap::new(),
            fetched_bytes: 0,
            reused_files: 0,
        }
    }

    fn cache_path(&self, hash: &Digest, exec: bool) -> PathBuf {
        let h = hex(hash);
        assert_eq!(h.len(), 2 * hash.len());
        self.cache.join(&h[..2]).join(if exec { format!("{}.x", &h[2..]) } else { h[2..].to_string() })
    }

    fn tmp_beside(&mut self, dest: &Path) -> (PathBuf, crate::tmp::Temp) {
        if let Some(dir) = dest.parent() {
            if self.swept.insert(dir.to_path_buf()) {
                crate::tmp::sweep(dir, crate::tmp::BESIDE);
            }
        }
        let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = dest.with_file_name(format!("{}{}-{}-{name}", crate::tmp::BESIDE, std::process::id(), crate::tmp::seq()));
        assert_eq!(tmp.parent(), dest.parent());
        assert_ne!(tmp, dest);
        let watched = crate::tmp::watch(&tmp);
        (tmp, watched)
    }

    fn observe(&self, d: &Dest, size: u64) -> Option<Existing> {
        let md = std::fs::symlink_metadata(&d.path).ok()?;
        if !md.is_file() {
            return None;
        }
        let mut info = Existing { size: md.len(), exec: md.mode() & 0o111 != 0, writable: md.mode() & 0o200 != 0, links: md.nlink(), hash: None };
        if d.fits(info, size) {
            info.hash = match self.placed.files.get(d.path.to_string_lossy().as_ref()) {
                Some((s, m, ino, hash)) if *s == size && *m == mtime_ns(&md) && *ino == md.ino() => Some(*hash),
                _ => hash_file(&d.path, size).ok(),
            };
        }
        Some(info)
    }

    fn find_cached(&self, d: &Dest, hash: &Digest, size: u64) -> Option<(PathBuf, bool)> {
        [d.exec, !d.exec].into_iter().find_map(|exec| {
            let path = self.cache_path(hash, exec);
            let fits = std::fs::metadata(&path).is_ok_and(|m| m.len() == size);
            (fits && hash_file(&path, size).is_ok_and(|h| h == *hash)).then_some((path, exec == d.exec))
        })
    }

    fn note(&mut self, path: &Path, hash: &Digest) {
        if let Ok(md) = std::fs::symlink_metadata(path) {
            self.placed.files.insert(path.to_string_lossy().into_owned(), (md.len(), mtime_ns(&md), md.ino(), *hash));
            self.changed = true;
        }
    }

    fn install(&mut self, src: &Path, d: &Dest, hash: &Digest, src_is_cache: bool) -> io::Result<()> {
        if let Some(parent) = d.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::symlink_metadata(&d.path).map(|m| m.is_dir()).unwrap_or(false) {
            std::fs::remove_dir_all(&d.path)?;
        }
        let (tmp, _watched) = self.tmp_beside(&d.path);
        let done = (|| -> io::Result<()> {
            for &op in d.materialize(src_is_cache) {
                if op.try_install(src, &tmp)? {
                    if op != Materialize::Link {
                        std::fs::set_permissions(&tmp, mode(d.exec, true))?;
                    }
                    return std::fs::rename(&tmp, &d.path);
                }
            }
            unreachable!("the placement plan ends in copy")
        })();
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done?;
        self.note(&d.path, hash);
        Ok(())
    }

    fn arrived(&mut self, hash: Digest, tmp: &Path) -> io::Result<()> {
        assert!(!self.parts.contains_key(&hash), "a finished download is no longer a part");
        let dests = self.pending.remove(&hash).unwrap_or_default();
        if dests.is_empty() {
            let cp = self.cache_path(&hash, false);
            if let Some(parent) = cp.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::set_permissions(tmp, mode(false, false));
            return std::fs::rename(tmp, &cp).inspect_err(|_| {
                let _ = std::fs::remove_file(tmp);
            });
        }
        let result = (|| -> io::Result<()> {
            let mut cached: HashMap<bool, PathBuf> = HashMap::new();
            for d in dests.iter().filter(|d| !d.own) {
                if cached.contains_key(&d.exec) {
                    continue;
                }
                let cp = self.cache_path(&hash, d.exec);
                if let Some(parent) = cp.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let (ctmp, _watched) = self.tmp_beside(&cp);
                let made = (|| -> io::Result<()> {
                    if !reflink(tmp, &ctmp) && std::fs::hard_link(tmp, &ctmp).is_err() {
                        std::fs::copy(tmp, &ctmp)?;
                    }
                    Ok(())
                })();
                if made.is_ok() && std::fs::set_permissions(&ctmp, mode(d.exec, false)).is_ok() && std::fs::rename(&ctmp, &cp).is_ok() {
                    cached.insert(d.exec, cp);
                } else {
                    let _ = std::fs::remove_file(&ctmp);
                }
            }
            for d in &dests {
                match cached.get(&d.exec) {
                    Some(cp) if !d.own => {
                        let cp = cp.clone();
                        self.install(&cp, d, &hash, true)?
                    }
                    _ => self.install(tmp, d, &hash, false)?,
                }
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(tmp);
        result
    }

    fn scratch(&mut self) -> io::Result<PathBuf> {
        std::fs::create_dir_all(&self.cache)?;
        Ok(self.cache.join(format!(".incoming-{}-{}", std::process::id(), crate::tmp::seq())))
    }

    pub fn blob(&mut self, hash: Digest, bytes: &[u8]) -> io::Result<()> {
        if *blake3::hash(bytes).as_bytes() != hash {
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

    pub fn deliver(&mut self, files: Vec<Delivery>, writes: Vec<Write>) -> io::Result<Vec<Digest>> {
        let plan = PlacementPlan::new(files, writes);
        for write in plan.writes {
            write.execute()?;
        }
        let mut need = Vec::new();
        for (d, hash, size) in plan.files {
            let existing = self.observe(&d, size);
            let mut op = d.plan(&hash, size, existing, None);
            if op == PlaceFile::Fetch {
                op = d.plan(&hash, size, existing, self.find_cached(&d, &hash, size));
            }
            match op {
                PlaceFile::Keep => {
                    self.reused_files += 1;
                    self.note(&d.path, &hash);
                }
                PlaceFile::Install { source, matching_mode } => {
                    self.reused_files += 1;
                    self.install(&source, &d, &hash, matching_mode)?;
                }
                PlaceFile::Fetch => {
                    let pending = self.pending.entry(hash).or_insert_with(|| {
                        need.push(hash);
                        Vec::new()
                    });
                    pending.push(d);
                }
            }
        }
        debug_assert!(need.iter().all(|h| self.pending.contains_key(h)));
        Ok(need)
    }

    pub fn wanting(&self) -> Vec<Digest> {
        self.pending.keys().copied().collect()
    }

    pub fn awaiting(&self, hash: &Digest) -> bool {
        self.pending.contains_key(hash) || self.parts.contains_key(hash)
    }

    pub fn settled(&self) -> bool {
        self.pending.is_empty() && self.parts.is_empty()
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

    fn d(path: &Path, content: &[u8], exec: bool, own: bool) -> Delivery {
        Delivery {
            path: path.to_string_lossy().into_owned(),
            hash: *blake3::hash(content).as_bytes(),
            size: content.len() as u64,
            exec,
            own,
        }
    }

    #[test]
    fn generated_target_placements_keep_unchanged_files_and_only_touch_manifest_paths() {
        let root = Path::new("/workspace");
        let target = root.join("target");
        let unrelated = root.join("src/main.rs");
        for seed in 0..64 {
            let mut current = BTreeMap::from([(unrelated.clone(), ([73; 32], 999))]);
            let mut expected = BTreeMap::new();
            let mut files = Vec::new();
            for i in 0..16 {
                let path = target.join(format!("debug/file{i}"));
                let same = (seed + i) % 3 == 0;
                let manifest = d(&path, &[i as u8], i % 2 == 0, i % 4 == 0);
                current.insert(path.clone(), (if same { manifest.hash } else { [0; 32] }, 500 + i));
                expected.insert(path, (manifest.hash, if same { 500 + i } else { 1000 }));
                files.push(manifest);
            }
            let plan = PlacementPlan::new(files, vec![]);
            assert!(plan.invariants());
            assert!(plan.writes.is_empty());
            for (dest, hash, size) in &plan.files {
                assert!(dest.path.starts_with(&target));
                let (old_hash, old_mtime) = current[&dest.path];
                let observed = Existing { size: *size, exec: dest.exec, writable: true, links: 1, hash: Some(old_hash) };
                let cached = Some((PathBuf::from("/cache/input"), true));
                let mtime = match dest.plan(hash, *size, Some(observed), cached) {
                    PlaceFile::Keep => old_mtime,
                    PlaceFile::Install { matching_mode, .. } => {
                        assert_eq!(dest.materialize(matching_mode).contains(&Materialize::Link), !dest.own);
                        1000
                    }
                    PlaceFile::Fetch => panic!("a cached file is always available"),
                };
                current.insert(dest.path.clone(), (*hash, mtime));
                assert_eq!(current[&unrelated], ([73; 32], 999));
                assert!(plan.invariants());
            }
            current.remove(&unrelated);
            assert_eq!(current, expected);
        }
    }

    #[test]
    fn installed_file_must_have_the_requested_mode_and_ownership() {
        let plan = PlacementPlan::new(vec![d(Path::new("target/debug/app"), b"app", true, true)], vec![]);
        assert!(plan.invariants());
        let (dest, hash, size) = &plan.files[0];
        let info = Existing { size: *size, exec: true, writable: true, links: 2, hash: Some(*hash) };
        assert_eq!(dest.plan(hash, *size, Some(info), None), PlaceFile::Fetch);
        assert!(plan.invariants());
        assert_eq!(dest.plan(hash, *size, Some(Existing { exec: false, links: 1, ..info }), None), PlaceFile::Fetch);
        assert!(plan.invariants());
        assert_eq!(dest.plan(hash, *size, Some(Existing { links: 1, ..info }), None), PlaceFile::Keep);
        assert!(plan.invariants());
    }

    #[test]
    fn large_content_arrives_in_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("t/big");
        let content: Vec<u8> = (0..300_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let h = *blake3::hash(&content).as_bytes();
        let mut p = Placer::new(&tmp.path().join("cache"), Placed::default());
        assert_eq!(p.deliver(vec![d(&dest, &content, false, true)], vec![]).unwrap(), vec![h]);
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
        p.blob(h, src).unwrap();
        assert!(tmp.path().join("gen/deep").is_dir());
        assert_eq!(std::fs::read_link(tmp.path().join("gen/link")).unwrap(), Path::new("deep"));
        assert_eq!(std::fs::read(tmp.path().join("src/generated.rs")).unwrap(), src);
        let md = std::fs::metadata(tmp.path().join("src/generated.rs")).unwrap();
        assert!(md.mode() & 0o200 != 0 && md.nlink() == 1, "a source file is an ordinary writable file");
        p.deliver(vec![], vec![Write::Remove { path: s("gen") }, Write::Text { path: s("target/debug/app.d"), text: None }]).unwrap();
        assert!(!tmp.path().join("gen").exists() && !tmp.path().join("target/debug/app.d").exists());
    }
}
