
use std::sync::Arc;
use std::time::Duration;


use super::fiber::*;
use super::net::*;
use super::sync::*;
use super::task::*;



#[test]
fn a_waiter_cut_short_by_a_timeout_does_not_take_the_next_waiters_wake() {
    drive(1, || {
        let short = Duration::from_millis(1);
        let long = Duration::from_secs(5);

        let notify = Arc::new(Notify::new());
        assert!(timeout(short, || notify.notified()).is_err());
        let waiting = Arc::clone(&notify);
        let mut woken = spawn(move || waiting.notified());
        yield_now();
        notify.notify_one();
        assert!(timeout(long, || woken.join()).is_ok(), "a notify reaches the live waiter");

        let lock = Arc::new(Mutex::new(()));
        let held = lock.lock();
        assert!(timeout(short, || drop(lock.lock())).is_err());
        let waiting = Arc::clone(&lock);
        let mut next = spawn(move || drop(waiting.lock()));
        yield_now();
        drop(held);
        assert!(timeout(long, || next.join()).is_ok(), "an unlock reaches the live waiter");

        let sem = Arc::new(Semaphore::new(0));
        assert!(timeout(short, || drop(sem.acquire_many(1))).is_err());
        let waiting = Arc::clone(&sem);
        let mut next = spawn(move || drop(waiting.acquire_many(1)));
        yield_now();
        sem.add_permits(1);
        assert!(timeout(long, || next.join()).is_ok(), "a permit reaches the live waiter");
    });
}

#[test]
fn a_datagram_goes_from_one_socket_to_another() {
    drive(1, || {
        let a = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let b = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let to = b.local_addr().unwrap();
        assert_eq!(a.send_to(b"ping", to).unwrap(), 4);
        let mut buf = [0u8; 64];
        let (n, from) = b.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(from, a.local_addr().unwrap());
    });
}

#[test]
fn notify_waiters_wakes_every_fiber_waiting_then() {
    drive(1, || {
        let notify = std::rc::Rc::new(Notify::new());
        let mut waiters: Vec<_> = (0..3)
            .map(|_| {
                let notify = notify.clone();
                spawn(move || notify.notified())
            })
            .collect();
        yield_now();
        notify.notify_waiters();
        for waiter in &mut waiters {
            assert!(timeout(Duration::from_secs(5), || waiter.join()).is_ok());
        }
    });
}

#[test]
fn a_race_over_two_ready_channels_takes_one_message_and_leaves_the_other() {
    drive(1, || {
        let (atx, mut arx) = unbounded_channel::<u32>();
        let (btx, mut brx) = unbounded_channel::<u32>();
        let mut got = Vec::new();
        for n in 0..3 {
            let a = spawn({
                let atx = atx.clone();
                move || {
                    let _ = atx.send(n);
                }
            });
            let b = spawn({
                let btx = btx.clone();
                move || {
                    let _ = btx.send(10 + n);
                }
            });
            let first = race_biased(vec![Box::new(|| arx.recv()), Box::new(|| brx.recv())]);
            got.push(first.unwrap());
            drop((a, b));
            let second = race_biased(vec![Box::new(|| arx.recv()), Box::new(|| brx.recv())]);
            got.push(second.unwrap());
        }
        got.sort();
        assert_eq!(got, vec![0, 1, 2, 10, 11, 12]);
    });
}

#[test]
fn an_accept_cut_short_by_a_timeout_leaves_the_next_connection_to_the_next_accept() {
    drive(1, || {
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(timeout(Duration::from_millis(20), || listener.accept()).is_err());
        let mut client = spawn(move || TcpStream::connect(addr).map(|_| ()));
        let accepted = timeout(Duration::from_secs(5), || listener.accept());
        assert!(matches!(accepted, Ok(Ok(_))));
        client.join().unwrap().unwrap();
    });
}

#[test]
fn an_abort_that_wakes_the_aborting_fiber_leaves_its_next_operation_whole() {
    drive(1, || {
        let handle = spawn(|| sleep(Duration::from_secs(5)));
        yield_now();
        handle.abort();
        assert_eq!(cpu(|| 41 + 1), 42);
        sleep(Duration::from_millis(1));
    });
}

#[test]
fn the_shell_runs_fibers_on_one_ring() {
    drive(1, || {
        assert_eq!(cpu(|| 41 + 1), 42);
        let (tx, mut rx) = unbounded_channel();
        spawn(move || {
            tx.send(1u8).ok();
        });
        assert_eq!(rx.recv(), Some(1));
        let won = race_biased(vec![
            Box::new(|| 1u8),
            Box::new(|| {
                sleep(Duration::from_secs(5));
                2
            }),
        ]);
        assert_eq!(won, 1);
        let handle = spawn(|| sleep(Duration::from_secs(5)));
        handle.abort();
        assert!(handle.is_finished());
        let lock = Arc::new(Mutex::new(0u8));
        let cloned = Arc::clone(&lock);
        let mut worker = spawn(move || {
            *cloned.lock() += 1;
        });
        *lock.lock() += 1;
        worker.join().unwrap();
        assert_eq!(*lock.lock(), 2);
    });
}


#[test]
fn a_race_whose_arms_wake_together_is_won_by_the_earlier_arm() {
    drive(1, || {
        let (atx, mut arx) = unbounded_channel::<u32>();
        let (btx, mut brx) = unbounded_channel::<u32>();
        let sender = spawn(move || {
            yield_now();
            let _ = btx.send(2);
            let _ = atx.send(1);
        });
        let first = race_biased(vec![Box::new(|| arx.recv()), Box::new(|| brx.recv())]);
        assert_eq!(first, Some(1));
        assert_eq!(brx.recv(), Some(2));
        drop(sender);
    });
}

#[test]
fn the_loop_ends_with_its_last_foreground_fiber_and_cancels_the_background() {
    drive(1, || {
        let wake = Arc::new(Notify::new());
        let waiting = Arc::clone(&wake);
        background(move || {
            let _ = timeout(Duration::from_secs(3600), || waiting.notified());
            unreachable!("a cancelled background fiber does not run on");
        });
        background(|| loop {
            sleep(Duration::from_millis(1));
        });
        yield_now();
    });
}

#[test]
fn blocking_work_runs_beside_a_busy_cpu_pool() {
    drive(1, || {
        assert_eq!(cpu(|| call(|| blocking(|| 41 + 1))), 42);
    });
}
