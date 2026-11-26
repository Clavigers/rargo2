//! Signal dispositions, and the two things a handler does that need the kernel.

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
