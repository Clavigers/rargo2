
use std::any::Any;
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::ThreadId;

use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};

use super::ops::{Event, Op};
use super::ring::Loop;
use super::sim::{Report, SimLoop};

pub(super) const STACK: usize = 8 << 20;

thread_local! {
    pub(super) static SCHED: Cell<*mut Sched> = const { Cell::new(std::ptr::null_mut()) };
    pub(super) static YIELDER: Cell<*const Yielder<Resume, Pause>> = const { Cell::new(std::ptr::null()) };
    pub(super) static CURRENT: Cell<u64> = const { Cell::new(0) };
    pub(super) static LOOP_THREAD: Cell<Option<ThreadId>> = const { Cell::new(None) };
}

pub(super) enum Pause {
    Park,
    Submit { op: Op, cpu: bool },
    /// The backend already holds the operation under this token.
    Wait { token: u64 },
}

pub(super) enum Resume {
    Go,
    Event(Event),
}

pub(super) struct Poke {
    pub(super) efd: i32,
    pub(super) q: StdMutex<Vec<u64>>,
    pub(super) spawn: StdMutex<Vec<Box<dyn FnOnce() + Send>>>,
}

pub(super) fn poke_global() -> &'static StdMutex<Option<Arc<Poke>>> {
    static POKE: StdMutex<Option<Arc<Poke>>> = StdMutex::new(None);
    &POKE
}

pub(super) struct Fiber {
    pub(super) co: Option<Coroutine<Resume, Pause, (), DefaultStack>>,
    pub(super) queued: bool,
    pub(super) token: Option<u64>,
    pub(super) in_cpu: bool,
    pub(super) cancel: bool,
    pub(super) parent: Option<u64>,
    pub(super) race: Option<u16>,
    pub(super) task: Option<Arc<dyn Finish>>,
    pub(super) background: bool,
}

pub(super) struct Sched {
    pub(super) ring: Backend,
    pub(super) poke: Arc<Poke>,
    pub(super) next_id: u64,
    pub(super) fibers: BTreeMap<u64, Fiber>,
    pub(super) ready: VecDeque<u64>,
    pub(super) inbox: BTreeMap<u64, Resume>,
    pub(super) waiting: BTreeMap<u64, u64>,
}

/// What answers the scheduler's operations: the kernel's io_uring, or a simulation.
pub(super) enum Backend {
    Real(Loop),
    Sim(SimLoop),
}

impl Backend {
    fn submit(&mut self, op: Op) -> io::Result<u64> {
        match self {
            Backend::Real(ring) => ring.submit(op),
            Backend::Sim(sim) => sim.submit(op),
        }
    }

    fn wait(&mut self) -> io::Result<Vec<(u64, Event)>> {
        match self {
            Backend::Real(ring) => ring.wait(),
            Backend::Sim(sim) => sim.wait(),
        }
    }

    fn cancel(&mut self, token: u64) {
        match self {
            Backend::Real(ring) => ring.cancel(token),
            Backend::Sim(sim) => sim.cancel(token),
        }
    }
}

/// Every fiber but the caller becomes a background fiber, so the loop ends them when the caller returns.
pub(super) fn rest_in_background() {
    let me = current_id();
    let s = unsafe { &mut *sched_ptr() };
    for (id, fiber) in s.fibers.iter_mut() {
        if *id != me {
            fiber.background = true;
        }
    }
}

/// The backend of the loop this thread is driving, if it is driving one.
pub(super) fn backend() -> Option<&'static mut Backend> {
    let p = SCHED.with(|c| c.get());
    if p.is_null() { None } else { Some(unsafe { &mut (*p).ring }) }
}

pub fn loop_running() -> bool {
    on_loop() || poke_global().lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

pub fn on_fiber() -> bool {
    CURRENT.with(|c| c.get()) != 0
}

/// Whether this thread drives a loop: one that must never block on what a
/// fiber of its own loop has yet to do.
pub fn on_loop_thread() -> bool {
    on_loop()
}

pub(super) fn on_loop() -> bool {
    LOOP_THREAD.with(|c| c.get().is_some())
}

pub(super) fn sched_ptr() -> *mut Sched {
    let p = SCHED.with(|c| c.get());
    assert!(!p.is_null(), "this runs on the event loop");
    p
}

pub(super) fn poke_arc() -> Arc<Poke> {
    Arc::clone(&unsafe { &*sched_ptr() }.poke)
}

struct Outside {
    current: u64,
    yielder: *const Yielder<Resume, Pause>,
}

impl Outside {
    fn enter() -> Outside {
        Outside { current: CURRENT.with(|c| c.replace(0)), yielder: YIELDER.with(|c| c.replace(std::ptr::null())) }
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.current));
        YIELDER.with(|c| c.set(self.yielder));
    }
}

fn drop_outside(fiber: Fiber) {
    let _outside = Outside::enter();
    drop(fiber);
}

pub(super) fn current_id() -> u64 {
    let id = CURRENT.with(|c| c.get());
    assert!(id != 0, "this runs on a fiber");
    id
}

const SPARE_STACKS: usize = 64;

thread_local! {
    static SPARES: std::cell::RefCell<Vec<DefaultStack>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) fn fresh_stack() -> DefaultStack {
    if let Some(stack) = SPARES.with(|s| s.borrow_mut().pop()) {
        return stack;
    }
    DefaultStack::new(STACK).unwrap_or_else(|e| panic!("a fiber stack could not be allocated: {e}"))
}

fn spare_stack(co: Coroutine<Resume, Pause, (), DefaultStack>) {
    let stack = co.into_stack();
    SPARES.with(|s| {
        let mut spares = s.borrow_mut();
        if spares.len() < SPARE_STACKS {
            spares.push(stack);
        }
    });
}

#[cfg(test)]
pub(super) fn spare_stacks() -> usize {
    SPARES.with(|s| s.borrow().len())
}

pub(super) fn boot(workers: usize) -> Loop {
    match Loop::new(workers) {
        Ok(ring) => ring,
        Err(err) => {
            match super::refusal(&err) {
                Some(msg) => eprintln!("error: {msg}"),
                None => eprintln!("error: io_uring_setup failed: {err}"),
            }
            std::process::exit(101);
        }
    }
}

pub(super) struct Spec {
    pub(super) parent: Option<u64>,
    pub(super) race: Option<u16>,
    pub(super) task: Option<Arc<dyn Finish>>,
    pub(super) background: bool,
}

pub(super) fn spawn_raw(spec: Spec, body: impl FnOnce()) -> u64 {
    let s = unsafe { &mut *sched_ptr() };
    let id = s.next_id;
    s.next_id += 1;
    let background = spec.background || spec.parent.and_then(|p| s.fibers.get(&p)).is_some_and(|f| f.background);
    let co = unsafe {
        Coroutine::with_stack_unchecked(fresh_stack(), move |yielder: &Yielder<Resume, Pause>, _: Resume| {
            YIELDER.with(|c| c.set(yielder as *const Yielder<Resume, Pause>));
            CURRENT.with(|c| c.set(id));
            body();
        })
    };
    s.fibers.insert(
        id,
        Fiber {
            co: Some(co),
            queued: false,
            token: None,
            in_cpu: false,
            cancel: false,
            parent: spec.parent,
            race: spec.race,
            task: spec.task,
            background,
        },
    );
    ready_id(s, id);
    id
}

pub(super) fn ready_id(s: &mut Sched, id: u64) {
    let Some(fiber) = s.fibers.get_mut(&id) else { return };
    if fiber.queued {
        return;
    }
    fiber.queued = true;
    let (parent, race) = (fiber.parent, fiber.race);
    let at = race.and_then(|race| {
        s.ready.iter().position(|other| {
            s.fibers.get(other).is_some_and(|o| o.parent == parent && o.race.is_some_and(|r| r > race))
        })
    });
    match at {
        Some(at) => s.ready.insert(at, id),
        None => s.ready.push_back(id),
    }
}

pub(super) fn wake(poke: Arc<Poke>, id: u64) {
    if on_loop() {
        ready_id(unsafe { &mut *sched_ptr() }, id);
        return;
    }
    poke.q.lock().unwrap_or_else(|e| e.into_inner()).push(id);
    let one = 1u64;
    unsafe { libc::write(poke.efd, (&one as *const u64).cast(), 8) };
}

pub(super) fn drain_poke(s: &mut Sched) {
    let ids: Vec<u64> = s.poke.q.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
    for id in ids {
        ready_id(s, id);
    }
    let posted: Vec<Box<dyn FnOnce() + Send>> = s.poke.spawn.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
    for job in posted {
        job();
    }
}

pub(super) fn signal_poke(poke: &Poke) {
    let one = 1u64;
    unsafe { libc::write(poke.efd, (&one as *const u64).cast(), 8) };
}

pub(super) fn pick(s: &mut Sched) -> Option<u64> {
    while let Some(id) = s.ready.pop_front() {
        if let Some(fiber) = s.fibers.get_mut(&id) {
            fiber.queued = false;
            return Some(id);
        }
    }
    None
}

pub(super) fn park_raw(pause: Pause) -> Resume {
    let yielder = YIELDER.with(|c| c.get());
    let id = CURRENT.with(|c| c.get());
    assert!(!yielder.is_null(), "park outside a fiber");
    let resumed = unsafe { (*yielder).suspend(pause) };
    YIELDER.with(|c| c.set(yielder));
    CURRENT.with(|c| c.set(id));
    resumed
}

pub(super) fn park() {
    let _ = park_raw(Pause::Park);
}

pub(super) fn park_op(op: Op, cpu: bool) -> Event {
    let pause = match backend() {
        Some(Backend::Sim(sim)) if sim.answers_in_place() => match sim.answer_in_place(op) {
            Ok(event) => return event,
            Err(token) => Pause::Wait { token },
        },
        _ => Pause::Submit { op, cpu },
    };
    let mut resumed = park_raw(pause);
    loop {
        match resumed {
            Resume::Event(event) => return event,
            Resume::Go if op_pending() => resumed = park_raw(Pause::Park),
            Resume::Go => return Event::Failed(io::Error::other("the operation was not completed")),
        }
    }
}

pub(super) fn op_pending() -> bool {
    let id = current_id();
    unsafe { &*sched_ptr() }.fibers.get(&id).is_some_and(|fiber| fiber.token.is_some())
}

pub(super) fn op_event(op: Op) -> io::Result<Event> {
    match park_op(op, false) {
        Event::Failed(err) => Err(err),
        event => Ok(event),
    }
}

pub fn drive(workers: usize, main: impl FnOnce() + 'static) {
    let ring = boot(workers);
    let efd = ring.wake_fd();
    drive_on(Backend::Real(ring), efd, main);
}

/// Drives `main` on a simulated backend, on this thread alone, and reports the run.
pub(super) fn drive_sim(sim: SimLoop, main: impl FnOnce() + 'static) -> Report {
    match drive_on(Backend::Sim(sim), -1, main) {
        Backend::Sim(sim) => sim.report(),
        Backend::Real(_) => unreachable!("the loop returns the backend it was given"),
    }
}

fn drive_on(ring: Backend, efd: i32, main: impl FnOnce() + 'static) -> Backend {
    assert!(!on_loop(), "the event loop is already running");
    let poke = Arc::new(Poke {
        efd,
        q: StdMutex::new(Vec::new()),
        spawn: StdMutex::new(Vec::new()),
    });
    *poke_global().lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&poke));
    let mut sched = Sched {
        ring,
        poke,
        next_id: 1,
        fibers: BTreeMap::new(),
        ready: VecDeque::new(),
        inbox: BTreeMap::new(),
        waiting: BTreeMap::new(),
    };
    let ptr = &mut sched as *mut Sched;
    SCHED.with(|c| c.set(ptr));
    LOOP_THREAD.with(|c| c.set(Some(std::thread::current().id())));
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            SCHED.with(|c| c.set(std::ptr::null_mut()));
            LOOP_THREAD.with(|c| c.set(None));
            *poke_global().lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    let clear = Clear;
    spawn_raw(Spec { parent: None, race: None, task: None, background: false }, main);
    run(ptr);
    drop(clear);
    sched.ring
}

pub(super) fn run(sched: *mut Sched) {
    loop {
        let id = {
            let s = unsafe { &mut *sched };
            drain_poke(s);
            if s.fibers.is_empty() {
                return;
            }
            if s.fibers.values().all(|fiber| fiber.background) {
                let left: Vec<u64> = s.fibers.iter().filter(|(_, f)| !f.cancel).map(|(id, _)| *id).collect();
                if !left.is_empty() {
                    for id in left {
                        cancel_request(id);
                    }
                    continue;
                }
            }
            pick(s)
        };
        if let Some(id) = id {
            resume_one(sched, id);
            continue;
        }
        let events = unsafe { &mut *sched }.ring.wait().unwrap_or_else(|err| {
            eprintln!("io_uring: {err}");
            std::process::exit(1);
        });
        dispatch(sched, events);
    }
}

pub(super) fn dispatch(sched: *mut Sched, events: Vec<(u64, Event)>) {
    let mut dead = Vec::new();
    {
        let s = unsafe { &mut *sched };
        for (token, event) in events {
            let Some(fid) = s.waiting.remove(&token) else {
                event.release();
                continue;
            };
            let Some(fiber) = s.fibers.get_mut(&fid) else {
                event.release();
                continue;
            };
            if fiber.cancel {
                event.release();
                dead.push(fid);
                continue;
            }
            fiber.token = None;
            fiber.in_cpu = false;
            s.inbox.insert(fid, Resume::Event(event));
            ready_id(s, fid);
        }
        drain_poke(s);
    }
    for id in dead {
        reap(sched, id);
    }
}

pub(super) fn resume_one(sched: *mut Sched, id: u64) {
    let input = {
        let s = unsafe { &mut *sched };
        s.inbox.remove(&id).unwrap_or(Resume::Go)
    };
    let mut co = {
        let s = unsafe { &mut *sched };
        s.fibers.get_mut(&id).expect("a ready fiber exists").co.take().expect("a fiber has its stack")
    };
    let resumed = catch_unwind(AssertUnwindSafe(|| co.resume(input)));
    CURRENT.with(|c| c.set(0));
    YIELDER.with(|c| c.set(std::ptr::null()));
    match resumed {
        Ok(CoroutineResult::Yield(pause)) => {
            let s = unsafe { &mut *sched };
            s.fibers.get_mut(&id).unwrap().co = Some(co);
            match pause {
                Pause::Park => {}
                Pause::Wait { token } => {
                    s.fibers.get_mut(&id).unwrap().token = Some(token);
                    s.waiting.insert(token, id);
                }
                Pause::Submit { op, cpu } => match s.ring.submit(op) {
                    Ok(token) => {
                        {
                            let fiber = s.fibers.get_mut(&id).unwrap();
                            fiber.token = Some(token);
                            fiber.in_cpu = cpu;
                        }
                        s.waiting.insert(token, id);
                    }
                    Err(err) => {
                        s.inbox.insert(id, Resume::Event(Event::Failed(err)));
                        ready_id(s, id);
                    }
                },
            }
        }
        Ok(CoroutineResult::Return(())) => {
            spare_stack(co);
            finish(sched, id, None);
        }
        Err(payload) => {
            drop(co);
            finish(sched, id, Some(payload));
        }
    }
}

pub(super) fn finish(sched: *mut Sched, id: u64, panic: Option<Box<dyn Any + Send>>) {
    let mut fiber = {
        let s = unsafe { &mut *sched };
        let mut fiber = s.fibers.remove(&id).expect("a finished fiber is still recorded");
        forget(s, &mut fiber, id);
        fiber
    };
    let parent = fiber.parent;
    let race = fiber.race;
    let task = fiber.task.take();
    drop(fiber);
    if let Some(task) = task {
        if let Some(payload) = panic {
            task.fail_panic(payload);
        } else if !task.is_done() {
            task.fail_abort();
        }
        if let Some(waiter) = task.take_waiter() {
            ready_id(unsafe { &mut *sched }, waiter);
        }
    } else if let Some(payload) = panic {
        resume_unwind(payload);
    }
    if race.is_some() {
        let siblings = {
            let s = unsafe { &mut *sched };
            s.fibers
                .iter()
                .filter(|(_, fiber)| fiber.parent == parent && fiber.race.is_some())
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        };
        for sibling in siblings {
            let _ = cancel_request(sibling);
        }
        if let Some(parent) = parent {
            ready_id(unsafe { &mut *sched }, parent);
        }
    } else if parent.is_some() && race.is_none() {
        ready_id(unsafe { &mut *sched }, parent.unwrap());
    }
}

pub(super) fn cancel_request(id: u64) -> bool {
    let fiber = {
        let s = unsafe { &mut *sched_ptr() };
        let Some(fiber) = s.fibers.get(&id) else { return false };
        if fiber.in_cpu {
            s.fibers.get_mut(&id).unwrap().cancel = true;
            return true;
        }
        let mut fiber = s.fibers.remove(&id).unwrap();
        forget(s, &mut fiber, id);
        fiber
    };
    let task = fiber.task.clone();
    drop_outside(fiber);
    if let Some(task) = task {
        task.fail_abort();
        if let Some(waiter) = task.take_waiter() {
            ready_id(unsafe { &mut *sched_ptr() }, waiter);
        }
    }
    false
}

pub(super) fn forget(s: &mut Sched, fiber: &mut Fiber, id: u64) {
    if let Some(token) = fiber.token.take() {
        s.waiting.remove(&token);
        s.ring.cancel(token);
    }
    if let Some(Resume::Event(event)) = s.inbox.remove(&id) {
        event.release();
    }
    s.ready.retain(|ready| *ready != id);
}

pub(super) fn reap(sched: *mut Sched, id: u64) {
    let fiber = {
        let s = unsafe { &mut *sched };
        let Some(mut fiber) = s.fibers.remove(&id) else { return };
        forget(s, &mut fiber, id);
        fiber.in_cpu = false;
        fiber
    };
    let task = fiber.task.clone();
    let parent = fiber.parent;
    let race = fiber.race;
    drop_outside(fiber);
    if let Some(task) = task {
        task.fail_abort();
        if let Some(waiter) = task.take_waiter() {
            ready_id(unsafe { &mut *sched }, waiter);
        }
    }
    if race.is_some() {
        if let Some(parent) = parent {
            ready_id(unsafe { &mut *sched }, parent);
        }
    }
}

pub(super) trait Finish: Send + Sync {
    fn fail_panic(&self, payload: Box<dyn Any + Send>);
    fn fail_abort(&self);
    fn arm(&self, id: u64);
    fn take_waiter(&self) -> Option<u64>;
    fn is_done(&self) -> bool;
}

