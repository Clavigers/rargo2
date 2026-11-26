use crate::hash::Hash;
use rargo_proto::{BATCH_SIZE, Blob, ClientMsg, Delivery, Digest, LAZY_SMALL, PART_SIZE, Part, ServerMsg, Write};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const WINDOW: usize = 32 << 20;
const _: () = assert!(WINDOW <= u32::MAX as usize && WINDOW > 0);
pub const LAZY_BATCH: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizes {
    pub part: usize,
    pub batch: usize,
    pub lazy_batch: usize,
    pub lazy_small: u64,
}

impl Sizes {
    pub const PROTOCOL: Sizes =
        Sizes { part: PART_SIZE, batch: BATCH_SIZE, lazy_batch: LAZY_BATCH, lazy_small: LAZY_SMALL };
}

pub fn bulk_cost(bytes: usize) -> usize {
    bytes.clamp(1, WINDOW)
}

pub fn bulk_size(m: &ServerMsg) -> usize {
    match m {
        ServerMsg::Blobs(v) => v.iter().map(|b| b.data.len()).sum(),
        ServerMsg::BlobPart { part, .. } => part.data.len(),
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    Trees,
    Env,
    Report,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Done,
    Wait,
    Gone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gone;

pub enum Question {
    Trees(Vec<Digest>),
    Env(Vec<String>),
    Report(Box<rargo_proto::FutureReport>),
}

#[derive(Debug, PartialEq)]
pub enum Answer {
    Trees(Vec<Vec<u8>>),
    Env(Vec<(String, Option<String>)>),
    Report(u32),
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    Store { hash: Digest, bytes: Vec<u8> },
    PartOpen(Digest),
    PartWrite(Digest, Vec<u8>),
    PartInstall { hash: Digest, size: u64 },
    PartDrop(Digest),
    Answer(Answer),
    Wake,
}

#[derive(Debug, Default, PartialEq)]
pub struct Step {
    pub effects: Vec<Effect>,
    pub refused: Option<&'static str>,
    pub bytes_in: u64,
}

pub const UNEXPECTED: &str = "unexpected message from the client";
pub const MISMATCH: &str = "the client sent content that does not match its hash";
pub const UNREADABLE: &str = "the client sent content that does not unpack";

#[derive(Clone, Debug, Default)]
pub struct ClientSet {
    set: BTreeSet<Digest>,
}

impl ClientSet {
    pub fn extend(&mut self, digests: impl IntoIterator<Item = Digest>) {
        self.set.extend(digests);
    }

    pub fn has(&self, d: &Digest) -> bool {
        self.set.contains(d)
    }

    fn push(&mut self, sizes: &BTreeMap<Digest, u64>, knows: bool) -> Vec<(Digest, u64)> {
        let mut push = Vec::new();
        for (h, size) in sizes {
            if self.set.insert(*h) && knows {
                push.push((*h, *size));
            }
        }
        push
    }

    fn push_small(&mut self, sizes: &BTreeMap<Digest, u64>, small: u64) -> Vec<(Digest, u64)> {
        sizes.iter().filter(|(h, size)| **size <= small && self.set.insert(**h)).map(|(h, s)| (*h, *s)).collect()
    }
}

#[derive(Debug)]
pub enum Next {
    Read { hash: Digest, at: u64, len: usize, whole: bool },
    Send(ServerMsg),
    Bulk(ServerMsg),
    Unreadable(Digest),
    Gone,
    Done,
}

#[derive(Debug)]
pub struct Outgoing {
    todo: VecDeque<(Digest, u64)>,
    lazy: bool,
    batch: Vec<Blob>,
    batch_bytes: usize,
    part: Option<(Digest, u64, u64)>,
    reading: Option<(Digest, bool)>,
    ready: VecDeque<Next>,
    end: Option<ServerMsg>,
    end_always: bool,
    sizes: Sizes,
}

impl Outgoing {
    fn new(sizes: Sizes, blobs: Vec<(Digest, u64)>, lazy: bool, end: ServerMsg, end_always: bool) -> Outgoing {
        Outgoing {
            sizes,
            todo: blobs.into(),
            lazy,
            batch: Vec::new(),
            batch_bytes: 0,
            part: None,
            reading: None,
            ready: VecDeque::new(),
            end: Some(end),
            end_always,
        }
    }

    fn limit(&self) -> usize {
        if self.lazy { self.sizes.lazy_batch } else { self.sizes.batch }
    }

    fn blobs(&mut self) -> Next {
        let m = ServerMsg::Blobs(std::mem::take(&mut self.batch));
        self.batch_bytes = 0;
        if self.lazy { Next::Send(m) } else { Next::Bulk(m) }
    }

    pub fn step(&mut self) -> Next {
        assert!(self.reading.is_none(), "a read is answered before the next step");
        if let Some(n) = self.ready.pop_front() {
            return n;
        }
        if let Some((hash, size, sent)) = self.part {
            self.reading = Some((hash, false));
            let len = self.sizes.part.min((size - sent) as usize).max(1);
            return Next::Read { hash, at: sent, len, whole: false };
        }
        if let Some((hash, size)) = self.todo.pop_front() {
            if self.lazy || size as usize <= self.sizes.part {
                self.reading = Some((hash, true));
                return Next::Read { hash, at: 0, len: size as usize, whole: true };
            }
            self.part = Some((hash, size, 0));
            return self.step();
        }
        if !self.batch.is_empty() {
            return self.blobs();
        }
        match self.end.take() {
            Some(m) => Next::Send(m),
            None => Next::Done,
        }
    }

    pub fn read(&mut self, bytes: Option<Vec<u8>>) {
        let (hash, whole) = self.reading.take().expect("a read answers the read asked for");
        let Some(bytes) = bytes else {
            if !(whole && self.lazy) {
                self.ready.push_back(Next::Unreadable(hash));
            }
            return;
        };
        if whole {
            let b = Blob::pack(hash, &bytes);
            self.batch_bytes += b.data.len();
            self.batch.push(b);
            if self.batch_bytes >= self.limit() {
                let n = self.blobs();
                self.ready.push_back(n);
            }
            assert!(self.batch_bytes < self.limit());
            return;
        }
        let (h, size, sent) = self.part.take().expect("a part is being read");
        assert_eq!(h, hash);
        let n = bytes.len() as u64;
        let sent = sent + n;
        let last = sent >= size || n == 0;
        if !last {
            self.part = Some((h, size, sent));
        }
        self.ready.push_back(Next::Bulk(ServerMsg::BlobPart { hash, part: Part::pack(bytes), last }));
    }

    pub fn abort(&mut self) -> Option<ServerMsg> {
        self.todo.clear();
        self.part = None;
        self.reading = None;
        self.ready.clear();
        self.batch.clear();
        let end = self.end.take();
        if self.end_always { end } else { None }
    }
}

struct Upload {
    size: u64,
    hasher: blake3::Hasher,
}

pub struct SessionState {
    knows: bool,
    lazy: bool,
    delivered: BTreeSet<Digest>,
    credit: usize,
    window_closed: bool,
    asking: Option<Ask>,
    wanted: BTreeSet<Digest>,
    parts: BTreeMap<Digest, Upload>,
    changed: bool,
    closed: bool,
    next_id: u64,
    sizes: Sizes,
}

impl Default for SessionState {
    fn default() -> SessionState {
        SessionState::new()
    }
}

fn sizes_of(files: &[Delivery], writes: &[Write]) -> BTreeMap<Digest, u64> {
    let mut sizes: BTreeMap<Digest, u64> = files.iter().map(|f| (f.hash, f.size)).collect();
    for w in writes {
        if let Write::File { hash, size, .. } = w {
            sizes.insert(*hash, *size);
        }
    }
    sizes
}

impl SessionState {
    pub fn new() -> SessionState {
        SessionState::with_sizes(Sizes::PROTOCOL)
    }

    pub fn with_sizes(sizes: Sizes) -> SessionState {
        SessionState {
            sizes,
            knows: false,
            lazy: false,
            delivered: BTreeSet::new(),
            credit: WINDOW,
            window_closed: false,
            asking: None,
            wanted: BTreeSet::new(),
            parts: BTreeMap::new(),
            changed: false,
            closed: false,
            next_id: 1,
        }
    }

    pub fn knows(&mut self, has: &mut ClientSet, digests: impl IntoIterator<Item = Digest>) {
        has.extend(digests);
        self.knows = true;
    }

    pub fn set_lazy(&mut self) {
        self.lazy = true;
    }

    pub fn is_lazy(&self) -> bool {
        self.lazy
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn has_changed(&self) -> bool {
        self.changed
    }

    pub fn delivered(&self) -> &BTreeSet<Digest> {
        &self.delivered
    }

    pub fn client(&mut self, msg: ClientMsg) -> Step {
        let mut step = Step::default();
        if self.closed {
            return step;
        }
        match msg {
            ClientMsg::Run(_)
            | ClientMsg::Packed(_)
            | ClientMsg::Fetch { .. }
            | ClientMsg::Recipe { .. }
            | ClientMsg::Chunks { .. }
            | ClientMsg::Offer(_)
            | ClientMsg::Cancel
            | ClientMsg::Ping(_) => step.refused = Some(UNEXPECTED),
            ClientMsg::Trees(t) => {
                if self.answer(Ask::Trees) {
                    step.effects.push(Effect::Answer(Answer::Trees(t)));
                }
            }
            ClientMsg::Env(e) => {
                if self.answer(Ask::Env) {
                    step.effects.push(Effect::Answer(Answer::Env(e)));
                }
            }
            ClientMsg::ReportId(id) => {
                if self.answer(Ask::Report) {
                    step.effects.push(Effect::Answer(Answer::Report(id)));
                }
            }
            ClientMsg::Changed => {
                self.changed = true;
                step.effects.push(Effect::Wake);
            }
            ClientMsg::Blobs(blobs) => {
                for b in blobs {
                    step.bytes_in += b.data.len() as u64;
                    let hash = b.hash;
                    let Ok(bytes) = b.unpack() else {
                        step.refused = Some(UNREADABLE);
                        break;
                    };
                    if Hash::of(&bytes).0 != hash {
                        step.refused = Some(MISMATCH);
                        break;
                    }
                    step.effects.push(Effect::Store { hash, bytes });
                }
                step.effects.push(Effect::Wake);
            }
            ClientMsg::BlobPart { hash, part, last } => {
                step.bytes_in += part.data.len() as u64;
                let Ok(data) = part.unpack() else {
                    step.refused = Some(UNREADABLE);
                    if self.parts.remove(&hash).is_some() {
                        step.effects.push(Effect::PartDrop(hash));
                    }
                    return step;
                };
                let up = match self.parts.entry(hash) {
                    std::collections::btree_map::Entry::Occupied(o) => o.into_mut(),
                    std::collections::btree_map::Entry::Vacant(v) => {
                        step.effects.push(Effect::PartOpen(hash));
                        v.insert(Upload { size: 0, hasher: blake3::Hasher::new() })
                    }
                };
                up.size += data.len() as u64;
                up.hasher.update(&data);
                step.effects.push(Effect::PartWrite(hash, data));
                if last {
                    let up = self.parts.remove(&hash).expect("the upload is held");
                    if *up.hasher.finalize().as_bytes() == hash {
                        step.effects.push(Effect::PartInstall { hash, size: up.size });
                        step.effects.push(Effect::Wake);
                    } else {
                        step.effects.push(Effect::PartDrop(hash));
                        step.refused = Some(MISMATCH);
                    }
                }
            }
        }
        step
    }

    pub fn part_failed(&mut self, hash: &Digest) {
        self.parts.remove(hash);
    }

    pub fn deliver(&mut self, has: &mut ClientSet, files: Vec<Delivery>, writes: Vec<Write>, targets: Vec<String>) -> Option<Outgoing> {
        if files.is_empty() && writes.is_empty() {
            return None;
        }
        let sizes = sizes_of(&files, &writes);
        let id = self.next_id;
        self.next_id += 1;
        self.delivered.extend(sizes.keys().copied());
        let deliver = ServerMsg::Deliver { id, files, writes, targets };
        if self.lazy {
            let small = has.push_small(&sizes, self.sizes.lazy_small);
            let mut out = Outgoing::new(self.sizes, small, true, ServerMsg::Delivered { id }, false);
            out.ready.push_back(Next::Send(deliver));
            return Some(out);
        }
        let push = has.push(&sizes, self.knows);
        let mut out = Outgoing::new(self.sizes, push, false, ServerMsg::Delivered { id }, false);
        out.ready.push_back(Next::Send(deliver));
        if self.closed {
            out.ready.push_back(Next::Gone);
        }
        Some(out)
    }

    pub fn fetch(&mut self, has: &mut ClientSet, id: u64, found: Vec<(Digest, Option<u64>)>) -> Outgoing {
        let wanted: Vec<(Digest, u64)> = found.into_iter().filter_map(|(h, size)| size.map(|s| (h, s))).collect();
        has.extend(wanted.iter().map(|(h, _)| *h));
        Outgoing::new(self.sizes, wanted, false, ServerMsg::Fetched { id }, true)
    }

    pub fn take_bulk(&mut self, bytes: usize) -> Result<bool, Gone> {
        if self.window_closed {
            return Err(Gone);
        }
        let n = bulk_cost(bytes);
        if self.credit < n {
            return Ok(false);
        }
        self.credit -= n;
        Ok(true)
    }

    pub fn sent_bulk(&mut self, bytes: usize) {
        self.credit = (self.credit + bulk_cost(bytes)).min(WINDOW);
    }

    pub fn close_window(&mut self) {
        self.window_closed = true;
    }

    pub fn ask(&mut self, q: Question) -> Result<ServerMsg, Gone> {
        if self.closed {
            return Err(Gone);
        }
        let (a, m) = match q {
            Question::Trees(t) => (Ask::Trees, ServerMsg::WantTrees(t)),
            Question::Env(e) => (Ask::Env, ServerMsg::WantEnv(e)),
            Question::Report(r) => (Ask::Report, ServerMsg::Report(r)),
        };
        self.asking = Some(a);
        Ok(m)
    }

    fn answer(&mut self, a: Ask) -> bool {
        if self.asking == Some(a) {
            self.asking = None;
            return true;
        }
        false
    }

    pub fn want(&mut self, hashes: &[Digest], stored: &dyn Fn(&Digest) -> bool) -> ServerMsg {
        self.wanted = hashes.iter().filter(|h| !stored(h)).copied().collect();
        ServerMsg::WantBlobs(hashes.to_vec())
    }

    pub fn progress(&mut self, stored: &dyn Fn(&Digest) -> bool) -> Progress {
        self.wanted.retain(|h| !stored(h));
        if self.wanted.is_empty() {
            return Progress::Done;
        }
        if self.closed || self.changed {
            self.wanted.clear();
            return Progress::Gone;
        }
        Progress::Wait
    }

    pub fn abandon_want(&mut self) {
        self.wanted.clear();
    }

    pub fn close(&mut self) -> Vec<Digest> {
        self.closed = true;
        self.window_closed = true;
        self.asking = None;
        self.wanted.clear();
        std::mem::take(&mut self.parts).into_keys().collect()
    }

    pub fn invariants(&self) {
        assert!(self.credit <= WINDOW, "the window never holds more than its size");
        assert!(self.next_id >= 1, "delivery ids start at 1");
        if self.closed {
            assert!(self.window_closed, "a closed session sends no bulk");
            assert!(self.asking.is_none(), "a closed session asks nothing");
            assert!(self.wanted.is_empty(), "a closed session holds no claim");
            assert!(self.parts.is_empty(), "a closed session holds no partial upload");
        }
    }
}

enum Hello {
    Need(usize),
    Foreign(Vec<u8>),
    Whole { user: String, msg1: Vec<u8> },
    BadUser,
}

fn reply(status: u8, body: &[u8]) -> Vec<u8> {
    assert!(body.len() <= u16::MAX as usize);
    let mut r = vec![status];
    r.extend_from_slice(&(body.len() as u16).to_le_bytes());
    r.extend_from_slice(body);
    r
}

fn hello(buf: &[u8], magic: &[u8; 8]) -> Hello {
    if buf.len() < 9 {
        return Hello::Need(9);
    }
    if buf[..5] != magic[..5] || buf[6..8] != magic[6..8] {
        return Hello::Foreign(refusal("this is a rargo build server, and what connected to it is not rargo"));
    }
    let user_end = 9 + buf[8] as usize;
    if buf.len() < user_end + 2 {
        return Hello::Need(user_end + 2);
    }
    let msg_end = user_end + 2 + u16::from_le_bytes([buf[user_end], buf[user_end + 1]]) as usize;
    if buf.len() < msg_end {
        return Hello::Need(msg_end);
    }
    assert_eq!(buf.len(), msg_end, "the shell reads exactly what the hello needs");
    if &buf[..8] != magic {
        return Hello::Foreign(refusal(&rargo_proto::protocol_mismatch(buf[5] as u32)));
    }
    let Ok(user) = String::from_utf8(buf[9..user_end].to_vec()) else { return Hello::BadUser };
    Hello::Whole { user, msg1: buf[user_end + 2..].to_vec() }
}

fn refusal(text: &str) -> Vec<u8> {
    reply(1, format!("{text}\n").as_bytes())
}

fn welcome(msg2: &[u8]) -> Vec<u8> {
    reply(0, msg2)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Shake {
    Read(usize),
    Check { user: String, msg1: Vec<u8> },
    Reply { write: Vec<u8>, end: Shook },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Shook {
    Accepted { user: String, admin: bool },
    Refused(String),
}

#[derive(Debug)]
pub struct Accepted {
    pub msg2: Vec<u8>,
    pub early: bool,
    pub admin: bool,
}

#[derive(Debug)]
pub struct Handshake {
    magic: [u8; 8],
    buf: Vec<u8>,
    user: Option<String>,
    ended: bool,
}

impl Handshake {
    pub fn new(magic: &[u8; 8]) -> (Handshake, Shake) {
        let mut h = Handshake { magic: *magic, buf: Vec::new(), user: None, ended: false };
        let first = h.received(&[]);
        (h, first)
    }

    pub fn received(&mut self, bytes: &[u8]) -> Shake {
        assert!(self.user.is_none() && !self.ended, "the hello is read before it is checked");
        self.buf.extend_from_slice(bytes);
        match hello(&self.buf, &self.magic) {
            Hello::Need(n) => Shake::Read(n - self.buf.len()),
            Hello::Foreign(write) => {
                self.end(write, Shook::Refused("not a rargo client (or a different protocol version)".into()))
            }
            Hello::BadUser => self.end(Vec::new(), Shook::Refused("user name is not UTF-8".into())),
            Hello::Whole { user, msg1 } => {
                self.user = Some(user.clone());
                Shake::Check { user, msg1 }
            }
        }
    }

    pub fn checked(&mut self, accepted: Option<Accepted>) -> Shake {
        assert!(!self.ended, "a handshake ends once");
        let user = self.user.clone().expect("a user is checked after the hello names them");
        match accepted {
            None => self.end(
                refusal(rargo_proto::REFUSED),
                Shook::Refused(format!("refused `{user}`: unknown user or wrong key")),
            ),
            Some(a) if a.early => self.end(
                welcome(&a.msg2),
                Shook::Refused(format!(
                    "`{user}` sent a request with the handshake, which this protocol version does not do"
                )),
            ),
            Some(a) => self.end(welcome(&a.msg2), Shook::Accepted { user, admin: a.admin }),
        }
    }

    fn end(&mut self, write: Vec<u8>, end: Shook) -> Shake {
        self.ended = true;
        Shake::Reply { write, end }
    }

    pub fn invariants(&self) {
        if self.user.is_some() {
            assert!(matches!(hello(&self.buf, &self.magic), Hello::Whole { .. }), "a user comes from a whole hello");
        }
        if !self.ended && self.user.is_none() {
            assert!(matches!(hello(&self.buf, &self.magic), Hello::Need(n) if n > self.buf.len()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Sizes = Sizes { part: 4096, batch: 4096, lazy_batch: 2048, lazy_small: 1024 };

    fn content(i: u8) -> Vec<u8> {
        let len = match i % 12 {
            0 => 2 * SMALL.part + 321,
            1 => SMALL.part + 1,
            2 => SMALL.lazy_small as usize + 7,
            _ => 1 + (i as usize * 997) % 1000,
        };
        (0..len).map(|k| (k as u32).wrapping_mul(2_654_435_761).wrapping_add(i as u32).to_le_bytes()[3]).collect()
    }

    fn digest(bytes: &[u8]) -> Digest {
        Hash::of(bytes).0
    }

    fn delivery(bytes: &[u8]) -> Delivery {
        Delivery { path: "f".into(), hash: digest(bytes), size: bytes.len() as u64, exec: false, own: false }
    }

    #[test]
    fn the_hello_is_read_in_steps_and_answered() {
        let magic = b"rargo\x08\r\n";
        let mut buf = magic.to_vec();
        assert!(matches!(hello(&buf[..4], magic), Hello::Need(9)));
        buf.push(3);
        assert!(matches!(hello(&buf, magic), Hello::Need(14)));
        buf.extend_from_slice(b"ann");
        buf.extend_from_slice(&2u16.to_le_bytes());
        assert!(matches!(hello(&buf, magic), Hello::Need(16)));
        buf.extend_from_slice(b"hi");
        match hello(&buf, magic) {
            Hello::Whole { user, msg1 } => assert_eq!((user.as_str(), msg1.as_slice()), ("ann", &b"hi"[..])),
            _ => panic!("a whole hello is read"),
        }
        let Hello::Foreign(r) = hello(b"GET / HTTP", magic) else { panic!("another protocol is told so") };
        assert_eq!(r[0], 1);
        let Hello::Foreign(r) = hello(b"rargo\x07\r\n\x03ann\x02\x00hi", magic) else { panic!("an older client is told so") };
        let text = String::from_utf8(r[3..].to_vec()).unwrap();
        assert!(text.starts_with("this rargo is older than the build server") && text.contains("cargo install rargo --version "), "{text}");
        assert_eq!(welcome(b"xy"), vec![0, 2, 0, b'x', b'y']);
        assert_eq!(refusal("no"), vec![1, 3, 0, b'n', b'o', b'\n']);
        let mut bad = magic.to_vec();
        bad.extend_from_slice(&[1, 0xff, 0, 0]);
        assert!(matches!(hello(&bad, magic), Hello::BadUser));
    }

    fn shake_until_check(h: &mut Handshake, mut next: Shake, bytes: &[u8]) -> Shake {
        let mut at = 0;
        while let Shake::Read(n) = next {
            assert!(n > 0);
            assert!(at < bytes.len(), "the handshake asks only for what the hello holds");
            let take = 1;
            next = h.received(&bytes[at..at + take]);
            at += take;
            h.invariants();
        }
        assert_eq!(at, bytes.len(), "the handshake reads the hello and nothing after it");
        next
    }

    fn a_hello(user: &str, msg1: &[u8]) -> Vec<u8> {
        let mut b = b"rargo\x08\r\n".to_vec();
        b.push(user.len() as u8);
        b.extend_from_slice(user.as_bytes());
        b.extend_from_slice(&(msg1.len() as u16).to_le_bytes());
        b.extend_from_slice(msg1);
        b
    }

    #[test]
    fn immediately_older_tcp_hellos_are_read_in_full_before_refusal() {
        use rargo_proto::secure::MAGIC;
        for version in [7, 8, 9] {
            let mut bytes = a_hello("ann", &[42; 48]);
            bytes[5] = version;
            let (mut handshake, step) = Handshake::new(MAGIC);
            let Shake::Reply { write, end: Shook::Refused(_) } = shake_until_check(&mut handshake, step, &bytes) else {
                panic!("the first message must end with a refusal");
            };
            assert_eq!(write[0], 1);
            assert_eq!(u16::from_le_bytes([write[1], write[2]]) as usize, write.len() - 3);
            assert_eq!(std::str::from_utf8(&write[3..]).unwrap().trim(), rargo_proto::protocol_mismatch(version.into()));
        }
    }

    #[test]
    fn the_handshake_accepts_refuses_and_rejects_early_data_as_steps() {
        let magic = b"rargo\x08\r\n";
        let bytes = a_hello("ann", b"m1");
        let (mut h, first) = Handshake::new(magic);
        assert_eq!(first, Shake::Read(9));
        let next = shake_until_check(&mut h, first, &bytes);
        assert_eq!(next, Shake::Check { user: "ann".into(), msg1: b"m1".to_vec() });
        assert_eq!(
            h.checked(Some(Accepted { msg2: b"xy".to_vec(), early: false, admin: true })),
            Shake::Reply { write: welcome(b"xy"), end: Shook::Accepted { user: "ann".into(), admin: true } }
        );
        h.invariants();

        let (mut h, first) = Handshake::new(magic);
        shake_until_check(&mut h, first, &bytes);
        let Shake::Reply { write, end: Shook::Refused(why) } = h.checked(None) else {
            panic!("an unknown user is refused")
        };
        assert_eq!(write, refusal(rargo_proto::REFUSED));
        assert_eq!(why, "refused `ann`: unknown user or wrong key");
        h.invariants();

        let (mut h, first) = Handshake::new(magic);
        shake_until_check(&mut h, first, &bytes);
        let Shake::Reply { write, end: Shook::Refused(why) } =
            h.checked(Some(Accepted { msg2: b"xy".to_vec(), early: true, admin: false }))
        else {
            panic!("early data ends the link")
        };
        assert_eq!(write, welcome(b"xy"), "the client is welcomed before the link ends");
        assert!(why.contains("sent a request with the handshake"));
        h.invariants();

        let (mut h, _) = Handshake::new(magic);
        let Shake::Reply { write, end: Shook::Refused(why) } = h.received(b"GET / HTT") else {
            panic!("another protocol is told so")
        };
        assert_eq!(write[0], 1);
        assert!(why.starts_with("not a rargo client"));
        h.invariants();

        let (mut h, _) = Handshake::new(magic);
        let mut bad = magic.to_vec();
        bad.extend_from_slice(&[1, 0xff, 0, 0]);
        let first = h.received(&bad[..9]);
        assert_eq!(first, Shake::Read(3));
        assert_eq!(
            h.received(&bad[9..]),
            Shake::Reply { write: Vec::new(), end: Shook::Refused("user name is not UTF-8".into()) }
        );
        h.invariants();
    }

    #[test]
    fn the_window_holds_back_bulk_until_it_is_sent() {
        let mut s = SessionState::with_sizes(SMALL);
        assert_eq!(s.take_bulk(WINDOW - 10), Ok(true));
        assert_eq!(s.take_bulk(20), Ok(false));
        assert_eq!(s.take_bulk(0), Ok(true), "an empty message costs one byte");
        s.sent_bulk(WINDOW - 10);
        assert_eq!(s.take_bulk(20), Ok(true));
        s.close_window();
        assert_eq!(s.take_bulk(1), Err(Gone));
        s.invariants();
    }

    #[test]
    fn answers_go_only_to_the_question_asked() {
        let mut s = SessionState::with_sizes(SMALL);
        assert_eq!(s.client(ClientMsg::Trees(vec![])).effects, vec![], "nobody asked");
        assert!(matches!(s.ask(Question::Env(vec!["A".into()])), Ok(ServerMsg::WantEnv(v)) if v == ["A"]));
        assert_eq!(s.client(ClientMsg::Trees(vec![])).effects, vec![]);
        assert_eq!(s.client(ClientMsg::Env(vec![])).effects, vec![Effect::Answer(Answer::Env(vec![]))]);
        assert_eq!(s.client(ClientMsg::Env(vec![])).effects, vec![], "a duplicate answer is dropped");
        assert_eq!(s.client(ClientMsg::Ping(1)).refused, Some(UNEXPECTED));
        s.close();
        assert!(s.ask(Question::Trees(vec![])).is_err());
        s.invariants();
    }

    #[test]
    fn a_blob_whose_content_is_not_its_hash_is_refused() {
        let mut s = SessionState::with_sizes(SMALL);
        let good = content(5);
        let mut bad = Blob::pack(digest(&good), &good);
        bad.hash = digest(b"other");
        let step = s.client(ClientMsg::Blobs(vec![Blob::pack(digest(&good), &good), bad]));
        assert_eq!(step.refused, Some(MISMATCH));
        assert!(matches!(&step.effects[0], Effect::Store { hash, .. } if *hash == digest(&good)));
        let big = content(1);
        let half = big.len() / 2;
        s.client(ClientMsg::BlobPart { hash: digest(b"x"), part: Part::pack(big[..half].to_vec()), last: false });
        let step =
            s.client(ClientMsg::BlobPart { hash: digest(b"x"), part: Part::pack(big[half..].to_vec()), last: true });
        assert_eq!(step.refused, Some(MISMATCH));
        assert_eq!(step.effects.last(), Some(&Effect::PartDrop(digest(b"x"))));
        assert!(s.parts.is_empty());
    }

    #[test]
    fn a_delivery_sends_whole_blobs_in_batches_and_big_ones_in_parts() {
        let (small, big) = (content(5), content(0));
        let mut s = SessionState::with_sizes(SMALL);
        let mut has = ClientSet::default();
        s.knows(&mut has, []);
        let mut out = s.deliver(&mut has, vec![delivery(&small), delivery(&big)], vec![], vec![]).unwrap();
        let mut seen = Vec::new();
        loop {
            match out.step() {
                Next::Read { hash, at, len, .. } => {
                    let all = if hash == digest(&small) { &small } else { &big };
                    out.read(Some(all[at as usize..(at as usize + len).min(all.len())].to_vec()));
                }
                Next::Send(m) => seen.push(format!("send {}", name(&m))),
                Next::Bulk(m) => seen.push(format!("bulk {}", name(&m))),
                Next::Done => break,
                n => panic!("{n:?}"),
            }
        }
        assert_eq!(
            seen,
            ["send deliver", "bulk part", "bulk part", "bulk last part", "bulk blobs 1", "send delivered"],
            "the big blob goes in parts as it is read; the small ones wait for a batch"
        );
        assert!(
            s.deliver(&mut has, vec![delivery(&small)], vec![], vec![]).is_some_and(|mut o| {
                let first = o.step();
                matches!(first, Next::Send(ServerMsg::Deliver { .. }))
                    && matches!(o.step(), Next::Send(ServerMsg::Delivered { .. }))
            }),
            "the same delivery again pushes nothing"
        );
    }

    fn name(m: &ServerMsg) -> String {
        match m {
            ServerMsg::Deliver { .. } => "deliver".into(),
            ServerMsg::Delivered { .. } => "delivered".into(),
            ServerMsg::Fetched { .. } => "fetched".into(),
            ServerMsg::Blobs(b) => format!("blobs {}", b.len()),
            ServerMsg::BlobPart { last, .. } => if *last { "last part" } else { "part" }.into(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn a_client_that_has_not_said_what_it_has_gets_nothing_pushed_and_a_lazy_one_gets_small_blobs() {
        let (small, mid) = (content(5), content(2));
        let mut s = SessionState::with_sizes(SMALL);
        let mut has = ClientSet::default();
        let mut out = s.deliver(&mut has, vec![delivery(&small)], vec![], vec![]).unwrap();
        assert!(matches!(out.step(), Next::Send(ServerMsg::Deliver { .. })));
        assert!(matches!(out.step(), Next::Send(ServerMsg::Delivered { .. })));
        let mut lazy = SessionState::with_sizes(SMALL);
        lazy.set_lazy();
        let mut has = ClientSet::default();
        let mut out = lazy.deliver(&mut has, vec![delivery(&small), delivery(&mid)], vec![], vec![]).unwrap();
        assert!(matches!(out.step(), Next::Send(ServerMsg::Deliver { .. })));
        assert!(matches!(out.step(), Next::Read { hash, .. } if hash == digest(&small)));
        out.read(None);
        assert!(
            matches!(out.step(), Next::Send(ServerMsg::Delivered { .. })),
            "an unreadable small blob is left to a fetch"
        );
    }

    #[test]
    fn a_fetch_ends_with_fetched_even_when_it_fails() {
        let mut s = SessionState::with_sizes(SMALL);
        let mut has = ClientSet::default();
        let mut out = s.fetch(&mut has, 7, vec![(digest(b"a"), Some(1)), (digest(b"gone"), None)]);
        assert!(matches!(out.step(), Next::Read { .. }));
        out.read(None);
        assert!(matches!(out.step(), Next::Unreadable(h) if h == digest(b"a")));
        assert!(matches!(out.abort(), Some(ServerMsg::Fetched { id: 7 })), "and the client is told");
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    struct Sending {
        out: Outgoing,
        fetched: BTreeSet<Digest>,
    }

    struct World {
        pool: Vec<Vec<u8>>,
        store: BTreeMap<Digest, Vec<u8>>,
        files: BTreeMap<Digest, Vec<u8>>,
        client: BTreeSet<Digest>,
        known: BTreeSet<Digest>,
        fetching: BTreeSet<Digest>,
        receiving: BTreeMap<Digest, Vec<u8>>,
        uploading: Option<(usize, usize)>,
        has: ClientSet,
        s: SessionState,
        sends: Vec<Sending>,
        in_flight: usize,
        waiting: bool,
        asked: Option<Ask>,
        seed: u64,
    }

    impl World {
        fn apply(&mut self, step: Step) -> bool {
            for e in step.effects {
                match e {
                    Effect::Store { hash, bytes } => {
                        assert_eq!(digest(&bytes), hash);
                        self.store.insert(hash, bytes);
                    }
                    Effect::PartOpen(h) => assert!(self.files.insert(h, Vec::new()).is_none(), "one file per upload"),
                    Effect::PartWrite(h, d) => self.files.get_mut(&h).expect("written to an open upload").extend(d),
                    Effect::PartInstall { hash, size } => {
                        let bytes = self.files.remove(&hash).expect("installed from an open upload");
                        assert_eq!((digest(&bytes), bytes.len() as u64), (hash, size));
                        self.store.insert(hash, bytes);
                    }
                    Effect::PartDrop(h) => assert!(self.files.remove(&h).is_some()),
                    Effect::Answer(a) => {
                        let kind = match a {
                            Answer::Trees(_) => Ask::Trees,
                            Answer::Env(_) => Ask::Env,
                            Answer::Report(_) => Ask::Report,
                        };
                        assert_eq!(self.asked.take(), Some(kind), "seed {}: an answer nobody asked for", self.seed);
                    }
                    Effect::Wake => {}
                }
            }
            step.refused.is_none()
        }

        fn receive(&mut self, m: ServerMsg, fetched: &BTreeSet<Digest>) {
            let seed = self.seed;
            match m {
                ServerMsg::Blobs(blobs) => {
                    for b in blobs {
                        let h = b.hash;
                        assert!(
                            fetched.contains(&h) || !self.known.contains(&h),
                            "seed {seed}: sent {h:?}, which the client has"
                        );
                        assert_eq!(digest(&b.unpack().unwrap()), h);
                        self.client.insert(h);
                        self.known.insert(h);
                    }
                }
                ServerMsg::BlobPart { hash, part, last } => {
                    let got = self.receiving.entry(hash).or_default();
                    if got.is_empty() {
                        assert!(
                            fetched.contains(&hash) || !self.known.contains(&hash),
                            "seed {seed}: began sending {hash:?}, which the client has"
                        );
                    }
                    got.extend(part.unpack().unwrap());
                    if last {
                        let all = self.receiving.remove(&hash).unwrap();
                        assert_eq!(digest(&all), hash, "seed {seed}: the parts make the blob");
                        self.client.insert(hash);
                        self.known.insert(hash);
                    }
                }
                _ => {}
            }
        }

        fn pump(&mut self, i: usize) {
            let next = self.sends[i].out.step();
            let fetched = self.sends[i].fetched.clone();
            match next {
                Next::Read { hash, at, len, .. } => {
                    let bytes =
                        self.store.get(&hash).map(|b| b[at as usize..(at as usize + len).min(b.len())].to_vec());
                    self.sends[i].out.read(bytes);
                }
                Next::Send(m) => self.receive(m, &fetched),
                Next::Bulk(m) => match self.s.take_bulk(bulk_size(&m)) {
                    Ok(true) => {
                        self.in_flight += bulk_cost(bulk_size(&m));
                        self.receive(m, &fetched);
                    }
                    Ok(false) => self.sends[i].out.ready.push_front(Next::Bulk(m)),
                    Err(Gone) => self.end(i, &fetched),
                },
                Next::Unreadable(_) | Next::Gone => self.end(i, &fetched),
                Next::Done => {
                    for h in &fetched {
                        self.fetching.remove(h);
                    }
                    self.sends.remove(i);
                }
            }
        }

        fn end(&mut self, i: usize, fetched: &BTreeSet<Digest>) {
            if let Some(m) = self.sends[i].out.abort() {
                self.receive(m, fetched);
            }
            for h in fetched {
                self.fetching.remove(h);
            }
            self.sends.remove(i);
        }

        fn start(&mut self, rng: &mut Rng) {
            if rng.below(2) == 0 {
                let told: Vec<Digest> = self.client.iter().copied().collect();
                self.known.extend(&told);
                self.s.knows(&mut self.has, told);
            }
            if rng.below(3) == 0 {
                self.s.set_lazy();
            }
        }

        fn drop_session(&mut self, reconnect: bool, rng: &mut Rng) {
            for h in self.s.close() {
                assert!(self.files.remove(&h).is_some(), "seed {}: an upload the shell did not hold", self.seed);
            }
            assert!(self.files.is_empty(), "seed {}: a dropped session left an upload behind", self.seed);
            self.s.invariants();
            assert_eq!(
                self.s.progress(&|_| false),
                Progress::Done,
                "seed {}: a dropped session holds a claim",
                self.seed
            );
            while !self.sends.is_empty() {
                self.pump(0);
            }
            self.receiving.clear();
            self.uploading = None;
            self.waiting = false;
            self.asked = None;
            self.in_flight = 0;
            self.s = SessionState::with_sizes(SMALL);
            if reconnect {
                self.has = ClientSet::default();
                self.known.clear();
            }
            self.start(rng);
        }
    }

    #[test]
    fn generated_client_messages_never_leave_a_claim_and_never_send_a_blob_the_client_has() {
        let pool: Vec<Vec<u8>> = (0..24u8).map(content).collect();
        let hashes: Vec<Digest> = pool.iter().map(|b| digest(b)).collect();
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut w = World {
                pool: pool.clone(),
                store: BTreeMap::new(),
                files: BTreeMap::new(),
                client: BTreeSet::new(),
                known: BTreeSet::new(),
                fetching: BTreeSet::new(),
                receiving: BTreeMap::new(),
                uploading: None,
                has: ClientSet::default(),
                s: SessionState::with_sizes(SMALL),
                sends: Vec::new(),
                in_flight: 0,
                waiting: false,
                asked: None,
                seed,
            };
            for _ in 0..40 {
                let i = rng.below(24) as usize;
                w.store.insert(hashes[i], w.pool[i].clone());
            }
            w.start(&mut rng);
            for _ in 0..300 {
                let pick = rng.below(24) as usize;
                let h = hashes[pick];
                let ok = match rng.below(16) {
                    0 | 1 => {
                        let files: Vec<Delivery> = (0..1 + rng.below(3))
                            .map(|_| rng.below(24) as usize)
                            .filter(|i| w.store.contains_key(&hashes[*i]))
                            .map(|i| Delivery {
                                path: "f".into(),
                                hash: hashes[i],
                                size: w.pool[i].len() as u64,
                                exec: false,
                                own: false,
                            })
                            .collect();
                        let twice = rng.below(3) == 0;
                        for _ in 0..1 + twice as usize {
                            if let Some(out) = w.s.deliver(&mut w.has, files.clone(), vec![], vec![]) {
                                w.sends.push(Sending { out, fetched: BTreeSet::new() });
                                if w.s.is_lazy() {
                                    let i = w.sends.len() - 1;
                                    while w.sends.len() > i {
                                        w.pump(i);
                                    }
                                }
                            }
                        }
                        true
                    }
                    2 if w.s.is_lazy() && !w.client.contains(&h) && !w.fetching.contains(&h) => {
                        w.fetching.insert(h);
                        let found = vec![(h, w.store.get(&h).map(|b| b.len() as u64))];
                        let out = w.s.fetch(&mut w.has, rng.below(100), found);
                        w.sends.push(Sending { out, fetched: [h].into() });
                        true
                    }
                    3 | 4 if !w.sends.is_empty() => {
                        for _ in 0..1 + rng.below(6) {
                            if !w.sends.is_empty() {
                                let i = rng.below(w.sends.len() as u64) as usize;
                                w.pump(i);
                            }
                        }
                        true
                    }
                    5 if w.in_flight > 0 => {
                        let n = w.in_flight.min(1 + rng.below(WINDOW as u64) as usize);
                        w.s.sent_bulk(n);
                        w.in_flight -= n;
                        true
                    }
                    6 => {
                        let bytes = &w.pool[pick];
                        let mut blobs = vec![Blob::pack(h, bytes)];
                        if rng.below(3) == 0 {
                            blobs.push(Blob::pack(h, bytes));
                        }
                        let step = w.s.client(ClientMsg::Blobs(blobs));
                        w.apply(step)
                    }
                    7 | 8 => {
                        let (i, at) = w.uploading.unwrap_or((pick, 0));
                        let bytes = w.pool[i].clone();
                        let end = (at + SMALL.part / 2 + rng.below(SMALL.part as u64) as usize).min(bytes.len());
                        let last = end == bytes.len();
                        w.uploading = if last { None } else { Some((i, end)) };
                        let msg = ClientMsg::BlobPart {
                            hash: digest(&bytes),
                            part: Part::pack(bytes[at..end].to_vec()),
                            last,
                        };
                        let step = w.s.client(msg);
                        w.apply(step)
                    }
                    9 if w.uploading.is_some() && rng.below(3) == 0 => {
                        w.uploading = None;
                        let step = w.s.client(ClientMsg::Changed);
                        w.apply(step)
                    }
                    10 if !w.waiting && !w.s.is_closed() => {
                        let want: Vec<Digest> = (0..1 + rng.below(3)).map(|_| hashes[rng.below(24) as usize]).collect();
                        let store = &w.store;
                        let asked = w.s.want(&want, &|d| store.contains_key(d));
                        assert!(matches!(asked, ServerMsg::WantBlobs(v) if v == want));
                        w.waiting = true;
                        true
                    }
                    11 => {
                        let q = match rng.below(3) {
                            0 => Question::Trees(vec![h]),
                            1 => Question::Env(vec!["A".into()]),
                            _ => Question::Report(Box::new(rargo_proto::FutureReport {
                                path: String::new(),
                                per_package: vec![],
                                suggestion: String::new(),
                            })),
                        };
                        let kind = match q {
                            Question::Trees(_) => Ask::Trees,
                            Question::Env(_) => Ask::Env,
                            Question::Report(_) => Ask::Report,
                        };
                        if w.s.ask(q).is_ok() {
                            w.asked = Some(kind);
                        }
                        true
                    }
                    12 => {
                        let msg = match rng.below(3) {
                            0 => ClientMsg::Trees(vec![]),
                            1 => ClientMsg::Env(vec![]),
                            _ => ClientMsg::ReportId(3),
                        };
                        let step = w.s.client(msg);
                        w.apply(step)
                    }
                    13 if rng.below(6) == 0 => {
                        let msg = if rng.below(2) == 0 { ClientMsg::Ping(1) } else { ClientMsg::Cancel };
                        let step = w.s.client(msg);
                        assert_eq!(step.refused, Some(UNEXPECTED));
                        false
                    }
                    14 if w.waiting && rng.below(4) == 0 => {
                        w.s.abandon_want();
                        w.waiting = false;
                        true
                    }
                    15 if rng.below(10) == 0 => {
                        let reconnect = rng.below(2) == 0;
                        w.drop_session(reconnect, &mut rng);
                        true
                    }
                    _ => true,
                };
                if !ok {
                    let reconnect = rng.below(2) == 0;
                    w.drop_session(reconnect, &mut rng);
                }
                if w.waiting {
                    let store = &w.store;
                    match w.s.progress(&|d| store.contains_key(d)) {
                        Progress::Wait => {}
                        Progress::Done | Progress::Gone => w.waiting = false,
                    }
                }
                if !w.waiting {
                    assert!(w.s.wanted.is_empty(), "seed {seed}: a finished ask left a claim behind");
                }
                assert_eq!(
                    w.files.keys().collect::<Vec<_>>(),
                    w.s.parts.keys().collect::<Vec<_>>(),
                    "seed {seed}: the shell holds a file for exactly the uploads in progress"
                );
                if !w.s.is_closed() {
                    assert!(w.s.credit + w.in_flight <= WINDOW, "seed {seed}: the window counts what is in flight");
                }
                w.s.invariants();
            }
            w.drop_session(false, &mut rng);
        }
    }
}
