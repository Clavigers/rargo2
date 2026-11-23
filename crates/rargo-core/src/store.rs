//! The content-addressed store.
//!
//! Blobs are whole files named by their blake3 hash under `blobs/xx/`. A blob
//! may exist in two variants, plain (`<hex>`, mode 0444) and executable
//! (`<hex>.x`, mode 0555), because a hard link shares its inode's mode with
//! the file it is linked from.
//!
//! Blobs nobody has used for a while go cold: the file is cut into
//! content-defined chunks (see `chunk`), each chunk is kept once, compressed,
//! under `chunks/xx/`, and the whole file is deleted. A cold blob is put back
//! together the moment anything asks for it, so the rest of rargo never sees
//! the difference. This is what makes many near-identical large files cheap
//! to keep, above all the incremental compilation states of successive
//! snapshots of one crate.

use crate::chunk::ChunkRef;
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

/// A blob held as chunks.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ColdMeta {
    size: u64,
    last_used: u64,
    /// Bytes of chunk files that had to be written when it went cold (what
    /// keeping it cost at the time; an estimate of what dropping it frees).
    added: u64,
    recipe: Vec<ChunkRef>,
}

/// Blobs smaller than this stay whole: there is nothing to share.
const COLD_MIN_SIZE: u64 = 64 << 10;
const COLD_DISK_KEY: &[u8] = b"cold-disk-bytes";

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
    chunks: PathBuf,
    cold: DashMap<Hash, ColdMeta>,
    /// Bytes of chunk files on disk.
    cold_disk: AtomicU64,
    /// Readers: anything reading or adding chunks. Writer: chunk collection.
    cold_lock: std::sync::RwLock<()>,
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
        let chunks = root.join("chunks");
        fs::create_dir_all(&chunks)?;
        for i in 0..256 {
            let d = chunks.join(format!("{i:02x}"));
            if !d.exists() {
                fs::create_dir(&d)?;
            }
        }
        let cold = DashMap::new();
        db.for_each(Table::Cold, |k, v| {
            if k.len() == 32 {
                if let Ok(m) = bincode::deserialize::<ColdMeta>(v) {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(k);
                    cold.insert(Hash(h), m);
                }
            }
        });
        let cold_disk = match db.get(Table::Meta, COLD_DISK_KEY) {
            Some(v) if v.len() == 8 => u64::from_le_bytes(v[..8].try_into().unwrap()),
            _ => 0,
        };
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
            chunks,
            cold,
            cold_disk: AtomicU64::new(cold_disk),
            cold_lock: std::sync::RwLock::new(()),
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

    /// Bytes the store believes it occupies: whole blobs plus chunk files.
    pub fn total_bytes(&self) -> u64 {
        self.total.load(Ordering::Relaxed) + self.cold_disk.load(Ordering::Relaxed)
    }

    /// `(blobs, bytes they stand for, bytes of chunk files on disk)` of the
    /// cold tier.
    pub fn cold_stats(&self) -> (usize, u64, u64) {
        let logical = self.cold.iter().map(|e| e.value().size).sum();
        (self.cold.len(), logical, self.cold_disk.load(Ordering::Relaxed))
    }

    /// Every blob hash in the store, whole or cold.
    pub fn all_hashes(&self) -> Vec<Hash> {
        self.meta.iter().map(|e| *e.key()).chain(self.cold.iter().map(|e| *e.key())).collect()
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
        self.meta.contains_key(h) || self.cold.contains_key(h)
    }

    pub fn size_of(&self, h: &Hash) -> Option<u64> {
        self.meta.get(h).map(|m| m.size).or_else(|| self.cold.get(h).map(|m| m.size))
    }

    fn chunk_path(&self, c: &Hash) -> PathBuf {
        let hex = c.hex();
        let mut p = self.chunks.join(&hex[..2]);
        p.push(&hex[2..]);
        p
    }

    fn set_cold_disk(&self) {
        self.db.put(Table::Meta, COLD_DISK_KEY, &self.cold_disk.load(Ordering::Relaxed).to_le_bytes());
    }

    /// Stores one chunk if it is not there yet. Returns the bytes written.
    fn write_chunk(&self, c: &Hash, data: &[u8]) -> io::Result<u64> {
        let dst = self.chunk_path(c);
        if dst.exists() {
            return Ok(0);
        }
        let packed = zstd::bulk::compress(data, 3)?;
        let (tag, body): (u8, &[u8]) = if packed.len() < data.len() { (1, &packed) } else { (0, data) };
        let tmp = self.tmp_path();
        {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o444).open(&tmp)?;
            f.write_all(&[tag])?;
            f.write_all(body)?;
        }
        // Link rather than rename: of two writers of one chunk, one counts.
        let linked = fs::hard_link(&tmp, &dst);
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => Ok(body.len() as u64 + 1),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn read_chunk(&self, c: &ChunkRef) -> io::Result<Vec<u8>> {
        let raw = fs::read(self.chunk_path(&c.hash))?;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("chunk {} is damaged", c.hash));
        let data = match raw.split_first() {
            Some((0, body)) => body.to_vec(),
            Some((1, body)) => zstd::bulk::decompress(body, c.len as usize).map_err(|_| bad())?,
            _ => return Err(bad()),
        };
        if data.len() != c.len as usize {
            return Err(bad());
        }
        Ok(data)
    }

    /// Turns a whole blob into chunks and deletes the file. Blobs that are
    /// small, or linked into a worktree (deleting the store's name would
    /// free nothing), are left alone. `before` is called once the blob is
    /// certain to go, before its file does. Returns the bytes freed.
    pub fn freeze(&self, h: &Hash, before: &dyn Fn(&Hash)) -> io::Result<Option<u64>> {
        let _g = self.cold_lock.read().unwrap();
        let Some(meta) = self.meta.get(h).map(|m| *m) else { return Ok(None) };
        if meta.size < COLD_MIN_SIZE {
            return Ok(None);
        }
        let variants: Vec<PathBuf> =
            [self.path_of(h, false), self.path_of(h, true)].into_iter().filter(|p| p.exists()).collect();
        let Some(first) = variants.first() else { return Ok(None) };
        for v in &variants {
            if fs::metadata(v)?.nlink() != 1 {
                return Ok(None);
            }
        }
        let f = File::open(first)?;
        let m = f.metadata()?;
        if m.len() != meta.size || (variants.len() == 1 && mtime_ns(&m) != meta.mtime_ns) {
            // Not the file that was stored; `open_blob` deals with that.
            return Ok(None);
        }
        let recipe = crate::chunk::chunk_file(&f)?;
        let mut added = 0u64;
        let mut off = 0u64;
        let mut buf = Vec::new();
        for c in &recipe {
            if !self.chunk_path(&c.hash).exists() {
                use std::os::unix::fs::FileExt;
                buf.resize(c.len as usize, 0);
                f.read_exact_at(&mut buf, off)?;
                if Hash::of(&buf) != c.hash {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("blob {h} changed while going cold")));
                }
                added += self.write_chunk(&c.hash, &buf)?;
            }
            off += c.len as u64;
        }
        self.cold_disk.fetch_add(added, Ordering::Relaxed);
        // Used again while we were at it: leave it whole.
        if self.meta.get(h).map(|x| x.last_used != meta.last_used).unwrap_or(true) {
            return Ok(None);
        }
        before(h);
        let cm = ColdMeta { size: meta.size, last_used: meta.last_used, added, recipe };
        self.db.put(Table::Cold, &h.0, &bincode::serialize(&cm).unwrap());
        self.cold.insert(*h, cm);
        self.forget_hot(h);
        for v in &variants {
            let _ = fs::remove_file(v);
        }
        self.set_cold_disk();
        Ok(Some((meta.size * variants.len() as u64).saturating_sub(added)))
    }

    /// Freezes every whole blob not used for `min_idle_secs`. Returns
    /// `(blobs, bytes freed)`.
    pub fn freeze_idle(&self, min_idle_secs: u64, before: &dyn Fn(&Hash)) -> (usize, u64) {
        let cutoff = now_secs().saturating_sub(min_idle_secs);
        let mut idle: Vec<(u64, Hash)> = self
            .meta
            .iter()
            .filter(|e| e.value().last_used <= cutoff && e.value().size >= COLD_MIN_SIZE)
            .map(|e| (e.value().last_used, *e.key()))
            .collect();
        idle.sort_unstable();
        let (mut n, mut freed) = (0, 0);
        for (_, h) in idle {
            if let Ok(Some(b)) = self.freeze(&h, before) {
                n += 1;
                freed += b;
            }
        }
        (n, freed)
    }

    /// Puts a cold blob back together as a whole file.
    fn thaw(&self, h: &Hash) -> io::Result<()> {
        let _g = self.cold_lock.read().unwrap();
        let Some(cm) = self.cold.get(h).map(|m| m.clone()) else { return Ok(()) };
        let tmp = self.tmp_path();
        let built = (|| -> io::Result<()> {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o444).open(&tmp)?;
            let mut hasher = blake3::Hasher::new();
            for c in &cm.recipe {
                let data = self.read_chunk(c)?;
                hasher.update(&data);
                f.write_all(&data)?;
            }
            if Hash(*hasher.finalize().as_bytes()) != *h {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("cold blob {h} does not add up")));
            }
            Ok(())
        })();
        if let Err(e) = built {
            let _ = fs::remove_file(&tmp);
            // Whatever is wrong with it will not get better: the blob is
            // gone, and what needed it is rebuilt.
            self.drop_cold(h);
            return Err(e);
        }
        // `install` records the blob as whole and drops the cold entry.
        self.install(&tmp, *h, false, cm.size)
    }

    fn ensure_whole(&self, h: &Hash) -> io::Result<()> {
        if !self.meta.contains_key(h) && self.cold.contains_key(h) {
            self.thaw(h)?;
        }
        Ok(())
    }

    fn drop_cold(&self, h: &Hash) -> Option<u64> {
        let (_, cm) = self.cold.remove(h)?;
        self.db.delete(Table::Cold, &h.0);
        Some(cm.added)
    }

    /// Deletes chunk files no cold blob refers to.
    pub fn collect_chunks(&self) -> (usize, u64) {
        let _g = self.cold_lock.write().unwrap();
        let mut live: std::collections::HashSet<Hash> = std::collections::HashSet::new();
        for e in self.cold.iter() {
            live.extend(e.value().recipe.iter().map(|c| c.hash));
        }
        let (mut n, mut freed, mut kept) = (0usize, 0u64, 0u64);
        for i in 0..256 {
            let prefix = format!("{i:02x}");
            let Ok(rd) = fs::read_dir(self.chunks.join(&prefix)) else { continue };
            for ent in rd.flatten() {
                let name = ent.file_name();
                let len = ent.metadata().map(|m| m.len()).unwrap_or(0);
                let hash = name.to_str().and_then(|rest| Hash::from_hex(&format!("{prefix}{rest}")));
                match hash {
                    Some(c) if live.contains(&c) => kept += len,
                    _ => {
                        if fs::remove_file(ent.path()).is_ok() {
                            n += 1;
                            freed += len;
                        }
                    }
                }
            }
        }
        self.cold_disk.store(kept, Ordering::Relaxed);
        self.set_cold_disk();
        (n, freed)
    }

    fn record(&self, h: Hash, size: u64, mtime_ns: i64) {
        // A blob that is whole again is not cold any more.
        self.drop_cold(&h);
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
        } else if let Some(mut m) = self.cold.get_mut(h) {
            m.last_used = now;
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
        self.ensure_whole(h)?;
        let meta = self.meta.get(h).map(|m| *m);
        let Some(meta) = meta else {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("blob {h} not in store")));
        };
        let Some(p) = self.existing(h, false) else {
            if self.cold.contains_key(h) {
                // It went cold between the two looks.
                return self.open_blob(h);
            }
            self.forget(h);
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("blob {h} missing")));
        };
        let f = match File::open(&p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && self.cold.contains_key(h) => return self.open_blob(h),
            r => r?,
        };
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

    fn forget_hot(&self, h: &Hash) {
        if let Some((_, m)) = self.meta.remove(h) {
            self.total.fetch_sub(m.size, Ordering::Relaxed);
        }
        self.db.delete(Table::Blobs, &h.0);
    }

    fn forget(&self, h: &Hash) {
        self.forget_hot(h);
        self.drop_cold(h);
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

    /// Places a blob at `dest` as a file of its own (writable, not sharing
    /// an inode with the store): a reflink where the filesystem has them,
    /// a copy otherwise. For paths that other tools write in place.
    pub fn materialize_owned(&self, h: &Hash, exec: bool, dest: &Path) -> Result<Placed> {
        self.ensure_whole(h)?;
        let parent = dest.parent().context("destination has no parent")?;
        let tmp = parent.join(format!(
            ".rargo-tmp-{}-{}",
            std::process::id(),
            self.tmp_seq.fetch_add(1, Ordering::Relaxed)
        ));
        let mode = if exec { 0o755 } else { 0o644 };
        let placed = (|| -> Result<Placed> {
            let src = self.open_blob(h)?;
            let dst = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
            Ok(if clone_or_copy(&src, &dst)? { Placed::Reflink } else { Placed::Copy })
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

    /// Whether `dest` is already a hard link to the blob (nothing to place
    /// and nothing to read to find that out).
    pub fn is_linked(&self, h: &Hash, exec: bool, dest: &Path) -> bool {
        let (Ok(d), Ok(s)) = (fs::metadata(dest), fs::metadata(self.path_of(h, exec))) else { return false };
        d.dev() == s.dev() && d.ino() == s.ino()
    }

    /// Places a blob at `dest` without using extra space where the
    /// filesystems allow it: reflink, then hard link, then copy. An existing
    /// `dest` is replaced atomically.
    pub fn materialize(&self, h: &Hash, exec: bool, dest: &Path) -> Result<Placed> {
        self.ensure_whole(h)?;
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
        // Whole blobs free their size; cold ones roughly what they added.
        let mut all: Vec<(u64, Hash, u64, bool)> = self
            .meta
            .iter()
            .map(|e| (e.value().last_used, *e.key(), e.value().size, false))
            .chain(self.cold.iter().map(|e| (e.value().last_used, *e.key(), e.value().added, true)))
            .collect();
        all.sort_unstable();
        let mut freed = 0u64;
        let mut dropped_cold = false;
        for (seen_used, h, size, cold) in all {
            if total.saturating_sub(freed) <= budget {
                break;
            }
            if pinned(&h) || (min_age_secs > 0 && seen_used > cutoff) {
                continue;
            }
            if cold {
                if self.cold.get(&h).map(|m| m.last_used != seen_used).unwrap_or(true) {
                    continue;
                }
                on_evict(&h);
                self.drop_cold(&h);
                dropped_cold = true;
                freed += size;
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
        if dropped_cold {
            self.collect_chunks();
        }
        freed
    }

    /// Removes every blob that `live` does not contain and that has not been
    /// used for `min_age_secs`. Returns `(blobs, bytes)` removed.
    pub fn sweep(
        &self,
        live: &std::collections::HashSet<Hash>,
        min_age_secs: u64,
        on_evict: &dyn Fn(&Hash),
    ) -> (usize, u64) {
        let cutoff = now_secs().saturating_sub(min_age_secs);
        let dead: Vec<(Hash, u64)> = self
            .meta
            .iter()
            .filter(|e| !live.contains(e.key()) && (min_age_secs == 0 || e.value().last_used <= cutoff))
            .map(|e| (*e.key(), e.value().size))
            .collect();
        let mut bytes = 0;
        for (h, size) in &dead {
            on_evict(h);
            self.forget(h);
            let _ = fs::remove_file(self.path_of(h, false));
            let _ = fs::remove_file(self.path_of(h, true));
            bytes += size;
        }
        let dead_cold: Vec<Hash> = self
            .cold
            .iter()
            .filter(|e| !live.contains(e.key()) && (min_age_secs == 0 || e.value().last_used <= cutoff))
            .map(|e| *e.key())
            .collect();
        for h in &dead_cold {
            on_evict(h);
            self.drop_cold(h);
        }
        if !dead_cold.is_empty() {
            bytes += self.collect_chunks().1;
        }
        (dead.len() + dead_cold.len(), bytes)
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
    fn cold_blobs_come_back_and_share_chunks() {
        let (_d, s) = store();
        // Two large files that differ in a few bytes in the middle.
        let mut a = vec![0u8; 3 << 20];
        let mut x = 0x1234_5678_9abc_def0u64;
        for b in a.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        let mut b = a.clone();
        b[1 << 20..(1 << 20) + 16].copy_from_slice(b"sixteen new byte");
        let (ha, hb) = (s.put_bytes(&a).unwrap(), s.put_bytes(&b).unwrap());
        let small = s.put_bytes(b"tiny").unwrap();
        assert_eq!(s.freeze(&small, &|_| {}).unwrap(), None);
        let whole = s.total_bytes();
        assert!(s.freeze(&ha, &|_| {}).unwrap().is_some());
        assert!(s.freeze(&hb, &|_| {}).unwrap().is_some());
        assert_eq!(s.cold_stats().0, 2);
        // The second file cost little beyond the first.
        assert!(s.total_bytes() < whole * 6 / 10, "{} vs {}", s.total_bytes(), whole);
        assert!(s.has(&ha) && s.size_of(&hb) == Some(b.len() as u64));
        // Reading brings a blob back, byte for byte.
        assert_eq!(s.read(&hb).unwrap(), b);
        assert_eq!(s.cold_stats().0, 1);
        // Dropping the other and collecting leaves no chunks behind.
        s.remove(&ha);
        s.collect_chunks();
        assert_eq!(s.cold_stats(), (0, 0, 0));
        assert_eq!(s.read(&hb).unwrap(), b);
        assert!(s.read(&ha).is_err());
    }

    #[test]
    fn linked_blobs_stay_whole_and_cold_ones_are_evicted() {
        let (d, s) = store();
        let data = vec![7u8; 1 << 20];
        let h = s.put_bytes(&data).unwrap();
        let out = d.path().join("out");
        assert_eq!(s.materialize(&h, false, &out).unwrap(), Placed::Hardlink);
        assert_eq!(s.freeze(&h, &|_| {}).unwrap(), None);
        std::fs::remove_file(&out).unwrap();
        assert!(s.freeze(&h, &|_| {}).unwrap().is_some());
        // Materializing a cold blob thaws it first.
        s.materialize(&h, true, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data);
        std::fs::remove_file(&out).unwrap();
        let _ = std::fs::remove_file(s.path_of(&h, true));
        assert!(s.freeze(&h, &|_| {}).unwrap().is_some());
        s.evict_to(0, &|_| false);
        assert!(!s.has(&h));
        assert_eq!(s.cold_stats(), (0, 0, 0));
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
