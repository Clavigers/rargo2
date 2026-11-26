
use std::any::Any;
use std::fmt;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;


use super::fiber::*;
use super::ops::{Event, Op};


pub(super) struct CancelGuard {
    ids: Vec<u64>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            if !cancel_request(id) {
                continue;
            }
            if std::thread::panicking() {
                continue;
            }
            while fiber_alive(id) {
                park();
            }
        }
    }
}


pub(super) enum Outcome<T> {
    Value(T),
    Panic(Box<dyn Any + Send>),
    Abort,
}

pub(super) struct TaskSlot<T> {
    mu: StdMutex<TaskState<T>>,
}

pub(super) struct TaskState<T> {
    out: Option<Outcome<T>>,
    waiter: Option<u64>,
}

impl<T: Send> TaskSlot<T> {
    fn new() -> Self {
        Self { mu: StdMutex::new(TaskState { out: None, waiter: None }) }
    }

    fn set_ok(&self, value: T) {
        let mut guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.out.is_some() {
            return;
        }
        guard.out = Some(Outcome::Value(value));
    }

    fn take(&self) -> Option<Result<T, JoinError>> {
        let mut guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        match guard.out.take() {
            Some(Outcome::Value(value)) => Some(Ok(value)),
            Some(Outcome::Panic(payload)) => Some(Err(JoinError { panic: Some(payload), abort: false })),
            Some(Outcome::Abort) => Some(Err(JoinError { panic: None, abort: true })),
            None => None,
        }
    }
}

impl<T: Send> Finish for TaskSlot<T> {
    fn fail_panic(&self, payload: Box<dyn Any + Send>) {
        let mut guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.out.is_none() {
            guard.out = Some(Outcome::Panic(payload));
        }
    }

    fn fail_abort(&self) {
        let mut guard = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.out.is_none() {
            guard.out = Some(Outcome::Abort);
        }
    }

    fn arm(&self, id: u64) {
        self.mu.lock().unwrap_or_else(|e| e.into_inner()).waiter = Some(id);
    }

    fn take_waiter(&self) -> Option<u64> {
        self.mu.lock().unwrap_or_else(|e| e.into_inner()).waiter.take()
    }

    fn is_done(&self) -> bool {
        self.mu.lock().unwrap_or_else(|e| e.into_inner()).out.is_some()
    }
}

pub struct JoinError {
    panic: Option<Box<dyn Any + Send>>,
    abort: bool,
}

impl JoinError {
    pub fn is_cancelled(&self) -> bool {
        self.abort
    }

    pub fn into_panic(self) -> Box<dyn Any + Send> {
        self.panic.unwrap_or_else(|| Box::new("task aborted"))
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.abort { f.write_str("task aborted") } else { f.write_str("task panicked") }
    }
}

impl std::error::Error for JoinError {}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinError").field("abort", &self.abort).finish()
    }
}

pub struct JoinHandle<T> {
    id: u64,
    slot: Arc<TaskSlot<T>>,
}

impl<T: Send + 'static> JoinHandle<T> {
    pub fn join(&mut self) -> Result<T, JoinError> {
        loop {
            if let Some(result) = self.slot.take() {
                return result;
            }
            self.slot.arm(current_id());
            if let Some(result) = self.slot.take() {
                return result;
            }
            park();
        }
    }

    pub fn is_finished(&self) -> bool {
        self.slot.is_done()
    }

    pub fn abort_handle(&self) -> AbortHandle<T> {
        AbortHandle { id: self.id, slot: Arc::clone(&self.slot) }
    }

    pub fn abort(&self) {
        if CURRENT.with(|c| c.get()) != 0 {
            self.abort_here();
            return;
        }
        if on_loop() {
            if !cancel_request(self.id) && !fiber_alive(self.id) {
                self.slot.fail_abort();
            }
            return;
        }
        let id = self.id;
        let slot = Arc::clone(&self.slot);
        call(move || JoinHandle { id, slot }.abort_here());
    }

    fn abort_here(&self) {
        loop {
            if self.is_finished() {
                return;
            }
            self.slot.arm(current_id());
            let deferred = cancel_request(self.id);
            if self.is_finished() {
                return;
            }
            if !deferred && !fiber_alive(self.id) {
                self.slot.fail_abort();
                return;
            }
            park();
        }
    }
}

pub struct AbortHandle<T> {
    id: u64,
    slot: Arc<TaskSlot<T>>,
}

impl<T: Send + 'static> AbortHandle<T> {
    pub fn abort(&self) {
        JoinHandle { id: self.id, slot: Arc::clone(&self.slot) }.abort();
    }
}

pub(super) fn fiber_alive(id: u64) -> bool {
    unsafe { &*sched_ptr() }.fibers.contains_key(&id)
}

pub fn spawn<T: Send + 'static>(body: impl FnOnce() -> T + 'static) -> JoinHandle<T> {
    let slot = Arc::new(TaskSlot::new());
    let task: Arc<dyn Finish> = slot.clone();
    let slot_for_body = slot.clone();
    let id = spawn_raw(Spec { parent: None, race: None, task: Some(task), background: false }, move || {
        slot_for_body.set_ok(body());
    });
    JoinHandle { id, slot }
}

pub fn race_biased<T: 'static>(arms: Vec<Box<dyn FnOnce() -> T + '_>>) -> T {
    assert!(!arms.is_empty(), "a race has an arm");
    let mut slot: Option<T> = None;
    let ptr = &mut slot as *mut Option<T>;
    let parent = current_id();
    let mut ids = Vec::with_capacity(arms.len());
    for (index, arm) in arms.into_iter().enumerate() {
        let ptr = ptr as usize;
        let id = spawn_raw(Spec { parent: Some(parent), race: Some(index as u16), task: None, background: false }, move || {
            let value = arm();
            unsafe { *(ptr as *mut Option<T>) = Some(value) };
        });
        ids.push(id);
    }
    let guard = CancelGuard { ids };
    while slot.is_none() {
        park();
    }
    drop(guard);
    slot.unwrap()
}

pub fn cpu<R: Send>(body: impl FnOnce() -> R + Send) -> R {
    on_pool(false, body)
}

pub fn blocking<R: Send>(body: impl FnOnce() -> R + Send) -> R {
    if CURRENT.with(|c| c.get()) == 0 {
        return body();
    }
    on_pool(true, body)
}

fn on_pool<R: Send>(blocking: bool, body: impl FnOnce() -> R + Send) -> R {
    if super::sim::active() {
        // A simulation has one thread: pool work runs where it is asked for.
        return body();
    }
    struct Pack<F, R> {
        body: Option<F>,
        out: Option<Result<R, Box<dyn Any + Send>>>,
    }
    fn typed_exec<F, R>(_: &mut Pack<F, R>) -> fn(usize)
    where
        F: FnOnce() -> R,
    {
        fn exec<F, R>(ptr: usize)
        where
            F: FnOnce() -> R,
        {
            let pack = unsafe { &mut *(ptr as *mut Pack<F, R>) };
            let body = pack.body.take().expect("a worker runs its job once");
            pack.out = Some(catch_unwind(AssertUnwindSafe(body)));
        }
        exec::<F, R>
    }
    let owner = poke_arc();
    let body = move || {
        let _home = Home::enter(owner);
        body()
    };
    let mut pack = Pack { body: Some(body), out: None };
    let go = typed_exec(&mut pack);
    let ptr = &mut pack as *mut Pack<_, R> as usize;
    let event = park_op(
        if blocking { Op::Blocking(Box::new(move || go(ptr))) } else { Op::Cpu(Box::new(move || go(ptr))) },
        true,
    );
    if let Event::Failed(err) = event {
        panic!("worker pool: {err}");
    }
    match pack.out.take() {
        Some(Ok(value)) => value,
        Some(Err(payload)) => resume_unwind(payload),
        None => panic!("a worker finished without a result"),
    }
}

pub fn sleep(duration: Duration) {
    if !on_fiber() {
        std::thread::sleep(duration);
        return;
    }
    let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    if ms == 0 {
        yield_now();
        return;
    }
    match park_op(Op::Timeout { ms }, false) {
        Event::Timeout | Event::Failed(_) => {}
        _ => {}
    }
}

pub fn yield_now() {
    let id = current_id();
    ready_id(unsafe { &mut *sched_ptr() }, id);
    park();
}

#[derive(Debug)]
pub struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

pub fn timeout<T: 'static>(duration: Duration, body: impl FnOnce() -> T) -> Result<T, Elapsed> {
    race_biased(vec![Box::new(|| Ok(body())), Box::new(|| {
        sleep(duration);
        Err(Elapsed)
    })])
}

pub struct Interval {
    every: Duration,
    first: bool,
}

pub fn interval(every: Duration) -> Interval {
    Interval { every, first: true }
}

impl Interval {
    pub fn tick(&mut self) {
        if self.first {
            self.first = false;
            return;
        }
        sleep(self.every);
    }
}

pub fn call<R: Send + 'static>(body: impl FnOnce() -> R + Send + 'static) -> R {
    if CURRENT.with(|c| c.get()) != 0 || on_loop() {
        return body();
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    post(move || {
        spawn(move || {
            let _ = tx.send(body());
        });
    });
    rx.recv().unwrap_or_else(|_| panic!("the event loop stopped before the call finished"))
}

thread_local! {
    static HOME: std::cell::RefCell<Option<Arc<Poke>>> = const { std::cell::RefCell::new(None) };
}

struct Home;

impl Home {
    fn enter(owner: Arc<Poke>) -> Home {
        HOME.with(|h| *h.borrow_mut() = Some(owner));
        Home
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        HOME.with(|h| *h.borrow_mut() = None);
    }
}

pub(super) fn home() -> Option<Arc<Poke>> {
    HOME.with(|h| h.borrow().clone())
}

pub(super) fn call_home<R: Send + 'static>(owner: Arc<Poke>, body: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    post_to(&owner, move || {
        spawn(move || {
            let _ = tx.send(body());
        });
    });
    rx.recv().unwrap_or_else(|_| panic!("the loop that runs this pool thread stopped"))
}

pub(super) fn post(job: impl FnOnce() + Send + 'static) {
    let poke = home()
        .or_else(|| poke_global().lock().unwrap_or_else(|e| e.into_inner()).clone())
        .expect("the event loop is running");
    post_to(&poke, job);
}

fn post_to(poke: &Arc<Poke>, job: impl FnOnce() + Send + 'static) {
    poke.spawn.lock().unwrap_or_else(|e| e.into_inner()).push(Box::new(job));
    signal_poke(poke);
}

pub fn spawn_off<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> JoinHandle<T> {
    if CURRENT.with(|c| c.get()) != 0 || on_loop() {
        return spawn(body);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    post(move || {
        let _ = tx.send(spawn(body));
    });
    rx.recv().unwrap_or_else(|_| panic!("the event loop stopped before the fiber started"))
}

pub fn background(body: impl FnOnce() + Send + 'static) {
    if on_loop() {
        spawn_raw(Spec { parent: None, race: None, task: None, background: true }, body);
        return;
    }
    post(move || {
        spawn_raw(Spec { parent: None, race: None, task: None, background: true }, body);
    });
}
