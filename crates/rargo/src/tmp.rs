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

/// Calls `each` with the NUL-terminated path of every temporary in use. It only reads the table, so a signal handler may call it.
fn each_live(mut each: impl FnMut(*const libc::c_char)) {
    for s in TABLE.iter() {
        if s.live.load(Ordering::SeqCst) {
            each(s.buf.get() as *const libc::c_char);
        }
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    each_live(|path| unsafe { crate::signals::unlink(path) });
    crate::signals::die_of(sig);
}

pub fn install_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if !crate::signals::ignored(sig) {
            crate::signals::handle(sig, on_signal);
        }
    }
}

pub(crate) fn alive(pid: u32) -> bool {
    rargo_net::io::process::alive(pid)
}

pub(crate) fn owner(name: &str, prefix: &str) -> Option<u32> {
    let rest = name.strip_prefix(prefix)?;
    let (pid, _) = rest.split_once('-')?;
    assert!(!pid.contains('-'));
    pid.parse().ok()
}

pub fn sweep(dir: &Path, prefix: &str) -> usize {
    let Ok(rd) = rargo_net::io::file::read_dir(dir) else { return 0 };
    let mut gone: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|n| owner(n, prefix)) else { continue };
        if pid != rargo_net::io::process::id() && !alive(pid) {
            gone.push(e.path());
        }
    }
    let total = gone.len();
    let mut n = 0;
    for p in gone {
        let removed = match rargo_net::io::file::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => rargo_net::io::file::remove_dir_all(&p).is_ok(),
            Ok(_) => rargo_net::io::file::remove_file(&p).is_ok(),
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
    use rargo_net::io::{file, process, sim};

    #[test]
    fn sweep_removes_only_what_dead_processes_left() {
        sim::test(|| {
            let d = sim::tempdir();
            let (me, dead) = (process::id(), process::id() + 1);
            assert!(alive(me) && alive(1) && !alive(dead));
            file::write(d.join(format!(".rargo-tmp-{dead}-3-libfoo.rlib")), b"x").unwrap();
            file::write(d.join(format!(".rargo-tmp-{dead}-app.d")), b"x").unwrap();
            file::write(d.join(format!(".rargo-tmp-{me}-1-mine")), b"x").unwrap();
            file::write(d.join(".rargo-tmp-1-9-init"), b"x").unwrap();
            file::write(d.join(".rargo-tmp-notapid-x"), b"x").unwrap();
            file::write(d.join("libfoo.rlib"), b"x").unwrap();
            assert_eq!(sweep(&d, BESIDE), 2);
            let mut left: Vec<String> = file::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            left.sort();
            assert_eq!(
                left,
                vec![
                    ".rargo-tmp-1-9-init".to_string(),
                    format!(".rargo-tmp-{me}-1-mine"),
                    ".rargo-tmp-notapid-x".to_string(),
                    "libfoo.rlib".to_string()
                ]
            );
        });
    }

    #[test]
    fn a_signal_would_remove_the_temporaries_in_use_and_no_others() {
        let (kept, lost) = (Path::new("/a-signal/kept"), Path::new("/a-signal/lost"));
        let live = || {
            let mut paths = Vec::new();
            each_live(|path| paths.push(unsafe { std::ffi::CStr::from_ptr(path) }.to_bytes().to_vec()));
            paths
        };
        drop(watch(kept));
        let guard = watch(lost);
        assert!(guard.slot.is_some(), "the table has a free slot");
        assert!(live().contains(&lost.as_os_str().as_bytes().to_vec()));
        assert!(!live().contains(&kept.as_os_str().as_bytes().to_vec()));
        drop(guard);
        assert!(!live().contains(&lost.as_os_str().as_bytes().to_vec()));
    }
}
