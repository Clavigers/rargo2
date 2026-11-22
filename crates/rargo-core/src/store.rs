//! The content-addressed store.
//!
//! Blobs are whole files named by their blake3 hash under `blobs/xx/`. A blob
//! may exist in two variants, plain (`<hex>`, mode 0444) and executable
//! (`<hex>.x`, mode 0555), because a hard link shares its inode's mode with
//! the file it is linked from.

use crate::db::{Db, Table};
use crate::hash::Hash;
use anyhow::{Context, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const FICLONE: libc::c_ulong = 0x4004_9409;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BlobMeta {
    pub size: u64,
    /// mtime of the blob file when it was stored, in nanoseconds. A blob whose
    /// file no longer matches was written through a hard link and is dropped.
    pub mtime_ns: i64,
    /// Seconds since the epoch.
    pub last_used: u64,
}

/// How a blob ended up at a destination path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    Reflink,
    Hardlink,
    Copy,
}

pub struct Store {
    root: PathBuf,
    blobs: PathBuf,
    tmp: PathBuf,
    db: Arc<Db>,
    meta: DashMap<Hash, BlobMeta>,
    reflink: bool,
    dev: u64,
    tmp_seq: AtomicU64,
    total: AtomicU64,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn mtime_ns(m: &fs::Metadata) -> i64 {
    m.mtime() * 1_000_000_000 + m.mtime_nsec()
}

/// Tries to make `dst` a reflink clone of `src`.
pub fn reflink(src: &File, dst: &File) -> bool {
    // SAFETY: both fds are valid for the duration of the call.
    unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) == 0 }
}

/// Copies `src` into the new file `dst` (reflink if possible).
pub fn clone_or_copy(src: &File, dst: &File) -> io::Result<bool> {
    if reflink(src, dst) {
        return Ok(true);
    }
    let mut s = src;
    let mut d = dst;
    use std::io::Seek;
    s.seek(io::SeekFrom::Start(0))?;
    io::copy(&mut s, &mut d)?;
    Ok(false)
}

impl Store {
    pub fn open(root: &Path, db: Arc<Db>) -> Result<Store> {
        let blobs = root.join("blobs");
        let tmp = root.join("tmp");
        fs::create_dir_all(&blobs)?;
        // Scratch from a previous run is garbage.
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;
        for i in 0..256 {
            let d = blobs.join(format!("{i:02x}"));
            if !d.exists() {
                fs::create_dir(&d)?;
            }
        }
        let dev = fs::metadata(&blobs)?.dev();
        let reflink = {
            let a = tmp.join("reflink-probe-a");
            let b = tmp.join("reflink-probe-b");
            let mut fa = File::create(&a)?;
            fa.write_all(b"probe")?;
            let fa = File::open(&a)?;
            let fb = File::create(&b)?;
            let ok = reflink(&fa, &fb);
            let _ = fs::remove_file(&a);
            let _ = fs::remove_file(&b);
            ok
        };
        let meta = DashMap::new();
        let mut total = 0u64;
        db.for_each(Table::Blobs, |k, v| {
            if k.len() == 32 {
                if let Ok(m) = bincode::deserialize::<BlobMeta>(v) {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(k);
                    total += m.size;
                    meta.insert(Hash(h), m);
                }
            }
        });
        Ok(Store {
            root: root.to_path_buf(),
            blobs,
            tmp,
            db,
            meta,
            reflink,
            dev,
            tmp_seq: AtomicU64::new(0),
            total: AtomicU64::new(total),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub fn supports_reflink(&self) -> bool {
        self.reflink
    }

    /// Bytes of blob content the store believes it holds.
    pub fn total_bytes(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn blob_count(&self) -> usize {
        self.meta.len()
    }

    fn path_of(&self, h: &Hash, exec: bool) -> PathBuf {
        let hex = h.hex();
        let mut p = self.blobs.join(&hex[..2]);
        p.push(if exec { format!("{}.x", &hex[2..]) } else { hex[2..].to_string() });
        p
    }

    /// A fresh path inside the store's scratch area (same filesystem as the
    /// blobs, so files created here can be renamed into place).
    pub fn tmp_path(&self) -> PathBuf {
        let n = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        self.tmp.join(format!("t{}-{n}", std::process::id()))
    }

    /// Creates a scratch directory for one action's upper layer.
    pub fn tmp_dir(&self, name: &str) -> io::Result<PathBuf> {
        let p = self.tmp.join(name);
        fs::create_dir_all(&p)?;
        Ok(p)
    }

    pub fn has(&self, h: &Hash) -> bool {
        self.meta.contains_key(h)
    }

    pub fn size_of(&self, h: &Hash) -> Option<u64> {
        self.meta.get(h).map(|m| m.size)
    }

    fn record(&self, h: Hash, size: u64, mtime_ns: i64) {
        let m = BlobMeta { size, mtime_ns, last_used: now_secs() };
        if self.meta.insert(h, m).is_none() {
            self.total.fetch_add(size, Ordering::Relaxed);
        }
        self.db.put(Table::Blobs, &h.0, &bincode::serialize(&m).unwrap());
    }

    /// Marks blobs as used now (for LRU eviction).
    pub fn touch(&self, h: &Hash) {
        let now = now_secs();
        if let Some(mut m) = self.meta.get_mut(h) {
            // Persist coarse-grained to keep write volume down.
            let stale = now.saturating_sub(m.last_used) > 600;
            m.last_used = now;
            if stale {
                self.db.put(Table::Blobs, &h.0, &bincode::serialize(&*m).unwrap());
            }
        }
    }

    /// Path of an existing variant of the blob, preferring `exec`.
    fn existing(&self, h: &Hash, exec: bool) -> Option<PathBuf> {
        let p = self.path_of(h, exec);
        if p.exists() {
            return Some(p);
        }
        let q = self.path_of(h, !exec);
        q.exists().then_some(q)
    }

    /// Opens a blob for reading, checking that the file is still the one that
    /// was stored. A blob that fails the check is forgotten.
    pub fn open_blob(&self, h: &Hash) -> io::Result<File> {
        let meta = self.meta.get(h).map(|m| *m);
        let Some(meta) = meta else {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("blob {h} not in store")));
        };
        let Some(p) = self.existing(h, false) else {
            self.forget(h);
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("blob {h} missing")));
        };
        let f = File::open(&p)?;
        let m = f.metadata()?;
        if m.len() != meta.size || mtime_ns(&m) != meta.mtime_ns {
            // Verify by content before giving up: a touch does not corrupt.
            let (actual, _) = Hash::of_file(&f)?;
            if actual != *h {
                self.forget(h);
                let _ = fs::remove_file(self.path_of(h, false));
                let _ = fs::remove_file(self.path_of(h, true));
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("blob {h} was modified on disk and has been dropped"),
                ));
            }
            self.record_force(*h, m.len(), mtime_ns(&m));
        }
        self.touch(h);
        Ok(f)
    }

    fn record_force(&self, h: Hash, size: u64, mtime_ns: i64) {
        let m = BlobMeta { size, mtime_ns, last_used: now_secs() };
        self.meta.insert(h, m);
        self.db.put(Table::Blobs, &h.0, &bincode::serialize(&m).unwrap());
    }

    fn forget(&self, h: &Hash) {
        if let Some((_, m)) = self.meta.remove(h) {
            self.total.fetch_sub(m.size, Ordering::Relaxed);
        }
        self.db.delete(Table::Blobs, &h.0);
    }

    pub fn read(&self, h: &Hash) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut f = self.open_blob(h)?;
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        Ok(v)
    }

    /// Stores bytes.
    pub fn put_bytes(&self, bytes: &[u8]) -> io::Result<Hash> {
        let h = Hash::of(bytes);
        if self.has(&h) {
            self.touch(&h);
            return Ok(h);
        }
        let tmp = self.tmp_path();
        {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o444).open(&tmp)?;
            f.write_all(bytes)?;
        }
        self.install(&tmp, h, false, bytes.len() as u64)?;
        Ok(h)
    }

    /// Moves a file that already lives on the store filesystem into the
    /// store. The file must not be modified by anyone afterwards.
    pub fn put_file_move(&self, path: &Path, exec: bool) -> io::Result<(Hash, u64)> {
        let (h, size) = Hash::of_path(path)?;
        if self.has(&h) && self.existing(&h, exec).is_some() {
            let _ = fs::remove_file(path);
            self.touch(&h);
            return Ok((h, size));
        }
        self.install(path, h, exec, size)?;
        Ok((h, size))
    }

    /// Hard-links a finished file on the store filesystem into the store,
    /// leaving the original name in place.
    pub fn put_file_link(&self, path: &Path, exec: bool) -> io::Result<(Hash, u64)> {
        let (h, size) = Hash::of_path(path)?;
        if self.has(&h) && self.existing(&h, exec).is_some() {
            self.touch(&h);
            return Ok((h, size));
        }
        let tmp = self.tmp_path();
        fs::hard_link(path, &tmp)?;
        self.install(&tmp, h, exec, size)?;
        Ok((h, size))
    }

    /// Copies (reflinks when possible) an arbitrary file into the store.
    pub fn put_file_copy(&self, src: &File, exec: bool) -> io::Result<(Hash, u64)> {
        let tmp = self.tmp_path();
        let dst = fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
        clone_or_copy(src, &dst)?;
        drop(dst);
        let r = self.put_file_move(&tmp, exec);
        if r.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        r
    }

    /// Installs a scratch file whose hash the caller has already computed
    /// while writing it.
    pub fn install_hashed(&self, tmp: &Path, h: Hash, size: u64) -> io::Result<()> {
        if self.has(&h) && self.existing(&h, false).is_some() {
            let _ = fs::remove_file(tmp);
            self.touch(&h);
            return Ok(());
        }
        self.install(tmp, h, false, size)
    }

    fn install(&self, tmp: &Path, h: Hash, exec: bool, size: u64) -> io::Result<()> {
        let dst = self.path_of(&h, exec);
        fs::set_permissions(tmp, fs::Permissions::from_mode(if exec { 0o555 } else { 0o444 }))?;
        fs::rename(tmp, &dst)?;
        let m = fs::metadata(&dst)?;
        self.record(h, size, mtime_ns(&m));
        Ok(())
    }

    /// Makes sure the requested variant exists and returns its path.
    fn variant(&self, h: &Hash, exec: bool) -> io::Result<PathBuf> {
        let want = self.path_of(h, exec);
        if want.exists() {
            return Ok(want);
        }
        let other = self.path_of(h, !exec);
        let src = File::open(&other)?;
        let tmp = self.tmp_path();
        let dst = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if exec { 0o555 } else { 0o444 })
            .open(&tmp)?;
        clone_or_copy(&src, &dst)?;
        drop(dst);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(if exec { 0o555 } else { 0o444 }))?;
        fs::rename(&tmp, &want)?;
        Ok(want)
    }

    /// Places a blob at `dest` without using extra space where the
    /// filesystems allow it: reflink, then hard link, then copy. An existing
    /// `dest` is replaced atomically.
    pub fn materialize(&self, h: &Hash, exec: bool, dest: &Path) -> Result<Placed> {
        let meta = self.meta.get(h).map(|m| *m).with_context(|| format!("blob {h} not in store"))?;
        self.touch(h);
        let parent = dest.parent().context("destination has no parent")?;
        let tmp = parent.join(format!(
            ".rargo-tmp-{}-{}",
            std::process::id(),
            self.tmp_seq.fetch_add(1, Ordering::Relaxed)
        ));
        let mode = if exec { 0o755 } else { 0o644 };
        let placed = (|| -> Result<Placed> {
            let same_fs = fs::metadata(parent)?.dev() == self.dev;
            if same_fs && self.reflink {
                let src = self.open_blob(h)?;
                let dst = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
                if reflink(&src, &dst) {
                    return Ok(Placed::Reflink);
                }
                drop(dst);
                fs::remove_file(&tmp)?;
            }
            if same_fs {
                let src = self.variant(h, exec)?;
                // Cheap integrity check before sharing the inode.
                let m = fs::metadata(&src)?;
                if m.len() != meta.size {
                    anyhow::bail!("blob {h} has the wrong size on disk");
                }
                if fs::hard_link(&src, &tmp).is_ok() {
                    return Ok(Placed::Hardlink);
                }
            }
            let src = self.open_blob(h)?;
            let mut dst = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
            let mut s = &src;
            io::copy(&mut s, &mut dst)?;
            Ok(Placed::Copy)
        })();
        match placed {
            Ok(p) => {
                fs::rename(&tmp, dest).with_context(|| format!("placing {}", dest.display()))?;
                Ok(p)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Evicts least-recently-used blobs until the store is under `budget`
    /// bytes. `pinned` blobs are never evicted. Returns bytes freed.
    pub fn evict_to(&self, budget: u64, pinned: &dyn Fn(&Hash) -> bool) -> u64 {
        self.evict(budget, 0, pinned, &|_| {})
    }

    /// Like [`Store::evict_to`], but leaves alone anything used in the last
    /// `min_age_secs` seconds (running builds touch what they use), and
    /// calls `on_evict` before removing each blob.
    pub fn evict(
        &self,
        budget: u64,
        min_age_secs: u64,
        pinned: &dyn Fn(&Hash) -> bool,
        on_evict: &dyn Fn(&Hash),
    ) -> u64 {
        let total = self.total_bytes();
        if total <= budget {
            return 0;
        }
        let cutoff = now_secs().saturating_sub(min_age_secs);
        let mut all: Vec<(u64, Hash, u64)> =
            self.meta.iter().map(|e| (e.value().last_used, *e.key(), e.value().size)).collect();
        all.sort_unstable();
        let mut freed = 0u64;
        for (seen_used, h, size) in all {
            if total - freed <= budget {
                break;
            }
            if pinned(&h) || (min_age_secs > 0 && seen_used > cutoff) {
                continue;
            }
            // Skip anything touched since the scan.
            if self.meta.get(&h).map(|m| m.last_used != seen_used).unwrap_or(true) {
                continue;
            }
            on_evict(&h);
            self.forget(&h);
            let _ = fs::remove_file(self.path_of(&h, false));
            let _ = fs::remove_file(self.path_of(&h, true));
            freed += size;
        }
        freed
    }

    /// Removes a blob outright (used when its content turns out to be bad).
    pub fn remove(&self, h: &Hash) {
        self.forget(h);
        let _ = fs::remove_file(self.path_of(h, false));
        let _ = fs::remove_file(self.path_of(h, true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&d.path().join("db.redb")).unwrap());
        let s = Store::open(&d.path().join("store"), db).unwrap();
        (d, s)
    }

    #[test]
    fn put_get_materialize() {
        let (d, s) = store();
        let h = s.put_bytes(b"hello").unwrap();
        assert_eq!(s.read(&h).unwrap(), b"hello");
        assert_eq!(h, Hash::of(b"hello"));
        let dest = d.path().join("out");
        let placed = s.materialize(&h, true, &dest).unwrap();
        assert_ne!(placed, Placed::Copy);
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
        assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o111, 0o111);
        // Replacing an existing destination works.
        let h2 = s.put_bytes(b"world").unwrap();
        s.materialize(&h2, false, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"world");
    }

    #[test]
    fn detects_write_through_hardlink() {
        let (d, s) = store();
        let h = s.put_bytes(b"original content").unwrap();
        let dest = d.path().join("linked");
        let placed = s.materialize(&h, false, &dest).unwrap();
        if placed != Placed::Hardlink {
            return; // reflink filesystems are immune
        }
        // Root can write through a 0444 hard link.
        let mut f = fs::OpenOptions::new().write(true).open(&dest).unwrap();
        f.write_all(b"CORRUPTED").unwrap();
        drop(f);
        assert!(s.open_blob(&h).is_err());
        assert!(!s.has(&h));
    }

    #[test]
    fn eviction_is_lru_and_respects_pins() {
        let (_d, s) = store();
        let a = s.put_bytes(&[1u8; 1000]).unwrap();
        let b = s.put_bytes(&[2u8; 1000]).unwrap();
        let c = s.put_bytes(&[3u8; 1000]).unwrap();
        s.meta.get_mut(&a).unwrap().last_used = 10;
        s.meta.get_mut(&b).unwrap().last_used = 20;
        s.meta.get_mut(&c).unwrap().last_used = 30;
        let freed = s.evict_to(1500, &|h| *h == a);
        assert_eq!(freed, 2000);
        assert!(s.has(&a) && !s.has(&b) && !s.has(&c));
    }

    #[test]
    fn move_dedupes() {
        let (_d, s) = store();
        let p = s.tmp_path();
        fs::write(&p, b"same").unwrap();
        let (h1, _) = s.put_file_move(&p, false).unwrap();
        let p2 = s.tmp_path();
        fs::write(&p2, b"same").unwrap();
        let (h2, _) = s.put_file_move(&p2, false).unwrap();
        assert_eq!(h1, h2);
        assert!(!p2.exists());
        assert_eq!(s.blob_count(), 1);
    }
}
