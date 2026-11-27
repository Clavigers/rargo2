use crate::io::file::PathRing as _;
use crate::chunk::ChunkRef;
use crate::db::{Db, Table};
use crate::hash::Hash;
use crate::store_index::{ColdRecord, HotMeta, StoreIndex, Touch};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use crate::io::file::{File, OpenOptions};
use std::cell::{Cell, OnceCell, RefCell};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BlobMeta {
    pub size: u64,
    pub mtime_ns: i64,
    pub last_used: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ColdMeta {
    size: u64,
    last_used: u64,
    added: u64,
    recipe: Vec<ChunkRef>,
}

const COLD_DISK_KEY: &[u8] = b"cold-disk-bytes";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    Reflink,
    Hardlink,
    Copy,
}

/// The content store. The loop thread owns it: what it holds is decided there, and
/// work for the pool is done by `StoreFiles` on names and records the loop hands out.
pub struct Store {
    files: StoreFiles,
    db: Arc<Db>,
    tmp_seq: Cell<u64>,
    cold: ColdGate,
    index: RefCell<StoreIndex>,
}

/// Where the store's files lie and how they are written. It holds no state, so any thread may use it.
struct StoreFiles {
    root: PathBuf,
    blobs: PathBuf,
    tmp: PathBuf,
    reflink: bool,
    dev: u64,
    chunks: PathBuf,
}

/// Keeps chunk collection apart from freezing and thawing, which write and read chunks
/// the index does not list yet. Fibers of the loop wait here while the pool does the work.
#[derive(Default)]
struct ColdGate {
    working: Cell<usize>,
    collecting: Cell<bool>,
    changed: OnceCell<crate::io::Notify>,
}

struct ColdWork<'a>(&'a ColdGate);
struct ColdCollection<'a>(&'a ColdGate);

impl ColdGate {
    fn wait(&self) {
        self.changed.get_or_init(crate::io::Notify::new).notified();
    }

    fn wake(&self) {
        if let Some(changed) = self.changed.get() {
            changed.notify_waiters();
        }
    }

    /// Enters as one that freezes or thaws, once no collection runs.
    fn work(&self) -> ColdWork<'_> {
        while self.collecting.get() {
            self.wait();
        }
        self.working.set(self.working.get() + 1);
        ColdWork(self)
    }

    /// Enters as the collector, once nothing freezes, thaws or collects.
    fn collect(&self) -> ColdCollection<'_> {
        while self.collecting.get() || self.working.get() > 0 {
            self.wait();
        }
        self.collecting.set(true);
        ColdCollection(self)
    }
}

impl Drop for ColdWork<'_> {
    fn drop(&mut self) {
        self.0.working.set(self.0.working.get() - 1);
        self.0.wake();
    }
}

impl Drop for ColdCollection<'_> {
    fn drop(&mut self) {
        self.0.collecting.set(false);
        self.0.wake();
    }
}

impl From<ColdMeta> for ColdRecord {
    fn from(c: ColdMeta) -> ColdRecord {
        ColdRecord { size: c.size, last_used: c.last_used, added: c.added, recipe: c.recipe }
    }
}

impl From<ColdRecord> for ColdMeta {
    fn from(c: ColdRecord) -> ColdMeta {
        ColdMeta { size: c.size, last_used: c.last_used, added: c.added, recipe: c.recipe }
    }
}

fn blob_meta(m: HotMeta) -> BlobMeta {
    BlobMeta { size: m.size, mtime_ns: m.mtime_ns, last_used: m.last_used }
}

fn now_secs() -> u64 {
    crate::io::system_now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn reflink(src: &impl AsRawFd, dst: &impl AsRawFd) -> bool {
    crate::io::file::reflink_fd(src.as_raw_fd(), dst.as_raw_fd())
}

pub fn clone_or_copy(src: &impl AsRawFd, dst: &impl AsRawFd) -> io::Result<bool> {
    if reflink(src, dst) {
        return Ok(true);
    }
    crate::io::file::copy_fd(src.as_raw_fd(), dst.as_raw_fd())?;
    Ok(false)
}

impl Store {
    pub fn open(root: &Path, db: Arc<Db>) -> Result<Store> {
        let blobs = root.join("blobs");
        let tmp = root.join("tmp");
        crate::io::file::create_dir_all(&blobs)?;
        let _ = crate::io::file::remove_dir_all(&tmp);
        crate::io::file::create_dir_all(&tmp)?;
        for i in 0..256 {
            let d = blobs.join(format!("{i:02x}"));
            if !d.ring_exists() {
                crate::io::file::create_dir(&d)?;
            }
        }
        let chunks = root.join("chunks");
        crate::io::file::create_dir_all(&chunks)?;
        for i in 0..256 {
            let d = chunks.join(format!("{i:02x}"));
            if !d.ring_exists() {
                crate::io::file::create_dir(&d)?;
            }
        }
        let mut index = StoreIndex::new();
        db.for_each(Table::Blobs, |k, v| {
            if k.len() == 32 {
                if let Ok(m) = bincode::deserialize::<BlobMeta>(v) {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(k);
                    index.load_hot(Hash(h), HotMeta { size: m.size, mtime_ns: m.mtime_ns, last_used: m.last_used });
                }
            }
        });
        db.for_each(Table::Cold, |k, v| {
            if k.len() == 32 {
                if let Ok(m) = bincode::deserialize::<ColdMeta>(v) {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(k);
                    index.load_cold(Hash(h), m.into());
                }
            }
        });
        match db.get(Table::Meta, COLD_DISK_KEY) {
            Some(v) if v.len() == 8 => index.set_cold_disk(u64::from_le_bytes(v[..8].try_into().unwrap())),
            _ => {}
        }
        let dev = crate::io::file::metadata(&blobs)?.dev();
        let reflink = {
            let a = tmp.join("reflink-probe-a");
            let b = tmp.join("reflink-probe-b");
            let mut fa = File::create(&a)?;
            fa.write_all(b"probe")?;
            let fa = File::open(&a)?;
            let fb = File::create(&b)?;
            let ok = reflink(&fa, &fb);
            let _ = crate::io::file::remove_file(&a);
            let _ = crate::io::file::remove_file(&b);
            ok
        };
        Ok(Store {
            files: StoreFiles { root: root.to_path_buf(), blobs, tmp, reflink, dev, chunks },
            db,
            tmp_seq: Cell::new(0),
            cold: ColdGate::default(),
            index: RefCell::new(index),
        })
    }

    pub fn root(&self) -> &Path {
        &self.files.root
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub fn supports_reflink(&self) -> bool {
        self.files.reflink
    }

    pub fn total_bytes(&self) -> u64 {
        self.index.borrow_mut().total_bytes()
    }

    pub fn cold_stats(&self) -> (usize, u64, u64) {
        self.index.borrow_mut().cold_stats()
    }

    pub fn all_hashes(&self) -> Vec<Hash> {
        self.index.borrow_mut().all_hashes()
    }

    pub fn blob_count(&self) -> usize {
        self.index.borrow_mut().whole_count()
    }

    fn path_of(&self, h: &Hash, exec: bool) -> PathBuf {
        self.files.path_of(h, exec)
    }

    fn next_tmp(&self) -> u64 {
        let n = self.tmp_seq.get();
        self.tmp_seq.set(n + 1);
        n
    }

    pub fn tmp_path(&self) -> PathBuf {
        let n = self.next_tmp();
        self.files.tmp.join(format!("t{}-{n}", crate::io::process::id()))
    }

    pub fn tmp_dir(&self, name: &str) -> io::Result<PathBuf> {
        let p = self.files.tmp.join(name);
        crate::io::file::create_dir_all(&p)?;
        Ok(p)
    }

    pub fn has(&self, h: &Hash) -> bool {
        self.index.borrow_mut().has(h)
    }

    pub fn blobs(self: &Arc<Self>) -> Arc<dyn Blobs> {
        self.clone()
    }

    pub fn size_of(&self, h: &Hash) -> Option<u64> {
        self.index.borrow_mut().size_of(h)
    }

    fn set_cold_disk(&self) {
        let bytes = self.index.borrow_mut().cold_stats().2;
        self.db.put(Table::Meta, COLD_DISK_KEY, &bytes.to_le_bytes());
    }

    pub fn freeze(&self, h: &Hash, before: &dyn Fn(&Hash)) -> io::Result<Option<u64>> {
        let _working = self.cold.work();
        let Some(start) = self.index.borrow_mut().begin_freeze(h) else {
            return Ok(None);
        };
        let (files, tmp) = (&self.files, self.tmp_path());
        let chunked = crate::io::cpu(|| files.write_cold_chunks(h, start.size, start.mtime_ns, &tmp));
        let (recipe, added, variants) = match chunked {
            Ok(Some(c)) => c,
            other => {
                self.index.borrow_mut().abort_freeze(h);
                return other.map(|_| None);
            }
        };
        let record = {
            let mut ix = self.index.borrow_mut();
            ix.chunks_written(added);
            ix.finish_freeze(h, recipe, added)
        };
        let Some(record) = record else {
            self.set_cold_disk();
            return Ok(None);
        };
        before(h);
        let size = record.size;
        self.db.put(Table::Cold, &h.0, &bincode::serialize(&ColdMeta::from(record)).unwrap());
        self.db.delete(Table::Blobs, &h.0);
        for v in &variants {
            let _ = crate::io::file::remove_file(v);
        }
        self.set_cold_disk();
        Ok(Some((size * variants.len() as u64).saturating_sub(added)))
    }

    pub fn freeze_idle(&self, min_idle_secs: u64, before: &dyn Fn(&Hash)) -> (usize, u64) {
        let idle = self.index.borrow_mut().freeze_candidates(now_secs(), min_idle_secs);
        let (mut n, mut freed) = (0, 0);
        for h in idle {
            if let Ok(Some(b)) = self.freeze(&h, before) {
                n += 1;
                freed += b;
            }
        }
        (n, freed)
    }

    fn thaw(&self, h: &Hash) -> io::Result<()> {
        let _working = self.cold.work();
        let (_, started) = self.index.borrow_mut().begin_thaw(h);
        let Some((size, recipe)) = started else {
            return Ok(());
        };
        let (files, tmp) = (&self.files, self.tmp_path());
        let built = crate::io::cpu(|| files.build_thawed(&tmp, h, size, &recipe));
        if let Err(e) = built {
            let _ = crate::io::file::remove_file(&tmp);
            if self.index.borrow_mut().fail_thaw(h) {
                self.db.delete(Table::Cold, &h.0);
            }
            return Err(e);
        }
        let installed = self.install(&tmp, *h, false, size);
        if installed.is_err() {
            self.index.borrow_mut().abort_thaw(h);
        }
        installed
    }

    fn ensure_whole(&self, h: &Hash) -> io::Result<()> {
        let cold = {
            let ix = self.index.borrow();
            !ix.is_whole(h) && ix.is_cold(h)
        };
        if cold {
            self.thaw(h)?;
        }
        Ok(())
    }

    pub fn generation(&self) -> u64 {
        self.index.borrow_mut().generation()
    }

    pub fn collect_chunks(&self) -> (usize, u64) {
        self.collect_chunks_once(|| {})
    }

    /// Removes the chunks no cold blob names. `held` runs once nothing else freezes or thaws.
    fn collect_chunks_once(&self, held: impl FnOnce()) -> (usize, u64) {
        let _collecting = self.cold.collect();
        held();
        let live = self.index.borrow().live_chunks();
        let (mut n, mut freed, mut kept) = (0usize, 0u64, 0u64);
        for i in 0..256 {
            let prefix = format!("{i:02x}");
            let Ok(rd) = crate::io::file::read_dir(self.files.chunks.join(&prefix)) else {
                continue;
            };
            for ent in rd.flatten() {
                let name = ent.file_name();
                let len = ent.metadata().map(|m| m.len()).unwrap_or(0);
                let hash = name.to_str().and_then(|rest| Hash::from_hex(&format!("{prefix}{rest}")));
                match hash {
                    Some(c) if live.contains(&c) => kept += len,
                    _ => {
                        if crate::io::file::remove_file(ent.path()).is_ok() {
                            n += 1;
                            freed += len;
                        }
                    }
                }
            }
        }
        self.index.borrow_mut().chunks_collected(kept);
        self.set_cold_disk();
        (n, freed)
    }

    fn record(&self, h: Hash, size: u64, mtime_ns: i64) {
        let m = BlobMeta { size, mtime_ns, last_used: now_secs() };
        self.db.put(Table::Blobs, &h.0, &bincode::serialize(&m).unwrap());
        let put = self.index.borrow_mut().put(h, size, mtime_ns, m.last_used);
        if put.was_cold {
            self.db.delete(Table::Cold, &h.0);
        }
    }

    pub fn touch(&self, h: &Hash) {
        let touched = self.index.borrow_mut().touch(h, now_secs());
        if let Touch::Persist(m) = touched {
            self.db.put(Table::Blobs, &h.0, &bincode::serialize(&blob_meta(m)).unwrap());
        }
    }

    fn existing(&self, h: &Hash, exec: bool) -> Option<PathBuf> {
        self.files.existing(h, exec)
    }

    /// The content hash and size of `bytes`, hashed on the pool when there is much to hash.
    fn hash_bytes(bytes: &[u8]) -> Hash {
        if bytes.len() >= 1 << 20 { crate::io::cpu(|| Hash::of(bytes)) } else { Hash::of(bytes) }
    }

    pub fn open_blob(&self, h: &Hash) -> io::Result<File> {
        self.ensure_whole(h)?;
        let meta = self.index.borrow_mut().hot(h);
        let Some(meta) = meta else {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("blob {h} not in store")));
        };
        let Some(p) = self.existing(h, false) else {
            if self.index.borrow_mut().is_cold(h) {
                return self.open_blob(h);
            }
            self.forget(h);
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("blob {h} missing")));
        };
        let f = match File::open(&p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && self.index.borrow_mut().is_cold(h) => {
                return self.open_blob(h);
            }
            r => r?,
        };
        let m = f.metadata()?;
        if m.len() != meta.size || m.mtime_ns() != meta.mtime_ns {
            let (actual, _) = crate::io::cpu(|| Hash::of_file(&f))?;
            if actual != *h {
                self.forget(h);
                let _ = crate::io::file::remove_file(self.path_of(h, false));
                let _ = crate::io::file::remove_file(self.path_of(h, true));
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("blob {h} was modified on disk and has been dropped"),
                ));
            }
            self.record(*h, m.len(), m.mtime_ns());
        }
        self.touch(h);
        Ok(f)
    }

    fn forget(&self, h: &Hash) {
        let gone = self.index.borrow_mut().forget(h);
        self.db.delete(Table::Blobs, &h.0);
        if gone.cold {
            self.db.delete(Table::Cold, &h.0);
        }
    }

    pub fn read(&self, h: &Hash) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut f = self.open_blob(h)?;
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        debug_assert_eq!(Hash::of(&v), *h);
        Ok(v)
    }

    pub fn put_bytes(&self, bytes: &[u8]) -> io::Result<Hash> {
        self.put_hashed_bytes(Self::hash_bytes(bytes), bytes)
    }

    /// Stores `bytes`, whose content hash the caller has worked out to be `h`.
    pub fn put_hashed_bytes(&self, h: Hash, bytes: &[u8]) -> io::Result<Hash> {
        debug_assert_eq!(Hash::of(bytes), h);
        if self.has(&h) {
            self.touch(&h);
            return Ok(h);
        }
        let tmp = self.tmp_path();
        {
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o444).open(&tmp)?;
            f.write_all(bytes)?;
        }
        self.install(&tmp, h, false, bytes.len() as u64)?;
        assert!(self.has(&h));
        Ok(h)
    }

    pub fn put_file_move(&self, path: &Path, exec: bool) -> io::Result<(Hash, u64)> {
        let (h, size) = crate::io::cpu(|| Hash::of_path(path))?;
        if self.has(&h) && self.existing(&h, exec).is_some() {
            let _ = crate::io::file::remove_file(path);
            self.touch(&h);
            return Ok((h, size));
        }
        self.install(path, h, exec, size)?;
        Ok((h, size))
    }

    pub fn put_file_link(&self, path: &Path, exec: bool) -> io::Result<(Hash, u64)> {
        let (h, size) = crate::io::cpu(|| Hash::of_path(path))?;
        if self.has(&h) && self.existing(&h, exec).is_some() {
            self.touch(&h);
            return Ok((h, size));
        }
        let tmp = self.tmp_path();
        crate::io::file::hard_link(path, &tmp)?;
        self.install(&tmp, h, exec, size)?;
        Ok((h, size))
    }

    pub fn put_file_copy(&self, src: &File, exec: bool) -> io::Result<(Hash, u64)> {
        let tmp = self.tmp_path();
        let dst = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
        clone_or_copy(src, &dst)?;
        drop(dst);
        let r = self.put_file_move(&tmp, exec);
        if r.is_err() {
            let _ = crate::io::file::remove_file(&tmp);
        }
        r
    }

    pub fn install_hashed(&self, tmp: &Path, h: Hash, size: u64) -> io::Result<()> {
        if self.has(&h) && self.existing(&h, false).is_some() {
            let _ = crate::io::file::remove_file(tmp);
            self.touch(&h);
            return Ok(());
        }
        self.install(tmp, h, false, size)
    }

    /// Stores each file by moving it in, as `put_file_move` does one, hashing and placing them on the pool together.
    pub fn put_files_move(&self, items: Vec<(PathBuf, bool)>) -> Vec<io::Result<(Hash, u64)>> {
        let files = &self.files;
        let hashed: Vec<(PathBuf, bool, io::Result<(Hash, u64)>)> = crate::io::cpu(|| {
            crate::io::thread::par_map(items, |(path, exec)| {
                let hashed = Hash::of_path(&path);
                (path, exec, hashed)
            })
        });
        // The index says which contents are held; the pool then keeps or places each file.
        let known: Vec<bool> =
            hashed.iter().map(|(_, _, hashed)| hashed.as_ref().is_ok_and(|(h, _)| self.has(h))).collect();
        let placed: Vec<io::Result<(Hash, u64, Option<i64>)>> = crate::io::cpu(|| {
            crate::io::thread::par_map(hashed.into_iter().zip(known).collect(), |((path, exec, hashed), known)| {
                let (h, size) = hashed?;
                if known && files.existing(&h, exec).is_some() {
                    let _ = crate::io::file::remove_file(&path);
                    return Ok((h, size, None));
                }
                Ok((h, size, Some(files.place(&path, &h, exec, size)?)))
            })
        });
        placed
            .into_iter()
            .map(|placed| {
                let (h, size, mtime_ns) = placed?;
                match mtime_ns {
                    Some(mtime_ns) => self.record(h, size, mtime_ns),
                    None => self.touch(&h),
                }
                Ok((h, size))
            })
            .collect()
    }

    fn install(&self, tmp: &Path, h: Hash, exec: bool, size: u64) -> io::Result<()> {
        let mtime_ns = self.files.place(tmp, &h, exec, size)?;
        self.record(h, size, mtime_ns);
        assert!(self.index.borrow().is_whole(&h));
        Ok(())
    }

    fn variant(&self, h: &Hash, exec: bool) -> io::Result<PathBuf> {
        let want = self.path_of(h, exec);
        if want.ring_exists() {
            return Ok(want);
        }
        let other = self.path_of(h, !exec);
        let src = File::open(&other)?;
        let tmp = self.tmp_path();
        let dst =
            OpenOptions::new().write(true).create_new(true).mode(if exec { 0o555 } else { 0o444 }).open(&tmp)?;
        clone_or_copy(&src, &dst)?;
        drop(dst);
        crate::io::file::set_mode(&tmp, if exec { 0o555 } else { 0o444 })?;
        crate::io::file::rename(&tmp, &want)?;
        Ok(want)
    }

    pub fn materialize_owned(&self, h: &Hash, exec: bool, dest: &Path) -> Result<Placed> {
        self.ensure_whole(h)?;
        let parent = dest.parent().context("destination has no parent")?;
        let tmp =
            parent.join(format!(".rargo-tmp-{}-{}", crate::io::process::id(), self.next_tmp()));
        let mode = if exec { 0o755 } else { 0o644 };
        let placed = (|| -> Result<Placed> {
            let src = self.open_blob(h)?;
            let dst = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
            Ok(if clone_or_copy(&src, &dst)? { Placed::Reflink } else { Placed::Copy })
        })();
        match placed {
            Ok(p) => {
                crate::io::file::rename(&tmp, dest).with_context(|| format!("placing {}", dest.display()))?;
                Ok(p)
            }
            Err(e) => {
                let _ = crate::io::file::remove_file(&tmp);
                Err(e)
            }
        }
    }

    pub fn is_linked(&self, h: &Hash, exec: bool, dest: &Path) -> bool {
        let (Ok(d), Ok(s)) = (crate::io::file::metadata(dest), crate::io::file::metadata(self.path_of(h, exec))) else {
            return false;
        };
        d.dev() == s.dev() && d.ino() == s.ino()
    }

    pub fn materialize(&self, h: &Hash, exec: bool, dest: &Path) -> Result<Placed> {
        self.ensure_whole(h)?;
        let meta = self.index.borrow_mut().hot(h).with_context(|| format!("blob {h} not in store"))?;
        self.touch(h);
        let parent = dest.parent().context("destination has no parent")?;
        let tmp =
            parent.join(format!(".rargo-tmp-{}-{}", crate::io::process::id(), self.next_tmp()));
        let mode = if exec { 0o755 } else { 0o644 };
        let placed = (|| -> Result<Placed> {
            let same_fs = crate::io::file::metadata(parent)?.dev() == self.files.dev;
            if same_fs && self.files.reflink {
                let src = self.open_blob(h)?;
                let dst = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
                if reflink(&src, &dst) {
                    return Ok(Placed::Reflink);
                }
                drop(dst);
                crate::io::file::remove_file(&tmp)?;
            }
            if same_fs {
                let src = self.variant(h, exec)?;
                let m = crate::io::file::metadata(&src)?;
                if m.len() != meta.size {
                    anyhow::bail!("blob {h} has the wrong size on disk");
                }
                if crate::io::file::hard_link(&src, &tmp).is_ok() {
                    return Ok(Placed::Hardlink);
                }
            }
            let src = self.open_blob(h)?;
            let dst = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
            crate::io::file::copy_fd(src.as_raw_fd(), dst.as_raw_fd())?;
            Ok(Placed::Copy)
        })();
        match placed {
            Ok(p) => {
                crate::io::file::rename(&tmp, dest).with_context(|| format!("placing {}", dest.display()))?;
                Ok(p)
            }
            Err(e) => {
                let _ = crate::io::file::remove_file(&tmp);
                Err(e)
            }
        }
    }

    pub fn pin_all(&self, hashes: &[Hash]) -> bool {
        self.index.borrow_mut().pin_all(hashes)
    }

    pub fn unpin_all(&self, hashes: &[Hash]) {
        self.index.borrow_mut().unpin_all(hashes)
    }

    pub fn is_pinned(&self, h: &Hash) -> bool {
        self.index.borrow_mut().is_pinned(h)
    }

    pub fn evict_to(&self, budget: u64, pinned: &dyn Fn(&Hash) -> bool) -> u64 {
        self.evict(budget, 0, pinned, &|_| {})
    }

    pub fn evict(
        &self,
        budget: u64,
        min_age_secs: u64,
        pinned: &dyn Fn(&Hash) -> bool,
        on_evict: &dyn Fn(&Hash),
    ) -> u64 {
        let gone = self.index.borrow_mut().evict(budget, min_age_secs, now_secs(), pinned);
        self.drop_evicted(&gone, on_evict);
        if gone.iter().any(|e| e.cold) {
            self.collect_chunks();
        }
        gone.iter().map(|e| e.freed).sum()
    }

    fn drop_evicted(&self, gone: &[crate::store_index::Evicted], on_evict: &dyn Fn(&Hash)) {
        for e in gone {
            on_evict(&e.hash);
            self.db.delete(Table::Blobs, &e.hash.0);
            if e.cold {
                self.db.delete(Table::Cold, &e.hash.0);
            } else {
                let _ = crate::io::file::remove_file(self.path_of(&e.hash, false));
                let _ = crate::io::file::remove_file(self.path_of(&e.hash, true));
            }
        }
    }

    pub fn sweep(
        &self,
        live: &std::collections::BTreeSet<Hash>,
        min_age_secs: u64,
        on_evict: &dyn Fn(&Hash),
    ) -> (usize, u64) {
        let gone = self.index.borrow_mut().sweep(&|h| live.contains(h), min_age_secs, now_secs());
        self.drop_evicted(&gone, on_evict);
        let mut bytes: u64 = gone.iter().map(|e| e.freed).sum();
        if gone.iter().any(|e| e.cold) {
            bytes += self.collect_chunks().1;
        }
        (gone.len(), bytes)
    }

    pub fn remove(&self, h: &Hash) {
        self.forget(h);
        let _ = crate::io::file::remove_file(self.path_of(h, false));
        let _ = crate::io::file::remove_file(self.path_of(h, true));
    }
}

impl StoreFiles {
    fn path_of(&self, h: &Hash, exec: bool) -> PathBuf {
        let hex = h.hex();
        let mut p = self.blobs.join(&hex[..2]);
        assert_eq!(hex.len(), 64);
        p.push(if exec { format!("{}.x", &hex[2..]) } else { hex[2..].to_string() });
        p
    }

    fn chunk_path(&self, c: &Hash) -> PathBuf {
        let hex = c.hex();
        let mut p = self.chunks.join(&hex[..2]);
        p.push(&hex[2..]);
        p
    }

    fn existing(&self, h: &Hash, exec: bool) -> Option<PathBuf> {
        let p = self.path_of(h, exec);
        if p.ring_exists() {
            return Some(p);
        }
        let q = self.path_of(h, !exec);
        q.ring_exists().then_some(q)
    }

    /// Moves the file at `tmp` to where blob `h` lies and gives the time it then carries.
    fn place(&self, tmp: &Path, h: &Hash, exec: bool, size: u64) -> io::Result<i64> {
        let dst = self.path_of(h, exec);
        crate::io::file::set_mode(tmp, if exec { 0o555 } else { 0o444 })?;
        crate::io::file::rename(tmp, &dst)?;
        let m = crate::io::file::metadata(&dst)?;
        assert_eq!(m.len(), size, "the installed file is the size the caller hashed");
        Ok(m.mtime_ns())
    }

    /// Writes chunk `c` unless it is there, by way of the temporary file `tmp`, and gives the bytes it added.
    fn write_chunk(&self, c: &Hash, data: &[u8], tmp: &Path) -> io::Result<u64> {
        let dst = self.chunk_path(c);
        if dst.ring_exists() {
            return Ok(0);
        }
        let packed = zstd::bulk::compress(data, 3)?;
        assert_eq!(Hash::of(data), *c, "a chunk is stored under its own hash");
        let (tag, body): (u8, &[u8]) = if packed.len() < data.len() { (1, &packed) } else { (0, data) };
        assert!(body.len() <= data.len());
        {
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o444).open(tmp)?;
            f.write_all(&[tag])?;
            f.write_all(body)?;
        }
        let linked = crate::io::file::hard_link(tmp, &dst);
        let _ = crate::io::file::remove_file(tmp);
        match linked {
            Ok(()) => Ok(body.len() as u64 + 1),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn read_chunk(&self, c: &ChunkRef) -> io::Result<Vec<u8>> {
        let raw = crate::io::file::read(self.chunk_path(&c.hash))?;
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

    /// Writes the chunks of whole blob `h` that are not stored yet. `tmp` is a name the loop
    /// gave this call alone; each chunk is written by way of it, one after the other.
    #[allow(clippy::type_complexity)]
    fn write_cold_chunks(
        &self,
        h: &Hash,
        size: u64,
        mtime: i64,
        tmp: &Path,
    ) -> io::Result<Option<(Vec<ChunkRef>, u64, Vec<PathBuf>)>> {
        let variants: Vec<PathBuf> =
            [self.path_of(h, false), self.path_of(h, true)].into_iter().filter(|p| p.ring_exists()).collect();
        let Some(first) = variants.first() else {
            return Ok(None);
        };
        for v in &variants {
            if crate::io::file::metadata(v)?.nlink() != 1 {
                return Ok(None);
            }
        }
        let f = File::open(first)?;
        let m = f.metadata()?;
        if m.len() != size || (variants.len() == 1 && m.mtime_ns() != mtime) {
            return Ok(None);
        }
        let recipe = crate::chunk::chunk_file(&f)?;
        assert_eq!(recipe.iter().map(|c| c.len as u64).sum::<u64>(), size, "the recipe covers the blob");
        let mut added = 0u64;
        let mut off = 0u64;
        let mut buf = Vec::new();
        for c in &recipe {
            if !self.chunk_path(&c.hash).ring_exists() {
                buf.resize(c.len as usize, 0);
                f.read_exact_at(&mut buf, off)?;
                if Hash::of(&buf) != c.hash {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("blob {h} changed while going cold"),
                    ));
                }
                added += self.write_chunk(&c.hash, &buf, tmp)?;
            }
            off += c.len as u64;
        }
        assert_eq!(off, size);
        Ok(Some((recipe, added, variants)))
    }

    /// Puts cold blob `h` together from its chunks in the file `tmp` and checks that it adds up.
    fn build_thawed(&self, tmp: &Path, h: &Hash, size: u64, recipe: &[ChunkRef]) -> io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o444).open(tmp)?;
        let mut hasher = blake3::Hasher::new();
        let mut written = 0u64;
        for c in recipe {
            let data = self.read_chunk(c)?;
            assert_eq!(data.len(), c.len as usize);
            hasher.update(&data);
            f.write_all(&data)?;
            written += data.len() as u64;
        }
        assert_eq!(written, size, "a cold recipe covers its blob");
        if Hash(*hasher.finalize().as_bytes()) != *h {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("cold blob {h} does not add up")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{file, sim};

    fn store() -> (PathBuf, Store) {
        let d = sim::tempdir();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let s = Store::open(&d.join("store"), db).unwrap();
        (d, s)
    }

    #[test]
    fn put_get_materialize() {
        sim::test(|| {
            let (d, s) = store();
            let h = s.put_bytes(b"hello").unwrap();
            assert_eq!(s.read(&h).unwrap(), b"hello");
            assert_eq!(h, Hash::of(b"hello"));
            let dest = d.join("out");
            let placed = s.materialize(&h, true, &dest).unwrap();
            assert_ne!(placed, Placed::Copy);
            assert_eq!(file::read(&dest).unwrap(), b"hello");
            assert_eq!(file::metadata(&dest).unwrap().mode() & 0o111, 0o111);
            let h2 = s.put_bytes(b"world").unwrap();
            s.materialize(&h2, false, &dest).unwrap();
            assert_eq!(file::read(&dest).unwrap(), b"world");
        });
    }

    #[test]
    fn detects_write_through_hardlink() {
        sim::test(|| {
            let (d, mut s) = store();
            s.files.reflink = false;
            let dest = d.join("linked");
            // Keep a writable handle while the store moves the inode and makes it
            // read-only. The materialized hardlink then shares that same inode.
            let mut f = File::create(&dest).unwrap();
            f.write_all(b"original content").unwrap();
            let (h, _) = s.put_file_move(&dest, false).unwrap();
            let placed = s.materialize(&h, false, &dest).unwrap();
            assert_eq!(placed, Placed::Hardlink);
            std::io::Seek::rewind(&mut f).unwrap();
            f.write_all(b"CORRUPTED").unwrap();
            drop(f);
            assert_eq!(s.open_blob(&h).err().unwrap().kind(), io::ErrorKind::InvalidData);
            assert!(!s.has(&h));
        });
    }

    fn noise(len: usize) -> Vec<u8> {
        // Eight bytes a step: these blobs are megabytes long.
        let mut a = vec![0u8; len.next_multiple_of(8)];
        let mut x = 0x1234_5678_9abc_def0u64;
        for b in a.as_chunks_mut::<8>().0 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x.to_le_bytes();
        }
        a.truncate(len);
        a
    }

    #[test]
    fn cold_blobs_come_back_and_share_chunks() {
        sim::test(|| {
            let (_d, s) = store();
            let a = noise(3 << 20);
            let mut b = a.clone();
            b[1 << 20..(1 << 20) + 16].copy_from_slice(b"sixteen new byte");
            let (ha, hb) = (s.put_bytes(&a).unwrap(), s.put_bytes(&b).unwrap());
            let small = s.put_bytes(b"tiny").unwrap();
            assert_eq!(s.freeze(&small, &|_| {}).unwrap(), None);
            let whole = s.total_bytes();
            assert!(s.freeze(&ha, &|_| {}).unwrap().is_some());
            assert!(s.freeze(&hb, &|_| {}).unwrap().is_some());
            assert_eq!(s.cold_stats().0, 2);
            assert!(s.total_bytes() < whole * 6 / 10, "{} vs {}", s.total_bytes(), whole);
            assert!(s.has(&ha) && s.size_of(&hb) == Some(b.len() as u64));
            assert_eq!(s.read(&hb).unwrap(), b);
            assert_eq!(s.cold_stats().0, 1);
            s.remove(&ha);
            s.collect_chunks();
            assert_eq!(s.cold_stats(), (0, 0, 0));
            assert_eq!(s.read(&hb).unwrap(), b);
            assert!(s.read(&ha).is_err());
        });
    }

    // A fiber that collects chunks holds the cold gate while it parks on its file work.
    // A fiber that thaws meanwhile parks at the gate until the collection is done, and
    // the loop keeps running both. That takes a real loop, where file work parks.
    #[test]
    fn a_thaw_while_chunks_are_collected_waits_for_the_collection() {
        sim::test(|| {
            let s = Arc::new(Store::open(&sim::tempdir().join("store"), Arc::new(Db::open_in_memory().unwrap())).unwrap());
            let data = noise(crate::store_index::COLD_MIN_SIZE as usize);
            let h = s.put_bytes(&data).unwrap();
            assert!(s.freeze(&h, &|_| {}).unwrap().is_some());
            file::write(s.files.chunks.join("00").join("dead"), b"x").unwrap();
            let (locked, lock_held) = crate::io::oneshot::channel();
            let (resume, resume_collection) = crate::io::oneshot::channel();
            let gc = {
                let s = Arc::clone(&s);
                crate::io::spawn(move || {
                    s.collect_chunks_once(|| {
                        locked.send(()).unwrap();
                        resume_collection.recv().unwrap();
                    })
                })
            };
            lock_held.recv().unwrap();
            assert!(s.cold.collecting.get(), "the collector holds the cold gate");
            // This fiber can only release the collector after the thaw yields.
            let mut release = crate::io::spawn(move || resume.send(()).unwrap());
            let read = s.read(&h).unwrap();
            release.join().unwrap();
            let (removed, _) = { gc }.join().unwrap();
            assert_eq!(removed, 1);
            assert_eq!(read, data, "the thawed blob reads back whole");
        });
    }

    #[test]
    fn linked_blobs_stay_whole_and_cold_ones_are_evicted() {
        sim::test(|| {
            let (d, s) = store();
            let data = vec![7u8; 1 << 20];
            let h = s.put_bytes(&data).unwrap();
            let out = d.join("out");
            assert_eq!(s.materialize(&h, false, &out).unwrap(), Placed::Hardlink);
            assert_eq!(s.freeze(&h, &|_| {}).unwrap(), None);
            file::remove_file(&out).unwrap();
            assert!(s.freeze(&h, &|_| {}).unwrap().is_some());
            s.materialize(&h, true, &out).unwrap();
            assert_eq!(file::read(&out).unwrap(), data);
            file::remove_file(&out).unwrap();
            let _ = file::remove_file(s.path_of(&h, true));
            assert!(s.freeze(&h, &|_| {}).unwrap().is_some());
            s.evict_to(0, &|_| false);
            assert!(!s.has(&h));
            assert_eq!(s.cold_stats(), (0, 0, 0));
        });
    }

    #[test]
    fn move_dedupes() {
        sim::test(|| {
            let (_d, s) = store();
            let p = s.tmp_path();
            file::write(&p, b"same").unwrap();
            let (h1, _) = s.put_file_move(&p, false).unwrap();
            let p2 = s.tmp_path();
            file::write(&p2, b"same").unwrap();
            let (h2, _) = s.put_file_move(&p2, false).unwrap();
            assert_eq!(h1, h2);
            assert!(!p2.ring_exists());
            assert_eq!(s.blob_count(), 1);
        });
    }
}

pub trait Blobs {
    fn put_bytes(&self, bytes: &[u8]) -> io::Result<Hash>;
    fn read(&self, h: &Hash) -> io::Result<Vec<u8>>;
    fn has(&self, h: &Hash) -> bool;
    fn touch(&self, h: &Hash);
}

impl Blobs for Store {
    fn put_bytes(&self, bytes: &[u8]) -> io::Result<Hash> {
        Store::put_bytes(self, bytes)
    }

    fn read(&self, h: &Hash) -> io::Result<Vec<u8>> {
        Store::read(self, h)
    }

    fn has(&self, h: &Hash) -> bool {
        Store::has(self, h)
    }

    fn touch(&self, h: &Hash) {
        Store::touch(self, h)
    }
}
