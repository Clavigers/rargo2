
use std::any::Any;
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::{self, ErrorKind};
use std::mem;
use std::net::{SocketAddr, ToSocketAddrs};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::ThreadId;
use std::time::Duration;

use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};

use super::{c_path, parse_addr, Event, Loop, Op};

const STACK: usize = 8 << 20;

thread_local! {
    static SCHED: Cell<*mut Sched> = const { Cell::new(std::ptr::null_mut()) };
    static YIELDER: Cell<*const Yielder<Resume, Pause>> = const { Cell::new(std::ptr::null()) };
    static CURRENT: Cell<u64> = const { Cell::new(0) };
    static LOOP_THREAD: Cell<Option<ThreadId>> = const { Cell::new(None) };
}

enum Pause {
    Park,
    Submit { op: Op, cpu: bool },
}

enum Resume {
    Go,
    Event(Event),
}

struct Poke {
    efd: i32,
    q: StdMutex<Vec<u64>>,
    spawn: StdMutex<Vec<Box<dyn FnOnce() + Send>>>,
}

fn poke_global() -> &'static StdMutex<Option<Arc<Poke>>> {
    static POKE: StdMutex<Option<Arc<Poke>>> = StdMutex::new(None);
    &POKE
}

struct Fiber {
    co: Option<Coroutine<Resume, Pause, (), DefaultStack>>,
    queued: bool,
    token: Option<u64>,
    in_cpu: bool,
    cancel: bool,
    parent: Option<u64>,
    race: Option<u16>,
    task: Option<Arc<dyn Finish>>,
}

struct Sched {
    ring: Loop,
    poke: Arc<Poke>,
    next_id: u64,
    fibers: BTreeMap<u64, Fiber>,
    ready: VecDeque<u64>,
    inbox: BTreeMap<u64, Resume>,
    waiting: BTreeMap<u64, u64>,
}

fn on_loop() -> bool {
    LOOP_THREAD.with(|c| c.get().is_some())
}

fn sched_ptr() -> *mut Sched {
    let p = SCHED.with(|c| c.get());
    assert!(!p.is_null(), "this runs on the event loop");
    p
}

fn poke_arc() -> Arc<Poke> {
    Arc::clone(&unsafe { &*sched_ptr() }.poke)
}

fn current_id() -> u64 {
    let id = CURRENT.with(|c| c.get());
    assert!(id != 0, "this runs on a fiber");
    id
}

fn fresh_stack() -> DefaultStack {
    DefaultStack::new(STACK).unwrap_or_else(|e| panic!("a fiber stack could not be allocated: {e}"))
}

fn boot(workers: usize) -> Loop {
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

struct Spec {
    parent: Option<u64>,
    race: Option<u16>,
    task: Option<Arc<dyn Finish>>,
}

fn spawn_raw(spec: Spec, body: impl FnOnce()) -> u64 {
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
            queued: true,
            token: None,
            in_cpu: false,
            cancel: false,
            parent: spec.parent,
            race: spec.race,
            task: spec.task,
        },
    );
    s.ready.push_back(id);
    id
}

fn ready_id(s: &mut Sched, id: u64) {
    let Some(fiber) = s.fibers.get_mut(&id) else { return };
    if fiber.queued {
        return;
    }
    fiber.queued = true;
    s.ready.push_back(id);
}

fn wake(poke: Arc<Poke>, id: u64) {
    if on_loop() {
        ready_id(unsafe { &mut *sched_ptr() }, id);
        return;
    }
    poke.q.lock().unwrap_or_else(|e| e.into_inner()).push(id);
    let one = 1u64;
    unsafe { libc::write(poke.efd, (&one as *const u64).cast(), 8) };
}

fn drain_poke(s: &mut Sched) {
    let ids: Vec<u64> = s.poke.q.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
    for id in ids {
        ready_id(s, id);
    }
    let posted: Vec<Box<dyn FnOnce() + Send>> = s.poke.spawn.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
    for job in posted {
        job();
    }
}

fn signal_poke(poke: &Poke) {
    let one = 1u64;
    unsafe { libc::write(poke.efd, (&one as *const u64).cast(), 8) };
}

pub fn call<R: Send + 'static>(body: impl FnOnce() -> R + Send + 'static) -> R {
    if CURRENT.with(|c| c.get()) != 0 {
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

fn post(job: impl FnOnce() + Send + 'static) {
    let poke = poke_global().lock().unwrap_or_else(|e| e.into_inner()).clone().expect("the event loop is running");
    poke.spawn.lock().unwrap_or_else(|e| e.into_inner()).push(Box::new(job));
    signal_poke(&poke);
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

fn prefer_biased(s: &mut Sched) {
    if s.ready.len() < 2 {
        return;
    }
    let mut ids: Vec<u64> = s.ready.drain(..).collect();
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            let (Some(a), Some(b)) = (s.fibers.get(&ids[i]), s.fibers.get(&ids[j])) else { continue };
            if a.parent == b.parent && a.race.is_some() && b.race.is_some() && b.race < a.race {
                ids.swap(i, j);
            }
        }
    }
    s.ready.extend(ids);
}

fn pick(s: &mut Sched) -> Option<u64> {
    while let Some(id) = s.ready.pop_front() {
        if let Some(fiber) = s.fibers.get_mut(&id) {
            fiber.queued = false;
            return Some(id);
        }
    }
    None
}

fn park_raw(pause: Pause) -> Resume {
    let yielder = YIELDER.with(|c| c.get());
    let id = CURRENT.with(|c| c.get());
    assert!(!yielder.is_null(), "park outside a fiber");
    let resumed = unsafe { (*yielder).suspend(pause) };
    YIELDER.with(|c| c.set(yielder));
    CURRENT.with(|c| c.set(id));
    resumed
}

fn park() {
    let _ = park_raw(Pause::Park);
}

fn park_op(op: Op, cpu: bool) -> Event {
    let mut resumed = park_raw(Pause::Submit { op, cpu });
    loop {
        match resumed {
            Resume::Event(event) => return event,
            Resume::Go if op_pending() => resumed = park_raw(Pause::Park),
            Resume::Go => return Event::Failed(io::Error::other("the operation was not completed")),
        }
    }
}

fn op_pending() -> bool {
    let id = current_id();
    unsafe { &*sched_ptr() }.fibers.get(&id).is_some_and(|fiber| fiber.token.is_some())
}

fn op_event(op: Op) -> io::Result<Event> {
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

fn run(sched: *mut Sched) {
    loop {
        let id = {
            let s = unsafe { &mut *sched };
            drain_poke(s);
            if s.fibers.is_empty() {
                return;
            }
            prefer_biased(s);
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

fn dispatch(sched: *mut Sched, events: Vec<(u64, Event)>) {
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

fn resume_one(sched: *mut Sched, id: u64) {
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

fn finish(sched: *mut Sched, id: u64, panic: Option<Box<dyn Any + Send>>) {
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

fn cancel_request(id: u64) -> bool {
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

fn forget(s: &mut Sched, fiber: &mut Fiber, id: u64) {
    if let Some(token) = fiber.token.take() {
        s.waiting.remove(&token);
        s.ring.cancel(token);
    }
    if let Some(Resume::Event(event)) = s.inbox.remove(&id) {
        event.release();
    }
    s.ready.retain(|ready| *ready != id);
}

fn reap(sched: *mut Sched, id: u64) {
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

struct CancelGuard {
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


trait Finish: Send + Sync {
    fn fail_panic(&self, payload: Box<dyn Any + Send>);
    fn fail_abort(&self);
    fn arm(&self, id: u64);
    fn take_waiter(&self) -> Option<u64>;
    fn is_done(&self) -> bool;
}

enum Outcome<T> {
    Value(T),
    Panic(Box<dyn Any + Send>),
    Abort,
}

struct TaskSlot<T> {
    mu: StdMutex<TaskState<T>>,
}

struct TaskState<T> {
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

fn fiber_alive(id: u64) -> bool {
    unsafe { &*sched_ptr() }.fibers.contains_key(&id)
}

pub fn spawn<T: Send + 'static>(body: impl FnOnce() -> T + 'static) -> JoinHandle<T> {
    let slot = Arc::new(TaskSlot::new());
    let task: Arc<dyn Finish> = slot.clone();
    let slot_for_body = slot.clone();
    let id = spawn_raw(Spec { parent: None, race: None, task: Some(task) }, move || {
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
        let id = spawn_raw(Spec { parent: Some(parent), race: Some(index as u16), task: None }, move || {
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
    let mut pack = Pack { body: Some(body), out: None };
    let go = typed_exec(&mut pack);
    let ptr = &mut pack as *mut Pack<_, R> as usize;
    let event = park_op(
        Op::Cpu(Box::new(move || {
            go(ptr);
            Ok(Box::new(()) as Box<dyn Any + Send>)
        })),
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


pub struct Notify {
    inner: StdMutex<NotifyState>,
    poke: Arc<Poke>,
}

struct NotifyState {
    permit: bool,
    waiters: VecDeque<u64>,
    handed: Vec<u64>,
    released: Vec<u64>,
}

impl Notify {
    pub fn new() -> Self {
        Self {
            inner: StdMutex::new(NotifyState { permit: false, waiters: VecDeque::new(), handed: Vec::new(), released: Vec::new() }),
            poke: poke_arc(),
        }
    }

    pub fn notify_one(&self) {
        let id = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(id) = guard.waiters.pop_front() {
                guard.handed.push(id);
                Some(id)
            } else {
                guard.permit = true;
                None
            }
        };
        if let Some(id) = id {
            wake(Arc::clone(&self.poke), id);
        }
    }

    pub fn notify_waiters(&self) {
        let ids: Vec<u64> = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.permit = false;
            let ids: Vec<u64> = guard.waiters.drain(..).collect();
            guard.released.extend(ids.iter().copied());
            ids
        };
        for id in ids {
            wake(Arc::clone(&self.poke), id);
        }
    }

    pub fn notified(&self) {
        struct Leave<'a> {
            notify: &'a Notify,
            me: u64,
            done: bool,
        }
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                if self.done {
                    return;
                }
                let handed = {
                    let mut guard = self.notify.inner.lock().unwrap_or_else(|e| e.into_inner());
                    guard.waiters.retain(|id| *id != self.me);
                    guard.released.retain(|id| *id != self.me);
                    let at = guard.handed.iter().position(|id| *id == self.me);
                    at.map(|at| guard.handed.swap_remove(at)).is_some()
                };
                if handed {
                    self.notify.notify_one();
                }
            }
        }
        let me = current_id();
        let mut leave = Leave { notify: self, me, done: false };
        loop {
            {
                let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(at) = guard.handed.iter().position(|id| *id == me) {
                    guard.handed.swap_remove(at);
                    guard.released.retain(|id| *id != me);
                    leave.done = true;
                    return;
                }
                if let Some(at) = guard.released.iter().position(|id| *id == me) {
                    guard.released.swap_remove(at);
                    leave.done = true;
                    return;
                }
                if guard.permit {
                    guard.permit = false;
                    leave.done = true;
                    return;
                }
                if !guard.waiters.contains(&me) {
                    guard.waiters.push_back(me);
                }
            }
            park();
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Mutex<T> {
    held: std::sync::atomic::AtomicBool,
    waiters: StdMutex<VecDeque<u64>>,
    value: std::cell::UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            held: std::sync::atomic::AtomicBool::new(false),
            waiters: StdMutex::new(VecDeque::new()),
            value: std::cell::UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        use std::sync::atomic::Ordering::{Acquire, AcqRel};
        struct Leave<'a, T> {
            lock: &'a Mutex<T>,
            me: u64,
            done: bool,
        }
        impl<T> Drop for Leave<'_, T> {
            fn drop(&mut self) {
                if self.done || self.me == 0 {
                    return;
                }
                let waiting = {
                    let mut waiters = self.lock.waiters.lock().unwrap_or_else(|e| e.into_inner());
                    let before = waiters.len();
                    waiters.retain(|id| *id != self.me);
                    waiters.len() != before
                };
                if !waiting {
                    self.lock.wake_next();
                }
            }
        }
        let mut leave = Leave { lock: self, me: 0, done: false };
        loop {
            if self.held.compare_exchange(false, true, AcqRel, Acquire).is_ok() {
                leave.done = true;
                return MutexGuard { lock: self };
            }
            let me = current_id();
            leave.me = me;
            self.waiters.lock().unwrap_or_else(|e| e.into_inner()).push_back(me);
            if self.held.compare_exchange(false, true, AcqRel, Acquire).is_ok() {
                self.waiters.lock().unwrap_or_else(|e| e.into_inner()).retain(|id| *id != me);
                leave.done = true;
                return MutexGuard { lock: self };
            }
            park();
        }
    }

    fn wake_next(&self) {
        let next = self.waiters.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        if let Some(id) = next {
            assert!(on_loop(), "a fiber lock is released on the event loop");
            ready_id(unsafe { &mut *sched_ptr() }, id);
        }
    }

    fn unlock(&self) {
        use std::sync::atomic::Ordering::Release;
        self.held.store(false, Release);
        self.wake_next();
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Mutex::new(T::default())
    }
}

pub struct MutexGuard<'a, T> {
    lock: &'a Mutex<T>,
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}

impl<T> std::ops::Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> std::ops::DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

struct SemState {
    permits: u32,
    closed: bool,
    waiters: VecDeque<(u64, u32)>,
}

struct SemInner {
    mu: StdMutex<SemState>,
    poke: Arc<Poke>,
}

pub struct Semaphore {
    inner: Arc<SemInner>,
}

pub struct Permit {
    inner: Arc<SemInner>,
    n: u32,
    forget: bool,
}

impl Semaphore {
    pub fn new(permits: u32) -> Self {
        Self {
            inner: Arc::new(SemInner {
                mu: StdMutex::new(SemState { permits, closed: false, waiters: VecDeque::new() }),
                poke: poke_arc(),
            }),
        }
    }

    pub fn acquire_many(&self, n: u32) -> io::Result<Permit> {
        struct Leave<'a> {
            sem: &'a Semaphore,
            me: u64,
            done: bool,
        }
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                if self.done {
                    return;
                }
                self.sem.inner.mu.lock().unwrap_or_else(|e| e.into_inner()).waiters.retain(|waiter| waiter.0 != self.me);
                self.sem.nudge();
            }
        }
        let mut leave = Leave { sem: self, me: current_id(), done: false };
        loop {
            let me = current_id();
            let ready = {
                let mut guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
                if guard.closed {
                    return Err(io::Error::new(ErrorKind::BrokenPipe, "the server went away"));
                }
                let head = guard.waiters.front().map(|waiter| waiter.0);
                let mine = head == Some(me);
                if (guard.waiters.is_empty() || mine) && guard.permits >= n {
                    if mine {
                        guard.waiters.pop_front();
                    }
                    guard.permits -= n;
                    true
                } else {
                    if !mine && !guard.waiters.iter().any(|waiter| waiter.0 == me) {
                        guard.waiters.push_back((me, n));
                    }
                    false
                }
            };
            if ready {
                leave.done = true;
                self.nudge();
                return Ok(Permit { inner: Arc::clone(&self.inner), n, forget: false });
            }
            park();
        }
    }

    pub fn add_permits(&self, n: u32) {
        {
            let mut guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
            guard.permits = guard.permits.saturating_add(n);
        }
        self.nudge();
    }

    pub fn close(&self) {
        let waiters: Vec<u64> = {
            let mut guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
            guard.closed = true;
            guard.waiters.drain(..).map(|waiter| waiter.0).collect()
        };
        for id in waiters {
            wake(Arc::clone(&self.inner.poke), id);
        }
    }

    fn nudge(&self) {
        let id = {
            let guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
            match guard.waiters.front() {
                Some(&(id, need)) if !guard.closed && guard.permits >= need => Some(id),
                _ => None,
            }
        };
        if let Some(id) = id {
            wake(Arc::clone(&self.inner.poke), id);
        }
    }
}

impl Permit {
    pub fn forget(mut self) {
        self.forget = true;
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if self.forget || self.n == 0 {
            return;
        }
        {
            let mut guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
            guard.permits = guard.permits.saturating_add(self.n);
        }
        let id = {
            let guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
            match guard.waiters.front() {
                Some(&(id, need)) if !guard.closed && guard.permits >= need => Some(id),
                _ => None,
            }
        };
        if let Some(id) = id {
            wake(Arc::clone(&self.inner.poke), id);
        }
    }
}

struct Chan<T> {
    q: StdMutex<ChanQ<T>>,
    poke: Arc<Poke>,
}

struct ChanQ<T> {
    values: VecDeque<T>,
    senders: usize,
    open: bool,
    waiter: Option<u64>,
}

pub struct UnboundedSender<T> {
    chan: Arc<Chan<T>>,
}

pub struct UnboundedReceiver<T> {
    chan: Arc<Chan<T>>,
}

pub fn unbounded_channel<T: Send>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let chan = Arc::new(Chan {
        q: StdMutex::new(ChanQ { values: VecDeque::new(), senders: 1, open: true, waiter: None }),
        poke: poke_arc(),
    });
    (UnboundedSender { chan: Arc::clone(&chan) }, UnboundedReceiver { chan })
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        self.chan.q.lock().unwrap_or_else(|e| e.into_inner()).senders += 1;
        Self { chan: Arc::clone(&self.chan) }
    }
}

impl<T> Drop for UnboundedSender<T> {
    fn drop(&mut self) {
        let waiter = {
            let mut guard = self.chan.q.lock().unwrap_or_else(|e| e.into_inner());
            guard.senders = guard.senders.saturating_sub(1);
            if guard.senders == 0 { guard.waiter.take() } else { None }
        };
        if let Some(id) = waiter {
            wake(Arc::clone(&self.chan.poke), id);
        }
    }
}

impl<T> UnboundedSender<T> {
    pub fn send(&self, value: T) -> Result<(), T> {
        let waiter = {
            let mut guard = self.chan.q.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.open {
                return Err(value);
            }
            guard.values.push_back(value);
            guard.waiter.take()
        };
        if let Some(id) = waiter {
            wake(Arc::clone(&self.chan.poke), id);
        }
        Ok(())
    }
}

impl<T> UnboundedReceiver<T> {
    pub fn recv(&mut self) -> Option<T> {
        loop {
            let me = current_id();
            {
                let mut guard = self.chan.q.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(value) = guard.values.pop_front() {
                    return Some(value);
                }
                if guard.senders == 0 {
                    return None;
                }
                guard.waiter = Some(me);
            }
            park();
        }
    }
}

impl<T> Drop for UnboundedReceiver<T> {
    fn drop(&mut self) {
        let mut guard = self.chan.q.lock().unwrap_or_else(|e| e.into_inner());
        guard.open = false;
        guard.values.clear();
    }
}

pub mod oneshot {
    use super::*;

    struct Inner<T> {
        mu: StdMutex<One<T>>,
        poke: Arc<Poke>,
    }

    struct One<T> {
        value: Option<T>,
        waiter: Option<u64>,
        tx: bool,
        rx: bool,
    }

    pub struct Sender<T> {
        inner: Arc<Inner<T>>,
    }

    pub struct Receiver<T> {
        inner: Arc<Inner<T>>,
    }

    pub fn channel<T: Send>() -> (Sender<T>, Receiver<T>) {
        let inner = Arc::new(Inner {
            mu: StdMutex::new(One { value: None, waiter: None, tx: true, rx: true }),
            poke: poke_arc(),
        });
        (Sender { inner: Arc::clone(&inner) }, Receiver { inner })
    }

    impl<T> Sender<T> {
        pub fn send(self, value: T) -> Result<(), T> {
            let waiter = {
                let mut guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
                guard.tx = false;
                if !guard.rx {
                    return Err(value);
                }
                guard.value = Some(value);
                guard.waiter.take()
            };
            if let Some(id) = waiter {
                wake(Arc::clone(&self.inner.poke), id);
            }
            Ok(())
        }

        pub fn is_closed(&self) -> bool {
            !self.inner.mu.lock().unwrap_or_else(|e| e.into_inner()).rx
        }
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            let waiter = {
                let mut guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
                if !guard.tx {
                    return;
                }
                guard.tx = false;
                guard.waiter.take()
            };
            if let Some(id) = waiter {
                wake(Arc::clone(&self.inner.poke), id);
            }
        }
    }

    impl<T> Receiver<T> {
        pub fn recv(self) -> Option<T> {
            loop {
                let me = current_id();
                {
                    let mut guard = self.inner.mu.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(value) = guard.value.take() {
                        return Some(value);
                    }
                    if !guard.tx {
                        return None;
                    }
                    guard.waiter = Some(me);
                }
                park();
            }
        }
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            self.inner.mu.lock().unwrap_or_else(|e| e.into_inner()).rx = false;
        }
    }
}

pub mod watch {
    use super::*;

    struct Shared<T> {
        mu: StdMutex<WatchState<T>>,
    }

    struct WatchState<T> {
        value: T,
        version: u64,
        closed: bool,
        waiters: Vec<(u64, Arc<Poke>)>,
    }

    pub struct Sender<T> {
        shared: Arc<Shared<T>>,
    }

    pub struct Receiver<T> {
        shared: Arc<Shared<T>>,
        seen: u64,
    }

    pub fn channel<T: Send>(value: T) -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new(Shared {
            mu: StdMutex::new(WatchState { value, version: 0, closed: false, waiters: Vec::new() }),
        });
        (Sender { shared: Arc::clone(&shared) }, Receiver { shared, seen: 0 })
    }

    impl<T> Sender<T> {
        pub fn send_replace(&self, value: T) -> T {
            let (old, waiters) = {
                let mut guard = self.shared.mu.lock().unwrap_or_else(|e| e.into_inner());
                let old = mem::replace(&mut guard.value, value);
                guard.version = guard.version.wrapping_add(1);
                (old, mem::take(&mut guard.waiters))
            };
            for (id, poke) in waiters {
                wake(poke, id);
            }
            old
        }

        pub fn subscribe(&self) -> Receiver<T> {
            let seen = self.shared.mu.lock().unwrap_or_else(|e| e.into_inner()).version;
            Receiver { shared: Arc::clone(&self.shared), seen }
        }
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            let waiters = {
                let mut guard = self.shared.mu.lock().unwrap_or_else(|e| e.into_inner());
                guard.closed = true;
                mem::take(&mut guard.waiters)
            };
            for (id, poke) in waiters {
                wake(poke, id);
            }
        }
    }

    impl<T> Receiver<T> {
        pub fn borrow_and_update(&mut self) {
            self.seen = self.shared.mu.lock().unwrap_or_else(|e| e.into_inner()).version;
        }

        pub fn changed(&mut self) -> Result<(), RecvError> {
            loop {
                let me = current_id();
                {
                    let mut guard = self.shared.mu.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.version != self.seen {
                        self.seen = guard.version;
                        return Ok(());
                    }
                    if guard.closed {
                        return Err(RecvError);
                    }
                    if !guard.waiters.iter().any(|(id, _)| *id == me) {
                        guard.waiters.push((me, poke_arc()));
                    }
                }
                park();
            }
        }
    }
}

#[derive(Debug)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the sender was dropped")
    }
}

impl std::error::Error for RecvError {}


fn sys_socket(domain: i32, ty: i32, proto: i32) -> io::Result<i32> {
    match op_event(Op::Socket { domain, ty: ty | libc::SOCK_CLOEXEC, proto })? {
        Event::Socket { fd } => Ok(fd),
        _ => Err(io::Error::other("socket")),
    }
}

fn sys_bind(fd: i32, addr: SocketAddr) -> io::Result<()> {
    match op_event(Op::Bind { fd, addr })? {
        Event::Bound => Ok(()),
        _ => Err(io::Error::other("bind")),
    }
}

fn sys_listen(fd: i32, backlog: i32) -> io::Result<()> {
    match op_event(Op::Listen { fd, backlog })? {
        Event::Listening => Ok(()),
        _ => Err(io::Error::other("listen")),
    }
}

fn reuseaddr(fd: i32) {
    let one = 1i32;
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, (&one as *const i32).cast(), 4);
    }
}

fn local_of(fd: i32) -> io::Result<SocketAddr> {
    unsafe {
        let mut storage = mem::zeroed::<libc::sockaddr_storage>();
        let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        if libc::getsockname(fd, (&mut storage as *mut libc::sockaddr_storage).cast(), &mut len) < 0 {
            return Err(io::Error::last_os_error());
        }
        parse_addr(&storage).ok_or_else(|| io::Error::other("unnamed socket"))
    }
}

fn read_fd(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    match op_event(Op::Recv { fd, len: buf.len(), flags: 0 })? {
        Event::Recv { n, buf: got } => {
            let n = n.min(buf.len()).min(got.len());
            buf[..n].copy_from_slice(&got[..n]);
            Ok(n)
        }
        _ => Err(io::Error::other("recv")),
    }
}

fn write_all_fd(fd: i32, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = match op_event(Op::Send { fd, buf: buf.to_vec() })? {
            Event::Send { n } if n == 0 => return Err(io::Error::new(ErrorKind::WriteZero, "write closed")),
            Event::Send { n } => n,
            _ => return Err(io::Error::other("send")),
        };
        buf = &buf[n..];
    }
    Ok(())
}

fn shutdown_fd(fd: i32) -> io::Result<()> {
    shutdown_how(fd, libc::SHUT_WR)
}

fn shutdown_how(fd: i32, how: i32) -> io::Result<()> {
    match op_event(Op::Shutdown { fd, how })? {
        Event::Shut => Ok(()),
        _ => Err(io::Error::other("shutdown")),
    }
}

struct OwnedFd {
    fd: i32,
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}

impl OwnedFd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.fd, buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        write_all_fd(self.fd, buf)
    }
    fn shutdown(&mut self) -> io::Result<()> {
        shutdown_fd(self.fd)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        local_of(self.fd)
    }
    fn into_raw(mut self) -> i32 {
        let fd = self.fd;
        self.fd = -1;
        fd
    }
}

pub struct ReadHalf {
    fd: i32,
}
pub struct WriteHalf {
    fd: i32,
}

impl Drop for ReadHalf {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}
impl Drop for WriteHalf {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}
impl ReadHalf {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.fd, buf)
    }
    pub fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut off = 0;
        while off < buf.len() {
            let n = self.read(&mut buf[off..])?;
            if n == 0 {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "early end"));
            }
            off += n;
        }
        Ok(())
    }
}
impl WriteHalf {
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        write_all_fd(self.fd, buf)
    }
    pub fn shutdown(&mut self) -> io::Result<()> {
        shutdown_fd(self.fd)
    }
    pub fn halt(&mut self) -> io::Result<()> {
        shutdown_how(self.fd, libc::SHUT_RDWR)
    }
}

fn split_fd(fd: i32) -> (ReadHalf, WriteHalf) {
    let other = unsafe { libc::dup(fd) };
    if other < 0 {
        unsafe { libc::close(fd) };
        panic!("dup: {}", io::Error::last_os_error());
    }
    (ReadHalf { fd }, WriteHalf { fd: other })
}

pub struct TcpListener {
    fd: OwnedFd,
}
pub struct TcpStream {
    fd: OwnedFd,
}
pub struct UdpSocket {
    fd: OwnedFd,
}
pub struct UnixListener {
    fd: OwnedFd,
}
pub struct UnixStream {
    fd: OwnedFd,
}

impl TcpListener {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let domain = domain_of(addr);
        let fd = sys_socket(domain, libc::SOCK_STREAM, 0)?;
        reuseaddr(fd);
        if let Err(err) = sys_bind(fd, addr) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        if let Err(err) = sys_listen(fd, 1024) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self { fd: OwnedFd { fd } })
    }

    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        match op_event(Op::Accept { fd: self.fd.fd })? {
            Event::Accept { fd, peer } => {
                let peer = peer.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
                Ok((TcpStream { fd: OwnedFd { fd } }, peer))
            }
            _ => Err(io::Error::other("accept")),
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.fd.local_addr()
    }
}

impl TcpStream {
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let fd = sys_socket(domain_of(addr), libc::SOCK_STREAM, 0)?;
        match op_event(Op::Connect { fd, addr }) {
            Ok(Event::Connected) => Ok(Self { fd: OwnedFd { fd } }),
            Ok(_) => {
                unsafe { libc::close(fd) };
                Err(io::Error::other("connect"))
            }
            Err(err) => {
                unsafe { libc::close(fd) };
                Err(err)
            }
        }
    }

    pub fn set_nodelay(&self, on: bool) -> io::Result<()> {
        let v = i32::from(on);
        let rc = unsafe {
            libc::setsockopt(self.fd.fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, (&v as *const i32).cast(), 4)
        };
        if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        match op_event(Op::Recv { fd: self.fd.fd, len: buf.len(), flags: libc::MSG_PEEK })? {
            Event::Recv { n, buf: got } => {
                let n = n.min(buf.len()).min(got.len());
                buf[..n].copy_from_slice(&got[..n]);
                Ok(n)
            }
            _ => Err(io::Error::other("peek")),
        }
    }
    pub fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut off = 0;
        while off < buf.len() {
            let n = self.read(&mut buf[off..])?;
            if n == 0 {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "early end"));
            }
            off += n;
        }
        Ok(())
    }
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.fd.write_all(buf)
    }
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.fd.shutdown()
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.fd.local_addr()
    }
    pub fn into_split(self) -> (ReadHalf, WriteHalf) {
        split_fd(self.fd.into_raw())
    }
}

impl UdpSocket {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let fd = sys_socket(domain_of(addr), libc::SOCK_DGRAM, 0)?;
        if let Err(err) = sys_bind(fd, addr) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self { fd: OwnedFd { fd } })
    }

    pub fn set_buffers(&self, bytes: i32) {
        unsafe {
            for (force, soft) in [
                (libc::SO_RCVBUFFORCE, libc::SO_RCVBUF),
                (libc::SO_SNDBUFFORCE, libc::SO_SNDBUF),
            ] {
                libc::setsockopt(self.fd.fd, libc::SOL_SOCKET, force, (&bytes as *const i32).cast(), 4);
                libc::setsockopt(self.fd.fd, libc::SOL_SOCKET, soft, (&bytes as *const i32).cast(), 4);
            }
        }
    }

    pub fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        match op_event(Op::Connect { fd: self.fd.fd, addr })? {
            Event::Connected => Ok(()),
            _ => Err(io::Error::other("connect")),
        }
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match op_event(Op::Send { fd: self.fd.fd, buf: buf.to_vec() })? {
            Event::Send { n } => Ok(n),
            _ => Err(io::Error::other("send")),
        }
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.fd.fd, buf)
    }

    pub fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        match op_event(Op::SendTo { fd: self.fd.fd, buf: buf.to_vec(), addr })? {
            Event::SendTo { n } => Ok(n),
            _ => Err(io::Error::other("send")),
        }
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match op_event(Op::RecvFrom { fd: self.fd.fd, len: buf.len() })? {
            Event::RecvFrom { n, buf: got, addr } => {
                let n = n.min(buf.len()).min(got.len());
                buf[..n].copy_from_slice(&got[..n]);
                let addr = addr.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
                Ok((n, addr))
            }
            _ => Err(io::Error::other("recv")),
        }
    }

    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsafe {
            let mut storage = mem::zeroed::<libc::sockaddr_storage>();
            let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            let n = libc::recvfrom(
                self.fd.fd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
                (&mut storage as *mut libc::sockaddr_storage).cast(),
                &mut len,
            );
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let addr = parse_addr(&storage).unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
            Ok((n as usize, addr))
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.fd.local_addr()
    }
}

impl UnixListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let fd = sys_socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
        let name = c_path(path)?;
        match op_event(Op::BindUnix { fd, path: name }) {
            Ok(Event::Bound) => {}
            Ok(_) => {
                unsafe { libc::close(fd) };
                return Err(io::Error::other("bind"));
            }
            Err(err) => {
                unsafe { libc::close(fd) };
                return Err(err);
            }
        }
        if let Err(err) = sys_listen(fd, 1024) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self { fd: OwnedFd { fd } })
    }

    pub fn accept(&self) -> io::Result<(UnixStream, ())> {
        match op_event(Op::Accept { fd: self.fd.fd })? {
            Event::Accept { fd, .. } => Ok((UnixStream { fd: OwnedFd { fd } }, ())),
            _ => Err(io::Error::other("accept")),
        }
    }
}

impl UnixStream {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let fd = sys_socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
        let name = c_path(path)?;
        match op_event(Op::ConnectUnix { fd, path: name }) {
            Ok(Event::Connected) => Ok(Self { fd: OwnedFd { fd } }),
            Ok(_) => {
                unsafe { libc::close(fd) };
                Err(io::Error::other("connect"))
            }
            Err(err) => {
                unsafe { libc::close(fd) };
                Err(err)
            }
        }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.fd.write_all(buf)
    }
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.fd.shutdown()
    }
    pub fn into_split(self) -> (ReadHalf, WriteHalf) {
        split_fd(self.fd.into_raw())
    }
}

fn domain_of(addr: SocketAddr) -> i32 {
    match addr {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    }
}

pub fn lookup_host(host: &str) -> io::Result<Vec<SocketAddr>> {
    let host = host.to_owned();
    cpu(move || host.to_socket_addrs().map(|iter| iter.collect::<Vec<_>>()).map_err(|err| err.to_string()))
        .map_err(|err| io::Error::other(err))
}

#[derive(Clone, Copy)]
pub enum SignalKind {
    Terminate,
    Interrupt,
}

impl SignalKind {
    pub fn terminate() -> Self {
        Self::Terminate
    }
    pub fn interrupt() -> Self {
        Self::Interrupt
    }
    fn signo(self) -> i32 {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Interrupt => libc::SIGINT,
        }
    }
}

pub struct Signal {
    fd: i32,
}

static SIGNAL_FDS: [std::sync::atomic::AtomicI32; 65] = [const { std::sync::atomic::AtomicI32::new(-1) }; 65];

extern "C" fn on_signal(signo: libc::c_int) {
    let Some(slot) = SIGNAL_FDS.get(signo as usize) else { return };
    let fd = slot.load(std::sync::atomic::Ordering::Acquire);
    if fd >= 0 {
        let one: u64 = 1;
        unsafe { libc::write(fd, &one as *const u64 as *const libc::c_void, 8) };
    }
}

pub fn signal(kind: SignalKind) -> io::Result<Signal> {
    use std::sync::atomic::Ordering::{AcqRel, Acquire};
    let signo = kind.signo();
    let slot = &SIGNAL_FDS[signo as usize];
    let existing = slot.load(Acquire);
    if existing >= 0 {
        return Ok(Signal { fd: existing });
    }
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    if let Err(other) = slot.compare_exchange(-1, fd, AcqRel, Acquire) {
        unsafe { libc::close(fd) };
        return Ok(Signal { fd: other });
    }
    unsafe {
        let mut action = mem::zeroed::<libc::sigaction>();
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signo, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(Signal { fd })
}

impl Signal {
    pub fn recv(&mut self) -> Option<()> {
        match read_at(self.fd, 0, 8) {
            Ok(buf) if !buf.is_empty() => Some(()),
            _ => None,
        }
    }
}

pub fn read_at(fd: i32, off: u64, len: usize) -> io::Result<Vec<u8>> {
    match op_event(Op::Read { fd, off, len })? {
        Event::Read { n, mut buf } => {
            buf.truncate(n);
            Ok(buf)
        }
        _ => Err(io::Error::other("read")),
    }
}

pub fn write_at(fd: i32, off: u64, buf: &[u8]) -> io::Result<usize> {
    match op_event(Op::Write { fd, off, buf: buf.to_vec() })? {
        Event::Write { n } => Ok(n),
        _ => Err(io::Error::other("write")),
    }
}

pub fn open_at(dir: i32, path: &Path, flags: i32, mode: u32) -> io::Result<i32> {
    let path = c_path(path)?;
    match op_event(Op::OpenAt { dir, path, flags, mode })? {
        Event::Open { fd } => Ok(fd),
        _ => Err(io::Error::other("open")),
    }
}

pub fn stat_at(dir: i32, path: &Path) -> io::Result<Box<libc::statx>> {
    let path = c_path(path)?;
    match op_event(Op::Statx { dir, path })? {
        Event::Stat { buf } => Ok(buf),
        _ => Err(io::Error::other("stat")),
    }
}

pub fn rename_at(old_dir: i32, old: &Path, new_dir: i32, new: &Path) -> io::Result<()> {
    let old = c_path(old)?;
    let new = c_path(new)?;
    match op_event(Op::RenameAt { old_dir, old, new_dir, new })? {
        Event::Renamed => Ok(()),
        _ => Err(io::Error::other("rename")),
    }
}

pub fn fsync_fd(fd: i32) -> io::Result<()> {
    match op_event(Op::Fsync { fd })? {
        Event::Synced => Ok(()),
        _ => Err(io::Error::other("fsync")),
    }
}

pub fn close_fd(fd: i32) -> io::Result<()> {
    match op_event(Op::Close { fd })? {
        Event::Closed => Ok(()),
        _ => Err(io::Error::other("close")),
    }
}

pub fn wait_pid(pid: i32) -> io::Result<i32> {
    match op_event(Op::WaitPid { pid })? {
        Event::Exited { status } => Ok(status),
        _ => Err(io::Error::other("wait")),
    }
}

pub fn try_read(fd: i32, len: usize) -> io::Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; len];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, len) };
        if n >= 0 {
            buf.truncate(n as usize);
            return Ok(Some(buf));
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            ErrorKind::Interrupted => continue,
            ErrorKind::WouldBlock => return Ok(None),
            _ => return Err(err),
        }
    }
}

pub fn read_ready(fd: i32, len: usize) -> io::Result<Vec<u8>> {
    loop {
        if let Some(buf) = try_read(fd, len)? {
            return Ok(buf);
        }
        match op_event(Op::Readable { fd })? {
            Event::Readable => {}
            _ => return Err(io::Error::other("poll")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
