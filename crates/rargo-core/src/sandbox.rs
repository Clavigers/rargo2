use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NetMode {
    Loopback,
    Host,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bind {
    pub host: PathBuf,
    pub inside: Vec<u8>,
    pub read_only: bool,
}

pub const EXTRA_FD_BASE: i32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpawnReq {
    pub root: PathBuf,
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub cwd: Vec<u8>,
    pub uid: u32,
    pub gid: u32,
    pub net: NetMode,
    pub binds: Vec<Bind>,
    pub cgroup: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ExitInfo {
    pub status: i32,
    pub setup_failed: bool,
    pub user_us: u64,
    pub sys_us: u64,
    pub max_rss_kb: u64,
    pub wall_us: u64,
}

impl ExitInfo {
    pub fn code(&self) -> Option<i32> {
        libc::WIFEXITED(self.status).then(|| libc::WEXITSTATUS(self.status))
    }
    pub fn signal(&self) -> Option<i32> {
        libc::WIFSIGNALED(self.status).then(|| libc::WTERMSIG(self.status))
    }
    pub fn success(&self) -> bool {
        self.code() == Some(0)
    }
}

#[derive(Serialize, Deserialize)]
enum ToZygote {
    Spawn { id: u64, req: SpawnReq, has_stdin: bool, extra: usize },
    Kill { id: u64 },
}

#[derive(Serialize, Deserialize)]
enum FromZygote {
    Exited { id: u64, info: ExitInfo },
}

pub struct Zygote {
    sock: Mutex<UnixStream>,
    pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<ExitInfo>>>>,
    next: AtomicU64,
    child: Mutex<Option<std::process::Child>>,
}

pub struct Running {
    pub id: u64,
    pub done: tokio::sync::oneshot::Receiver<ExitInfo>,
}

fn send_msg(sock: &UnixStream, payload: &[u8], fds: &[RawFd]) -> io::Result<()> {
    assert!(payload.len() <= u32::MAX as usize);
    assert!(fds.len() <= 16, "descriptors per message fit the control buffer");
    let len = (payload.len() as u32).to_le_bytes();
    let mut iov = libc::iovec { iov_base: len.as_ptr() as *mut _, iov_len: 4 };
    let mut cbuf = [0u8; 128];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let space = unsafe { libc::CMSG_SPACE((fds.len() * 4) as u32) } as usize;
        assert!(space <= cbuf.len());
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = space;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as usize;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg) as *mut RawFd, fds.len());
        }
    }
    loop {
        let r = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        if r != 4 {
            return Err(io::Error::other("short header write"));
        }
        break;
    }
    let mut s = sock;
    s.write_all(payload)
}

fn recv_msg(sock: &UnixStream) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    let mut fds = Vec::new();
    while got < 4 {
        let mut iov = libc::iovec { iov_base: len[got..].as_mut_ptr() as *mut _, iov_len: 4 - got };
        let mut cbuf = [0u8; 128];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cbuf.len();
        let r = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            return Ok(None);
        }
        got += r as usize;
        assert!(got <= 4);
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let n = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / 4;
                    let data = libc::CMSG_DATA(c) as *const RawFd;
                    for i in 0..n {
                        fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
    }
    let n = u32::from_le_bytes(len) as usize;
    let mut payload = vec![0u8; n];
    let mut s = sock;
    s.read_exact(&mut payload)?;
    Ok(Some((payload, fds)))
}

impl Zygote {
    pub fn start(exe: &Path) -> Result<Arc<Zygote>> {
        let (ours, theirs) = UnixStream::pair()?;
        let theirs_fd = theirs.as_raw_fd();
        unsafe { libc::fcntl(theirs_fd, libc::F_SETFD, 0) };
        let child = std::process::Command::new(exe)
            .arg("--internal-zygote")
            .arg(theirs_fd.to_string())
            .stdin(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("starting zygote {}", exe.display()))?;
        drop(theirs);
        let pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<ExitInfo>>>> = Default::default();
        let reader = ours.try_clone()?;
        let p2 = pending.clone();
        std::thread::Builder::new().name("zygote-reader".into()).spawn(move || {
            while let Ok(Some((payload, _))) = recv_msg(&reader) {
                let Ok(FromZygote::Exited { id, info }) = bincode::deserialize(&payload) else { continue };
                if let Some(tx) = p2.lock().remove(&id) {
                    let _ = tx.send(info);
                }
            }
            p2.lock().clear();
        })?;
        Ok(Arc::new(Zygote {
            sock: Mutex::new(ours),
            pending,
            next: AtomicU64::new(1),
            child: Mutex::new(Some(child)),
        }))
    }

    pub fn spawn(
        &self,
        req: SpawnReq,
        stdin: Option<OwnedFd>,
        stdout: OwnedFd,
        stderr: OwnedFd,
        extra: &[RawFd],
    ) -> Result<Running> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        assert!(self.pending.lock().insert(id, tx).is_none(), "spawn ids are unique");
        let msg = bincode::serialize(&ToZygote::Spawn {
            id,
            req,
            has_stdin: stdin.is_some(),
            extra: extra.len(),
        })?;
        let mut fds = vec![stdout.as_raw_fd(), stderr.as_raw_fd()];
        if let Some(s) = &stdin {
            fds.push(s.as_raw_fd());
        }
        fds.extend_from_slice(extra);
        assert_eq!(fds.len(), 2 + stdin.is_some() as usize + extra.len());
        let r = send_msg(&self.sock.lock(), &msg, &fds);
        if let Err(e) = r {
            self.pending.lock().remove(&id);
            bail!("zygote is not reachable: {e}");
        }
        Ok(Running { id, done: rx })
    }

    pub fn kill(&self, id: u64) {
        if let Ok(msg) = bincode::serialize(&ToZygote::Kill { id }) {
            let _ = send_msg(&self.sock.lock(), &msg, &[]);
        }
    }
}

impl Drop for Zygote {
    fn drop(&mut self) {
        let _ = self.sock.lock().shutdown(std::net::Shutdown::Both);
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.wait();
        }
    }
}


struct Child {
    id: u64,
    status_pipe: OwnedFd,
    started: std::time::Instant,
}

pub fn zygote_main(fd: RawFd) -> ! {
    let sock = unsafe { UnixStream::from_raw_fd(fd) };
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        libc::prctl(libc::PR_SET_NAME, c"rargo-zygote".as_ptr());
    }
    let sigfd = unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGCHLD);
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK)
    };
    let mut children: HashMap<i32, Child> = HashMap::new();
    let mut by_id: HashMap<u64, i32> = HashMap::new();
    loop {
        let mut fds = [
            libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: sigfd, events: libc::POLLIN, revents: 0 },
        ];
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            continue;
        }
        if fds[1].revents != 0 {
            let mut buf = [0u8; 1024];
            while unsafe { libc::read(sigfd, buf.as_mut_ptr() as *mut _, buf.len()) } > 0 {}
            reap(&sock, &mut children, &mut by_id);
        }
        if fds[0].revents != 0 {
            match recv_msg(&sock) {
                Ok(Some((payload, fds))) => match bincode::deserialize::<ToZygote>(&payload) {
                    Ok(ToZygote::Spawn { id, req, has_stdin, extra }) => {
                        let mut it = fds.into_iter();
                        let (Some(out), Some(err)) = (it.next(), it.next()) else { continue };
                        let stdin = if has_stdin { it.next() } else { None };
                        let extras: Vec<OwnedFd> = it.take(extra).collect();
                        match spawn_init(&req, stdin, out, err, extras) {
                            Ok((pid, status_pipe)) => {
                                assert!(children.insert(pid, Child { id, status_pipe, started: std::time::Instant::now() }).is_none());
                                assert!(by_id.insert(id, pid).is_none(), "spawn ids are unique");
                                assert_eq!(children.len(), by_id.len());
                            }
                            Err(_) => {
                                let info = ExitInfo { status: 127 << 8, setup_failed: true, ..Default::default() };
                                let _ = send_msg(&sock, &bincode::serialize(&FromZygote::Exited { id, info }).unwrap(), &[]);
                            }
                        }
                    }
                    Ok(ToZygote::Kill { id }) => {
                        if let Some(pid) = by_id.get(&id) {
                            unsafe { libc::kill(*pid, libc::SIGKILL) };
                        }
                    }
                    Err(_) => {}
                },
                Ok(None) | Err(_) => {
                    for pid in children.keys() {
                        unsafe { libc::kill(*pid, libc::SIGKILL) };
                    }
                    std::process::exit(0);
                }
            }
        }
    }
}

fn reap(sock: &UnixStream, children: &mut HashMap<i32, Child>, by_id: &mut HashMap<u64, i32>) {
    loop {
        let mut status = 0i32;
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let pid = unsafe { libc::wait4(-1, &mut status, libc::WNOHANG, &mut ru) };
        if pid <= 0 {
            return;
        }
        let Some(child) = children.remove(&pid) else { continue };
        assert_eq!(by_id.remove(&child.id), Some(pid));
        assert_eq!(children.len(), by_id.len());
        let mut buf = [0u8; 5];
        let mut f = std::fs::File::from(child.status_pipe);
        let mut info = ExitInfo {
            status,
            setup_failed: false,
            user_us: ru.ru_utime.tv_sec as u64 * 1_000_000 + ru.ru_utime.tv_usec as u64,
            sys_us: ru.ru_stime.tv_sec as u64 * 1_000_000 + ru.ru_stime.tv_usec as u64,
            max_rss_kb: ru.ru_maxrss as u64,
            wall_us: child.started.elapsed().as_micros() as u64,
        };
        match f.read_exact(&mut buf) {
            Ok(()) => {
                info.status = i32::from_le_bytes(buf[..4].try_into().unwrap());
                info.setup_failed = buf[4] != 0;
            }
            Err(_) => {
                if !libc::WIFSIGNALED(status) {
                    info.setup_failed = true;
                }
            }
        }
        let msg = bincode::serialize(&FromZygote::Exited { id: child.id, info }).unwrap();
        let _ = send_msg(sock, &msg, &[]);
    }
}

fn cstr(b: &[u8]) -> CString {
    CString::new(b.to_vec()).unwrap_or_else(|_| CString::new("invalid").unwrap())
}

fn path_cstr(p: &Path) -> CString {
    cstr(p.as_os_str().as_bytes())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn spawn_init(
    req: &SpawnReq,
    stdin: Option<OwnedFd>,
    stdout: OwnedFd,
    stderr: OwnedFd,
    extras: Vec<OwnedFd>,
) -> io::Result<(i32, OwnedFd)> {
    let (status_r, status_w) = pipe()?;
    let plan = SetupPlan::new(req)?;
    let pid = unsafe {
        libc::syscall(libc::SYS_clone, (plan.namespaces | libc::SIGCHLD) as libc::c_ulong, 0usize, 0usize, 0usize, 0usize)
    } as i32;
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        drop(status_r);
        if let Some(file) = &plan.join_cgroup {
            let _ = std::fs::write(file, "0");
        }
        let code = init_main(req, &plan, stdin, stdout, stderr, extras, status_w);
        unsafe { libc::_exit(code) };
    }
    Ok((pid, status_r))
}

fn mount(src: Option<&Path>, tgt: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let src_c = src.map(path_cstr);
    let tgt_c = path_cstr(tgt);
    let fs_c = fstype.map(|s| CString::new(s).unwrap());
    let data_c = data.map(|s| CString::new(s).unwrap());
    let r = unsafe {
        libc::mount(
            src_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            tgt_c.as_ptr(),
            fs_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags,
            data_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr() as *const libc::c_void),
        )
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("mount {}: {e}", tgt.display())));
    }
    Ok(())
}

struct MountOp {
    src: Option<PathBuf>,
    target: PathBuf,
    fstype: Option<&'static str>,
    flags: libc::c_ulong,
    data: Option<&'static str>,
}

enum SetupOp {
    Mount(MountOp),
    CreateFile(PathBuf),
    CreateDir(PathBuf),
    Symlink(&'static str, PathBuf),
    Devpts(MountOp, PathBuf),
    Pivot(PathBuf),
}

struct SetupPlan {
    namespaces: i32,
    loopback: bool,
    join_cgroup: Option<PathBuf>,
    mounts: Vec<SetupOp>,
}

impl SetupPlan {
    fn new(req: &SpawnReq) -> io::Result<Self> {
        let loopback = req.net == NetMode::Loopback;
        let namespaces = libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS
            | if loopback { libc::CLONE_NEWNET } else { 0 };
        let root = &req.root;
        let op = |src: Option<&Path>, target, fstype, flags, data| {
            MountOp { src: src.map(Path::to_path_buf), target, fstype, flags, data }
        };
        let dev = root.join("dev");
        let tmpfs = |target, flags, mode| SetupOp::Mount(op(Some(Path::new("tmpfs")), target, Some("tmpfs"), flags, Some(mode)));
        let mut mounts = vec![
            SetupOp::Mount(op(None, "/".into(), None, libc::MS_REC | libc::MS_PRIVATE, None)),
            SetupOp::Mount(op(Some(root), root.clone(), None, libc::MS_BIND | libc::MS_REC, None)),
            tmpfs(root.join("tmp"), libc::MS_NOSUID | libc::MS_NODEV, "mode=1777"),
            SetupOp::Mount(op(Some(Path::new("proc")), root.join("proc"), Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None)),
            SetupOp::Mount(op(Some(Path::new("/sys")), root.join("sys"), None, libc::MS_BIND | libc::MS_REC, None)),
            tmpfs(dev.clone(), libc::MS_NOSUID, "mode=755"),
        ];
        for name in ["null", "zero", "full", "random", "urandom", "tty"] {
            let target = dev.join(name);
            mounts.push(SetupOp::CreateFile(target.clone()));
            mounts.push(SetupOp::Mount(op(Some(&Path::new("/dev").join(name)), target, None, libc::MS_BIND, None)));
        }
        for (target, name) in [("/proc/self/fd", "fd"), ("/proc/self/fd/0", "stdin"), ("/proc/self/fd/1", "stdout"), ("/proc/self/fd/2", "stderr")] {
            mounts.push(SetupOp::Symlink(target, dev.join(name)));
        }
        mounts.push(SetupOp::CreateDir(dev.join("shm")));
        mounts.push(tmpfs(dev.join("shm"), libc::MS_NOSUID | libc::MS_NODEV, "mode=1777"));
        mounts.push(SetupOp::CreateDir(dev.join("pts")));
        mounts.push(SetupOp::Devpts(op(Some(Path::new("devpts")), dev.join("pts"), Some("devpts"),
            libc::MS_NOSUID | libc::MS_NOEXEC, Some("newinstance,ptmxmode=0666,mode=620")), dev.join("ptmx")));
        for bind in &req.binds {
            let rel = Path::new(OsStr::from_bytes(&bind.inside));
            let rel = rel.strip_prefix("/").unwrap_or(rel);
            if !rel.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "sandbox bind destination must stay within the root"));
            }
            let target = if rel.as_os_str().is_empty() { root.clone() } else { root.join(rel) };
            mounts.push(SetupOp::Mount(op(Some(&bind.host), target.clone(), None, libc::MS_BIND | libc::MS_REC, None)));
            if bind.read_only {
                mounts.push(SetupOp::Mount(op(None, target, None, libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC, None)));
            }
        }
        mounts.push(SetupOp::Pivot(root.clone()));
        let join_cgroup = req.cgroup.as_ref().map(|group| crate::cgroup::membership_file(group));
        Ok(Self { namespaces, loopback, join_cgroup, mounts })
    }

    #[cfg(test)]
    fn invariants(&self, root: &Path) -> bool {
        self.mounts.iter().enumerate().all(|(i, op)| match op {
            SetupOp::Mount(op) if i == 0 => op.target == Path::new("/") && op.flags == (libc::MS_REC | libc::MS_PRIVATE),
            SetupOp::Mount(op) | SetupOp::Devpts(op, _) => op.target.starts_with(root),
            SetupOp::CreateFile(target) | SetupOp::CreateDir(target) | SetupOp::Symlink(_, target) => target.starts_with(root),
            SetupOp::Pivot(at) => at == root && i + 1 == self.mounts.len(),
        })
    }

    fn execute(&self) -> io::Result<()> {
        for op in &self.mounts {
            match op {
                SetupOp::Mount(op) => mount(op.src.as_deref(), &op.target, op.fstype, op.flags, op.data)?,
                SetupOp::CreateFile(path) => { std::fs::File::create(path)?; }
                SetupOp::CreateDir(path) => std::fs::create_dir(path)?,
                SetupOp::Symlink(target, path) => std::os::unix::fs::symlink(target, path)?,
                SetupOp::Devpts(op, link) => {
                    if mount(op.src.as_deref(), &op.target, op.fstype, op.flags, op.data).is_ok() {
                        std::os::unix::fs::symlink("pts/ptmx", link)?;
                    }
                }
                SetupOp::Pivot(root) => pivot(root)?,
            }
        }
        Ok(())
    }
}

fn pivot(root: &Path) -> io::Result<()> {
    let root_c = path_cstr(root);
    unsafe {
        if libc::chdir(root_c.as_ptr()) != 0
            || libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) != 0
            || libc::umount2(c".".as_ptr(), libc::MNT_DETACH) != 0
            || libc::chdir(c"/".as_ptr()) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn loopback_up() {
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            return;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        ifr.ifr_name[0] = b'l' as libc::c_char;
        ifr.ifr_name[1] = b'o' as libc::c_char;
        if libc::ioctl(s, libc::SIOCGIFFLAGS, &mut ifr) == 0 {
            ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            libc::ioctl(s, libc::SIOCSIFFLAGS, &ifr);
        }
        libc::close(s);
    }
}

fn write_all_fd(fd: RawFd, mut b: &[u8]) {
    while !b.is_empty() {
        let r = unsafe { libc::write(fd, b.as_ptr() as *const _, b.len()) };
        if r <= 0 {
            return;
        }
        b = &b[r as usize..];
    }
}

fn init_main(
    req: &SpawnReq,
    plan: &SetupPlan,
    stdin: Option<OwnedFd>,
    stdout: OwnedFd,
    stderr: OwnedFd,
    extras: Vec<OwnedFd>,
    status_w: OwnedFd,
) -> i32 {
    let report = |status: i32, setup_failed: bool| {
        let mut buf = [0u8; 5];
        buf[..4].copy_from_slice(&status.to_le_bytes());
        buf[4] = setup_failed as u8;
        write_all_fd(status_w.as_raw_fd(), &buf);
    };
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        let name = b"rargo";
        libc::sethostname(name.as_ptr() as *const _, name.len());
    }
    if plan.loopback {
        loopback_up();
    }
    if let Err(e) = plan.execute() {
        write_all_fd(stderr.as_raw_fd(), format!("rargo: sandbox setup failed: {e}\n").as_bytes());
        report(126 << 8, true);
        return 126;
    }
    let Ok((exec_r, exec_w)) = pipe() else {
        report(126 << 8, true);
        return 126;
    };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        report(126 << 8, true);
        return 126;
    }
    if pid == 0 {
        drop(exec_r);
        let exec_w_hi = unsafe { libc::fcntl(exec_w.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
        drop(exec_w);
        let err = exec_child(req, stdin, stdout, stderr, extras);
        write_all_fd(exec_w_hi, &err.raw_os_error().unwrap_or(libc::EIO).to_le_bytes());
        unsafe { libc::_exit(127) };
    }
    drop(exec_w);
    let stderr_fd = stderr.into_raw_fd();
    drop(stdout);
    drop(stdin);
    drop(extras);
    let mut errbuf = [0u8; 4];
    let mut f = std::fs::File::from(exec_r);
    let exec_failed = f.read_exact(&mut errbuf).is_ok();
    if exec_failed {
        let e = io::Error::from_raw_os_error(i32::from_le_bytes(errbuf));
        let prog = String::from_utf8_lossy(req.argv.first().map(|v| &v[..]).unwrap_or(b""));
        write_all_fd(stderr_fd, format!("rargo: cannot execute `{prog}`: {e}\n").as_bytes());
    }
    unsafe { libc::close(stderr_fd) };
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(-1, &mut status, 0) };
        if r == pid {
            report(status, exec_failed);
            return 0;
        }
        if r < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            report(126 << 8, true);
            return 126;
        }
    }
}

struct ExecPlan {
    argv: Vec<CString>,
    env: Vec<CString>,
    candidates: Vec<Vec<u8>>,
}

impl ExecPlan {
    fn new(req: &SpawnReq) -> io::Result<Self> {
        let Some(prog) = req.argv.first() else { return Err(io::Error::from_raw_os_error(libc::EINVAL)) };
        let argv: Vec<CString> = req.argv.iter().map(|a| cstr(a)).collect();
        let env: Vec<CString> = req
            .env
            .iter()
            .map(|(k, v)| {
                let mut kv = k.clone();
                kv.push(b'=');
                kv.extend_from_slice(v);
                cstr(&kv)
            })
            .collect();
        let candidates: Vec<Vec<u8>> = if prog.contains(&b'/') {
            vec![prog.clone()]
        } else {
            let path = req
                .env
                .iter()
                .find(|(k, _)| k == b"PATH")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| b"/usr/local/bin:/usr/bin:/bin".to_vec());
            path.split(|&c| c == b':')
                .filter(|d| !d.is_empty())
                .map(|d| {
                    let mut p = d.to_vec();
                    p.push(b'/');
                    p.extend_from_slice(prog);
                    p
                })
                .collect()
        };
        Ok(Self { argv, env, candidates })
    }
}

fn exec_child(
    req: &SpawnReq,
    stdin: Option<OwnedFd>,
    stdout: OwnedFd,
    stderr: OwnedFd,
    extras: Vec<OwnedFd>,
) -> io::Error {
    unsafe {
        match &stdin {
            Some(fd) => {
                libc::dup2(fd.as_raw_fd(), 0);
            }
            None => {
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
                if null >= 0 {
                    libc::dup2(null, 0);
                    libc::close(null);
                }
            }
        }
        libc::dup2(stdout.as_raw_fd(), 1);
        libc::dup2(stderr.as_raw_fd(), 2);
        let parked: Vec<i32> =
            extras.iter().map(|fd| libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100)).collect();
        drop(extras);
        for (i, fd) in parked.iter().enumerate() {
            if *fd < 0 || libc::dup2(*fd, EXTRA_FD_BASE + i as i32) < 0 {
                return io::Error::last_os_error();
            }
        }
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        if req.gid != libc::getgid() || req.uid != libc::getuid() {
            libc::setgroups(0, std::ptr::null());
            if libc::setgid(req.gid) != 0 || libc::setuid(req.uid) != 0 {
                return io::Error::last_os_error();
            }
        }
        let cwd = cstr(&req.cwd);
        if libc::chdir(cwd.as_ptr()) != 0 {
            return io::Error::last_os_error();
        }
    }
    let exec = match ExecPlan::new(req) {
        Ok(exec) => exec,
        Err(error) => return error,
    };
    let mut argv_p: Vec<*const libc::c_char> = exec.argv.iter().map(|arg| arg.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> = exec.env.iter().map(|var| var.as_ptr()).collect();
    env_p.push(std::ptr::null());
    let mut last = io::Error::from_raw_os_error(libc::ENOENT);
    for c in exec.candidates {
        let c = cstr(&c);
        unsafe { libc::execve(c.as_ptr(), argv_p.as_ptr(), env_p.as_ptr()) };
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOENT) && e.raw_os_error() != Some(libc::ENOTDIR) {
            return e;
        }
        last = e;
    }
    last
}


#[cfg(test)]
mod setup_tests {
    use super::*;

    #[test]
    fn generated_setup_plans_keep_mounts_inside_the_action() {
        for net in [NetMode::Host, NetMode::Loopback] {
            for flake in [false, true] {
                for worker in [false, true] {
                    let root = PathBuf::from("/fixture/sandbox");
                    let binds = if flake { vec![Bind { host: "/nix/store".into(), inside: b"/nix/store".to_vec(), read_only: true }] } else { vec![] };
                    let req = SpawnReq { root: root.clone(), argv: vec![b"rustc".to_vec()], env: vec![], cwd: b"/work".to_vec(), uid: 700, gid: 701,
                        net, binds, cgroup: (!worker).then(|| "/fixture/cgroup/action-5".into()) };
                    let plan = SetupPlan::new(&req).unwrap();
                    assert!(plan.invariants(&root));
                    assert_eq!(plan.namespaces & libc::CLONE_NEWNET != 0, net == NetMode::Loopback);
                    assert_eq!(plan.join_cgroup, (!worker).then(|| "/fixture/cgroup/action-5/cgroup.procs".into()));
                    let mounted: Vec<_> = plan.mounts.iter().filter_map(|op| match op {
                        SetupOp::Mount(op) | SetupOp::Devpts(op, _) => Some(op.target.as_path()),
                        _ => None,
                    }).collect();
                    let mut expected = vec!["/", "/fixture/sandbox", "/fixture/sandbox/tmp", "/fixture/sandbox/proc", "/fixture/sandbox/sys",
                        "/fixture/sandbox/dev", "/fixture/sandbox/dev/null", "/fixture/sandbox/dev/zero", "/fixture/sandbox/dev/full",
                        "/fixture/sandbox/dev/random", "/fixture/sandbox/dev/urandom", "/fixture/sandbox/dev/tty", "/fixture/sandbox/dev/shm",
                        "/fixture/sandbox/dev/pts"];
                    if flake {
                        expected.extend(["/fixture/sandbox/nix/store"; 2]);
                    }
                    assert_eq!(mounted, expected.into_iter().map(Path::new).collect::<Vec<_>>());
                    let exec = ExecPlan::new(&req).unwrap();
                    assert_eq!(exec.argv[0].as_bytes(), b"rustc");
                    assert_eq!(exec.candidates, [b"/usr/local/bin/rustc".to_vec(), b"/usr/bin/rustc".to_vec(), b"/bin/rustc".to_vec()]);
                }
            }
        }
    }
}
