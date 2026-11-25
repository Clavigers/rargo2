use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Place {
    Server,
    Worker,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetupSpec {
    pub argv: Vec<Vec<u8>>,
    pub env: BTreeMap<Vec<u8>, Vec<u8>>,
    pub cwd: Vec<u8>,
    pub net: NetMode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostLayout {
    pub place: Place,
    pub root: PathBuf,
    pub seq: u64,
    pub cgroup_root: Option<PathBuf>,
    pub uid: u32,
    pub gid: u32,
    pub caller: (u32, u32),
    pub flake: bool,
    pub binds: Vec<Bind>,
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
    Spawn { id: u64, plan: Plan, has_stdin: bool, extra: usize },
    Kill { id: u64 },
}

#[derive(Serialize, Deserialize)]
enum FromZygote {
    Exited { id: u64, info: ExitInfo },
}

pub struct Zygote {
    sock: Mutex<UnixStream>,
    pending: Arc<Mutex<HashMap<u64, crate::io::oneshot::Sender<ExitInfo>>>>,
    next: AtomicU64,
    child: Mutex<Option<std::process::Child>>,
}

pub struct Running {
    pub id: u64,
    pub done: Option<crate::io::oneshot::Receiver<ExitInfo>>,
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
        let pending: Arc<Mutex<HashMap<u64, crate::io::oneshot::Sender<ExitInfo>>>> = Default::default();
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
        plan: Plan,
        stdin: Option<OwnedFd>,
        stdout: OwnedFd,
        stderr: OwnedFd,
        extra: &[RawFd],
    ) -> Result<Running> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = crate::io::oneshot::channel();
        assert!(self.pending.lock().insert(id, tx).is_none(), "spawn ids are unique");
        let msg = bincode::serialize(&ToZygote::Spawn {
            id,
            plan,
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
        Ok(Running { id, done: Some(rx) })
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
                    Ok(ToZygote::Spawn { id, plan, has_stdin, extra }) => {
                        let mut it = fds.into_iter();
                        let (Some(out), Some(err)) = (it.next(), it.next()) else { continue };
                        let stdin = if has_stdin { it.next() } else { None };
                        let extras: Vec<OwnedFd> = it.take(extra).collect();
                        match spawn_init(&plan, stdin, out, err, extras) {
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
    plan: &Plan,
    stdin: Option<OwnedFd>,
    stdout: OwnedFd,
    stderr: OwnedFd,
    extras: Vec<OwnedFd>,
) -> io::Result<(i32, OwnedFd)> {
    let (status_r, status_w) = pipe()?;
    let clone_at = plan
        .ops
        .iter()
        .position(|op| matches!(op, Op::Clone { .. }))
        .ok_or_else(|| io::Error::other("sandbox plan has no clone"))?;
    for op in &plan.ops[..clone_at] {
        apply(op)?;
    }
    let flags = match &plan.ops[clone_at] {
        Op::Clone { flags } => *flags,
        _ => unreachable!("the clone op is at clone_at"),
    };
    let pid = unsafe {
        libc::syscall(libc::SYS_clone, (flags | libc::SIGCHLD) as libc::c_ulong, 0usize, 0usize, 0usize, 0usize)
    } as i32;
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        drop(status_r);
        let code = run_child(plan, clone_at + 1, stdin, stdout, stderr, extras, status_w);
        unsafe { libc::_exit(code) };
    }
    Ok((pid, status_r))
}

fn run_child(
    plan: &Plan,
    from: usize,
    stdin: Option<OwnedFd>,
    stdout: OwnedFd,
    stderr: OwnedFd,
    extras: Vec<OwnedFd>,
    status_w: OwnedFd,
) -> i32 {
    let fail = |status_w: &OwnedFd, stderr: &OwnedFd, status: i32, setup_failed: bool| {
        report(status_w.as_raw_fd(), status, setup_failed);
        let _ = stderr;
    };
    let Some(fork_at) = plan.ops.iter().position(|op| matches!(op, Op::Fork)) else {
        report(status_w.as_raw_fd(), 126 << 8, true);
        return 126;
    };
    for op in &plan.ops[from..fork_at] {
        if let Err(e) = apply(op) {
            write_all_fd(stderr.as_raw_fd(), format!("rargo: sandbox setup failed: {e}\n").as_bytes());
            fail(&status_w, &stderr, 126 << 8, true);
            return 126;
        }
    }
    let Ok((exec_r, exec_w)) = pipe() else {
        fail(&status_w, &stderr, 126 << 8, true);
        return 126;
    };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail(&status_w, &stderr, 126 << 8, true);
        return 126;
    }
    if pid == 0 {
        drop(exec_r);
        let exec_w_hi = unsafe { libc::fcntl(exec_w.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
        drop(exec_w);
        let err = run_exec(plan, fork_at + 1, stdin, stdout, stderr, extras);
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
        let prog = plan.ops.iter().find_map(|op| match op {
            Op::Exec { argv, .. } => Some(String::from_utf8_lossy(argv.first().map(|v| &v[..]).unwrap_or(b""))),
            _ => None,
        });
        let prog = prog.as_deref().unwrap_or("");
        write_all_fd(stderr_fd, format!("rargo: cannot execute `{prog}`: {e}\n").as_bytes());
    }
    unsafe { libc::close(stderr_fd) };
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(-1, &mut status, 0) };
        if r == pid {
            report(status_w.as_raw_fd(), status, exec_failed);
            return 0;
        }
        if r < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            report(status_w.as_raw_fd(), 126 << 8, true);
            return 126;
        }
    }
}

fn run_exec(
    plan: &Plan,
    from: usize,
    stdin: Option<OwnedFd>,
    stdout: OwnedFd,
    stderr: OwnedFd,
    extras: Vec<OwnedFd>,
) -> io::Error {
    if let Err(e) = wire(stdin, stdout, stderr, extras) {
        return e;
    }
    for op in &plan.ops[from..] {
        if let Err(e) = apply(op) {
            return e;
        }
    }
    io::Error::from_raw_os_error(libc::ENOENT)
}

fn wire(stdin: Option<OwnedFd>, stdout: OwnedFd, stderr: OwnedFd, extras: Vec<OwnedFd>) -> io::Result<()> {
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
        let parked: Vec<i32> = extras.iter().map(|fd| libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100)).collect();
        drop(extras);
        for (i, fd) in parked.iter().enumerate() {
            if *fd < 0 || libc::dup2(*fd, EXTRA_FD_BASE + i as i32) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
    }
    Ok(())
}

fn report(fd: RawFd, status: i32, setup_failed: bool) {
    let mut buf = [0u8; 5];
    buf[..4].copy_from_slice(&status.to_le_bytes());
    buf[4] = u8::from(setup_failed);
    write_all_fd(fd, &buf);
}

fn mount_at(src: Option<&Path>, tgt: &Path, fstype: Option<&str>, flags: u64, data: Option<&str>) -> io::Result<()> {
    let src_c = src.map(path_cstr);
    let tgt_c = path_cstr(tgt);
    let fs_c = fstype.map(|s| CString::new(s).unwrap());
    let data_c = data.map(|s| CString::new(s).unwrap());
    let r = unsafe {
        libc::mount(
            src_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            tgt_c.as_ptr(),
            fs_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags as libc::c_ulong,
            data_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr() as *const libc::c_void),
        )
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("mount {}: {e}", tgt.display())));
    }
    Ok(())
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

fn loopback_up() -> io::Result<()> {
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        ifr.ifr_name[0] = b'l' as libc::c_char;
        ifr.ifr_name[1] = b'o' as libc::c_char;
        if libc::ioctl(s, libc::SIOCGIFFLAGS, &mut ifr) != 0 {
            let e = io::Error::last_os_error();
            libc::close(s);
            return Err(e);
        }
        ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        let set = libc::ioctl(s, libc::SIOCSIFFLAGS, &ifr);
        let err = (set != 0).then(io::Error::last_os_error);
        libc::close(s);
        if let Some(e) = err {
            return Err(e);
        }
    }
    Ok(())
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Mount {
    src: Option<PathBuf>,
    target: PathBuf,
    fstype: Option<String>,
    flags: u64,
    data: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Op {
    CreateDir { path: PathBuf, parents: bool },
    Write { path: PathBuf, data: Vec<u8> },
    Clone { flags: i32 },
    DeathSignal,
    Hostname(String),
    Loopback,
    Mount(Mount),
    CreateFile(PathBuf),
    Symlink { target: String, path: PathBuf },
    Devpts { mount: Mount, link: PathBuf },
    Pivot(PathBuf),
    Fork,
    SetIds { uid: u32, gid: u32 },
    Chdir(Vec<u8>),
    Exec { argv: Vec<Vec<u8>>, env: Vec<(Vec<u8>, Vec<u8>)>, candidates: Vec<Vec<u8>> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    place: Place,
    asked: NetMode,
    flake: bool,
    root: PathBuf,
    seq: u64,
    cgroup_root: Option<PathBuf>,
    uid: u32,
    gid: u32,
    caller: (u32, u32),
    env_in: BTreeMap<Vec<u8>, Vec<u8>>,
    cwd: Vec<u8>,
    argv: Vec<Vec<u8>>,
    ops: Vec<Op>,
}

pub(crate) fn apply(op: &Op) -> io::Result<()> {
    match op {
        Op::CreateDir { path, parents: true } => {
            std::fs::create_dir_all(path)?;
            Ok(())
        }
        Op::CreateDir { path, parents: false } => {
            std::fs::create_dir(path)?;
            Ok(())
        }
        Op::Write { path, data } => {
            std::fs::write(path, data)?;
            Ok(())
        }
        Op::DeathSignal => {
            if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        Op::Hostname(name) => {
            let b = name.as_bytes();
            if unsafe { libc::sethostname(b.as_ptr() as *const _, b.len()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        Op::Loopback => loopback_up(),
        Op::Mount(m) => mount_at(m.src.as_deref(), &m.target, m.fstype.as_deref(), m.flags, m.data.as_deref()),
        Op::CreateFile(path) => {
            std::fs::File::create(path)?;
            Ok(())
        }
        Op::Symlink { target, path } => {
            std::os::unix::fs::symlink(target, path)?;
            Ok(())
        }
        Op::Devpts { mount, link } => {
            if mount_at(mount.src.as_deref(), &mount.target, mount.fstype.as_deref(), mount.flags, mount.data.as_deref()).is_ok() {
                std::os::unix::fs::symlink("pts/ptmx", link)?;
            }
            Ok(())
        }
        Op::Pivot(root) => pivot(root),
        Op::SetIds { uid, gid } => unsafe {
            libc::setgroups(0, std::ptr::null());
            if libc::setgid(*gid) != 0 || libc::setuid(*uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        },
        Op::Chdir(cwd) => {
            let c = cstr(cwd);
            if unsafe { libc::chdir(c.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        Op::Exec { argv, env, candidates } => exec_at(argv, env, candidates),
        Op::Clone { .. } | Op::Fork => Err(io::Error::other("clone and fork are applied by the executor")),
    }
}

fn exec_at(argv: &[Vec<u8>], env: &[(Vec<u8>, Vec<u8>)], candidates: &[Vec<u8>]) -> io::Result<()> {
    let argv_c: Vec<CString> = argv.iter().map(|a| cstr(a)).collect();
    let env_c: Vec<CString> = env
        .iter()
        .map(|(k, v)| {
            let mut kv = k.clone();
            kv.push(b'=');
            kv.extend_from_slice(v);
            cstr(&kv)
        })
        .collect();
    let mut argv_p: Vec<*const libc::c_char> = argv_c.iter().map(|a| a.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> = env_c.iter().map(|a| a.as_ptr()).collect();
    env_p.push(std::ptr::null());
    let mut last = io::Error::from_raw_os_error(libc::ENOENT);
    for c in candidates {
        let c = cstr(c);
        unsafe { libc::execve(c.as_ptr(), argv_p.as_ptr(), env_p.as_ptr()) };
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOENT) && e.raw_os_error() != Some(libc::ENOTDIR) {
            return Err(e);
        }
        last = e;
    }
    Err(last)
}

fn jobserver_value() -> Vec<u8> {
    let a = EXTRA_FD_BASE;
    let b = EXTRA_FD_BASE + 1;
    format!("-j --jobserver-fds={a},{b} --jobserver-auth={a},{b}").into_bytes()
}

fn exec_env(spec: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut env = spec.clone();
    let value = jobserver_value();
    for name in [b"CARGO_MAKEFLAGS".as_slice(), b"MAKEFLAGS".as_slice(), b"MFLAGS".as_slice()] {
        env.insert(name.to_vec(), value.clone());
    }
    env.into_iter().collect()
}

fn program_candidates(argv: &[Vec<u8>], env: &BTreeMap<Vec<u8>, Vec<u8>>) -> io::Result<Vec<Vec<u8>>> {
    let Some(prog) = argv.first() else { return Err(io::Error::from_raw_os_error(libc::EINVAL)) };
    if prog.contains(&b'/') {
        return Ok(vec![prog.clone()]);
    }
    let path = env.get(b"PATH".as_slice()).cloned().unwrap_or_else(|| b"/usr/local/bin:/usr/bin:/bin".to_vec());
    Ok(path
        .split(|&c| c == b':')
        .filter(|d| !d.is_empty())
        .map(|d| {
            let mut p = d.to_vec();
            p.push(b'/');
            p.extend_from_slice(prog);
            p
        })
        .collect())
}

fn bind_target(root: &Path, inside: &[u8]) -> io::Result<PathBuf> {
    let rel = Path::new(OsStr::from_bytes(inside));
    let rel = rel.strip_prefix("/").unwrap_or(rel);
    if !rel.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "sandbox bind destination must stay within the root"));
    }
    Ok(if rel.as_os_str().is_empty() { root.to_path_buf() } else { root.join(rel) })
}

fn isolated(place: Place, net: NetMode) -> bool {
    place == Place::Worker || net == NetMode::Loopback
}

struct Builder<'a> {
    root: &'a Path,
    cgroup_root: Option<&'a Path>,
    ops: Vec<Op>,
}

impl Builder<'_> {
    fn push(&mut self, op: Op) {
        admit(self.root, self.cgroup_root, &op);
        self.ops.push(op);
    }

    fn mount(&mut self, src: Option<&Path>, target: PathBuf, fstype: Option<&str>, flags: u64, data: Option<&str>) {
        self.push(Op::Mount(Mount {
            src: src.map(Path::to_path_buf),
            target,
            fstype: fstype.map(str::to_string),
            flags,
            data: data.map(str::to_string),
        }));
    }
}

fn clean(path: &Path) -> bool {
    path.components().all(|c| !matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir))
}

fn admit(root: &Path, cgroup_root: Option<&Path>, op: &Op) {
    let sandbox = |path: &Path| {
        assert!(clean(path), "path {} has a parent component", path.display());
        assert!(
            path == Path::new("/") || path.starts_with(root),
            "path {} is outside {}",
            path.display(),
            root.display()
        );
    };
    match op {
        Op::Mount(m) | Op::Devpts { mount: m, .. } => sandbox(&m.target),
        Op::CreateFile(path) | Op::Symlink { path, .. } | Op::Pivot(path) => sandbox(path),
        Op::CreateDir { path, parents: false } => {
            assert!(clean(path), "path {} has a parent component", path.display());
            if path.starts_with(root) {
                return;
            }
            let Some(cg) = cgroup_root else {
                panic!("directory {} is outside {}", path.display(), root.display());
            };
            assert!(path.starts_with(cg), "directory {} is outside {}", path.display(), cg.display());
        }
        Op::CreateDir { parents: true, .. } => panic!("an action plan creates one directory at a time"),
        Op::Write { path, .. } => {
            assert!(clean(path), "path {} has a parent component", path.display());
            let Some(cg) = cgroup_root else { panic!("write {} has no cgroup", path.display()) };
            assert!(path.starts_with(cg), "write {} is outside {}", path.display(), cg.display());
        }
        Op::Clone { .. } | Op::Fork | Op::DeathSignal | Op::Hostname(_) | Op::Loopback | Op::SetIds { .. } | Op::Chdir(_) | Op::Exec { .. } => {}
    }
}

pub fn plan(spec: &SetupSpec, host: &HostLayout) -> io::Result<Plan> {
    let loopback = isolated(host.place, spec.net);
    let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS;
    if loopback {
        flags |= libc::CLONE_NEWNET;
    }
    let cgroup_dir = host.cgroup_root.as_ref().map(|cg| cg.join(format!("action-{}", host.seq)));
    let mut b = Builder { root: &host.root, cgroup_root: host.cgroup_root.as_deref(), ops: Vec::new() };
    if let Some(dir) = &cgroup_dir {
        b.push(Op::CreateDir { path: dir.clone(), parents: false });
    }
    b.push(Op::Clone { flags });
    if let Some(dir) = &cgroup_dir {
        b.push(Op::Write { path: crate::cgroup::membership_file(dir), data: b"0".to_vec() });
    }
    b.push(Op::DeathSignal);
    b.push(Op::Hostname("rargo".into()));
    if loopback {
        b.push(Op::Loopback);
    }
    let root = &host.root;
    b.mount(None, PathBuf::from("/"), None, (libc::MS_REC | libc::MS_PRIVATE) as u64, None);
    b.mount(Some(root), root.clone(), None, (libc::MS_BIND | libc::MS_REC) as u64, None);
    b.mount(Some(Path::new("tmpfs")), root.join("tmp"), Some("tmpfs"), (libc::MS_NOSUID | libc::MS_NODEV) as u64, Some("mode=1777"));
    b.mount(
        Some(Path::new("proc")),
        root.join("proc"),
        Some("proc"),
        (libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as u64,
        None,
    );
    b.mount(Some(Path::new("/sys")), root.join("sys"), None, (libc::MS_BIND | libc::MS_REC) as u64, None);
    let dev = root.join("dev");
    b.mount(Some(Path::new("tmpfs")), dev.clone(), Some("tmpfs"), libc::MS_NOSUID as u64, Some("mode=755"));
    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        let target = dev.join(name);
        b.push(Op::CreateFile(target.clone()));
        let src = Path::new("/dev").join(name);
        b.mount(Some(&src), target, None, libc::MS_BIND as u64, None);
    }
    for (target, name) in [
        ("/proc/self/fd", "fd"),
        ("/proc/self/fd/0", "stdin"),
        ("/proc/self/fd/1", "stdout"),
        ("/proc/self/fd/2", "stderr"),
    ] {
        b.push(Op::Symlink { target: target.into(), path: dev.join(name) });
    }
    b.push(Op::CreateDir { path: dev.join("shm"), parents: false });
    b.mount(Some(Path::new("tmpfs")), dev.join("shm"), Some("tmpfs"), (libc::MS_NOSUID | libc::MS_NODEV) as u64, Some("mode=1777"));
    b.push(Op::CreateDir { path: dev.join("pts"), parents: false });
    b.push(Op::Devpts {
        mount: Mount {
            src: Some(PathBuf::from("devpts")),
            target: dev.join("pts"),
            fstype: Some("devpts".into()),
            flags: (libc::MS_NOSUID | libc::MS_NOEXEC) as u64,
            data: Some("newinstance,ptmxmode=0666,mode=620".into()),
        },
        link: dev.join("ptmx"),
    });
    for bind in &host.binds {
        let target = bind_target(root, &bind.inside)?;
        b.mount(Some(&bind.host), target.clone(), None, (libc::MS_BIND | libc::MS_REC) as u64, None);
        if bind.read_only {
            b.mount(None, target, None, (libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC) as u64, None);
        }
    }
    if host.flake {
        let target = bind_target(root, b"/nix/store")?;
        b.mount(Some(Path::new("/nix/store")), target.clone(), None, (libc::MS_BIND | libc::MS_REC) as u64, None);
        b.mount(None, target, None, (libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC) as u64, None);
    }
    b.push(Op::Pivot(root.clone()));
    b.push(Op::Fork);
    b.push(Op::DeathSignal);
    if (host.uid, host.gid) != host.caller {
        b.push(Op::SetIds { uid: host.uid, gid: host.gid });
    }
    b.push(Op::Chdir(spec.cwd.clone()));
    let env = exec_env(&spec.env);
    let candidates = program_candidates(&spec.argv, &spec.env)?;
    b.push(Op::Exec { argv: spec.argv.clone(), env, candidates });
    let built = Plan {
        place: host.place,
        asked: spec.net,
        flake: host.flake,
        root: host.root.clone(),
        seq: host.seq,
        cgroup_root: host.cgroup_root.clone(),
        uid: host.uid,
        gid: host.gid,
        caller: host.caller,
        env_in: spec.env.clone(),
        cwd: spec.cwd.clone(),
        argv: spec.argv.clone(),
        ops: b.ops,
    };
    built.invariants();
    Ok(built)
}

impl Plan {
    pub fn cgroup_dir(&self) -> Option<PathBuf> {
        self.cgroup_root.as_ref().map(|root| root.join(format!("action-{}", self.seq)))
    }

    fn invariants(&self) {
        let root = &self.root;
        for op in &self.ops {
            admit(root, self.cgroup_root.as_deref(), op);
        }
        let clone_at = self.ops.iter().position(|op| matches!(op, Op::Clone { .. })).expect("clone");
        let fork_at = self.ops.iter().position(|op| matches!(op, Op::Fork)).expect("fork");
        let pivot_at = self.ops.iter().position(|op| matches!(op, Op::Pivot(_))).expect("pivot");
        assert_eq!(self.ops.iter().filter(|op| matches!(op, Op::Clone { .. })).count(), 1);
        assert_eq!(self.ops.iter().filter(|op| matches!(op, Op::Fork)).count(), 1);
        assert_eq!(self.ops.iter().filter(|op| matches!(op, Op::Pivot(_))).count(), 1);
        assert_eq!(self.ops.iter().filter(|op| matches!(op, Op::Exec { .. })).count(), 1);
        assert_eq!(pivot_at + 1, fork_at, "pivot is the last filesystem op");
        assert!(matches!(self.ops.last(), Some(Op::Exec { .. })), "exec is the last op");
        assert!(clone_at < fork_at && fork_at < self.ops.len() - 1);
        let mut expect_flags = libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS;
        if isolated(self.place, self.asked) {
            expect_flags |= libc::CLONE_NEWNET;
        }
        match &self.ops[clone_at] {
            Op::Clone { flags } => assert_eq!(*flags, expect_flags),
            _ => unreachable!("clone_at is a clone"),
        }
        let loopbacks = self.ops.iter().filter(|op| matches!(op, Op::Loopback)).count();
        assert_eq!(loopbacks, usize::from(isolated(self.place, self.asked)));
        assert_eq!(self.ops.iter().filter(|op| matches!(op, Op::Hostname(name) if name == "rargo")).count(), 1);
        assert_eq!(self.ops.iter().filter(|op| matches!(op, Op::DeathSignal)).count(), 2);
        assert!(matches!(self.ops.get(fork_at + 1), Some(Op::DeathSignal)), "the executed process dies with its parent");
        match &self.ops[pivot_at] {
            Op::Pivot(at) => assert_eq!(at, root),
            _ => unreachable!("pivot_at is a pivot"),
        }
        let private = self.ops.iter().filter(|op| {
            matches!(op, Op::Mount(m) if m.target == Path::new("/") && m.flags == (libc::MS_REC | libc::MS_PRIVATE) as u64 && m.src.is_none())
        }).count();
        assert_eq!(private, 1, "the mount tree is made private");
        let proc_flags = self.ops.iter().find_map(|op| match op {
            Op::Mount(m) if m.target == root.join("proc") => Some(m.flags),
            _ => None,
        }).expect("proc is mounted");
        let noexec = libc::MS_NOSUID as u64 | libc::MS_NODEV as u64 | libc::MS_NOEXEC as u64;
        assert_eq!(proc_flags & noexec, noexec, "proc is nosuid, nodev, noexec");
        let nix: Vec<&Mount> = self
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Mount(m) if m.target.ends_with("nix/store") => Some(m),
                _ => None,
            })
            .collect();
        if self.flake {
            assert_eq!(nix.len(), 2, "a flake store is bound read-only");
            assert_eq!(nix[0].flags, (libc::MS_BIND | libc::MS_REC) as u64);
            assert_eq!(nix[1].flags, (libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC) as u64);
            assert!(nix[1].src.is_none());
        } else {
            assert!(nix.is_empty(), "no flake store is mounted");
        }
        match self.cgroup_dir() {
            Some(dir) => {
                assert!(matches!(&self.ops[0], Op::CreateDir { path, parents: false } if path == &dir));
                assert_eq!(clone_at, 1);
                let procs = crate::cgroup::membership_file(&dir);
                assert!(matches!(&self.ops[2], Op::Write { path, data } if path == &procs && data == b"0"));
            }
            None => {
                assert_eq!(clone_at, 0);
                assert!(!self.ops.iter().any(|op| matches!(op, Op::Write { .. })));
                assert!(!self.ops.iter().any(|op| matches!(op, Op::CreateDir { path, .. } if !path.starts_with(root))));
            }
        }
        let want_ids = (self.uid, self.gid) != self.caller;
        let setids: Vec<_> = self.ops.iter().filter_map(|op| match op {
            Op::SetIds { uid, gid } => Some((*uid, *gid)),
            _ => None,
        }).collect();
        if want_ids {
            assert_eq!(setids, vec![(self.uid, self.gid)]);
        } else {
            assert!(setids.is_empty());
        }
        assert!(self.ops.iter().any(|op| matches!(op, Op::Chdir(cwd) if cwd == &self.cwd)));
        let Op::Exec { argv, env, .. } = self.ops.last().expect("exec") else { unreachable!("exec is last") };
        assert_eq!(argv, &self.argv);
        assert_eq!(env, &exec_env(&self.env_in));
        for (i, op) in self.ops.iter().enumerate() {
            if let Op::Mount(m) = op {
                if m.target == Path::new("/") {
                    assert_eq!(m.flags, (libc::MS_REC | libc::MS_PRIVATE) as u64);
                } else {
                    assert!(m.target.starts_with(root), "mount {} at op {i} leaves the root", m.target.display());
                }
            }
        }
    }
}

fn bits(value: u64, names: &[(u64, &str)]) -> String {
    let mut out = Vec::new();
    let mut left = value;
    for &(bit, name) in names {
        if bit != 0 && value & bit == bit {
            out.push(name.to_string());
            left &= !bit;
        }
    }
    if left != 0 {
        out.push(format!("{left:#x}"));
    }
    if out.is_empty() { "0".into() } else { out.join(",") }
}

fn mount_bits(flags: u64) -> String {
    bits(flags, &[
        (libc::MS_RDONLY as u64, "rdonly"),
        (libc::MS_NOSUID as u64, "nosuid"),
        (libc::MS_NODEV as u64, "nodev"),
        (libc::MS_NOEXEC as u64, "noexec"),
        (libc::MS_REMOUNT as u64, "remount"),
        (libc::MS_BIND as u64, "bind"),
        (libc::MS_REC as u64, "rec"),
        (libc::MS_PRIVATE as u64, "private"),
    ])
}

fn clone_bits(flags: i32) -> String {
    bits(flags as u64, &[
        (libc::CLONE_NEWNS as u64, "newns"),
        (libc::CLONE_NEWPID as u64, "newpid"),
        (libc::CLONE_NEWIPC as u64, "newipc"),
        (libc::CLONE_NEWUTS as u64, "newuts"),
        (libc::CLONE_NEWNET as u64, "newnet"),
    ])
}

fn show_opt(value: &Option<impl std::fmt::Display>) -> String {
    value.as_ref().map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

impl std::fmt::Display for Mount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "src={} target={} type={} flags={} data={}",
            show_opt(&self.src.as_ref().map(|p| p.display().to_string())),
            self.target.display(),
            self.fstype.as_deref().unwrap_or("-"),
            mount_bits(self.flags),
            self.data.as_deref().unwrap_or("-"),
        )
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::CreateDir { path, parents: true } => write!(f, "ensure-dir {}", path.display()),
            Op::CreateDir { path, parents: false } => write!(f, "create-dir {}", path.display()),
            Op::Write { path, data } => write!(f, "write {} {}", path.display(), String::from_utf8_lossy(data)),
            Op::Clone { flags } => write!(f, "clone {}", clone_bits(*flags)),
            Op::DeathSignal => write!(f, "death-signal"),
            Op::Hostname(name) => write!(f, "hostname {name}"),
            Op::Loopback => write!(f, "loopback"),
            Op::Mount(m) => write!(f, "mount {m}"),
            Op::CreateFile(path) => write!(f, "create-file {}", path.display()),
            Op::Symlink { target, path } => write!(f, "symlink {target} {}", path.display()),
            Op::Devpts { mount, link } => write!(f, "devpts {mount} link={}", link.display()),
            Op::Pivot(path) => write!(f, "pivot {}", path.display()),
            Op::Fork => write!(f, "fork"),
            Op::SetIds { uid, gid } => write!(f, "setids {uid}:{gid}"),
            Op::Chdir(cwd) => write!(f, "chdir {}", String::from_utf8_lossy(cwd)),
            Op::Exec { argv, env, candidates } => {
                let prog = String::from_utf8_lossy(argv.first().map(|a| a.as_slice()).unwrap_or(b""));
                writeln!(f, "exec {prog}")?;
                for (k, v) in env {
                    writeln!(f, "  env {}={}", String::from_utf8_lossy(k), String::from_utf8_lossy(v))?;
                }
                for (i, arg) in argv.iter().enumerate() {
                    writeln!(f, "  argv {i} {}", String::from_utf8_lossy(arg))?;
                }
                for c in candidates {
                    writeln!(f, "  try {}", String::from_utf8_lossy(c))?;
                }
                Ok(())
            }
        }
    }
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let place = match self.place {
            Place::Server => "server",
            Place::Worker => "worker",
        };
        let asked = match self.asked {
            NetMode::Host => "host",
            NetMode::Loopback => "loopback",
        };
        let network = if isolated(self.place, self.asked) { "loopback" } else { "host" };
        writeln!(f, "place: {place}")?;
        writeln!(f, "asked-network: {asked}")?;
        writeln!(f, "network: {network}")?;
        writeln!(f, "flake: {}", if self.flake { "yes" } else { "no" })?;
        writeln!(f, "root: {}", self.root.display())?;
        match self.cgroup_dir() {
            Some(dir) => writeln!(f, "cgroup: {}", dir.display())?,
            None => writeln!(f, "cgroup: -")?,
        }
        writeln!(f, "ids: {}:{}", self.uid, self.gid)?;
        writeln!(f, "caller: {}:{}", self.caller.0, self.caller.1)?;
        writeln!(f, "cwd: {}", String::from_utf8_lossy(&self.cwd))?;
        for op in &self.ops {
            write!(f, "{op}")?;
            if !matches!(op, Op::Exec { .. }) {
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod setup_tests {
    use super::*;
    use expect_test::expect;

    fn fixture(
        place: Place,
        net: NetMode,
        flake: bool,
        cgroup: bool,
        caller: (u32, u32),
        binds: Vec<Bind>,
    ) -> io::Result<Plan> {
        let mut env = BTreeMap::new();
        env.insert(b"TERM".to_vec(), b"dumb".to_vec());
        let spec = SetupSpec { argv: vec![b"rustc".to_vec()], env, cwd: b"/work".to_vec(), net };
        let host = HostLayout {
            place,
            root: PathBuf::from("/fixture/sandbox"),
            seq: 5,
            cgroup_root: cgroup.then(|| PathBuf::from("/fixture/cgroup")),
            uid: 700,
            gid: 701,
            caller,
            flake,
            binds,
        };
        plan(&spec, &host)
    }

    fn check(place: Place, net: NetMode, flake: bool, cgroup: bool, exp: expect_test::Expect) {
        let plan = fixture(place, net, flake, cgroup, (0, 0), vec![]).unwrap();
        plan.invariants();
        exp.assert_eq(&plan.to_string());
    }

    #[test]
    fn generated_setup_plans_match_their_snapshots() {
        check(Place::Server, NetMode::Host, false, true, expect![[r#"
            place: server
            asked-network: host
            network: host
            flake: no
            root: /fixture/sandbox
            cgroup: /fixture/cgroup/action-5
            ids: 700:701
            caller: 0:0
            cwd: /work
            create-dir /fixture/cgroup/action-5
            clone newns,newpid,newipc,newuts
            write /fixture/cgroup/action-5/cgroup.procs 0
            death-signal
            hostname rargo
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Server, NetMode::Host, true, true, expect![[r#"
            place: server
            asked-network: host
            network: host
            flake: yes
            root: /fixture/sandbox
            cgroup: /fixture/cgroup/action-5
            ids: 700:701
            caller: 0:0
            cwd: /work
            create-dir /fixture/cgroup/action-5
            clone newns,newpid,newipc,newuts
            write /fixture/cgroup/action-5/cgroup.procs 0
            death-signal
            hostname rargo
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            mount src=/nix/store target=/fixture/sandbox/nix/store type=- flags=bind,rec data=-
            mount src=- target=/fixture/sandbox/nix/store type=- flags=rdonly,remount,bind,rec data=-
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Server, NetMode::Loopback, false, true, expect![[r#"
            place: server
            asked-network: loopback
            network: loopback
            flake: no
            root: /fixture/sandbox
            cgroup: /fixture/cgroup/action-5
            ids: 700:701
            caller: 0:0
            cwd: /work
            create-dir /fixture/cgroup/action-5
            clone newns,newpid,newipc,newuts,newnet
            write /fixture/cgroup/action-5/cgroup.procs 0
            death-signal
            hostname rargo
            loopback
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Server, NetMode::Loopback, true, true, expect![[r#"
            place: server
            asked-network: loopback
            network: loopback
            flake: yes
            root: /fixture/sandbox
            cgroup: /fixture/cgroup/action-5
            ids: 700:701
            caller: 0:0
            cwd: /work
            create-dir /fixture/cgroup/action-5
            clone newns,newpid,newipc,newuts,newnet
            write /fixture/cgroup/action-5/cgroup.procs 0
            death-signal
            hostname rargo
            loopback
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            mount src=/nix/store target=/fixture/sandbox/nix/store type=- flags=bind,rec data=-
            mount src=- target=/fixture/sandbox/nix/store type=- flags=rdonly,remount,bind,rec data=-
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Worker, NetMode::Host, false, true, expect![[r#"
            place: worker
            asked-network: host
            network: loopback
            flake: no
            root: /fixture/sandbox
            cgroup: /fixture/cgroup/action-5
            ids: 700:701
            caller: 0:0
            cwd: /work
            create-dir /fixture/cgroup/action-5
            clone newns,newpid,newipc,newuts,newnet
            write /fixture/cgroup/action-5/cgroup.procs 0
            death-signal
            hostname rargo
            loopback
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Worker, NetMode::Host, true, true, expect![[r#"
            place: worker
            asked-network: host
            network: loopback
            flake: yes
            root: /fixture/sandbox
            cgroup: /fixture/cgroup/action-5
            ids: 700:701
            caller: 0:0
            cwd: /work
            create-dir /fixture/cgroup/action-5
            clone newns,newpid,newipc,newuts,newnet
            write /fixture/cgroup/action-5/cgroup.procs 0
            death-signal
            hostname rargo
            loopback
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            mount src=/nix/store target=/fixture/sandbox/nix/store type=- flags=bind,rec data=-
            mount src=- target=/fixture/sandbox/nix/store type=- flags=rdonly,remount,bind,rec data=-
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Worker, NetMode::Loopback, false, true, expect![[r#"
            place: worker
            asked-network: loopback
            network: loopback
            flake: no
            root: /fixture/sandbox
            cgroup: /fixture/cgroup/action-5
            ids: 700:701
            caller: 0:0
            cwd: /work
            create-dir /fixture/cgroup/action-5
            clone newns,newpid,newipc,newuts,newnet
            write /fixture/cgroup/action-5/cgroup.procs 0
            death-signal
            hostname rargo
            loopback
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Worker, NetMode::Loopback, true, true, expect![[r#"
            place: worker
            asked-network: loopback
            network: loopback
            flake: yes
            root: /fixture/sandbox
            cgroup: /fixture/cgroup/action-5
            ids: 700:701
            caller: 0:0
            cwd: /work
            create-dir /fixture/cgroup/action-5
            clone newns,newpid,newipc,newuts,newnet
            write /fixture/cgroup/action-5/cgroup.procs 0
            death-signal
            hostname rargo
            loopback
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            mount src=/nix/store target=/fixture/sandbox/nix/store type=- flags=bind,rec data=-
            mount src=- target=/fixture/sandbox/nix/store type=- flags=rdonly,remount,bind,rec data=-
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Server, NetMode::Loopback, false, false, expect![[r#"
            place: server
            asked-network: loopback
            network: loopback
            flake: no
            root: /fixture/sandbox
            cgroup: -
            ids: 700:701
            caller: 0:0
            cwd: /work
            clone newns,newpid,newipc,newuts,newnet
            death-signal
            hostname rargo
            loopback
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
        check(Place::Worker, NetMode::Host, false, false, expect![[r#"
            place: worker
            asked-network: host
            network: loopback
            flake: no
            root: /fixture/sandbox
            cgroup: -
            ids: 700:701
            caller: 0:0
            cwd: /work
            clone newns,newpid,newipc,newuts,newnet
            death-signal
            hostname rargo
            loopback
            mount src=- target=/ type=- flags=rec,private data=-
            mount src=/fixture/sandbox target=/fixture/sandbox type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/tmp type=tmpfs flags=nosuid,nodev data=mode=1777
            mount src=proc target=/fixture/sandbox/proc type=proc flags=nosuid,nodev,noexec data=-
            mount src=/sys target=/fixture/sandbox/sys type=- flags=bind,rec data=-
            mount src=tmpfs target=/fixture/sandbox/dev type=tmpfs flags=nosuid data=mode=755
            create-file /fixture/sandbox/dev/null
            mount src=/dev/null target=/fixture/sandbox/dev/null type=- flags=bind data=-
            create-file /fixture/sandbox/dev/zero
            mount src=/dev/zero target=/fixture/sandbox/dev/zero type=- flags=bind data=-
            create-file /fixture/sandbox/dev/full
            mount src=/dev/full target=/fixture/sandbox/dev/full type=- flags=bind data=-
            create-file /fixture/sandbox/dev/random
            mount src=/dev/random target=/fixture/sandbox/dev/random type=- flags=bind data=-
            create-file /fixture/sandbox/dev/urandom
            mount src=/dev/urandom target=/fixture/sandbox/dev/urandom type=- flags=bind data=-
            create-file /fixture/sandbox/dev/tty
            mount src=/dev/tty target=/fixture/sandbox/dev/tty type=- flags=bind data=-
            symlink /proc/self/fd /fixture/sandbox/dev/fd
            symlink /proc/self/fd/0 /fixture/sandbox/dev/stdin
            symlink /proc/self/fd/1 /fixture/sandbox/dev/stdout
            symlink /proc/self/fd/2 /fixture/sandbox/dev/stderr
            create-dir /fixture/sandbox/dev/shm
            mount src=tmpfs target=/fixture/sandbox/dev/shm type=tmpfs flags=nosuid,nodev data=mode=1777
            create-dir /fixture/sandbox/dev/pts
            devpts src=devpts target=/fixture/sandbox/dev/pts type=devpts flags=nosuid,noexec data=newinstance,ptmxmode=0666,mode=620 link=/fixture/sandbox/dev/ptmx
            pivot /fixture/sandbox
            fork
            death-signal
            setids 700:701
            chdir /work
            exec rustc
              env CARGO_MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MAKEFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env MFLAGS=-j --jobserver-fds=3,4 --jobserver-auth=3,4
              env TERM=dumb
              argv 0 rustc
              try /usr/local/bin/rustc
              try /usr/bin/rustc
              try /bin/rustc
        "#]]);
    }

    #[test]
    fn an_escaping_bind_is_rejected() {
        for inside in [b"/../etc".as_slice(), b"a/../../x".as_slice()] {
            let err = fixture(
                Place::Server,
                NetMode::Loopback,
                false,
                true,
                (0, 0),
                vec![Bind { host: PathBuf::from("/etc"), inside: inside.to_vec(), read_only: true }],
            )
            .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            assert!(err.to_string().contains("within the root"), "{err}");
        }
    }

    #[test]
    fn matching_caller_ids_omit_setids() {
        let plan = fixture(Place::Server, NetMode::Loopback, false, true, (700, 701), vec![]).unwrap();
        plan.invariants();
        assert!(!plan.ops.iter().any(|op| matches!(op, Op::SetIds { .. })));
    }
}
