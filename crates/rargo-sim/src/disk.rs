use rargo_core::db::{Db, Table};
use rargo_core::hash::Hash;
use rargo_core::store::Store;
use rargo_net::io::sim;
use std::io;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    Full,
    Torn,
    Crash,
}

pub(crate) fn open(root: &Path) -> Store {
    let db = Arc::new(Db::open_simulated(&root.join("state.redb")).expect("open simulated database"));
    Store::open(&root.join("store"), db).expect("open simulated store")
}

pub(crate) fn cached(store: &Store, key: Hash) -> Option<Hash> {
    let value = store.db().get(Table::Traces, &key.0)?;
    Some(Hash(value.as_ref().try_into().expect("a cache entry holds a hash")))
}

pub(crate) fn cache(store: &Store, key: Hash, out: Hash) {
    store.db().put(Table::Traces, &key.0, &out.0);
}

pub(crate) fn put(store: &mut Option<Store>, bytes: &[u8], fault: Option<Fault>) -> io::Result<Hash> {
    let current = store.as_ref().expect("the server has started");
    match fault {
        None => current.put_bytes(bytes),
        Some(Fault::Full) => {
            sim::set_disk_free(Some(0));
            let result = current.put_bytes(bytes);
            sim::set_disk_free(None);
            result
        }
        Some(fault @ (Fault::Torn | Fault::Crash)) => {
            let hash = Hash::of(bytes);
            let root = current.root().parent().expect("the server disk root").to_path_buf();
            current.db().flush_checked().map_err(io::Error::other)?;
            // The simulator models a disk-wide sync at this boundary. The
            // attempted output below is interrupted before the next barrier.
            let checkpoint = sim::checkpoint_disk();
            if fault == Fault::Torn {
                sim::tear_next_write(bytes.len() / 2);
                current.put_bytes(bytes).expect_err("a torn write fails");
            } else {
                current.put_bytes(bytes)?;
            }
            drop(store.take());
            sim::restart_disk(checkpoint);
            let restarted = open(&root);
            assert!(!restarted.has(&hash), "an unsynced output survived the power cut");
            *store = Some(restarted);
            store.as_ref().unwrap().put_bytes(bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rargo_net::io::file::{self, File};
    use std::io::Write;

    #[test]
    fn power_cut_loses_the_torn_file_and_keeps_committed_redb_records() {
        sim::test(|| {
            let root = sim::tempdir();
            let store = open(&root);
            let pinned = store.put_bytes(b"pinned").unwrap();
            assert!(store.pin_all(&[pinned]));
            cache(&store, Hash::of(b"action"), pinned);
            store.db().flush_checked().unwrap();

            let checkpoint = sim::checkpoint_disk();
            let staging = store.tmp_path();
            let mut file = File::create(&staging).unwrap();
            sim::tear_next_write(2);
            assert!(file.write_all(b"unsynced").is_err());
            drop(file);
            assert_eq!(file::read(&staging).unwrap(), b"un");
            store.db().put(Table::Meta, b"unsynced", b"value");
            drop(store);
            sim::restart_disk(checkpoint);
            assert_eq!(file::read(&staging).unwrap_err().kind(), io::ErrorKind::NotFound);
            let restarted = open(&root);
            assert_eq!(restarted.read(&pinned).unwrap(), b"pinned");
            assert_eq!(cached(&restarted, Hash::of(b"action")), Some(pinned));
            assert!(restarted.db().get(Table::Meta, b"unsynced").is_none());
        });
    }

    #[test]
    fn redb_retries_after_the_simulated_disk_has_space_again() {
        sim::test(|| {
            let store = open(&sim::tempdir());
            let db = store.db();
            let value = vec![7; 2 * 1024 * 1024];
            db.put(Table::Meta, b"large", &value);
            sim::set_disk_free(Some(0));
            assert!(db.flush_checked().is_err());
            assert_eq!(db.get(Table::Meta, b"large").as_deref(), Some(value.as_slice()));
            sim::set_disk_free(None);
            db.flush_checked().unwrap();
            let root = store.root().parent().unwrap().to_path_buf();
            drop(store);
            let restarted = open(&root);
            assert_eq!(restarted.db().get(Table::Meta, b"large").as_deref(), Some(value.as_slice()));
        });
    }

}
