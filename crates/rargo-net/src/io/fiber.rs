
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
}

pub(super) struct Sched {
    pub(super) ring: Loop,
    pub(super) poke: Arc<Poke>,
    pub(super) next_id: u64,
    pub(super) fibers: BTreeMap<u64, Fiber>,
    pub(super) ready: VecDeque<u64>,
    pub(super) inbox: BTreeMap<u64, Resume>,
    pub(super) waiting: BTreeMap<u64, u64>,
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

pub(super) fn current_id() -> u64 {
    let id = CURRENT.with(|c| c.get());
    assert!(id != 0, "this runs on a fiber");
    id
}

pub(super) fn fresh_stack() -> DefaultStack {
    DefaultStack::new(STACK).unwrap_or_else(|e| panic!("a fiber stack could not be allocated: {e}"))
}

pub(super) fn boot(workers: usize) -> Loop {
    match Loop::new(workers) {
        Ok(ring) => ring,
        Err(err) => {
            match super::refusal(&err) {
                Some(msg) => eprintln!("{msg}"),
                None => eprintln!("io_uring_setup failed: {err}"),
            }
            std::process::exit(1);
        }
    }
}

pub(super) struct Spec {
    pub(super) parent: Option<u64>,
    pub(super) race: Option<u16>,
    pub(super) task: Option<Arc<dyn Finish>>,
}

pub(super) fn spawn_raw(spec: Spec, body: impl FnOnce()) -> u64 {
    let s = unsafe { &mut *sched_ptr() };
    let id = s.next_id;
    s.next_id += 1;
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
    let mut resumed = park_raw(Pause::Submit { op, cpu });
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
    assert!(!on_loop(), "the event loop is already running");
    let ring = boot(workers);
    let poke = Arc::new(Poke {
        efd: ring.wake_fd(),
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
    let _clear = Clear;
    spawn_raw(Spec { parent: None, race: None, task: None }, main);
    run(ptr);
}

pub(super) fn run(sched: *mut Sched) {
    loop {
        let id = {
            let s = unsafe { &mut *sched };
            drain_poke(s);
            if s.fibers.is_empty() {
                return;
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
    match resumed {
        Ok(CoroutineResult::Yield(pause)) => {
            let s = unsafe { &mut *sched };
            s.fibers.get_mut(&id).unwrap().co = Some(co);
            match pause {
                Pause::Park => {}
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
            drop(co);
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
    drop(fiber);
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
    drop(fiber);
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

