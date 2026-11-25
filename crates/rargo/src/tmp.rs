use std::cell::UnsafeCell;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const SLOTS: usize = 64;
const PATH_MAX: usize = 4096;

const _: () = assert!(SLOTS > 0);
const _: () = assert!(PATH_MAX as u32 == libc::PATH_MAX as u32);

struct Slot {
    live: AtomicBool,
    taken: AtomicBool,
    buf: UnsafeCell<[u8; PATH_MAX]>,
}

unsafe impl Sync for Slot {}

static TABLE: [Slot; SLOTS] =
    [const { Slot { live: AtomicBool::new(false), taken: AtomicBool::new(false), buf: UnsafeCell::new([0; PATH_MAX]) } }; SLOTS];

pub struct Temp {
    slot: Option<usize>,
}

impl Drop for Temp {
    fn drop(&mut self) {
        if let Some(i) = self.slot {
            assert!(TABLE[i].taken.load(Ordering::SeqCst));
            assert!(TABLE[i].live.load(Ordering::SeqCst));
            TABLE[i].live.store(false, Ordering::SeqCst);
            TABLE[i].taken.store(false, Ordering::SeqCst);
        }
    }
}

pub fn watch(path: &Path) -> Temp {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= PATH_MAX || bytes.contains(&0) {
        return Temp { slot: None };
    }
    for (i, s) in TABLE.iter().enumerate() {
        if s.taken.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            continue;
        }
        assert!(!s.live.load(Ordering::SeqCst), "a free slot is never live");
        unsafe {
            let buf = &mut *s.buf.get();
            buf[..bytes.len()].copy_from_slice(bytes);
            buf[bytes.len()] = 0;
        }
        s.live.store(true, Ordering::SeqCst);
        return Temp { slot: Some(i) };
    }
    Temp { slot: None }
}

extern "C" fn on_signal(sig: libc::c_int) {
    for s in TABLE.iter() {
        if s.live.load(Ordering::SeqCst) {
            unsafe { libc::unlink(s.buf.get() as *const libc::c_char) };
        }
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

pub fn install_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &mut old);
            if old.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

pub(crate) fn alive(pid: u32) -> bool {
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub(crate) fn owner(name: &str, prefix: &str) -> Option<u32> {
    let rest = name.strip_prefix(prefix)?;
    let (pid, _) = rest.split_once('-')?;
    assert!(!pid.contains('-'));
    pid.parse().ok()
}

pub fn sweep(dir: &Path, prefix: &str) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut gone: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|n| owner(n, prefix)) else { continue };
        if pid != std::process::id() && !alive(pid) {
            gone.push(e.path());
        }
    }
    let total = gone.len();
    let mut n = 0;
    for p in gone {
        let removed = match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p).is_ok(),
            Ok(_) => std::fs::remove_file(&p).is_ok(),
            Err(_) => false,
        };
        n += removed as usize;
    }
    assert!(n <= total);
    n
}

pub const BESIDE: &str = ".rargo-tmp-";

pub fn seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_only_what_dead_processes_left() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(d.join(format!(".rargo-tmp-{dead}-3-libfoo.rlib")), b"x").unwrap();
        std::fs::write(d.join(format!(".rargo-tmp-{dead}-app.d")), b"x").unwrap();
        std::fs::write(d.join(format!(".rargo-tmp-{}-1-mine", std::process::id())), b"x").unwrap();
        std::fs::write(d.join(".rargo-tmp-1-9-init"), b"x").unwrap();
        std::fs::write(d.join(".rargo-tmp-notapid-x"), b"x").unwrap();
        std::fs::write(d.join("libfoo.rlib"), b"x").unwrap();
        assert_eq!(sweep(d, BESIDE), 2);
        let mut left: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                ".rargo-tmp-1-9-init".to_string(),
                format!(".rargo-tmp-{}-1-mine", std::process::id()),
                ".rargo-tmp-notapid-x".to_string(),
                "libfoo.rlib".to_string()
            ]
        );
    }

    #[test]
    fn a_signal_removes_temporaries_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let (kept, lost) = (tmp.path().join("kept"), tmp.path().join("lost"));
        std::fs::write(&kept, b"x").unwrap();
        std::fs::write(&lost, b"x").unwrap();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            for s in TABLE.iter() {
                s.live.store(false, Ordering::SeqCst);
                s.taken.store(false, Ordering::SeqCst);
            }
            install_handlers();
            let g = watch(&kept);
            drop(g);
            let _g = watch(&lost);
            unsafe { libc::raise(libc::SIGTERM) };
            std::process::exit(0);
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGTERM, "status {status:#x}");
        assert!(kept.exists());
        assert!(!lost.exists());
    }
}
