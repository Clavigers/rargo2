use std::path::{Path, PathBuf};

pub fn setup() -> Option<PathBuf> {
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let me = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let dir = delegated_root(&me)?;
    if !delegated(&dir) {
        return None;
    }
    let procs = std::fs::read_to_string(membership_file(&dir)).ok()?;
    for op in delegation_ops(&dir, &procs) {
        match op {
            DelegationOp::Create(path) => std::fs::create_dir_all(path).ok()?,
            DelegationOp::Write(path, value) => std::fs::write(path, value).ok()?,
        }
    }
    Some(dir)
}

enum DelegationOp<'a> {
    Create(PathBuf),
    Write(PathBuf, &'a str),
}

fn delegation_ops<'a>(dir: &Path, procs: &'a str) -> Vec<DelegationOp<'a>> {
    let leaf = dir.join("daemon");
    let mut ops = vec![DelegationOp::Create(leaf.clone())];
    let membership = membership_file(&leaf);
    ops.extend(procs.split_whitespace().map(|pid| DelegationOp::Write(membership.clone(), pid)));
    ops.push(DelegationOp::Write(dir.join("cgroup.subtree_control"), "+memory"));
    ops
}

fn delegated_root(membership: &str) -> Option<PathBuf> {
    let rel = membership.lines().find_map(|line| line.strip_prefix("0::"))?.trim();
    let relative = Path::new(rel).strip_prefix("/").ok()?;
    if relative.as_os_str().is_empty() || !relative.components().all(|part| matches!(part, std::path::Component::Normal(_))) {
        return None;
    }
    Some(Path::new("/sys/fs/cgroup").join(relative))
}

pub(crate) fn membership_file(group: &Path) -> PathBuf {
    group.join("cgroup.procs")
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegation_enables_memory_after_moving_processes() {
        let root = delegated_root("0::/service/build.scope\n").unwrap();
        let plan = delegation_ops(&root, "17\n23\n");
        assert!(matches!(&plan[..], [DelegationOp::Create(leaf), DelegationOp::Write(a, "17"), DelegationOp::Write(b, "23"), DelegationOp::Write(control, "+memory")]
            if leaf == &root.join("daemon") && a == &membership_file(leaf) && a == b && control == &root.join("cgroup.subtree_control")));
        assert!(delegated_root("0::/\n").is_none());
    }
}
