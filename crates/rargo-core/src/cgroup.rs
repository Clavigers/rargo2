use std::path::{Path, PathBuf};

pub fn setup() -> Option<PathBuf> {
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

fn delegated(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    let mut buf = [0u8; 8];
    for name in [c"trusted.delegate", c"user.delegate"] {
        let n = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 1 && buf[0] == b'1' {
            return true;
        }
    }
    false
}

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

    pub fn anon_kb(&self) -> u64 {
        self.stat("memory.stat", &["anon", "shmem", "kernel_stack", "pagetables"]) / 1024
    }

    pub fn cpu_us(&self) -> (u64, u64) {
        (self.stat("cpu.stat", &["user_usec"]), self.stat("cpu.stat", &["system_usec"]))
    }

    pub fn freeze(&self) {
        if std::fs::write(self.dir.join("cgroup.freeze"), "1").is_err() {
            return;
        }
        for _ in 0..50 {
            if self.stat("cgroup.events", &["frozen"]) == 1 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub fn helper_tokens(&self) -> usize {
        let Ok(procs) = std::fs::read_to_string(self.dir.join("cgroup.procs")) else { return 0 };
        let mut held = 0;
        for pid in procs.split_whitespace() {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
            let workers = tasks
                .flatten()
                .filter_map(|t| std::fs::read_to_string(t.path().join("comm")).ok())
                .filter(|comm| is_codegen_thread(comm))
                .count();
            held += workers.saturating_sub(1);
        }
        held
    }
}

fn is_codegen_thread(comm: &str) -> bool {
    ["opt ", "lto ", "copy "].iter().any(|p| comm.starts_with(p))
}

impl Drop for ActionGroup {
    fn drop(&mut self) {
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
