use std::path::{Path, PathBuf};

pub fn setup() -> Option<PathBuf> {
    if crate::io::process::euid() != 0 {
        return None;
    }
    let me = crate::io::file::read_to_string("/proc/self/cgroup").ok()?;
    let dir = delegated_root(&me)?;
    if !delegated(&dir) {
        return None;
    }
    let procs = crate::io::file::read_to_string(membership_file(&dir)).ok()?;
    for op in delegation_ops(&dir, &procs) {
        match op {
            crate::sandbox::Op::CreateDir { path, .. } => crate::io::file::create_dir_all(path).ok()?,
            crate::sandbox::Op::Write { path, data } => crate::io::file::write(path, data).ok()?,
            _ => unreachable!("delegation only creates directories and writes files"),
        }
    }
    Some(dir)
}

pub(crate) fn delegation_ops(dir: &Path, procs: &str) -> Vec<crate::sandbox::Op> {
    let leaf = dir.join("daemon");
    let mut ops = vec![crate::sandbox::Op::CreateDir { path: leaf.clone(), parents: true }];
    let membership = membership_file(&leaf);
    ops.extend(procs.split_whitespace().map(|pid| crate::sandbox::Op::Write { path: membership.clone(), data: pid.as_bytes().to_vec() }));
    ops.push(crate::sandbox::Op::Write { path: dir.join("cgroup.subtree_control"), data: b"+memory".to_vec() });
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
    let mut buf = [0u8; 8];
    [c"trusted.delegate", c"user.delegate"].into_iter().any(|name| crate::io::sys::xattr(dir, name, &mut buf).is_ok_and(|n| n >= 1 && buf[0] == b'1'))
}

pub struct ActionGroup {
    pub dir: PathBuf,
}

impl ActionGroup {
    pub fn open(dir: PathBuf) -> ActionGroup {
        ActionGroup { dir }
    }

    fn stat(&self, file: &str, keys: &[&str]) -> u64 {
        let Ok(text) = crate::io::file::read_to_string(self.dir.join(file)) else { return 0 };
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
        if crate::io::file::write(self.dir.join("cgroup.freeze"), "1").is_err() {
            return;
        }
        for _ in 0..50 {
            if self.stat("cgroup.events", &["frozen"]) == 1 {
                return;
            }
            crate::io::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub fn helper_tokens(&self) -> usize {
        let Ok(procs) = crate::io::file::read_to_string(self.dir.join("cgroup.procs")) else { return 0 };
        let mut held = 0;
        for pid in procs.split_whitespace() {
            let Ok(tasks) = crate::io::file::read_dir(format!("/proc/{pid}/task")) else { continue };
            let workers = tasks
                .flatten()
                .filter_map(|t| crate::io::file::read_to_string(t.path().join("comm")).ok())
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
            match crate::io::file::remove_dir(&self.dir) {
                Ok(()) => return,
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    crate::io::sleep(std::time::Duration::from_millis(2))
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
        use expect_test::expect;
        let root = delegated_root("0::/service/build.scope\n").unwrap();
        let text = delegation_ops(&root, "17\n23\n").iter().map(|op| op.to_string()).collect::<Vec<_>>().join("\n");
        expect![[r#"
            ensure-dir /sys/fs/cgroup/service/build.scope/daemon
            write /sys/fs/cgroup/service/build.scope/daemon/cgroup.procs 17
            write /sys/fs/cgroup/service/build.scope/daemon/cgroup.procs 23
            write /sys/fs/cgroup/service/build.scope/cgroup.subtree_control +memory"#]]
        .assert_eq(&text);
        assert!(delegated_root("0::/\n").is_none());
    }
}
