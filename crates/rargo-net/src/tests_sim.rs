//! The real endpoint, handshake and connection code, unmodified, on the simulated backend.

use std::sync::Arc;

use crate::io::sim::{self, Net, Report};
use crate::{io, Endpoint};

// Enough messages to cycle through the payload sizes and drive retransmission.
const MESSAGES: usize = 32;

fn body(i: usize) -> Vec<u8> {
    // Sizes from a few bytes to several fragments.
    let len = 1 + (i * 977) % 6000;
    (0..len).map(|b| (b ^ i) as u8).collect()
}

/// A client and a server exchange `messages` messages each way over an encrypted link.
fn exchange(seed: u64, net: Net, messages: usize) -> Report {
    let ((), report) = sim::run(seed, net, move || {
        let key = Some(Arc::new([7u8; 32]));
        let server_addr = "10.0.0.1:7000".parse().unwrap();
        let server = Endpoint::bind_keyed(server_addr, true, key.clone()).expect("the server binds");
        let client = Endpoint::bind_keyed("10.0.0.2:0".parse().unwrap(), false, key).expect("the client binds");

        let mut served = io::spawn(move || {
            let conn = server.accept().expect("a client arrives");
            for i in 0..messages {
                assert_eq!(conn.recv().expect("the link stays up"), body(i), "message {i} arrives whole and in order");
                conn.send(&body(i + 1)).expect("the reply is queued");
            }
        });

        let conn = client.connect(server_addr).expect("the handshake completes");
        for i in 0..messages {
            conn.send(&body(i)).expect("the message is queued");
            assert_eq!(conn.recv().expect("the link stays up"), body(i + 1), "reply {i} arrives whole and in order");
        }
        served.join().expect("the server side finished");
    });
    report
}

#[test]
fn a_clean_network_carries_every_message() {
    let report = exchange(1, Net::default(), MESSAGES);
    assert_eq!(report.datagrams_lost, 0);
}

#[test]
fn one_seed_is_one_run() {
    let net = Net { loss_per_mille: 100, duplicate_per_mille: 50, delay_ms: (1, 40) };
    // A short exchange still exercises fragmentation and loss with these seeds.
    let messages = 8;
    let first = exchange(42, net, messages);
    assert!(first.datagrams_lost > 0);
    assert_eq!(first, exchange(42, net, messages), "the same seed replays the same run");
    assert_ne!(first.trace, exchange(43, net, messages).trace, "another seed is another run");
}

mod a_bad_network_delivers_everything_in_order_for_many_seeds {
    rargo_machine::test_batches! {
        super::check_bad_network;
        seeds_0_and_1: 0..2,
        seeds_2_and_3: 2..4,
    }
}

fn check_bad_network(seeds: std::ops::Range<u64>) {
    let net = Net { loss_per_mille: 200, duplicate_per_mille: 100, delay_ms: (1, 80) };
    let (mut simulated_ms, mut lost) = (0u64, 0u64);
    for seed in seeds {
        let report = exchange(seed, net, MESSAGES);
        simulated_ms += report.elapsed_ms;
        lost += report.datagrams_lost;
    }
    assert!(lost > 0, "the network did lose datagrams");
    assert!(simulated_ms > 2_000, "retransmission timers ran on simulated time: {simulated_ms} ms");
}

#[test]
fn a_connection_the_server_forgot_closes_at_the_clients_next_packet() {
    let ((), _) = sim::run(5, Net::default(), || {
        let server_addr = "10.0.0.1:7000".parse().unwrap();
        let server = Endpoint::bind(server_addr, true).expect("the server binds");
        let client = Endpoint::bind("10.0.0.2:0".parse().unwrap(), false).expect("the client binds");
        let conn = client.connect(server_addr).expect("the client connects");
        conn.send(b"hello").expect("the message is queued");
        let accepted = server.accept().expect("a client arrives");
        assert_eq!(accepted.recv().expect("the message arrives"), b"hello");
        server.forget(accepted.id);
        let start = io::now();
        conn.send(b"again").expect("the message is queued");
        while !conn.is_closed() {
            assert!(io::now() - start < std::time::Duration::from_secs(1), "the client hears at once that the server forgot it");
            io::sleep(std::time::Duration::from_millis(10));
        }
    });
}

#[test]
fn files_live_in_memory() {
    sim::test(|| {
        use crate::io::file;
        let dir = sim::tempdir();
        file::create_dir_all(dir.join("store/sub")).unwrap();
        file::write(dir.join("store/a.tmp"), b"hello").unwrap();
        file::rename(dir.join("store/a.tmp"), dir.join("store/a")).unwrap();
        assert!(!file::exists(dir.join("store/a.tmp")));
        assert_eq!(file::metadata(dir.join("store/a")).unwrap().len(), 5);
        assert!(file::read(dir.join("nope")).is_err());
        assert_eq!(file::read(dir.join("store/a")).unwrap(), b"hello");

        file::hard_link(dir.join("store/a"), dir.join("store/b")).unwrap();
        assert_eq!(file::metadata(dir.join("store/b")).unwrap().nlink(), 2);
        let open = file::File::open(dir.join("store/a")).unwrap();
        file::remove_file(dir.join("store/a")).unwrap();
        file::remove_file(dir.join("store/b")).unwrap();
        let mut held = [0u8; 5];
        open.read_exact_at(&mut held, 0).unwrap();
        assert_eq!(&held, b"hello", "an unlinked file keeps its bytes while it is open");

        file::symlink("sub", dir.join("store/link")).unwrap();
        file::write(dir.join("store/link/x"), b"1").unwrap();
        assert_eq!(file::read(dir.join("store/sub/x")).unwrap(), b"1");
        assert_eq!(file::canonicalize(dir.join("store/link/x")).unwrap(), dir.join("store/sub/x"));
        let mut names: Vec<_> = file::read_dir(dir.join("store")).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["link", "sub"]);
        file::set_mode(dir.join("store/sub/x"), 0o755).unwrap();
        assert!(file::metadata(dir.join("store/sub/x")).unwrap().is_executable());
        let mut log = file::OpenOptions::new().append(true).create(true).open(dir.join("store/log")).unwrap();
        std::io::Write::write_all(&mut log, b"one ").unwrap();
        let mut again = file::OpenOptions::new().append(true).open(dir.join("store/log")).unwrap();
        std::io::Write::write_all(&mut again, b"two").unwrap();
        assert_eq!(file::read(dir.join("store/log")).unwrap(), b"one two", "an appending write lands at the end");
        assert_eq!(file::metadata(dir.join("store/log")).unwrap().mode() & 0o777, 0o644, "a new file gets the usual umask");
        file::remove_dir_all(dir.join("store")).unwrap();
        assert!(!file::exists(dir.join("store")));
    });
}

#[test]
fn a_simulated_read_into_a_buffer_fills_its_front_and_hands_back_the_whole_buffer() {
    sim::test(|| {
        let dir = sim::tempdir();
        io::file::write(dir.join("f"), b"hello").unwrap();
        let fd = io::open_at(libc::AT_FDCWD, &dir.join("f"), libc::O_RDONLY, 0).unwrap();
        let (buf, n) = io::read_into(fd, 1, vec![7; 8]).unwrap();
        assert_eq!((n, &buf[..]), (4, &b"ello\x07\x07\x07\x07"[..]));
        let (buf, n) = io::read_into(fd, 9, buf).unwrap();
        assert_eq!((n, buf.len()), (0, 8), "a read past the end reads nothing");
        io::close_fd(fd).unwrap();
    });
}

#[test]
fn a_simulated_file_keeps_what_a_content_store_relies_on() {
    sim::test(|| {
        use crate::io::file;
        use std::os::fd::AsRawFd;
        let dir = sim::tempdir();
        let path = dir.join("blob");
        let held = file::File::create(&path).unwrap();
        held.write_all_at(b"first", 0).unwrap();
        let before = file::metadata(&path).unwrap().mtime_ns();
        held.write_all_at(b"again", 0).unwrap();
        let after = file::metadata(&path).unwrap();
        assert!(after.mtime_ns() > before, "a second write at the same instant still moves the modification time");
        assert_eq!(after.len(), 5);

        file::set_mode(&path, 0o444).unwrap();
        let refused = file::OpenOptions::new().write(true).open(&path).unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(libc::EACCES), "a read-only file does not open for writing");
        held.write_all_at(b"still", 0).unwrap();
        let read = file::File::open(&path).unwrap();
        assert_eq!(&*read.map(5).unwrap(), b"still", "a descriptor opened before the mode changed keeps writing");
        assert!(read.map(6).is_err(), "a mapping is no longer than its file");

        let copy = file::File::create(dir.join("copy")).unwrap();
        assert!(!file::reflink_fd(read.as_raw_fd(), copy.as_raw_fd()), "the simulated filesystem shares no blocks");
        assert_eq!(file::lchown(&path, 0, 0).unwrap_err().raw_os_error(), Some(libc::EPERM));
        file::lchown(&path, io::process::uid(), io::process::gid()).unwrap();

        let first = file::File::create(dir.join("lock")).unwrap();
        first.lock_exclusive().unwrap();
        let other = dir.join("lock");
        let mut waiter = io::spawn(move || {
            let second = file::File::open(other).unwrap();
            second.lock_exclusive().unwrap();
            io::now()
        });
        let start = io::now();
        io::sleep(std::time::Duration::from_secs(1));
        assert!(!waiter.is_finished(), "a second descriptor waits for the lock");
        drop(first);
        let got = waiter.join().unwrap();
        assert!(got >= start + std::time::Duration::from_secs(1), "and gets it once the first is closed");
    });
}

#[test]
fn the_clock_the_environment_and_commands_are_the_simulations() {
    sim::test(|| {
        let (start, wall) = (io::now(), io::system_now());
        io::sleep(std::time::Duration::from_secs(3600));
        assert_eq!(io::now() - start, std::time::Duration::from_secs(3600));
        assert_eq!(io::system_now().duration_since(wall).unwrap(), std::time::Duration::from_secs(3600));

        assert!(io::env::var_os("HOME").is_none(), "the environment starts empty");
        sim::set_env("HOME", "/home/sim");
        assert_eq!(io::env::var("HOME").unwrap(), "/home/sim");

        assert!(io::process::Command::new("rustc").output().is_err(), "a command nobody described does not run");
        sim::on_command(|cmd| {
            assert_eq!(cmd.program, "rustc");
            Ok(sim::Ran { status: 1, stdout: b"out".to_vec(), stderr: cmd.args[0].as_encoded_bytes().to_vec() })
        });
        let out = io::process::Command::new("rustc").arg("--version").output().unwrap();
        assert_eq!((out.status.code(), out.stdout, out.stderr), (Some(1), b"out".to_vec(), b"--version".to_vec()));
        assert_eq!(io::blocking(|| io::file::exists("/tmp")), io::file::exists("/tmp"), "pool work sees the same files");
    });
}

#[test]
#[should_panic(expected = "deadlock at 0 ms")]
fn a_deadlock_is_reported() {
    sim::run(0, Net::default(), || {
        let notify = io::Notify::new();
        notify.notified();
    });
}

#[test]
fn a_pipe_holds_its_bytes_in_memory() {
    sim::test(|| {
        use std::os::fd::AsRawFd;
        let (r, w) = io::pipe().unwrap();
        let (r_fd, w_fd) = (r.as_raw_fd(), w.as_raw_fd());
        assert_eq!(io::try_read(r_fd, 1).unwrap(), None, "an empty pipe has nothing to take");
        assert_eq!(io::write_now(w_fd, b"abc").unwrap(), 3);
        assert_eq!(io::queued(r_fd).unwrap(), 3);
        assert_eq!(io::try_read(r_fd, 1).unwrap(), Some(b"a".to_vec()));
        let second = r.nonblocking_reader().unwrap();
        assert_eq!(io::try_read(second.as_raw_fd(), 8).unwrap(), Some(b"bc".to_vec()), "a second reader takes from the same pipe");

        let mut reader = io::spawn(move || io::read_at(r_fd, 0, 8).unwrap());
        io::sleep(std::time::Duration::from_millis(5));
        assert!(!reader.is_finished(), "a read of an empty pipe waits");
        io::write_at(w_fd, 0, b"late").unwrap();
        assert_eq!(reader.join().unwrap(), b"late");

        let mut ended = io::spawn(move || io::read_at(r_fd, 0, 8).unwrap());
        io::yield_now();
        drop(w);
        assert_eq!(ended.join().unwrap(), b"", "a pipe with no writer left ends");
        assert_eq!(io::read_at(r_fd, 0, 8).unwrap(), b"", "and a read that starts after the writer closed ends at once");
        drop((r, second));

        let (r, w) = io::pipe().unwrap();
        io::write_at(w.as_raw_fd(), 0, b"last").unwrap();
        drop(w);
        assert_eq!(io::read_at(r.as_raw_fd(), 0, 8).unwrap(), b"last", "what was written before the writer closed is still read");
        assert_eq!(io::read_at(r.as_raw_fd(), 0, 8).unwrap(), b"", "and then the pipe ends");
    });
}

#[test]
fn a_file_takes_times_and_allocation_through_its_descriptor() {
    sim::test(|| {
        use crate::io::file;
        let path = sim::tempdir().join("f");
        let f = file::File::create(&path).unwrap();
        f.write_all_at(b"abcdef", 0).unwrap();
        let omit = libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT };
        f.set_times(omit, libc::timespec { tv_sec: 77, tv_nsec: 5 }).unwrap();
        assert_eq!(file::metadata(&path).unwrap().mtime_ns(), 77_000_000_005);
        f.set_times(omit, omit).unwrap();
        assert_eq!(file::metadata(&path).unwrap().mtime_ns(), 77_000_000_005, "an omitted time stays");
        f.allocate(0, 0, 10).unwrap();
        assert_eq!(file::metadata(&path).unwrap().len(), 10);
        f.allocate(libc::FALLOC_FL_KEEP_SIZE, 0, 100).unwrap();
        assert_eq!(file::metadata(&path).unwrap().len(), 10);
        f.allocate(libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, 1, 2).unwrap();
        assert_eq!(file::read(&path).unwrap(), b"a\0\0def\0\0\0\0");
    });
}
