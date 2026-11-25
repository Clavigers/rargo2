//! A delivered content read by range: what is read is fetched as it is
//! read, and nothing is served that has not been proven against the
//! content's hash.
//!
//! The content's bytes gather in a sparse file next to its place in the
//! client's cache. They arrive by chunk (the server's recipe), and each
//! chunk is checked against its own hash; they are served by group (see
//! [`rargo_proto::outline`]), and a group is served only once all of it is
//! here and its chaining value is the one in the outline, which was proven
//! against the content hash when the reader was made. The recipe itself
//! is not trusted: a recipe that puts the wrong bytes in a place fails the
//! group's check.
//!
//! What is fetched: a read waits for the chunks under it, and its first
//! read in a region asks for the rest of that region behind them, so a
//! sequential read does not cost a round trip per chunk. The regions are
//! the parts that only a debugger or a backtrace reads (the server names
//! them: debug sections), each a region of its own, and everything else,
//! one region. Running a debug binary so fetches its code and data at its
//! first read and its debug information only for a backtrace.
//!
//! Once every group is proven, the file is the content: it is put in its
//! place in the cache, and from then on it is an ordinary cached content.

use rargo_proto::outline::{self, Cv, GROUP};
use rargo_proto::Digest;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// Fetches chunks for a [`Partial`]. `request` returns at once; each
/// chunk is handed back with [`Partial::deliver`] as it arrives, and the
/// ones that cannot be had with [`Partial::failed`].
pub trait Chunks: Send + Sync {
    fn request(&self, p: &Arc<Partial>, idx: Vec<u32>);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chunk {
    Missing,
    Asked,
    Here,
    /// The last request for it failed: a read that was waiting for it gets
    /// an error, and a read that comes later asks again.
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    Unproven,
    Good,
    /// All its chunks are here, each matching its recipe hash, and still
    /// the group is not the content: the recipe is wrong.
    Bad,
}

struct State {
    chunks: Vec<Chunk>,
    groups: Vec<Group>,
    good: usize,
    /// Whether the rest of a region has been asked for.
    started: Vec<bool>,
    /// The file is the content and is in its place in the cache.
    done: bool,
}

pub struct Partial {
    hash: Digest,
    size: u64,
    recipe: Vec<(Digest, u32)>,
    offsets: Vec<u64>,
    /// The region of each chunk: 0 for everything read at once, `1 + i`
    /// for the `i`th range read only on demand.
    region: Vec<u32>,
    cvs: Vec<Cv>,
    file: File,
    tmp: PathBuf,
    slot: PathBuf,
    st: Mutex<State>,
    changed: Condvar,
    source: Arc<dyn Chunks>,
}

impl Partial {
    /// A reader of the content `hash` of `size` bytes, to be put at `slot`
    /// in the cache once it is whole. Fails unless the outline proves the
    /// content and the recipe covers it.
    pub fn new(
        hash: Digest,
        size: u64,
        recipe: Vec<(Digest, u32)>,
        cvs: Vec<Cv>,
        lazy: &[(u64, u64)],
        slot: &Path,
        source: Arc<dyn Chunks>,
    ) -> std::io::Result<Arc<Partial>> {
        if !outline::proves(&cvs, size, &hash) {
            return Err(std::io::Error::other("the server's outline of the content does not give its hash"));
        }
        let mut offsets = Vec::with_capacity(recipe.len());
        let mut at = 0u64;
        for (_, len) in &recipe {
            offsets.push(at);
            at += *len as u64;
        }
        if at != size || recipe.iter().any(|(_, l)| *l == 0) {
            return Err(std::io::Error::other("the server's recipe of the content does not cover it"));
        }
        let region = offsets
            .iter()
            .zip(&recipe)
            .map(|(&o, (_, l))| {
                let end = o + *l as u64;
                let i = lazy.partition_point(|r| r.0 <= o);
                if i > 0 && lazy[i - 1].1 >= end { i as u32 } else { 0 }
            })
            .collect();
        let dir = slot.parent().ok_or_else(|| std::io::Error::other("no cache directory"))?;
        std::fs::create_dir_all(dir)?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = slot.with_file_name(format!(".partial-{}-{n}", std::process::id()));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&tmp)?;
        file.set_len(size)?;
        let groups = outline::groups(size);
        Ok(Arc::new(Partial {
            hash,
            size,
            st: Mutex::new(State {
                chunks: vec![Chunk::Missing; recipe.len()],
                groups: vec![Group::Unproven; groups],
                good: 0,
                started: vec![false; lazy.len() + 1],
                done: false,
            }),
            recipe,
            offsets,
            region,
            cvs,
            file,
            tmp,
            slot: slot.to_path_buf(),
            changed: Condvar::new(),
            source,
        }))
    }

    pub fn hash(&self) -> &Digest {
        &self.hash
    }

    pub fn recipe(&self) -> &[(Digest, u32)] {
        &self.recipe
    }

    pub fn offset(&self, i: u32) -> u64 {
        self.offsets[i as usize]
    }

    /// Whether the content is whole, proven and in its place in the cache.
    pub fn is_done(&self) -> bool {
        self.st.lock().unwrap().done
    }

    /// The chunks that hold bytes `start..end`.
    fn chunks_of(&self, start: u64, end: u64) -> std::ops::Range<usize> {
        assert!(start < end && end <= self.size);
        let first = self.offsets.partition_point(|&o| o <= start) - 1;
        let last = self.offsets.partition_point(|&o| o < end);
        first..last
    }

    /// Reads up to `len` bytes at `offset`, fetching and proving what is
    /// not here yet. Blocks until they are proven.
    pub fn read_at(self: &Arc<Self>, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let end = (offset + len as u64).min(self.size);
        if offset >= end {
            return Ok(Vec::new());
        }
        self.wait_for(offset, end)?;
        let mut out = vec![0u8; (end - offset) as usize];
        self.file.read_exact_at(&mut out, offset)?;
        Ok(out)
    }

    /// Fetches what is missing of the whole content and waits until it is
    /// in its place in the cache; says where that is.
    pub fn whole(self: &Arc<Self>) -> std::io::Result<PathBuf> {
        self.wait_for(0, self.size)?;
        assert!(self.is_done());
        Ok(self.slot.clone())
    }

    fn wait_for(self: &Arc<Self>, start: u64, end: u64) -> std::io::Result<()> {
        let (g0, g1) = ((start / GROUP) as usize, ((end - 1) / GROUP) as usize);
        let mut st = self.st.lock().unwrap();
        let mut waited = false;
        loop {
            if st.groups[g0..=g1].contains(&Group::Bad) {
                return Err(std::io::Error::other("the server sent content that does not match its hash"));
            }
            if st.groups[g0..=g1].iter().all(|g| *g == Group::Good) {
                return Ok(());
            }
            let mut ask = Vec::new();
            let mut failed = false;
            for g in g0..=g1 {
                if st.groups[g] != Group::Unproven {
                    continue;
                }
                let (gs, ge) = outline::group_range(g, self.size);
                for c in self.chunks_of(gs, ge) {
                    match st.chunks[c] {
                        Chunk::Missing => {
                            st.chunks[c] = Chunk::Asked;
                            ask.push(c as u32);
                        }
                        Chunk::Failed if waited => {
                            st.chunks[c] = Chunk::Missing;
                            failed = true;
                        }
                        Chunk::Failed => {
                            st.chunks[c] = Chunk::Asked;
                            ask.push(c as u32);
                        }
                        Chunk::Asked | Chunk::Here => {}
                    }
                }
            }
            if failed {
                // What was asked for anyway is released for the next read.
                for &c in &ask {
                    st.chunks[c as usize] = Chunk::Missing;
                }
                return Err(std::io::Error::other("part of the content could not be fetched"));
            }
            // The rest of the regions this read is in, after it.
            let mut rest = Vec::new();
            for &c in &ask {
                let r = self.region[c as usize] as usize;
                if !st.started[r] {
                    st.started[r] = true;
                    for (i, &cr) in self.region.iter().enumerate() {
                        if cr as usize == r && matches!(st.chunks[i], Chunk::Missing | Chunk::Failed) {
                            st.chunks[i] = Chunk::Asked;
                            rest.push(i as u32);
                        }
                    }
                }
            }
            if ask.is_empty() {
                st = self.changed.wait(st).unwrap();
                waited = true;
                continue;
            }
            drop(st);
            self.source.request(self, ask);
            if !rest.is_empty() {
                self.source.request(self, rest);
            }
            st = self.st.lock().unwrap();
            waited = true;
        }
    }

    /// Chunk `i` arrived. It is kept only if it is the chunk the recipe
    /// names; the groups it completes are proven.
    pub fn deliver(&self, i: u32, data: &[u8]) {
        let i = i as usize;
        let Some((hash, len)) = self.recipe.get(i) else { return };
        if data.len() != *len as usize || blake3::hash(data).as_bytes() != hash {
            self.failed(&[i as u32]);
            return;
        }
        {
            let st = self.st.lock().unwrap();
            if st.chunks[i] == Chunk::Here {
                return;
            }
        }
        if self.file.write_all_at(data, self.offsets[i]).is_err() {
            self.failed(&[i as u32]);
            return;
        }
        let start = self.offsets[i];
        let end = start + *len as u64;
        let mut st = self.st.lock().unwrap();
        st.chunks[i] = Chunk::Here;
        // The groups that may be complete now.
        let (g0, g1) = ((start / GROUP) as usize, ((end - 1) / GROUP) as usize);
        let ready: Vec<usize> = (g0..=g1)
            .filter(|&g| {
                let (gs, ge) = outline::group_range(g, self.size);
                st.groups[g] == Group::Unproven && self.chunks_of(gs, ge).all(|c| st.chunks[c] == Chunk::Here)
            })
            .collect();
        drop(st);
        let proven: Vec<(usize, bool)> = ready
            .into_iter()
            .map(|g| {
                let (gs, ge) = outline::group_range(g, self.size);
                let mut buf = vec![0u8; (ge - gs) as usize];
                let ok = self.file.read_exact_at(&mut buf, gs).is_ok() && outline::group_cv(g, &buf) == self.cvs[g];
                (g, ok)
            })
            .collect();
        let mut st = self.st.lock().unwrap();
        for (g, ok) in proven {
            if st.groups[g] == Group::Unproven {
                st.groups[g] = if ok { Group::Good } else { Group::Bad };
                if ok {
                    st.good += 1;
                } else {
                    eprintln!(
                        "rargo daemon: content {}: bytes {}..{} do not match its hash",
                        hex(&self.hash),
                        g as u64 * GROUP,
                        outline::group_range(g, self.size).1
                    );
                }
            }
        }
        if st.good == st.groups.len() && !st.done {
            // Every group is proven, so the file is the content.
            use std::os::unix::fs::PermissionsExt;
            let placed = std::fs::set_permissions(&self.tmp, std::fs::Permissions::from_mode(0o444))
                .and_then(|_| std::fs::rename(&self.tmp, &self.slot));
            match placed {
                Ok(()) => st.done = true,
                Err(e) => eprintln!("rargo daemon: content {}: could not be put in the cache: {e}", hex(&self.hash)),
            }
        }
        drop(st);
        self.changed.notify_all();
    }

    /// The chunks that could not be had. Their regions are asked for
    /// again at the next read in them.
    pub fn failed(&self, idx: &[u32]) {
        let mut st = self.st.lock().unwrap();
        for &i in idx {
            let Some(c) = st.chunks.get_mut(i as usize) else { continue };
            if *c != Chunk::Here {
                *c = Chunk::Failed;
                let r = self.region[i as usize] as usize;
                st.started[r] = false;
            }
        }
        drop(st);
        self.changed.notify_all();
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.st.get_mut().map(|s| s.done).unwrap_or(false) {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

pub fn hex(h: &Digest) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// Chunks of 50,000 bytes (so that chunks and groups do not line up),
    /// the last one shorter.
    fn recipe_of(data: &[u8]) -> Vec<(Digest, u32)> {
        data.chunks(50_000).map(|c| (*blake3::hash(c).as_bytes(), c.len() as u32)).collect()
    }

    /// A server answering from `data`, possibly wrongly, counting what it
    /// sends.
    struct Server {
        data: Vec<u8>,
        recipe: Vec<(Digest, u32)>,
        sent: AtomicU64,
        /// Bytes it sends wrong, at this offset of the content.
        corrupt_at: Option<u64>,
        /// Whether it sends nothing at all.
        down: std::sync::atomic::AtomicBool,
    }

    impl Chunks for Server {
        fn request(&self, p: &Arc<Partial>, idx: Vec<u32>) {
            if self.down.load(Ordering::SeqCst) {
                p.failed(&idx);
                return;
            }
            for i in idx {
                let o = p.offset(i) as usize;
                let mut c = self.data[o..o + self.recipe[i as usize].1 as usize].to_vec();
                if let Some(at) = self.corrupt_at {
                    if (o as u64..(o + c.len()) as u64).contains(&at) {
                        c[(at - o as u64) as usize] ^= 0xff;
                    }
                }
                self.sent.fetch_add(c.len() as u64, Ordering::SeqCst);
                p.deliver(i, &c);
            }
        }
    }

    fn setup(size: usize, lazy: &[(u64, u64)], corrupt_at: Option<u64>) -> (tempfile::TempDir, Arc<Server>, Arc<Partial>) {
        let dir = tempfile::tempdir().unwrap();
        let data = noise(size, size as u64);
        let recipe = recipe_of(&data);
        let hash = *blake3::hash(&data).as_bytes();
        let cvs = outline::outline(&data);
        let server = Arc::new(Server { data, recipe: recipe.clone(), sent: AtomicU64::new(0), corrupt_at, down: Default::default() });
        let p = Partial::new(hash, size as u64, recipe, cvs, lazy, &dir.path().join("ab").join("slot"), server.clone()).unwrap();
        (dir, server, p)
    }

    #[test]
    fn a_read_fetches_its_region_and_not_the_parts_read_on_demand() {
        let size = 3_000_000;
        // The middle third is read only on demand.
        let lazy = [(1_000_000, 2_000_000)];
        let (_dir, server, p) = setup(size, &lazy, None);
        let got = p.read_at(10, 100).unwrap();
        assert_eq!(got, server.data[10..110]);
        let sent = server.sent.load(Ordering::SeqCst);
        // Everything but the chunks wholly inside the lazy range.
        assert!(sent < size as u64 * 3 / 4, "sent {sent}");
        assert!(sent >= size as u64 * 2 / 3 - 100_000, "sent {sent}");
        assert!(!p.is_done());
        // Reading in the lazy range fetches it.
        let got = p.read_at(1_500_000, 70_000).unwrap();
        assert_eq!(got, server.data[1_500_000..1_570_000]);
        assert!(p.is_done());
        assert_eq!(server.sent.load(Ordering::SeqCst), size as u64, "each chunk once");
    }

    /// A file read through to the end is the content, and is then in the
    /// cache, read without the server.
    #[test]
    fn read_to_the_end_it_is_the_content_and_in_the_cache() {
        let size = 2_345_678;
        let (dir, server, p) = setup(size, &[(0, 2_345_678)], None);
        let mut all = Vec::new();
        let mut at = 0;
        loop {
            let part = p.read_at(at, 131_072).unwrap();
            if part.is_empty() {
                break;
            }
            at += part.len() as u64;
            all.extend(part);
        }
        assert_eq!(all, server.data);
        assert!(p.is_done());
        let slot = dir.path().join("ab").join("slot");
        assert_eq!(std::fs::read(&slot).unwrap(), server.data, "in its place in the cache");
        // Without the server, a reader still reads (it has it all), and the
        // cache has it.
        server.down.store(true, Ordering::SeqCst);
        assert_eq!(p.read_at(1_000_000, 10).unwrap(), server.data[1_000_000..1_000_010]);
        assert_eq!(p.whole().unwrap(), slot);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("ab")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("slot")]);
    }

    /// A server that answers with wrong bytes: the reader gets an error,
    /// never the bytes, and the rest of the content still reads.
    #[test]
    fn wrong_bytes_never_reach_the_reader() {
        let size = 1_500_000;
        let (_dir, server, p) = setup(size, &[], Some(700_000));
        assert!(p.read_at(699_990, 20).is_err(), "the chunk does not match its recipe hash");
        // Bytes 655,360.. are the group that holds the start of that chunk.
        assert!(p.read_at(656_000, 10).is_err(), "nor does anything in its groups");
        assert_eq!(p.read_at(650_000, 10).unwrap(), server.data[650_000..650_010], "the group before is good");
        assert_eq!(p.read_at(0, 1000).unwrap(), server.data[..1000]);
        assert_eq!(p.read_at(1_400_000, 1000).unwrap(), server.data[1_400_000..1_401_000]);
        assert!(!p.is_done());
    }

    /// A server whose recipe is wrong but self-consistent (its chunk hashes
    /// are of the wrong bytes): the group's value catches it.
    #[test]
    fn a_wrong_recipe_is_caught_by_the_outline() {
        let dir = tempfile::tempdir().unwrap();
        let size = 1_200_000;
        let data = noise(size, 3);
        let hash = *blake3::hash(&data).as_bytes();
        let cvs = outline::outline(&data);
        let mut wrong = data.clone();
        wrong[300_000] ^= 1;
        let recipe = recipe_of(&wrong);
        let server = Arc::new(Server { data: wrong, recipe: recipe.clone(), sent: AtomicU64::new(0), corrupt_at: None, down: Default::default() });
        let p = Partial::new(hash, size as u64, recipe, cvs, &[], &dir.path().join("slot"), server).unwrap();
        let g = (300_000 / GROUP) * GROUP;
        assert!(p.read_at(g, 10).is_err());
        assert!(p.read_at(300_000, 1).is_err());
        assert_eq!(p.read_at(0, 10).unwrap(), data[..10]);
        assert!(p.whole().is_err());
        assert!(!dir.path().join("slot").exists(), "nothing unproven in the cache");
    }

    #[test]
    fn an_outline_that_does_not_give_the_hash_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let data = noise(1_100_000, 9);
        let hash = *blake3::hash(&data).as_bytes();
        let mut cvs = outline::outline(&data);
        cvs[2][5] ^= 1;
        let server = Arc::new(Server { data: data.clone(), recipe: recipe_of(&data), sent: AtomicU64::new(0), corrupt_at: None, down: Default::default() });
        assert!(Partial::new(hash, data.len() as u64, recipe_of(&data), cvs, &[], &dir.path().join("slot"), server.clone()).is_err());
        let mut short = recipe_of(&data);
        short.pop();
        assert!(Partial::new(hash, data.len() as u64, short, outline::outline(&data), &[], &dir.path().join("slot"), server).is_err());
    }

    /// A failed fetch is an error for that read, and the next read asks
    /// again.
    #[test]
    fn a_failed_fetch_is_asked_again() {
        let (_dir, server, p) = setup(1_300_000, &[], None);
        server.down.store(true, Ordering::SeqCst);
        assert!(p.read_at(5, 5).is_err());
        server.down.store(false, Ordering::SeqCst);
        assert_eq!(p.read_at(5, 5).unwrap(), server.data[5..10]);
    }
}
