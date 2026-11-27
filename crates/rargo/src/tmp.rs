use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub struct Temp {
    slot: Option<usize>,
}

impl Drop for Temp {
    fn drop(&mut self) {
        if let Some(i) = self.slot {
            crate::signals::release(i);
        }
    }
}

pub fn watch(path: &Path) -> Temp {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= crate::signals::PATH_MAX || bytes.contains(&0) {
        return Temp { slot: None };
    }
    Temp { slot: crate::signals::hold(bytes) }
}

/// Calls `each` with the NUL-terminated path of every temporary in use. A signal handler may call it.
fn each_live(each: impl FnMut(*const libc::c_char)) {
    crate::signals::each_held(each);
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

/// Numbers that tell the temporary files of this process apart. No two `Names` cut from one give the same number, so each can go to a thread of its own.
#[derive(Debug)]
pub struct Names {
    next: u64,
    end: u64,
}

const PART: u64 = 1 << 32;

impl Names {
    /// Every number. A process uses one of these at a time, and cuts from it what its threads use.
    pub fn all() -> Names {
        Names { next: 1, end: u64::MAX }
    }

    /// A number this gives once.
    pub fn next(&mut self) -> u64 {
        assert!(self.next < self.end, "the numbers for temporary files are used up");
        let n = self.next;
        self.next += 1;
        n
    }

    /// Cuts off numbers that this then no longer gives.
    pub fn part(&mut self) -> Names {
        assert!(self.end - self.next > PART, "the numbers for temporary files are used up");
        self.end -= PART;
        Names { next: self.end, end: self.end + PART }
    }
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
    fn names_cut_from_one_never_give_the_same_number() {
        let mut all = Names::all();
        let (mut a, mut b) = (all.part(), all.part());
        let mut seen = std::collections::BTreeSet::new();
        for names in [&mut all, &mut a, &mut b] {
            for _ in 0..100 {
                assert!(seen.insert(names.next()));
            }
        }
        assert!(a.end <= b.next || b.end <= a.next);
        assert!(all.end <= a.next.min(b.next));
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
