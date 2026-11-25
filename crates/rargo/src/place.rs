//! Putting what the server sends where it belongs: artifacts into the
//! target directory, files a build wrote into the worktree.
//!
//! Artifacts cargo itself would hard-link into place (`target/debug/app`)
//! are kept once in the client's cache and shared between worktrees: as a
//! reflink where the file system has them, as a read-only hard link
//! otherwise. Files other tools rewrite in place (an executable at the path
//! rustc writes it to, documentation, source files) are always ordinary
//! writable files of their own.

use crate::scan::hash_file;
use rargo_proto::{Delivery, Digest, Write};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn hex(d: &Digest) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// What was placed where, so that an unchanged artifact is recognised by a
/// `stat` instead of being hashed or fetched again.
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

struct Part {
    tmp: PathBuf,
    file: std::fs::File,
    _watched: crate::tmp::Temp,
}

pub struct Placer {
    cache: PathBuf,
    pub placed: Placed,
    pub changed: bool,
    pending: HashMap<Digest, Vec<Dest>>,
    parts: HashMap<Digest, Part>,
    /// Directories already cleared of what dead clients left behind.
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
    // SAFETY: both descriptors are valid for the duration of the call.
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

/// Where the cache holds a content, if it does (in either of its two
/// modes) with the right size.
pub fn cached(cache_dir: &Path, hash: &Digest, size: u64) -> Option<PathBuf> {
    let h = hex(hash);
    let dir = cache_dir.join("blobs").join(&h[..2]);
    [dir.join(&h[2..]), dir.join(format!("{}.x", &h[2..]))]
        .into_iter()
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && m.len() == size).unwrap_or(false))
}

/// Where a content fetched for the cache alone goes.
pub fn cache_slot(cache_dir: &Path, hash: &Digest) -> PathBuf {
    let h = hex(hash);
    assert_eq!(h.len(), 64);
    cache_dir.join("blobs").join(&h[..2]).join(&h[2..])
}

/// Temporary names are unique in the process: a daemon places for several
/// commands at once.
impl Drop for Placer {
    /// A command that was interrupted leaves no half-received files.
    fn drop(&mut self) {
        for (_, part) in self.parts.drain() {
            let _ = std::fs::remove_file(&part.tmp);
        }
    }
}

impl Placer {
    pub fn new(cache_dir: &Path, placed: Placed) -> Placer {
        // Downloads a killed client never finished.
        crate::tmp::sweep(&cache_dir.join("blobs"), ".incoming-");
        Placer {
            swept: Default::default(),
            cache: cache_dir.join("blobs"),
            placed,
            changed: false,
            pending: HashMap::new(),
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

    /// A temporary name beside `dest`. The first time a directory is
    /// written to, temporaries that clients which no longer exist left in
    /// it are removed.
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

    /// Whether `dest` already holds this content in the right form.
    fn already(&self, d: &Dest, hash: &Digest, size: u64) -> bool {
        let Ok(md) = std::fs::symlink_metadata(&d.path) else { return false };
        if !md.is_file() || md.len() != size || (md.mode() & 0o111 != 0) != d.exec {
            return false;
        }
        // A file of its own must not share an inode with anything, and must
        // be writable.
        if d.own && (md.nlink() > 1 || md.mode() & 0o200 == 0) {
            return false;
        }
        if let Some((s, m, ino, h)) = self.placed.files.get(d.path.to_string_lossy().as_ref()) {
            if *s == size && *m == mtime_ns(&md) && *ino == md.ino() {
                return h == hash;
            }
        }
        hash_file(&d.path, size).map(|h| h == *hash).unwrap_or(false)
    }

    fn note(&mut self, path: &Path, hash: &Digest) {
        if let Ok(md) = std::fs::symlink_metadata(path) {
            self.placed.files.insert(path.to_string_lossy().into_owned(), (md.len(), mtime_ns(&md), md.ino(), *hash));
            self.changed = true;
        }
    }

    /// Makes `dest` a file with the content of `src` (a cache entry or a
    /// finished download).
    fn install(&mut self, src: &Path, d: &Dest, hash: &Digest, src_is_cache: bool) -> io::Result<()> {
        if let Some(parent) = d.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::symlink_metadata(&d.path).map(|m| m.is_dir()).unwrap_or(false) {
            std::fs::remove_dir_all(&d.path)?;
        }
        let (tmp, _watched) = self.tmp_beside(&d.path);
        let done = (|| -> io::Result<()> {
            if reflink(src, &tmp) {
                std::fs::set_permissions(&tmp, mode(d.exec, true))?;
            } else if !d.own && src_is_cache && std::fs::hard_link(src, &tmp).is_ok() {
                // Shares the cache's inode and its read-only mode.
            } else {
                std::fs::copy(src, &tmp)?;
                std::fs::set_permissions(&tmp, mode(d.exec, true))?;
            }
            std::fs::rename(&tmp, &d.path)
        })();
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done?;
        self.note(&d.path, hash);
        Ok(())
    }

    /// A finished download at `tmp` goes to everyone waiting for it; if
    /// nobody is (content for a virtual output directory), into the cache.
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
            // Anything shareable goes through the cache, once per mode.
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
                // The cache may be on another file system or unwritable;
                // then the artifact is simply not shared.
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

    /// Applies a delivery as far as possible without new content and
    /// returns the hashes whose content is needed.
    pub fn deliver(&mut self, files: Vec<Delivery>, writes: Vec<Write>) -> io::Result<Vec<Digest>> {
        let mut wanted: Vec<(Dest, Digest, u64)> = Vec::new();
        for w in writes {
            match w {
                Write::Dir { path } => std::fs::create_dir_all(&path)?,
                Write::Remove { path } => {
                    if let Ok(md) = std::fs::symlink_metadata(&path) {
                        let _ = if md.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                    }
                }
                Write::Symlink { path, target } => {
                    use std::os::unix::ffi::OsStrExt;
                    if let Some(parent) = Path::new(&path).parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let _ = std::fs::remove_file(&path);
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), &path)?;
                }
                Write::Text { path, text: None } => {
                    let _ = std::fs::remove_file(&path);
                }
                Write::Text { path, text: Some(text) } => {
                    // Unchanged: leave the file (and its mtime) alone.
                    if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                        crate::write_atomic(Path::new(&path), text.as_bytes())?;
                    }
                }
                Write::File { path, hash, size, exec } => {
                    wanted.push((Dest { path: PathBuf::from(path), exec, own: true }, hash, size))
                }
            }
        }
        for f in files {
            wanted.push((Dest { path: PathBuf::from(f.path), exec: f.exec, own: f.own }, f.hash, f.size));
        }
        let mut need = Vec::new();
        for (d, hash, size) in wanted {
            if self.already(&d, &hash, size) {
                self.reused_files += 1;
                self.note(&d.path.clone(), &hash);
                continue;
            }
            // In the cache from another worktree (or an earlier build)?
            let mut from_cache = None;
            for exec in [d.exec, !d.exec] {
                let cp = self.cache_path(&hash, exec);
                let fits = std::fs::metadata(&cp).map(|m| m.len() == size).unwrap_or(false);
                // The cache is only as good as its content: check it.
                if fits && hash_file(&cp, size).map(|h| h == hash).unwrap_or(false) {
                    from_cache = Some((cp, exec == d.exec));
                    break;
                }
            }
            if let Some((cp, same_mode)) = from_cache {
                self.reused_files += 1;
                self.install(&cp, &d, &hash, same_mode)?;
                continue;
            }
            if !self.pending.contains_key(&hash) {
                debug_assert!(!need.contains(&hash));
                need.push(hash);
            }
            self.pending.entry(hash).or_default().push(d);
        }
        debug_assert!(need.iter().all(|h| self.pending.contains_key(h)));
        Ok(need)
    }

    /// The contents still waited for.
    pub fn wanting(&self) -> Vec<Digest> {
        self.pending.keys().copied().collect()
    }

    pub fn awaiting(&self, hash: &Digest) -> bool {
        self.pending.contains_key(hash) || self.parts.contains_key(hash)
    }

    /// Whether everything asked for has arrived.
    pub fn settled(&self) -> bool {
        self.pending.is_empty() && self.parts.is_empty()
    }

    /// Removes cache entries no worktree links to that have not been
    /// written for a while; at most once an hour.
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
                // A download a killed client left behind.
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
    fn places_shares_and_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let (a, b, c) = (tmp.path().join("wt1/target/debug/app"), tmp.path().join("wt2/target/debug/app"), tmp.path().join("wt1/target/debug/deps/app-1234"));
        let content = b"an executable";
        let h = *blake3::hash(content).as_bytes();
        let mut p = Placer::new(&cache, Placed::default());
        let need = p.deliver(vec![d(&a, content, true, false), d(&c, content, true, true)], vec![]).unwrap();
        assert_eq!(need, vec![h]);
        assert!(!p.settled());
        p.blob(h, content).unwrap();
        assert!(p.settled());
        assert_eq!(std::fs::read(&a).unwrap(), content);
        assert_eq!(std::fs::read(&c).unwrap(), content);
        let (ma, mc) = (std::fs::metadata(&a).unwrap(), std::fs::metadata(&c).unwrap());
        assert!(ma.mode() & 0o111 != 0 && mc.mode() & 0o111 != 0);
        // The file of its own is writable and shares nothing.
        assert_eq!(mc.nlink(), 1);
        assert!(mc.mode() & 0o200 != 0);
        assert_ne!(ma.ino(), mc.ino());
        // Again: nothing is needed, nothing is rewritten.
        let before = mtime_ns(&ma);
        assert!(p.deliver(vec![d(&a, content, true, false), d(&c, content, true, true)], vec![]).unwrap().is_empty());
        assert_eq!(mtime_ns(&std::fs::metadata(&a).unwrap()), before);
        // Another worktree: served from the cache without the server.
        let mut p2 = Placer::new(&cache, Placed::default());
        assert!(p2.deliver(vec![d(&b, content, true, false)], vec![]).unwrap().is_empty());
        assert_eq!(std::fs::read(&b).unwrap(), content);
        // Content that does not match its hash is refused.
        let need = p2.deliver(vec![d(&tmp.path().join("x"), b"wanted", false, true)], vec![]).unwrap();
        assert!(p2.blob(need[0], b"forged").is_err());
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
