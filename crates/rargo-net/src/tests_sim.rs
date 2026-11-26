//! The real endpoint, handshake and connection code, unmodified, on the simulated backend.

use std::sync::Arc;

use crate::io::sim::{self, Net, Report};
use crate::{io, Endpoint};

const MESSAGES: usize = 200;

fn body(i: usize) -> Vec<u8> {
    // Sizes from a few bytes to several fragments.
    let len = 1 + (i * 977) % 6000;
    (0..len).map(|b| (b ^ i) as u8).collect()
}

/// A client and a server exchange `MESSAGES` messages each way over an encrypted link.
fn exchange(seed: u64, net: Net) -> Report {
    let ((), report) = sim::run(seed, net, || {
        let key = Some(Arc::new([7u8; 32]));
        let server_addr = "10.0.0.1:7000".parse().unwrap();
        let server = Endpoint::bind_keyed(server_addr, true, key.clone()).expect("the server binds");
        let client = Endpoint::bind_keyed("10.0.0.2:0".parse().unwrap(), false, key).expect("the client binds");

        let mut served = io::spawn(move || {
            let conn = server.accept().expect("a client arrives");
            for i in 0..MESSAGES {
                assert_eq!(conn.recv().expect("the link stays up"), body(i), "message {i} arrives whole and in order");
                conn.send(&body(i + 1)).expect("the reply is queued");
            }
        });

        let conn = client.connect(server_addr).expect("the handshake completes");
        for i in 0..MESSAGES {
            conn.send(&body(i)).expect("the message is queued");
            assert_eq!(conn.recv().expect("the link stays up"), body(i + 1), "reply {i} arrives whole and in order");
        }
        served.join().expect("the server side finished");
    });
    report
}

#[test]
fn a_clean_network_carries_every_message() {
    let report = exchange(1, Net::default());
    assert_eq!(report.datagrams_lost, 0);
}

#[test]
fn one_seed_is_one_run() {
    let net = Net { loss_per_mille: 100, duplicate_per_mille: 50, delay_ms: (1, 40) };
    assert_eq!(exchange(42, net), exchange(42, net), "the same seed replays the same run");
    assert_ne!(exchange(42, net).trace, exchange(43, net).trace, "another seed is another run");
}

#[test]
fn a_bad_network_delivers_everything_in_order_for_many_seeds() {
    let net = Net { loss_per_mille: 200, duplicate_per_mille: 100, delay_ms: (1, 80) };
    let (mut simulated_ms, mut lost) = (0u64, 0u64);
    for seed in 0..100 {
        let report = exchange(seed, net);
        simulated_ms += report.elapsed_ms;
        lost += report.datagrams_lost;
    }
    assert!(lost > 0, "the network did lose datagrams");
    assert!(simulated_ms > 60_000, "retransmission timers ran on simulated time: {simulated_ms} ms");
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
        file::remove_dir_all(dir.join("store")).unwrap();
        assert!(!file::exists(dir.join("store")));
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
