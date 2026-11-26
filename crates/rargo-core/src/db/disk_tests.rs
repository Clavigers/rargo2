use super::{Db, Table};
use std::cell::RefCell;
use std::io;

#[derive(Clone)]
enum Change {
    Write(usize, Vec<u8>),
    Resize(usize),
}

impl Change {
    fn apply(&self, image: &mut Vec<u8>) {
        match self {
            Change::Resize(len) => image.resize(*len, 0),
            Change::Write(at, data) => {
                if image.len() < at + data.len() {
                    image.resize(at + data.len(), 0);
                }
                image[*at..at + data.len()].copy_from_slice(data);
            }
        }
    }
}

#[derive(Default)]
struct State {
    live: Vec<u8>,
    safe: Vec<u8>,
    unsynced: Vec<Change>,
    changes: usize,
    cut_after: Option<usize>,
}

thread_local! {
    static DISKS: RefCell<Vec<State>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy, Debug)]
struct FakeDisk(usize);

impl FakeDisk {
    fn holding(image: Vec<u8>) -> FakeDisk {
        DISKS.with_borrow_mut(|all| {
            all.push(State { live: image.clone(), safe: image, ..State::default() });
            FakeDisk(all.len() - 1)
        })
    }

    fn with<T>(self, f: impl FnOnce(&mut State) -> T) -> T {
        DISKS.with_borrow_mut(|all| f(&mut all[self.0]))
    }

    fn change(&self, change: Change) -> io::Result<()> {
        self.with(|s| {
            if s.cut_after.is_some_and(|n| s.changes >= n) {
                return Err(io::Error::other("the power is off"));
            }
            s.changes += 1;
            change.apply(&mut s.live);
            s.unsynced.push(change);
            Ok(())
        })
    }

    fn dead(&self) -> bool {
        self.with(|s| s.cut_after.is_some_and(|n| s.changes >= n))
    }

    fn after_power_cut(&self, kept: u64) -> Vec<u8> {
        let (mut image, unsynced, len) = self.with(|s| (s.safe.clone(), s.unsynced.clone(), s.live.len()));
        let mut bits = kept.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        for change in &unsynced {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            match (bits % 3, change) {
                (0, _) => {}
                (1, Change::Write(at, data)) => Change::Write(*at, data[..data.len() / 2].to_vec()).apply(&mut image),
                _ => change.apply(&mut image),
            }
        }
        image.resize(len.max(image.len()), 0);
        image
    }
}

impl redb::StorageBackend for FakeDisk {
    fn len(&self) -> io::Result<u64> {
        Ok(self.with(|s| s.live.len() as u64))
    }

    fn read(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let at = offset as usize;
        self.with(|s| s.live.get(at..at + len).map(<[u8]>::to_vec)).ok_or_else(|| io::ErrorKind::UnexpectedEof.into())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.change(Change::Resize(len as usize))
    }

    fn sync_data(&self, _eventual: bool) -> io::Result<()> {
        self.with(|s| {
            if s.cut_after.is_some_and(|n| s.changes >= n) {
                return Err(io::Error::other("the power is off"));
            }
            s.safe = s.live.clone();
            s.unsynced.clear();
            Ok(())
        })
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.change(Change::Write(offset as usize, data.to_vec()))
    }
}

const BATCHES: u8 = 2;

fn write_batch(db: &Db, n: u8) {
    db.put(Table::Meta, b"first", &[n]);
    db.put(Table::Blobs, b"second", &[n]);
    db.flush();
}

fn batch_on(image: Vec<u8>) -> u8 {
    let db = Db::on(FakeDisk::holding(image)).expect("the database opens after a power cut");
    let first = db.get(Table::Meta, b"first").map_or(0, |v| v[0]);
    let second = db.get(Table::Blobs, b"second").map_or(0, |v| v[0]);
    assert_eq!(first, second, "a batch is there whole or not at all");
    first
}

#[test]
fn a_power_cut_keeps_every_finished_batch_and_tears_none() {
    let empty = {
        let disk = FakeDisk::holding(Vec::new());
        drop(Db::on(disk).unwrap());
        disk.with(|s| s.safe.clone())
    };
    let mut cut = 0;
    loop {
        let disk = FakeDisk::holding(empty.clone());
        let db = Db::on(disk).unwrap();
        let start = disk.with(|s| s.changes);
        disk.with(|s| s.cut_after = Some(start + cut));
        let mut finished = 0;
        for n in 1..=BATCHES {
            write_batch(&db, n);
            if disk.dead() {
                break;
            }
            finished = n;
        }
        let found = batch_on(disk.after_power_cut(cut as u64));
        assert!(found >= finished && found <= finished + 1, "cut {cut}: batch {finished} had finished, found {found}");
        std::mem::forget(db);
        if finished == BATCHES {
            break;
        }
        cut += 1;
    }
    assert!(cut > 0, "the cut fell inside a commit at least once");
}
