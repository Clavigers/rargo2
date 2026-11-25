
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, ErrorKind};
use std::mem;
use std::sync::{Arc, Mutex as StdMutex};


use super::fiber::*;


pub struct Notify {
    inner: StdMutex<NotifyState>,
    poke: Arc<Poke>,
}

pub(super) struct NotifyState {
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

    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
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

pub(super) struct SemState {
    permits: u32,
    closed: bool,
    waiters: VecDeque<(u64, u32)>,
}

pub(super) struct SemInner {
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

pub(super) struct Chan<T> {
    q: StdMutex<ChanQ<T>>,
    poke: Arc<Poke>,
}

pub(super) struct ChanQ<T> {
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


