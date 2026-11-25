use rargo_proto::{Digest, LAZY_SMALL};
use std::collections::{BTreeMap, BTreeSet};

pub const WINDOW: usize = 32 << 20;
const _: () = assert!(WINDOW <= u32::MAX as usize && WINDOW > 0);

pub fn bulk_cost(bytes: usize) -> usize {
    bytes.clamp(1, WINDOW)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartStep {
    pub begin: bool,
    pub finish: Option<u64>,
}

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

    pub fn push(&mut self, sizes: &BTreeMap<Digest, u64>, knows: bool) -> Vec<(Digest, u64)> {
        let mut push = Vec::new();
        for (h, size) in sizes {
            if self.set.insert(*h) && knows {
                push.push((*h, *size));
            }
        }
        push
    }

    pub fn push_small(&mut self, sizes: &BTreeMap<Digest, u64>) -> Vec<Digest> {
        sizes.iter().filter(|(h, size)| **size <= LAZY_SMALL && self.set.insert(**h)).map(|(h, _)| *h).collect()
    }
}

#[derive(Clone, Debug)]
pub struct SessionState {
    knows: bool,
    lazy: bool,
    delivered: BTreeSet<Digest>,
    credit: usize,
    window_closed: bool,
    asking: Option<Ask>,
    wanted: BTreeSet<Digest>,
    parts: BTreeMap<Digest, u64>,
    changed: bool,
    closed: bool,
    next_id: u64,
}

impl Default for SessionState {
    fn default() -> SessionState {
        SessionState::new()
    }
}

impl SessionState {
    pub fn new() -> SessionState {
        SessionState {
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

    pub fn set_knows(&mut self) {
        self.knows = true;
    }

    pub fn knows(&self) -> bool {
        self.knows
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

    pub fn delivery(&mut self, sizes: &BTreeMap<Digest, u64>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.delivered.extend(sizes.keys().copied());
        id
    }

    pub fn delivered(&self) -> &BTreeSet<Digest> {
        &self.delivered
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

    pub fn ask(&mut self, a: Ask) -> Result<(), Gone> {
        if self.closed {
            return Err(Gone);
        }
        self.asking = Some(a);
        Ok(())
    }

    pub fn answer(&mut self, a: Ask) -> bool {
        if self.asking == Some(a) {
            self.asking = None;
            return true;
        }
        false
    }

    pub fn want(&mut self, hashes: &[Digest], stored: &dyn Fn(&Digest) -> bool) {
        self.wanted = hashes.iter().filter(|h| !stored(h)).copied().collect();
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

    pub fn part(&mut self, hash: Digest, len: u64, last: bool) -> PartStep {
        let begin = !self.parts.contains_key(&hash);
        let total = self.parts.entry(hash).or_insert(0);
        *total += len;
        let finish = if last { self.parts.remove(&hash) } else { None };
        PartStep { begin, finish }
    }

    pub fn part_failed(&mut self, hash: &Digest) {
        self.parts.remove(hash);
    }

    pub fn changed(&mut self) {
        self.changed = true;
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

pub enum Hello {
    Need(usize),
    Foreign(Vec<u8>),
    Hello { user: String, msg1: Vec<u8> },
    BadUser,
}

fn reply(status: u8, body: &[u8]) -> Vec<u8> {
    assert!(body.len() <= u16::MAX as usize);
    let mut r = vec![status];
    r.extend_from_slice(&(body.len() as u16).to_le_bytes());
    r.extend_from_slice(body);
    r
}

pub fn hello(buf: &[u8], magic: &[u8; 8]) -> Hello {
    if buf.len() < 9 {
        return Hello::Need(9);
    }
    if &buf[..8] != magic {
        return Hello::Foreign(reply(
            1,
            b"this is a rargo build server; the client speaks a different protocol version\n",
        ));
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
    let Ok(user) = String::from_utf8(buf[9..user_end].to_vec()) else { return Hello::BadUser };
    Hello::Hello { user, msg1: buf[user_end + 2..].to_vec() }
}

pub fn refusal(text: &str) -> Vec<u8> {
    reply(1, format!("{text}\n").as_bytes())
}

pub fn welcome(msg2: &[u8]) -> Vec<u8> {
    reply(0, msg2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(i: u8) -> Digest {
        [i; 32]
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
            Hello::Hello { user, msg1 } => assert_eq!((user.as_str(), msg1.as_slice()), ("ann", &b"hi"[..])),
            _ => panic!("a whole hello is read"),
        }
        let Hello::Foreign(r) = hello(b"GET / HTTP", magic) else { panic!("another protocol is told so") };
        assert_eq!(r[0], 1);
        assert_eq!(welcome(b"xy"), vec![0, 2, 0, b'x', b'y']);
        assert_eq!(refusal("no"), vec![1, 3, 0, b'n', b'o', b'\n']);
        let mut bad = magic.to_vec();
        bad.extend_from_slice(&[1, 0xff, 0, 0]);
        assert!(matches!(hello(&bad, magic), Hello::BadUser));
    }

    #[test]
    fn the_window_holds_back_bulk_until_it_is_sent() {
        let mut s = SessionState::new();
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
        let mut s = SessionState::new();
        assert!(!s.answer(Ask::Trees), "nobody asked");
        s.ask(Ask::Env).unwrap();
        assert!(!s.answer(Ask::Trees));
        assert!(s.answer(Ask::Env));
        assert!(!s.answer(Ask::Env), "a duplicate answer is dropped");
        s.close();
        assert_eq!(s.ask(Ask::Report), Err(Gone));
        s.invariants();
    }

    #[test]
    fn a_blob_already_sent_or_fetched_is_not_pushed_again() {
        let mut has = ClientSet::default();
        let sizes: BTreeMap<Digest, u64> = [(d(1), 10), (d(2), LAZY_SMALL + 1)].into();
        assert_eq!(has.push(&sizes, false), vec![], "a client that has not said what it has gets nothing pushed");
        let mut fresh = ClientSet::default();
        assert_eq!(fresh.push(&sizes, true), vec![(d(1), 10), (d(2), LAZY_SMALL + 1)]);
        assert_eq!(fresh.push(&sizes, true), vec![]);
        let mut lazy = ClientSet::default();
        assert_eq!(lazy.push_small(&sizes), vec![d(1)], "only small blobs go ahead of a lazy delivery");
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

    #[test]
    fn generated_client_messages_leave_no_claim_and_push_nothing_the_client_has() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut has = ClientSet::default();
            let mut client: BTreeSet<Digest> = BTreeSet::new();
            let mut store: BTreeSet<Digest> = BTreeSet::new();
            let mut s = SessionState::new();
            let mut waiting = false;
            let mut in_flight = 0usize;
            for _ in 0..300 {
                match rng.below(11) {
                    0 => {
                        let set: Vec<Digest> = (0..rng.below(4)).map(|_| d(rng.below(24) as u8)).collect();
                        client.extend(&set);
                        has.extend(set);
                        s.set_knows();
                    }
                    1 => {
                        let sizes: BTreeMap<Digest, u64> = (0..1 + rng.below(4))
                            .map(|_| (d(rng.below(24) as u8), rng.below(2 * LAZY_SMALL)))
                            .collect();
                        let before = client.clone();
                        let pushed: Vec<Digest> = if rng.below(2) == 0 {
                            has.push(&sizes, s.knows()).into_iter().map(|(h, _)| h).collect()
                        } else {
                            has.push_small(&sizes)
                        };
                        for h in &pushed {
                            assert!(!before.contains(h), "seed {seed}: pushed {h:?}, which the client has");
                        }
                        client.extend(pushed);
                        let id = s.delivery(&sizes);
                        assert!(id >= 1);
                    }
                    2 if !s.is_closed() && !waiting => {
                        let want: Vec<Digest> = (0..1 + rng.below(4)).map(|_| d(rng.below(24) as u8)).collect();
                        s.want(&want, &|h| store.contains(h));
                        waiting = true;
                    }
                    3 => {
                        for _ in 0..1 + rng.below(3) {
                            store.insert(d(rng.below(24) as u8));
                        }
                    }
                    4 => {
                        let h = d(rng.below(24) as u8);
                        let last = rng.below(3) == 0;
                        let step = s.part(h, rng.below(1 << 20), last);
                        if step.finish.is_some() {
                            store.insert(h);
                        }
                    }
                    5 => s.changed(),
                    6 => {
                        if rng.below(2) == 0 {
                            let _ = s.ask([Ask::Trees, Ask::Env, Ask::Report][rng.below(3) as usize]);
                        } else {
                            s.answer([Ask::Trees, Ask::Env, Ask::Report][rng.below(3) as usize]);
                        }
                    }
                    7 => {
                        let n = rng.below(WINDOW as u64 / 2) as usize;
                        if s.take_bulk(n) == Ok(true) {
                            in_flight += bulk_cost(n);
                        }
                    }
                    8 if in_flight > 0 => {
                        let n = in_flight.min(1 + rng.below(WINDOW as u64) as usize);
                        s.sent_bulk(n);
                        in_flight -= n;
                    }
                    9 if waiting && rng.below(4) == 0 => {
                        s.abandon_want();
                        waiting = false;
                    }
                    10 if rng.below(8) == 0 => {
                        s.close();
                        s.invariants();
                        assert_eq!(s.progress(&|h| store.contains(h)), Progress::Done);
                        s = SessionState::new();
                        waiting = false;
                        in_flight = 0;
                    }
                    _ => {}
                }
                if waiting {
                    match s.progress(&|h| store.contains(h)) {
                        Progress::Wait => {}
                        Progress::Done | Progress::Gone => waiting = false,
                    }
                }
                if !waiting {
                    assert!(s.wanted.is_empty(), "seed {seed}: a finished ask left a claim behind");
                }
                if !s.is_closed() {
                    assert_eq!(s.credit + in_flight, WINDOW, "seed {seed}: the window counts what is in flight");
                }
                s.invariants();
            }
            s.close();
            s.invariants();
        }
    }
}
