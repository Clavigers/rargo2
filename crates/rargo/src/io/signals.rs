//! Signal dispositions, the two things a handler does that need the kernel, and the table of paths a handler reads.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering};

const SLOTS: usize = 64;

/// One more than the longest path the table holds.
pub(crate) const PATH_MAX: usize = 4096;

const _: () = assert!(SLOTS > 0);
const _: () = assert!(PATH_MAX as u32 == libc::PATH_MAX as u32);

/// A path that any thread may claim and release and that a signal handler, which interrupts any of them, reads.
struct Slot {
    live: AtomicBool,
    taken: AtomicBool,
    buf: UnsafeCell<[u8; PATH_MAX]>,
}

unsafe impl Sync for Slot {}

static TABLE: [Slot; SLOTS] =
    [const { Slot { live: AtomicBool::new(false), taken: AtomicBool::new(false), buf: UnsafeCell::new([0; PATH_MAX]) } }; SLOTS];

/// Keeps `path` where a signal handler finds it and returns its slot, or `None` when every slot is in use.
pub(crate) fn hold(path: &[u8]) -> Option<usize> {
    assert!(path.len() < PATH_MAX && !path.contains(&0), "a held path fits a slot with its terminator");
    for (i, s) in TABLE.iter().enumerate() {
        if s.taken.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            continue;
        }
        assert!(!s.live.load(Ordering::SeqCst), "a free slot is never live");
        unsafe {
            let buf = &mut *s.buf.get();
            buf[..path.len()].copy_from_slice(path);
            buf[path.len()] = 0;
        }
        s.live.store(true, Ordering::SeqCst);
        return Some(i);
    }
    None
}

/// Frees a slot that `hold` returned.
pub(crate) fn release(slot: usize) {
    assert!(TABLE[slot].taken.load(Ordering::SeqCst));
    assert!(TABLE[slot].live.load(Ordering::SeqCst));
    TABLE[slot].live.store(false, Ordering::SeqCst);
    TABLE[slot].taken.store(false, Ordering::SeqCst);
}

/// Calls `each` with the NUL-terminated path in every slot in use. It only reads the table, so a signal handler may call it.
pub(crate) fn each_held(mut each: impl FnMut(*const libc::c_char)) {
    for s in TABLE.iter() {
        if s.live.load(Ordering::SeqCst) {
            each(s.buf.get() as *const libc::c_char);
        }
    }
}

/// Whether the process ignores `sig`.
pub(crate) fn ignored(sig: libc::c_int) -> bool {
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(sig, std::ptr::null(), &mut old);
        old.sa_sigaction == libc::SIG_IGN
    }
}

/// Makes `handler` run when `sig` arrives.
pub(crate) fn handle(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// Removes the file a NUL-terminated path names. A signal handler may call this.
///
/// # Safety
/// `path` points to a NUL-terminated string that stays readable for the call.
pub(crate) unsafe fn unlink(path: *const libc::c_char) {
    unsafe { libc::unlink(path) };
}

/// Ends the process the way `sig` ends one that does not handle it. A signal handler may call this.
pub(crate) fn die_of(sig: libc::c_int) {
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}
