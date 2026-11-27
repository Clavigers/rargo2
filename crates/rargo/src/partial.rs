#[cfg(test)]
use rargo_machine::StateMachine;
use rargo_proto::outline::{self, Cv, GROUP};
use rargo_proto::Digest;
use rargo_net::io::file::File;
use std::path::{Path, PathBuf};
use rargo_net::io;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

pub trait Chunks {
    fn request(&self, p: &Rc<Partial>, idx: Vec<u32>);
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

#[derive(Debug)]
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
    placing: bool,
    done: bool,
}

pub struct Partial {
    hash: Digest,
    size: u64,
    recipe: Arc<[(Digest, u32)]>,
    offsets: Arc<[u64]>,
    file: Arc<File>,
    tmp: PathBuf,
    slot: PathBuf,
    st: RefCell<RangeState>,
    changed: io::Notify,
    source: Rc<dyn Chunks>,
}

/// The checked outline and the open file of a content that is about to be read in parts.
pub struct Prepared {
    state: RangeState,
    file: File,
    tmp: PathBuf,
    slot: PathBuf,
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
            placing: false,
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
        self.good == self.groups.len() && !self.done && !self.placing
    }

    fn begin_place(&mut self) -> bool {
        let place = self.can_place();
        self.placing |= place;
        place
    }

    fn placed(&mut self, done: bool) {
        assert!(self.placing && !self.done, "a file is put in the cache once at a time");
        self.placing = false;
        self.done = done;
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
        assert!(!self.placing || (self.good == self.groups.len() && !self.done), "only a whole file not yet in the cache is being put there");
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
    /// Checks the server's outline and recipe and opens the file the content is gathered in. `n` tells this file from the others this process gathers.
    pub fn prepare(
        hash: &Digest,
        size: u64,
        recipe: Vec<(Digest, u32)>,
        cvs: Vec<Cv>,
        lazy: &[(u64, u64)],
        slot: &Path,
        n: u64,
    ) -> std::io::Result<Prepared> {
        let state = RangeState::new(hash, size, recipe, cvs, lazy).map_err(std::io::Error::other)?;
        let dir = slot.parent().ok_or_else(|| std::io::Error::other("no cache directory"))?;
        rargo_net::io::file::create_dir_all(dir)?;
        let tmp = slot.with_file_name(format!(".partial-{}-{n}", rargo_net::io::process::id()));
        let file = rargo_net::io::file::OpenOptions::new().read(true).write(true).create_new(true).open(&tmp)?;
        file.set_len(size)?;
        Ok(Prepared { state, file, tmp, slot: slot.to_path_buf() })
    }

    pub fn new(hash: Digest, prepared: Prepared, source: Rc<dyn Chunks>) -> Rc<Partial> {
        let Prepared { state, file, tmp, slot } = prepared;
        Rc::new(Partial {
            hash,
            size: state.size,
            recipe: state.recipe.clone(),
            offsets: state.offsets.clone(),
            st: RefCell::new(state),
            file: Arc::new(file),
            tmp,
            slot,
            changed: io::Notify::new(),
            source,
        })
    }

    pub fn hash(&self) -> &Digest {
        &self.hash
    }

    pub fn recipe(&self) -> &Arc<[(Digest, u32)]> {
        &self.recipe
    }

    pub fn offset(&self, i: u32) -> u64 {
        self.offsets[i as usize]
    }

    pub fn is_done(&self) -> bool {
        self.st.borrow().done
    }

    /// Whether `bytes` are the chunk the recipe names at `i`.
    pub fn valid_chunk(recipe: &[(Digest, u32)], i: u32, bytes: &[u8]) -> bool {
        recipe.get(i as usize).is_some_and(|chunk| RangeState::valid_chunk(chunk, bytes))
    }

    pub fn read_at(self: &Rc<Self>, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let end = offset.saturating_add(len as u64).min(self.size);
        if offset >= end {
            return Ok(Vec::new());
        }
        self.wait_for(offset, end)?;
        let mut out = vec![0u8; (end - offset) as usize];
        self.file.read_exact_at(&mut out, offset)?;
        Ok(out)
    }

    pub fn whole(self: &Rc<Self>) -> std::io::Result<PathBuf> {
        self.wait_for(0, self.size)?;
        while self.st.borrow().placing {
            self.changed.notified();
        }
        assert!(self.is_done());
        Ok(self.slot.clone())
    }

    fn wait_for(self: &Rc<Self>, start: u64, end: u64) -> std::io::Result<()> {
        let mut waited = false;
        loop {
            let read = self.st.borrow_mut().read(start, end, waited);
            match read {
                Read::Ready => return Ok(()),
                Read::BadHash => return Err(std::io::Error::other("the server sent content that does not match its hash")),
                Read::Failed => return Err(std::io::Error::other("part of the content could not be fetched")),
                Read::Wait => self.changed.notified(),
                Read::Fetch { needed, prefetch } => {
                    self.source.request(self, needed);
                    if !prefetch.is_empty() {
                        self.source.request(self, prefetch);
                    }
                }
            }
            waited = true;
        }
    }

    /// Takes chunk `i` of the content; `valid` is whether `data` passed `valid_chunk`.
    pub fn deliver(&self, i: u32, data: &[u8], valid: bool) {
        if self.recipe.get(i as usize).is_none() {
            return;
        }
        if !valid {
            self.failed(&[i]);
            return;
        }
        let needed = self.st.borrow().needs_chunk(i);
        if !needed {
            return;
        }
        if self.file.write_all_at(data, self.offset(i)).is_err() {
            self.failed(&[i]);
            return;
        }
        let ready = self.st.borrow_mut().stored(i);
        let groups: Vec<(Proof, bool)> = if ready.is_empty() {
            Vec::new()
        } else {
            let file = self.file.clone();
            io::cpu(move || {
                ready
                    .into_iter()
                    .map(|proof| {
                        let mut buf = vec![0u8; (proof.end - proof.start) as usize];
                        let valid = file.read_exact_at(&mut buf, proof.start).is_ok() && proof.verify(&buf);
                        (proof, valid)
                    })
                    .collect()
            })
        };
        let (bad, place) = {
            let mut st = self.st.borrow_mut();
            let bad: Vec<Proof> = groups.into_iter().filter_map(|(proof, valid)| (st.proved(proof.group, valid) == Some(Group::Bad)).then_some(proof)).collect();
            (bad, st.begin_place())
        };
        for proof in bad {
            eprintln!("rargo daemon: content {}: bytes {}..{} do not match its hash", hex(&self.hash), proof.start, proof.end);
        }
        if place {
            let mut placing = Placing { partial: self, done: false };
            let placed = rargo_net::io::file::set_mode(&self.tmp, 0o444)
                .and_then(|_| rargo_net::io::file::rename(&self.tmp, &self.slot));
            placing.done = placed.is_ok();
            drop(placing);
            if let Err(e) = placed {
                eprintln!("rargo daemon: content {}: could not be put in the cache: {e}", hex(&self.hash));
            }
        }
        self.changed.notify_waiters();
    }

    pub fn failed(&self, idx: &[u32]) {
        self.st.borrow_mut().failed(idx);
        self.changed.notify_waiters();
    }
}

/// The time a whole file is being moved into the cache. It ends with the file there or not, and readers waiting on it woken.
struct Placing<'a> {
    partial: &'a Partial,
    done: bool,
}

impl Drop for Placing<'_> {
    fn drop(&mut self) {
        self.partial.st.borrow_mut().placed(self.done);
        self.partial.changed.notify_waiters();
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.st.get_mut().done {
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
    use expect_test::expect;

    fn recipe(data: &[u8]) -> Vec<(Digest, u32)> {
        data.chunks(30_000).map(|c| (*blake3::hash(c).as_bytes(), c.len() as u32)).collect()
    }

    fn content(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i / 11 + i / 29) as u8).collect()
    }

    enum Ev {
        /// A read of bytes `start..end`, `waited` once it has waited for a change.
        Read(u64, u64, bool),
        /// Every chunk asked for and not yet delivered or failed arrives, in order, as `sent` holds it.
        DeliverFetched,
        /// The source could not fetch the chunks the last fetch needed.
        FailNeeded,
    }

    /// Steps a file's range state through the events with `sent` as the bytes the server sends, checks its
    /// invariants after each step, and lists what each step decided and what the file holds at the end.
    fn run(original: &[u8], sent: &[u8], lazy: &[(u64, u64)], events: impl IntoIterator<Item = Ev>) -> String {
        let hash = blake3::hash(original);
        let mut state =
            RangeState::new(hash.as_bytes(), original.len() as u64, recipe(sent), outline::outline(original), lazy).unwrap();
        state.invariants();
        let mut file = vec![0; original.len()];
        let mut needed = Vec::new();
        let mut asked = std::collections::BTreeSet::new();
        let mut out = String::new();
        for ev in events {
            match ev {
                Ev::Read(start, end, waited) => {
                    let read = state.read(start, end, waited);
                    out += &format!("read {start}..{end} waited={waited} => {read:?}\n");
                    if let Read::Fetch { needed: now, prefetch } = read {
                        asked.extend(now.iter().chain(&prefetch).copied());
                        needed = now;
                    }
                }
                Ev::FailNeeded => {
                    state.failed(&needed);
                    for chunk in &needed {
                        asked.remove(chunk);
                    }
                    out += &format!("failed {needed:?}\n");
                }
                Ev::DeliverFetched => {
                    for chunk in std::mem::take(&mut asked) {
                        assert!(state.needs_chunk(chunk));
                        let i = chunk as usize;
                        let start = state.offsets[i] as usize;
                        let end = start + state.recipe[i].1 as usize;
                        file[start..end].copy_from_slice(&sent[start..end]);
                        let proofs = state.stored(chunk);
                        state.invariants();
                        for proof in proofs {
                            let valid = proof.verify(&file[proof.start as usize..proof.end as usize]);
                            out += &format!("chunk {chunk} proves group {} => {:?}\n", proof.group, state.proved(proof.group, valid));
                            state.invariants();
                        }
                    }
                }
            }
            state.invariants();
        }
        let groups: Vec<bool> = (0..outline::groups(original.len() as u64))
            .map(|g| {
                let (start, end) = outline::group_range(g, original.len() as u64);
                file[start as usize..end as usize] == original[start as usize..end as usize]
            })
            .collect();
        out + &format!("can place: {}; groups holding the original bytes: {groups:?}\n", state.can_place())
    }

    struct Server {
        data: Vec<u8>,
        asked: RefCell<Vec<Vec<u32>>>,
    }

    impl Chunks for Server {
        fn request(&self, p: &Rc<Partial>, idx: Vec<u32>) {
            self.asked.borrow_mut().push(idx.clone());
            let (p, data) = (p.clone(), self.data.clone());
            io::spawn(move || {
                for i in idx {
                    let start = p.offset(i) as usize;
                    let bytes = &data[start..start + p.recipe()[i as usize].1 as usize];
                    p.deliver(i, bytes, Partial::valid_chunk(p.recipe(), i, bytes));
                }
            });
        }
    }

    fn partial(dir: &Path, data: &[u8], served: &[u8], lazy: &[(u64, u64)]) -> (Rc<Partial>, Rc<Server>, PathBuf) {
        let hash = *blake3::hash(data).as_bytes();
        let slot = dir.join("blobs").join("slot");
        let prepared = Partial::prepare(&hash, data.len() as u64, recipe(data), outline::outline(data), lazy, &slot, 0).unwrap();
        let server = Rc::new(Server { data: served.to_vec(), asked: RefCell::new(Vec::new()) });
        (Partial::new(hash, prepared, server.clone()), server, slot)
    }

    #[test]
    fn a_read_waits_for_the_chunks_it_asked_for_and_a_whole_read_for_the_file_in_the_cache() {
        io::sim::test(|| {
            let dir = io::sim::tempdir();
            let data = content(GROUP as usize * 3);
            let (p, server, slot) = partial(&dir, &data, &data, &[(GROUP * 2, GROUP * 3)]);
            assert_eq!(p.read_at(5, 10).unwrap(), data[5..15]);
            assert!(!p.is_done());
            let first = server.asked.borrow().clone();
            assert_eq!(first.len(), 2, "a first read asks for its group and then for the rest of its region");
            assert_eq!(p.read_at(GROUP, 10).unwrap(), data[GROUP as usize..GROUP as usize + 10]);
            assert_eq!(*server.asked.borrow(), first, "chunks already asked for are not asked for again");
            assert_eq!(p.read_at(GROUP * 3 - 4, 100).unwrap(), data[data.len() - 4..]);
            assert_eq!(p.whole().unwrap(), slot);
            assert!(p.is_done());
            assert_eq!(io::file::read(&slot).unwrap(), data);
            drop(p);
            assert_eq!(io::file::read(&slot).unwrap(), data, "a file put in the cache stays when its reader goes");
        });
    }

    #[test]
    fn a_read_of_chunks_that_arrive_wrong_fails_and_the_unfinished_file_is_removed() {
        io::sim::test(|| {
            let dir = io::sim::tempdir();
            let data = content(GROUP as usize * 2);
            let mut wrong = data.clone();
            wrong[7] ^= 1;
            let (p, _server, slot) = partial(&dir, &data, &wrong, &[(GROUP, GROUP * 2)]);
            assert!(p.read_at(0, 10).is_err());
            assert_eq!(p.read_at(GROUP, 10).unwrap(), data[GROUP as usize..GROUP as usize + 10]);
            assert!(!p.is_done());
            let gathered = slot.with_file_name(format!(".partial-{}-0", io::process::id()));
            assert!(io::file::exists(&gathered));
            drop(p);
            assert!(!io::file::exists(&gathered) && !io::file::exists(&slot));
        });
    }

    #[test]
    fn overlapping_reads_share_fetches_and_failed_chunks_are_retried() {
        let data = content(GROUP as usize * 4);
        let events = [
            Ev::Read(0, 1, false),
            Ev::Read(1, 3, false),
            Ev::FailNeeded,
            Ev::Read(0, 1, true),
            Ev::Read(0, 1, false),
            Ev::DeliverFetched,
            Ev::Read(1, 3, true),
            Ev::Read(0, data.len() as u64, false),
            Ev::DeliverFetched,
            Ev::Read(0, data.len() as u64, false),
        ];
        expect![[r#"
            read 0..1 waited=false => Fetch { needed: [0, 1, 2], prefetch: [3, 4] }
            read 1..3 waited=false => Wait
            failed [0, 1, 2]
            read 0..1 waited=true => Failed
            read 0..1 waited=false => Fetch { needed: [0, 1, 2], prefetch: [] }
            chunk 2 proves group 0 => Some(Good)
            chunk 4 proves group 1 => Some(Good)
            read 1..3 waited=true => Ready
            read 0..262144 waited=false => Fetch { needed: [5, 6, 7, 8], prefetch: [] }
            chunk 6 proves group 2 => Some(Good)
            chunk 8 proves group 3 => Some(Good)
            read 0..262144 waited=false => Ready
            can place: true; groups holding the original bytes: [true, true, true, true]
        "#]].assert_eq(&run(&data, &data, &[(GROUP * 2, GROUP * 4)], events));
    }

    #[test]
    fn the_outline_controls_which_bytes_can_be_read() {
        let original = content(GROUP as usize * 3);
        let mut wrong = original.clone();
        wrong[GROUP as usize + 17] ^= 1;
        let events = [
            Ev::Read(0, original.len() as u64, false),
            Ev::DeliverFetched,
            Ev::Read(GROUP, GROUP + 1, false),
            Ev::Read(0, GROUP, false),
        ];
        expect![[r#"
            read 0..196608 waited=false => Fetch { needed: [0, 1, 2, 3, 4, 5, 6], prefetch: [] }
            chunk 2 proves group 0 => Some(Good)
            chunk 4 proves group 1 => Some(Bad)
            chunk 6 proves group 2 => Some(Good)
            read 65536..65537 waited=false => BadHash
            read 0..65536 waited=false => Ready
            can place: false; groups holding the original bytes: [true, false, true]
        "#]].assert_eq(&run(&original, &wrong, &[], events));
    }

    #[test]
    fn a_chunk_that_does_not_match_its_recipe_or_an_outline_that_does_not_give_the_hash_is_refused() {
        let original = content(GROUP as usize * 3);
        let mut wrong = original.clone();
        wrong[GROUP as usize + 17] ^= 1;
        let correct = recipe(&original);
        let i = (GROUP as usize + 17) / 30_000;
        let chunk_ok = |bytes: &[u8]| RangeState::valid_chunk(&correct[i], &bytes[i * 30_000..(i + 1) * 30_000]);
        let hash = blake3::hash(&original);
        let mut bad_outline = outline::outline(&original);
        bad_outline[0][0] ^= 1;
        let mut short = correct.clone();
        short.pop();
        let new = |recipe, outline| RangeState::new(hash.as_bytes(), original.len() as u64, recipe, outline, &[]).err();
        let found = format!(
            "original chunk valid: {}\nwrong chunk valid: {}\nbad outline: {:?}\nshort recipe: {:?}\n",
            chunk_ok(&original),
            chunk_ok(&wrong),
            new(correct.clone(), bad_outline),
            new(short, outline::outline(&original)),
        );
        expect![[r#"
            original chunk valid: true
            wrong chunk valid: false
            bad outline: Some("the server's outline of the content does not give its hash")
            short recipe: Some("the server's recipe of the content does not cover it")
        "#]].assert_eq(&found);
    }
}
