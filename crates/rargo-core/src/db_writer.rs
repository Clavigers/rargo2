use rargo_machine::StateMachine;
use crate::db::Table;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const DELAY_MS: u64 = 100;
pub const IDLE_MS: u64 = 500;
pub const FULL_BYTES: usize = 64 << 20;

type Key = (Table, Vec<u8>);
pub type Value = Option<Arc<[u8]>>;

#[derive(Clone, Debug)]
struct Entry {
    value: Value,
    seq: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    Idle,
    Now,
    At(u64),
}

#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub writes: Vec<(Table, Vec<u8>, Value)>,
    seqs: Vec<u64>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DbWriter {
    pending: BTreeMap<Key, Entry>,
    bytes: usize,
    seq: u64,
    since: Option<u64>,
    flushing: bool,
}

fn size(key: &[u8], value: &Value) -> usize {
    key.len() + value.as_ref().map_or(0, |v| v.len())
}

impl DbWriter {
    pub fn new() -> DbWriter {
        DbWriter::default()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn write(&mut self, table: Table, key: &[u8], value: Value, now_ms: u64) {
        self.seq += 1;
        self.bytes += size(key, &value);
        let old = self.pending.insert((table, key.to_vec()), Entry { value, seq: self.seq });
        if let Some(old) = old {
            self.bytes -= size(key, &old.value);
        }
        self.since.get_or_insert(now_ms);
    }

    pub fn put(&mut self, table: Table, key: &[u8], value: &[u8], now_ms: u64) {
        self.write(table, key, Some(Arc::from(value)), now_ms);
    }

    pub fn delete(&mut self, table: Table, key: &[u8], now_ms: u64) {
        self.write(table, key, None, now_ms);
    }

    pub fn get(&self, table: Table, key: &[u8]) -> Option<Value> {
        self.pending.get(&(table, key.to_vec())).map(|e| e.value.clone())
    }

    pub fn pending_with_prefix(&self, table: Table, prefix: &[u8]) -> Vec<(Vec<u8>, Value)> {
        self.pending
            .range((table, prefix.to_vec())..)
            .take_while(|((t, k), _)| *t == table && k.starts_with(prefix))
            .map(|((_, k), e)| (k.clone(), e.value.clone()))
            .collect()
    }

    pub fn due(&self, now_ms: u64) -> Due {
        let Some(since) = self.since else { return Due::Idle };
        if self.flushing {
            return Due::At(now_ms + DELAY_MS);
        }
        if self.bytes >= FULL_BYTES || now_ms >= since + DELAY_MS {
            return Due::Now;
        }
        Due::At(since + DELAY_MS)
    }

    pub fn take(&mut self) -> Batch {
        assert!(!self.flushing, "one flush at a time, so writes reach the database in order");
        let mut b = Batch::default();
        for ((t, k), e) in &self.pending {
            b.writes.push((*t, k.clone(), e.value.clone()));
            b.seqs.push(e.seq);
        }
        self.flushing = !b.is_empty();
        b
    }

    pub fn flushed(&mut self, batch: &Batch, now_ms: u64) {
        assert!(self.flushing || batch.is_empty(), "a flush ends only once it began");
        for ((t, k, _), seq) in batch.writes.iter().zip(&batch.seqs) {
            let key = (*t, k.clone());
            if self.pending.get(&key).is_some_and(|e| e.seq == *seq) {
                let e = self.pending.remove(&key).expect("checked above");
                self.bytes -= size(k, &e.value);
            }
        }
        self.end_flush(now_ms);
    }

    pub fn failed(&mut self, now_ms: u64) {
        self.end_flush(now_ms);
    }

    fn end_flush(&mut self, now_ms: u64) {
        self.flushing = false;
        self.since = if self.pending.is_empty() { None } else { Some(now_ms) };
    }
}

impl StateMachine for DbWriter {
    fn invariants(&self) {
        let bytes: usize = self.pending.iter().map(|((_, k), e)| size(k, &e.value)).sum();
        assert_eq!(bytes, self.bytes, "pending bytes are the sum of the pending writes");
        assert_eq!(self.since.is_some(), !self.pending.is_empty(), "the clock runs exactly while writes wait");
        for e in self.pending.values() {
            assert!(e.seq >= 1 && e.seq <= self.seq, "every write has its own number");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rargo_machine::Checked;

    struct Writer(Checked<DbWriter>);

    impl std::ops::Deref for Writer {
        type Target = DbWriter;
        fn deref(&self) -> &DbWriter {
            &self.0
        }
    }

    impl Writer {
        fn new() -> Writer {
            Writer(Checked::new(DbWriter::new()))
        }

        fn put(&mut self, table: Table, key: &[u8], value: &[u8], now_ms: u64) {
            self.0.step(|w| w.put(table, key, value, now_ms))
        }

        fn delete(&mut self, table: Table, key: &[u8], now_ms: u64) {
            self.0.step(|w| w.delete(table, key, now_ms))
        }

        fn take(&mut self) -> Batch {
            self.0.step(|w| w.take())
        }

        fn flushed(&mut self, batch: &Batch, now_ms: u64) {
            self.0.step(|w| w.flushed(batch, now_ms))
        }

        fn failed(&mut self, now_ms: u64) {
            self.0.step(|w| w.failed(now_ms))
        }
    }

    #[test]
    fn a_flush_is_due_a_delay_after_the_first_write_or_when_full() {
        let mut w = Writer::new();
        assert_eq!(w.due(1000), Due::Idle);
        w.put(Table::Meta, b"a", b"1", 1000);
        assert_eq!(w.due(1000), Due::At(1000 + DELAY_MS));
        w.put(Table::Meta, b"b", b"2", 1050);
        assert_eq!(w.due(1050), Due::At(1000 + DELAY_MS), "later writes do not push the flush back");
        assert_eq!(w.due(1000 + DELAY_MS - 1), Due::At(1000 + DELAY_MS));
        assert_eq!(w.due(1000 + DELAY_MS), Due::Now);
        let b = w.take();
        assert_eq!(w.due(1200), Due::At(1300), "nothing more is due while a flush runs");
        w.flushed(&b, 1200);
        assert_eq!(w.due(1200), Due::Idle);
        w.put(Table::Blobs, b"big", &vec![0u8; FULL_BYTES], 2000);
        assert_eq!(w.due(2000), Due::Now, "a full queue is flushed at once");
        w.invariants();
    }

    #[test]
    fn a_write_made_during_a_flush_stays_for_the_next() {
        let mut w = Writer::new();
        w.put(Table::Meta, b"k", b"old", 0);
        w.put(Table::Meta, b"j", b"x", 0);
        let b = w.take();
        assert_eq!(b.writes.len(), 2);
        w.put(Table::Meta, b"k", b"new", 50);
        w.flushed(&b, 60);
        assert_eq!(w.get(Table::Meta, b"k"), Some(Some(Arc::from(&b"new"[..]))));
        assert_eq!(w.get(Table::Meta, b"j"), None);
        assert_eq!(w.due(60), Due::At(60 + DELAY_MS));
        w.invariants();
    }

    #[test]
    fn a_failed_flush_keeps_every_write_and_tries_again_later() {
        let mut w = Writer::new();
        w.put(Table::Traces, b"t", b"v", 0);
        w.delete(Table::Traces, b"u", 0);
        let _ = w.take();
        w.failed(500);
        assert_eq!(w.due(500), Due::At(500 + DELAY_MS));
        assert_eq!(w.take().writes.len(), 2);
    }

    #[test]
    fn pending_writes_are_found_by_prefix_and_deletes_show() {
        let mut w = Writer::new();
        w.put(Table::Incr, b"ab1", b"1", 0);
        w.put(Table::Incr, b"ab2", b"2", 0);
        w.put(Table::Incr, b"ac", b"3", 0);
        w.put(Table::Meta, b"ab9", b"4", 0);
        w.delete(Table::Incr, b"ab3", 0);
        let found: Vec<(Vec<u8>, bool)> =
            w.pending_with_prefix(Table::Incr, b"ab").into_iter().map(|(k, v)| (k, v.is_some())).collect();
        assert_eq!(found, vec![(b"ab1".to_vec(), true), (b"ab2".to_vec(), true), (b"ab3".to_vec(), false)]);
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
    fn generated_writes_and_flushes_lose_nothing_and_keep_the_last_write() {
        let tables = [Table::Blobs, Table::Meta, Table::Cold];
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut w = Writer::new();
            let mut disk: BTreeMap<(Table, Vec<u8>), Vec<u8>> = BTreeMap::new();
            let mut last: BTreeMap<(Table, Vec<u8>), Option<Vec<u8>>> = BTreeMap::new();
            let mut flushed_seqs = std::collections::BTreeSet::new();
            let mut in_flight: Option<Batch> = None;
            let mut now = 0u64;
            let mut n = 0u32;
            for _ in 0..400 {
                now += rng.below(40);
                match rng.below(6) {
                    0..=2 => {
                        let t = tables[rng.below(3) as usize];
                        let k = vec![rng.below(8) as u8];
                        n += 1;
                        if rng.below(4) == 0 {
                            w.delete(t, &k, now);
                            last.insert((t, k), None);
                        } else {
                            let v = n.to_le_bytes().to_vec();
                            w.put(t, &k, &v, now);
                            last.insert((t, k), Some(v));
                        }
                    }
                    3 if in_flight.is_none() && w.due(now) == Due::Now => in_flight = Some(w.take()),
                    4 if in_flight.is_some() => {
                        let b = in_flight.take().unwrap();
                        if rng.below(5) == 0 {
                            w.failed(now);
                        } else {
                            for ((t, k, v), seq) in b.writes.iter().zip(&b.seqs) {
                                assert!(flushed_seqs.insert(*seq), "seed {seed}: write {seq} was flushed twice");
                                match v {
                                    Some(v) => disk.insert((*t, k.clone()), v.to_vec()),
                                    None => disk.remove(&(*t, k.clone())),
                                };
                            }
                            w.flushed(&b, now);
                        }
                    }
                    _ => {}
                }
                if in_flight.is_none() {
                    for (key, want) in &last {
                        let seen = match w.get(key.0, &key.1) {
                            Some(p) => p.map(|v| v.to_vec()),
                            None => disk.get(key).cloned(),
                        };
                        assert_eq!(&seen, want, "seed {seed}: a read sees the last write to {key:?}");
                    }
                }
                w.invariants();
            }
            if let Some(b) = in_flight.take() {
                w.failed(now);
                drop(b);
            }
            while !w.is_empty() {
                let b = w.take();
                for (t, k, v) in &b.writes {
                    match v {
                        Some(v) => disk.insert((*t, k.clone()), v.to_vec()),
                        None => disk.remove(&(*t, k.clone())),
                    };
                }
                w.flushed(&b, now);
            }
            for (key, want) in &last {
                assert_eq!(
                    disk.get(key),
                    want.as_ref(),
                    "seed {seed}: after the last flush {key:?} holds its last write"
                );
            }
        }
    }
}
