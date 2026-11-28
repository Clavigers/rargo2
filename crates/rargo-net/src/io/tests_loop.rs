
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
        assert!(lock.try_lock().is_none(), "a held lock is not taken without waiting");
        assert!(timeout(short, || drop(lock.lock())).is_err());
        let waiting = Arc::clone(&lock);
        let mut next = spawn(move || drop(waiting.lock()));
        yield_now();
        drop(held);
        assert!(timeout(long, || next.join()).is_ok(), "an unlock reaches the live waiter");
        assert!(lock.try_lock().is_some(), "a free lock is taken without waiting");

        let sem = Arc::new(Semaphore::new(0));
        assert!(timeout(short, || drop(sem.acquire_many(1))).is_err());
        let waiting = Arc::clone(&sem);
        let mut next = spawn(move || drop(waiting.acquire_many(1)));
        yield_now();
        sem.add_permits(1);
        assert!(timeout(long, || next.join()).is_ok(), "a permit reaches the live waiter");
    });
}

struct Seen(std::rc::Rc<std::cell::Cell<Option<bool>>>);

impl Drop for Seen {
    fn drop(&mut self) {
        self.0.set(Some(on_fiber()));
    }
}

#[test]
fn a_cancelled_fiber_unwinds_outside_every_fiber_so_its_destructors_do_no_ring_io_through_another_s_stack() {
    drive(1, || {
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let held = Seen(seen.clone());
        let won = race_biased(vec![
            Box::new(move || {
                let _held = held;
                sleep(Duration::from_secs(3600));
                "the sleeper"
            }),
            Box::new(|| "the quick arm"),
        ]);
        assert_eq!(won, "the quick arm");
        assert_eq!(seen.get(), Some(false), "a race's loser is dropped by the scheduler, where no fiber runs");

        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let held = Seen(seen.clone());
        let sleeper = spawn(move || {
            let _held = held;
            sleep(Duration::from_secs(3600));
        });
        yield_now();
        sleeper.abort();
        assert_eq!(seen.get(), Some(false), "a fiber that aborts another does not lend it its own context");
        assert!(on_fiber(), "the aborting fiber gets its context back");
    });
}

struct Aborts(Option<JoinHandle<()>>);

impl Drop for Aborts {
    fn drop(&mut self) {
        self.0.take().unwrap().abort();
    }
}

#[test]
fn a_cancelled_fiber_whose_destructor_aborts_its_own_task_does_not_stop_the_loop() {
    drive(1, || {
        let inner_ended = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = Seen2(inner_ended.clone());
        let outer = spawn(move || {
            let _aborts = Aborts(Some(spawn(move || {
                let _flag = flag;
                sleep(Duration::from_secs(3600));
            })));
            sleep(Duration::from_secs(3600));
        });
        yield_now();
        yield_now();
        outer.abort();
        assert!(inner_ended.get(), "the task the destructor aborted is unwound too");
    });
}

struct Seen2(std::rc::Rc<std::cell::Cell<bool>>);

impl Drop for Seen2 {
    fn drop(&mut self) {
        self.0.set(true);
    }
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

#[test]
fn a_pool_thread_calls_back_into_the_loop_that_gave_it_work_even_while_another_loop_stops() {
    drive(1, || {
        let answer = cpu(|| {
            let other = super::fiber::poke_global().lock().unwrap_or_else(|e| e.into_inner()).take();
            let answer = call(|| 41 + 1);
            *super::fiber::poke_global().lock().unwrap_or_else(|e| e.into_inner()) = other;
            answer
        });
        assert_eq!(answer, 42);
    });
}

#[test]
fn a_finished_fiber_lends_its_stack_to_the_next_and_only_a_few_are_kept() {
    drive(1, || {
        let spares = super::fiber::spare_stacks();
        spawn(|| ()).join().unwrap();
        assert_eq!(super::fiber::spare_stacks(), spares + 1, "a finished fiber's stack is kept");
        let mut deep = spawn(|| {
            let mut frame = [1u8; 64 << 10];
            std::hint::black_box(&mut frame);
            yield_now();
            frame.iter().map(|&b| u64::from(b)).sum::<u64>()
        });
        assert_eq!(super::fiber::spare_stacks(), spares, "the next fiber runs on the kept stack");
        assert_eq!(deep.join().unwrap(), 64 << 10);

        let many: Vec<_> = (0..100).map(|_| spawn(yield_now)).collect();
        for mut fiber in many {
            fiber.join().unwrap();
        }
        assert_eq!(super::fiber::spare_stacks(), 64, "no more stacks are kept than the loop is likely to reuse");
    });
}

#[test]
fn a_read_into_a_buffer_fills_its_front_and_hands_back_the_whole_buffer() {
    drive(1, || {
        use std::os::fd::AsRawFd;
        let (read, write) = super::pipe().unwrap();
        assert_eq!(super::write_at(write.as_raw_fd(), 0, b"hello").unwrap(), 5);
        let (buf, n) = super::read_into(read.as_raw_fd(), 0, vec![7; 64]).unwrap();
        assert_eq!((n, buf.len()), (5, 64));
        assert_eq!(&buf[..5], b"hello");
        assert!(buf[5..].iter().all(|&b| b == 7), "the bytes past the read are left alone");
    });
}
