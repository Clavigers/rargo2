
use std::io::{self};
use std::mem;


use super::fs::*;


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

pub(super) static SIGNAL_FDS: [std::sync::atomic::AtomicI32; 65] = [const { std::sync::atomic::AtomicI32::new(-1) }; 65];

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

