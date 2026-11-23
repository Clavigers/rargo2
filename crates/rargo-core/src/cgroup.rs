//! Per-action cgroups: what an action really used.
//!
//! `rusage` of the sandbox's first process knows the largest single process
//! and the CPU time of processes that were waited for. A build script that
//! runs a compiler per core, or a test that leaves children behind, is
//! under-reported that way, and the scheduler's memory admission works from
//! these figures. When rargod runs in a cgroup that systemd delegated to it
//! (`Delegate=yes`, as `dev/rargod.sh` asks for), every action gets its own
//! cgroup: CPU time is read from it exactly, and memory is sampled as the
//! anonymous memory of all its processes together.
//!
//! Only the memory controller is enabled for the children. Enabling the CPU
//! controller would make the kernel share CPU equally between actions
//! regardless of how many threads each has, which is not wanted.

use std::path::{Path, PathBuf};

/// Prepares the delegated cgroup rargod runs in and returns the directory
/// under which action cgroups are created, or `None` when the cgroup is not
/// ours to arrange (not delegated, not cgroup v2, not root).
///
/// Must run before any other process is forked: everything in the cgroup is
/// moved into a `daemon` child, because a cgroup with children that have
/// controllers enabled may not hold processes itself.
pub fn setup() -> Option<PathBuf> {
    // SAFETY: plain syscall.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let me = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = me.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    if rel == "/" || rel.is_empty() {
        return None;
    }
    let dir = PathBuf::from(format!("/sys/fs/cgroup{rel}"));
    if !delegated(&dir) {
        return None;
    }
    let leaf = dir.join("daemon");
    std::fs::create_dir_all(&leaf).ok()?;
    let procs = std::fs::read_to_string(dir.join("cgroup.procs")).ok()?;
    for pid in procs.split_whitespace() {
        std::fs::write(leaf.join("cgroup.procs"), pid).ok()?;
    }
    std::fs::write(dir.join("cgroup.subtree_control"), "+memory").ok()?;
    Some(dir)
}

/// systemd marks cgroups it has handed over with an extended attribute.
fn delegated(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    let mut buf = [0u8; 8];
    for name in [c"trusted.delegate", c"user.delegate"] {
        // SAFETY: valid NUL-terminated strings and a buffer of the given size.
        let n = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 1 && buf[0] == b'1' {
            return true;
        }
    }
    false
}

/// The cgroup of one running action. Removed when dropped.
pub struct ActionGroup {
    pub dir: PathBuf,
}

impl ActionGroup {
    pub fn create(root: &Path, seq: u64) -> Option<ActionGroup> {
        let dir = root.join(format!("action-{seq}"));
        std::fs::create_dir(&dir).ok()?;
        Some(ActionGroup { dir })
    }

    fn stat(&self, file: &str, keys: &[&str]) -> u64 {
        let Ok(text) = std::fs::read_to_string(self.dir.join(file)) else { return 0 };
        text.lines()
            .filter_map(|l| l.split_once(' '))
            .filter(|(k, _)| keys.contains(k))
            .filter_map(|(_, v)| v.trim().parse::<u64>().ok())
            .sum()
    }

    /// Memory that cannot simply be dropped, of all processes in the group,
    /// now, in KiB.
    pub fn anon_kb(&self) -> u64 {
        self.stat("memory.stat", &["anon", "shmem", "kernel_stack", "pagetables"]) / 1024
    }

    /// `(user, system)` CPU time in microseconds of everything that ran in
    /// the group.
    pub fn cpu_us(&self) -> (u64, u64) {
        (self.stat("cpu.stat", &["user_usec"]), self.stat("cpu.stat", &["system_usec"]))
    }
}

impl Drop for ActionGroup {
    fn drop(&mut self) {
        // The processes are gone (the sandbox's init took them with it),
        // but the kernel may need a moment to notice.
        for _ in 0..50 {
            match std::fs::remove_dir(&self.dir) {
                Ok(()) => return,
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(2))
                }
                Err(_) => return,
            }
        }
    }
}
