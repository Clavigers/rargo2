#[cfg(test)]
use rargo_machine::StateMachine;
use rargo_proto::outline::{self, Cv, GROUP};
use rargo_proto::Digest;
use rargo_net::io::file::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

pub trait Chunks: Send + Sync {
    fn request(&self, p: &Arc<Partial>, idx: Vec<u32>);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chunk {
    Missing,
    Asked,
    Here,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    Unproven,
    Good,
    Bad,
}

enum Read {
    Ready,
    BadHash,
    Failed,
    Wait,
    Fetch { needed: Vec<u32>, prefetch: Vec<u32> },
}

struct Proof {
    group: usize,
    start: u64,
    end: u64,
    cv: Cv,
}

impl Proof {
    fn verify(&self, bytes: &[u8]) -> bool {
        bytes.len() as u64 == self.end - self.start && outline::group_cv(self.group, bytes) == self.cv
    }
}

struct RangeState {
    size: u64,
    recipe: Arc<[(Digest, u32)]>,
    offsets: Arc<[u64]>,
    region: Vec<u32>,
    cvs: Vec<Cv>,
    chunks: Vec<Chunk>,
    groups: Vec<Group>,
    good: usize,
    started: Vec<bool>,
    done: bool,
}

pub struct Partial {
    hash: Digest,
    size: u64,
    recipe: Arc<[(Digest, u32)]>,
    offsets: Arc<[u64]>,
    file: File,
    tmp: PathBuf,
    slot: PathBuf,
    st: Mutex<RangeState>,
    changed: Condvar,
    source: Arc<dyn Chunks>,
}

impl RangeState {
    fn new(
        hash: &Digest,
        size: u64,
        recipe: Vec<(Digest, u32)>,
        cvs: Vec<Cv>,
        lazy: &[(u64, u64)],
    ) -> Result<Self, &'static str> {
        if !outline::proves(&cvs, size, hash) {
            return Err("the server's outline of the content does not give its hash");
        }
        let mut offsets = Vec::with_capacity(recipe.len());
        let mut at = 0u64;
        for (_, len) in &recipe {
            offsets.push(at);
            at += *len as u64;
        }
        if at != size || recipe.iter().any(|(_, l)| *l == 0) {
            return Err("the server's recipe of the content does not cover it");
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
        Ok(Self {
            size,
            chunks: vec![Chunk::Missing; recipe.len()],
            groups: vec![Group::Unproven; outline::groups(size)],
            good: 0,
            started: vec![false; lazy.len() + 1],
            done: false,
            recipe: recipe.into(),
            offsets: offsets.into(),
            region,
            cvs,
        })
    }

    fn chunks_of(&self, start: u64, end: u64) -> std::ops::Range<usize> {
        assert!(start < end && end <= self.size);
        let first = self.offsets.partition_point(|&o| o <= start) - 1;
        let last = self.offsets.partition_point(|&o| o < end);
        first..last
    }

    fn read(&mut self, start: u64, end: u64, waited: bool) -> Read {
        let (g0, g1) = ((start / GROUP) as usize, ((end - 1) / GROUP) as usize);
        if self.groups[g0..=g1].contains(&Group::Bad) {
            return Read::BadHash;
        }
        if self.groups[g0..=g1].iter().all(|g| *g == Group::Good) {
            return Read::Ready;
        }

        let mut needed = Vec::new();
        let mut failed = false;
        for g in g0..=g1 {
            if self.groups[g] != Group::Unproven {
                continue;
            }
            let (start, end) = outline::group_range(g, self.size);
            for c in self.chunks_of(start, end) {
                match self.chunks[c] {
                    Chunk::Failed if waited => {
                        self.chunks[c] = Chunk::Missing;
                        failed = true;
                    }
                    Chunk::Missing | Chunk::Failed => {
                        self.chunks[c] = Chunk::Asked;
                        needed.push(c as u32);
                    }
                    Chunk::Asked | Chunk::Here => {}
                }
            }
        }
        if failed {
            for &c in &needed {
                self.chunks[c as usize] = Chunk::Missing;
            }
            return Read::Failed;
        }
        if needed.is_empty() {
            return Read::Wait;
        }

        let mut prefetch = Vec::new();
        for &c in &needed {
            let r = self.region[c as usize] as usize;
            if !self.started[r] {
                self.started[r] = true;
                for (i, &region) in self.region.iter().enumerate() {
                    if region as usize == r && matches!(self.chunks[i], Chunk::Missing | Chunk::Failed) {
                        self.chunks[i] = Chunk::Asked;
                        prefetch.push(i as u32);
                    }
                }
            }
        }
        Read::Fetch { needed, prefetch }
    }

    fn valid_chunk((hash, len): &(Digest, u32), bytes: &[u8]) -> bool {
        bytes.len() == *len as usize && blake3::hash(bytes).as_bytes() == hash
    }

    fn needs_chunk(&self, chunk: u32) -> bool {
        self.chunks[chunk as usize] != Chunk::Here
    }

    fn stored(&mut self, chunk: u32) -> Vec<Proof> {
        let i = chunk as usize;
        self.chunks[i] = Chunk::Here;
        let start = self.offsets[i];
        let end = start + self.recipe[i].1 as u64;
        let (g0, g1) = ((start / GROUP) as usize, ((end - 1) / GROUP) as usize);
        (g0..=g1).filter_map(|g| {
            let (start, end) = outline::group_range(g, self.size);
            (self.groups[g] == Group::Unproven && self.chunks_of(start, end).all(|c| self.chunks[c] == Chunk::Here))
                .then_some(Proof { group: g, start, end, cv: self.cvs[g] })
        }).collect()
    }

    fn proved(&mut self, group: usize, valid: bool) -> Option<Group> {
        if self.groups[group] != Group::Unproven {
            return None;
        }
        let result = if valid { Group::Good } else { Group::Bad };
        self.groups[group] = result;
        self.good += usize::from(valid);
        Some(result)
    }

    fn can_place(&self) -> bool {
        self.good == self.groups.len() && !self.done
    }

    fn failed(&mut self, indices: &[u32]) {
        for &i in indices {
            let Some(c) = self.chunks.get_mut(i as usize) else { continue };
            if *c != Chunk::Here {
                *c = Chunk::Failed;
                self.started[self.region[i as usize] as usize] = false;
            }
        }
    }
}

#[cfg(test)]
impl StateMachine for RangeState {
    fn invariants(&self) {
        assert_eq!(self.offsets.len(), self.recipe.len(), "every chunk of the recipe has an offset");
        assert_eq!(self.chunks.len(), self.recipe.len(), "every chunk of the recipe has a state");
        assert_eq!(self.region.len(), self.chunks.len(), "every chunk has a region");
        assert!(self.region.iter().all(|&r| (r as usize) < self.started.len()), "every chunk's region is a started region");
        let good = self.groups.iter().filter(|&&g| g == Group::Good).count();
        assert_eq!(self.good, good, "the count of good groups is the number of good groups");
        assert!(!self.done || self.good == self.groups.len(), "a done file has only good groups");
        for (g, group) in self.groups.iter().enumerate() {
            let (start, end) = outline::group_range(g, self.size);
            assert!(
                *group == Group::Unproven || self.chunks_of(start, end).all(|c| self.chunks[c] == Chunk::Here),
                "group {g} is proven only once every chunk of it is here"
            );
        }
    }
}

impl Partial {
    pub fn new(
        hash: Digest,
        size: u64,
        recipe: Vec<(Digest, u32)>,
        cvs: Vec<Cv>,
        lazy: &[(u64, u64)],
        slot: &Path,
        source: Arc<dyn Chunks>,
    ) -> std::io::Result<Arc<Partial>> {
        let state = RangeState::new(&hash, size, recipe, cvs, lazy).map_err(std::io::Error::other)?;
        let dir = slot.parent().ok_or_else(|| std::io::Error::other("no cache directory"))?;
        rargo_net::io::file::create_dir_all(dir)?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = slot.with_file_name(format!(".partial-{}-{n}", std::process::id()));
        let file = rargo_net::io::file::OpenOptions::new().read(true).write(true).create_new(true).open(&tmp)?;
        file.set_len(size)?;
        Ok(Arc::new(Partial {
            hash,
            size,
            recipe: state.recipe.clone(),
            offsets: state.offsets.clone(),
            st: Mutex::new(state),
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

    pub fn is_done(&self) -> bool {
        self.st.lock().unwrap().done
    }

    pub fn read_at(self: &Arc<Self>, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let end = offset.saturating_add(len as u64).min(self.size);
        if offset >= end {
            return Ok(Vec::new());
        }
        self.wait_for(offset, end)?;
        let mut out = vec![0u8; (end - offset) as usize];
        self.file.read_exact_at(&mut out, offset)?;
        Ok(out)
    }

    pub fn whole(self: &Arc<Self>) -> std::io::Result<PathBuf> {
        self.wait_for(0, self.size)?;
        assert!(self.is_done());
        Ok(self.slot.clone())
    }

    fn wait_for(self: &Arc<Self>, start: u64, end: u64) -> std::io::Result<()> {
        let mut st = self.st.lock().unwrap();
        let mut waited = false;
        loop {
            match st.read(start, end, waited) {
                Read::Ready => return Ok(()),
                Read::BadHash => return Err(std::io::Error::other("the server sent content that does not match its hash")),
                Read::Failed => return Err(std::io::Error::other("part of the content could not be fetched")),
                Read::Wait => st = self.changed.wait(st).unwrap(),
                Read::Fetch { needed, prefetch } => {
                    drop(st);
                    self.source.request(self, needed);
                    if !prefetch.is_empty() {
                        self.source.request(self, prefetch);
                    }
                    st = self.st.lock().unwrap();
                }
            }
            waited = true;
        }
    }

    pub fn deliver(&self, i: u32, data: &[u8]) {
        let Some(recipe) = self.recipe.get(i as usize) else { return };
        if !RangeState::valid_chunk(recipe, data) {
            self.failed(&[i]);
            return;
        }
        if !self.st.lock().unwrap().needs_chunk(i) {
            return;
        }
        if self.file.write_all_at(data, self.offset(i)).is_err() {
            self.failed(&[i]);
            return;
        }
        let ready = self.st.lock().unwrap().stored(i);
        let groups: Vec<_> = ready.into_iter().map(|proof| {
            let mut buf = vec![0u8; (proof.end - proof.start) as usize];
            let valid = self.file.read_exact_at(&mut buf, proof.start).is_ok() && proof.verify(&buf);
            (proof, valid)
        }).collect();
        let mut st = self.st.lock().unwrap();
        for (proof, valid) in groups {
            if st.proved(proof.group, valid) == Some(Group::Bad) {
                eprintln!("rargo daemon: content {}: bytes {}..{} do not match its hash", hex(&self.hash), proof.start, proof.end);
            }
        }
        if st.can_place() {
            use std::os::unix::fs::PermissionsExt;
            let placed = rargo_net::io::file::set_permissions(&self.tmp, std::fs::Permissions::from_mode(0o444))
                .and_then(|_| rargo_net::io::file::rename(&self.tmp, &self.slot));
            match placed {
                Ok(()) => st.done = true,
                Err(e) => eprintln!("rargo daemon: content {}: could not be put in the cache: {e}", hex(&self.hash)),
            }
        }
        drop(st);
        self.changed.notify_all();
    }

    pub fn failed(&self, idx: &[u32]) {
        let mut st = self.st.lock().unwrap();
        st.failed(idx);
        drop(st);
        self.changed.notify_all();
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.st.get_mut().map(|s| s.done).unwrap_or(false) {
            let _ = rargo_net::io::file::remove_file(&self.tmp);
        }
    }
}

pub fn hex(h: &Digest) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(data: &[u8]) -> Vec<(Digest, u32)> {
        data.chunks(30_000).map(|c| (*blake3::hash(c).as_bytes(), c.len() as u32)).collect()
    }

    fn content(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i / 11 + i / 29) as u8).collect()
    }

    fn state(data: &[u8], lazy: &[(u64, u64)]) -> RangeState {
        let state = RangeState::new(blake3::hash(data).as_bytes(), data.len() as u64, recipe(data), outline::outline(data), lazy).unwrap();
        state.invariants();
        state
    }

    fn requested(state: &RangeState, read: Read) -> (Vec<u32>, Vec<u32>) {
        state.invariants();
        let Read::Fetch { needed, prefetch } = read else { panic!("expected a fetch") };
        (needed, prefetch)
    }

    fn deliver(state: &mut RangeState, chunks: impl IntoIterator<Item = u32>, data: &[u8], file: &mut [u8]) {
        for chunk in chunks {
            assert!(state.needs_chunk(chunk));
            let i = chunk as usize;
            let start = state.offsets[i] as usize;
            let end = start + state.recipe[i].1 as usize;
            assert!(RangeState::valid_chunk(&state.recipe[i], &data[start..end]));
            file[start..end].copy_from_slice(&data[start..end]);
            let proofs = state.stored(chunk);
            state.invariants();
            for proof in proofs {
                state.proved(proof.group, proof.verify(&file[proof.start as usize..proof.end as usize]));
                state.invariants();
            }
        }
    }

    #[test]
    fn overlapping_reads_share_fetches_and_failed_chunks_are_retried() {
        let data = content(GROUP as usize * 4);
        let mut state = state(&data, &[(GROUP * 2, GROUP * 4)]);
        let mut file = vec![0; data.len()];
        let read = state.read(0, 1, false);
        let (needed, prefetch) = requested(&state, read);
        assert!(matches!(state.read(1, 3, false), Read::Wait));
        state.invariants();

        state.failed(&needed);
        state.invariants();
        assert!(matches!(state.read(0, 1, true), Read::Failed));
        state.invariants();
        let read = state.read(0, 1, false);
        let (retry, extra) = requested(&state, read);
        assert_eq!(retry, needed);
        assert!(extra.is_empty());
        deliver(&mut state, retry.into_iter().chain(prefetch), &data, &mut file);
        assert!(matches!(state.read(1, 3, true), Read::Ready));
        state.invariants();
        assert_eq!(&file[..3], &data[..3]);
        assert!(!state.can_place());

        let read = state.read(0, data.len() as u64, false);
        let (needed, prefetch) = requested(&state, read);
        deliver(&mut state, needed.into_iter().chain(prefetch), &data, &mut file);
        assert!(matches!(state.read(0, data.len() as u64, false), Read::Ready));
        state.invariants();
        assert!(state.can_place());
        assert_eq!(file, data);
    }

    #[test]
    fn the_outline_controls_which_bytes_can_be_read() {
        let original = content(GROUP as usize * 3);
        let mut wrong = original.clone();
        wrong[GROUP as usize + 17] ^= 1;
        let correct_recipe = recipe(&original);
        let i = (GROUP as usize + 17) / 30_000;
        assert!(!RangeState::valid_chunk(&correct_recipe[i], &wrong[i * 30_000..(i + 1) * 30_000]));

        let hash = blake3::hash(&original);
        let mut state = RangeState::new(hash.as_bytes(), original.len() as u64, recipe(&wrong), outline::outline(&original), &[]).unwrap();
        state.invariants();
        let read = state.read(0, original.len() as u64, false);
        let (needed, prefetch) = requested(&state, read);
        let mut file = vec![0; original.len()];
        deliver(&mut state, needed.into_iter().chain(prefetch), &wrong, &mut file);
        assert!(matches!(state.read(GROUP, GROUP + 1, false), Read::BadHash));
        state.invariants();
        assert!(matches!(state.read(0, GROUP, false), Read::Ready));
        state.invariants();
        assert_eq!(file[..GROUP as usize], original[..GROUP as usize]);
        assert!(!state.can_place());

        let mut bad_outline = outline::outline(&original);
        bad_outline[0][0] ^= 1;
        assert!(RangeState::new(hash.as_bytes(), original.len() as u64, correct_recipe, bad_outline, &[]).is_err());
    }
}
