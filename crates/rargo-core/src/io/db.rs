//! The owner of the database writer.
//!
//! While an I/O loop runs and is written from its own thread or its worker
//! pool, one fiber on it owns the `DbWriter`: it steps it
//! for every read, write and flush that other fibers and pool threads send it,
//! starts each commit on the worker pool and steps the writer with the
//! result. Without a running loop the writer stays with the database, and
//! whoever calls steps it in turn and commits only when asked to flush.
//!
//! When the owning fiber ends, because the database is dropped or because its
//! loop ends, it hands the writer back with everything sent to it applied, so
//! no write is lost and a later loop can own it again.

use super::{background, call, cpu, loop_at_hand, on_fiber, on_loop_thread, oneshot, spawn, timeout, unbounded_channel};
use super::{UnboundedReceiver, UnboundedSender};
use crate::db_writer::{Batch, DbWriter, Due, IDLE_MS};
use std::mem;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::Duration;

/// What the owner needs from the database: its clock and its commits.
pub(crate) trait Disk: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
    fn commit(&self, batch: &Batch) -> anyhow::Result<()>;
}

type Step = Box<dyn FnOnce(&mut DbWriter) + Send>;
type Done = Box<dyn FnOnce() + Send>;
type Reply<R> = Box<dyn FnOnce(R) + Send>;
type Answer<R> = Box<dyn FnOnce() -> Option<R>>;

enum Msg {
    Step(Step),
    Committed(Batch, anyhow::Result<()>),
    Flush(Done),
    Stop(Done),
}

enum Slot {
    Here(DbWriter),
    Away(UnboundedSender<Msg>),
}

pub(crate) struct Owner {
    slot: Arc<Mutex<Slot>>,
    disk: Arc<dyn Disk>,
}

impl Owner {
    pub(crate) fn new(disk: Arc<dyn Disk>) -> Owner {
        Owner { slot: Arc::new(Mutex::new(Slot::Here(DbWriter::new()))), disk }
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asks the writer something and waits for the answer.
    pub(crate) fn read<R: Send + 'static>(&self, f: impl FnOnce(&DbWriter) -> R + Send + 'static) -> R {
        let mut slot = self.lock();
        let tx = match &mut *slot {
            Slot::Here(w) => return f(w),
            Slot::Away(tx) => tx,
        };
        let (reply, answer) = rendezvous();
        send(tx, Msg::Step(Box::new(move |w| reply(f(w)))));
        drop(slot);
        answer().expect("the writer's owner answers everything it is sent")
    }

    /// Changes the writer, and hands it to a fiber that commits it when a
    /// loop runs. A later read from any thread sees the change.
    pub(crate) fn write(&self, f: impl FnOnce(&mut DbWriter) + Send + 'static) {
        let mut slot = self.lock();
        match &mut *slot {
            Slot::Away(tx) => return send(tx, Msg::Step(Box::new(f))),
            Slot::Here(w) => f(w),
        }
        if loop_at_hand() {
            drop(self.hand_over(slot));
        }
    }

    /// Commits what is pending now and waits for the commit to end.
    pub(crate) fn flush(&self) {
        let mut slot = self.lock();
        if let Slot::Here(w) = &mut *slot {
            if !loop_at_hand() || w.is_empty() {
                commit_here(w, &*self.disk);
                return;
            }
            slot = self.hand_over(slot);
        }
        let Slot::Away(tx) = &mut *slot else { unreachable!("the writer was just handed over") };
        let (done, wait) = rendezvous();
        send(tx, Msg::Flush(Box::new(move || done(()))));
        drop(slot);
        let _ = wait();
    }

    /// Takes the writer back from its fiber, committing what is pending.
    /// On a loop's own thread outside a fiber nothing can wait for the
    /// fiber, so it is only told to commit and stop.
    pub(crate) fn stop(&self) {
        let mut slot = self.lock();
        if let Slot::Away(tx) = &mut *slot {
            if !on_fiber() && on_loop_thread() {
                send(tx, Msg::Stop(Box::new(|| {})));
                return;
            }
            let (done, wait) = rendezvous();
            send(tx, Msg::Stop(Box::new(move || done(()))));
            drop(slot);
            let _ = wait();
            slot = self.lock();
        }
        if let Slot::Here(w) = &mut *slot {
            commit_here(w, &*self.disk);
        }
    }

    /// Hands the writer to a new owning fiber, and gives back the slot,
    /// locked, holding either that fiber's channel or another's.
    fn hand_over<'a>(&'a self, mut slot: MutexGuard<'a, Slot>) -> MutexGuard<'a, Slot> {
        // A channel belongs to the loop that will own the writer. Off the
        // loop's thread only the loop can make one, and the slot must not be
        // locked while waiting for it: the loop itself may be waiting for
        // the lock.
        let (tx, rx) = if on_loop_thread() {
            unbounded_channel()
        } else {
            drop(slot);
            let made = call(unbounded_channel);
            slot = self.lock();
            if matches!(*slot, Slot::Away(_)) {
                return slot;
            }
            made
        };
        let Slot::Here(w) = mem::replace(&mut *slot, Slot::Away(tx.clone())) else {
            unreachable!("only a writer held here can be handed over")
        };
        let owning = Owning {
            slot: Arc::clone(&self.slot),
            disk: Arc::clone(&self.disk),
            writer: Some(w),
            rx,
            tx,
            asked: Vec::new(),
            in_flight: None,
            stopped: Vec::new(),
        };
        background(move || owning.run());
        slot
    }
}

fn send(tx: &UnboundedSender<Msg>, msg: Msg) {
    // The receiver lives until the writer is back in the slot, and the slot
    // is locked around every send.
    assert!(tx.send(msg).is_ok(), "the writer's owner takes messages until it hands the writer back");
}

/// A reply and the wait for it: a fiber parks, any other thread blocks.
fn rendezvous<R: Send + 'static>() -> (Reply<R>, Answer<R>) {
    if on_fiber() {
        let (tx, rx) = oneshot::channel();
        return (Box::new(move |v| drop(tx.send(v))), Box::new(move || rx.recv()));
    }
    assert!(!on_loop_thread(), "a loop's own thread outside a fiber cannot wait for the database writer");
    let (tx, rx) = mpsc::sync_channel(1);
    (Box::new(move |v| drop(tx.send(v))), Box::new(move || rx.recv().ok()))
}

fn settle(w: &mut DbWriter, batch: &Batch, done: anyhow::Result<()>, now_ms: u64) {
    match done {
        Ok(()) => w.flushed(batch, now_ms),
        Err(e) => {
            eprintln!("rargod: db commit failed: {e:#}");
            w.failed(now_ms);
        }
    }
}

/// One commit, on this thread or from a fiber on the worker pool, while
/// nothing else can step the writer.
fn commit_here(w: &mut DbWriter, disk: &dyn Disk) {
    let batch = w.take();
    if batch.is_empty() {
        return;
    }
    let done = if on_fiber() { cpu(|| disk.commit(&batch)) } else { disk.commit(&batch) };
    settle(w, &batch, done, disk.now_ms());
}

/// The fiber that owns the writer, and what it has been asked.
struct Owning {
    slot: Arc<Mutex<Slot>>,
    disk: Arc<dyn Disk>,
    writer: Option<DbWriter>,
    rx: UnboundedReceiver<Msg>,
    tx: UnboundedSender<Msg>,
    /// Flushes asked for since the last commit began.
    asked: Vec<Done>,
    /// The flushes the commit in flight answers, while one is in flight.
    in_flight: Option<Vec<Done>>,
    /// Whoever waits for the writer to come back.
    stopped: Vec<Done>,
}

impl Owning {
    fn run(mut self) {
        loop {
            let now = self.disk.now_ms();
            let w = self.writer.as_mut().expect("the owner holds the writer until it ends");
            let due = w.due(now);
            if self.in_flight.is_none() && (due == Due::Now || !self.asked.is_empty()) {
                let batch = w.take();
                let waiting = mem::take(&mut self.asked);
                if batch.is_empty() {
                    waiting.into_iter().for_each(|done| done());
                } else {
                    self.in_flight = Some(waiting);
                    let (disk, tx) = (Arc::clone(&self.disk), self.tx.clone());
                    spawn(move || {
                        let done = cpu(|| disk.commit(&batch));
                        let _ = tx.send(Msg::Committed(batch, done));
                    });
                }
                continue;
            }
            if !self.stopped.is_empty() && self.in_flight.is_none() && self.asked.is_empty() {
                return;
            }
            let wait = match due {
                Due::Idle => IDLE_MS,
                Due::At(t) => t.saturating_sub(now).max(1),
                Due::Now => 1,
            };
            let rx = &mut self.rx;
            match timeout(Duration::from_millis(wait), || rx.recv()) {
                Err(_) => {}
                Ok(None) => return,
                Ok(Some(msg)) => self.take(msg),
            }
        }
    }

    fn take(&mut self, msg: Msg) {
        let w = self.writer.as_mut().expect("the owner holds the writer until it ends");
        match msg {
            Msg::Step(f) => f(w),
            Msg::Committed(batch, done) => {
                settle(w, &batch, done, self.disk.now_ms());
                let answered = self.in_flight.take().expect("a commit ends only once it began");
                answered.into_iter().for_each(|done| done());
            }
            Msg::Flush(done) => self.asked.push(done),
            Msg::Stop(done) => {
                self.asked.push(Box::new(|| {}));
                self.stopped.push(done);
            }
        }
    }
}

impl Drop for Owning {
    fn drop(&mut self) {
        // With the database gone nobody will take the writer back, so what
        // is pending is committed here or never.
        let orphaned = Arc::strong_count(&self.slot) == 1;
        let held = Arc::clone(&self.slot);
        let mut slot = held.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(msg) = self.rx.try_recv() {
            self.take(msg);
        }
        let mut w = self.writer.take().expect("the owner holds the writer until it ends");
        let mut answered = mem::take(&mut self.asked);
        if let Some(waiting) = self.in_flight.take() {
            // Its commit can no longer be heard; the writes stay to try again.
            w.failed(self.disk.now_ms());
            answered.extend(waiting);
        }
        answered.append(&mut self.stopped);
        if orphaned {
            commit_here(&mut w, &*self.disk);
        }
        *slot = Slot::Here(w);
        drop(slot);
        answered.into_iter().for_each(|done| done());
    }
}
