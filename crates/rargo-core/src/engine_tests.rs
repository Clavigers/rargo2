//! The engine, driven in a simulation with no kernel under it.
//!
//! One thing here is made up: the process of an action. `Machine` takes the
//! zygote's place, and each process it starts is a fiber that does what the
//! action's command would do. That fiber reaches files the way a process in a
//! sandbox does, through FUSE: `Proc` frames each request as the kernel would
//! and hands it to fusewire's request handler over the engine's own
//! `SandboxFs`. The host files live in the simulated filesystem and are read
//! through the real views. Keys, traces, caching, joining and the timeout are
//! the engine's own.

use crate::buildview::{BaseView, BuildView, HostMemo, Hosted, Mount, OutputsLayer, Region, LEVEL_FULL};
use crate::db::Db;
use crate::engine::{ClientEnv, Engine, Kept, NullObserver, Request};
use crate::fn_code::{
    self,
    fixture::{self, Function, Run},
};
use crate::hash::Hash;
use crate::io::{self, file, sim};
use crate::sandbox::{ExitInfo, Launcher, NetMode, Plan, Running};
use crate::sandboxfs::SandboxFs;
use crate::sched::{Priority, Scheduler};
use crate::store::Store;
use crate::trace::{ActionKind, ActionSpec, MEntry};
use crate::view::{show, vjoin, vsplit, Content, HostRegion, View};
use fusewire::abi::{
    fuse_attr, fuse_create_in, fuse_dirent, fuse_entry_out, fuse_in_header, fuse_open_in, fuse_open_out, fuse_out_header,
    fuse_read_in, fuse_release_in, fuse_write_in, FUSE_CREATE, FUSE_LOOKUP, FUSE_OPEN, FUSE_READ, FUSE_READDIR,
    FUSE_RELEASE, FUSE_ROOT_ID, FUSE_WRITE,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::mem::{offset_of, size_of};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// The requests below are written out field by field; these are the sizes that layout relies on.
const _: () = assert!(size_of::<fuse_in_header>() == 40 && size_of::<fuse_out_header>() == 16);
const _: () = assert!(size_of::<fuse_open_in>() == 8 && size_of::<fuse_create_in>() == 16);
const _: () = assert!(size_of::<fuse_read_in>() == 40 && size_of::<fuse_write_in>() == 40);
const _: () = assert!(size_of::<fuse_release_in>() == 24 && size_of::<fuse_dirent>() == 24);

/// The most bytes one read or write request carries.
const CHUNK: usize = 128 * 1024;
/// The size of the buffer the kernel offers a directory read.
const PAGE: u32 = 4096;
/// What `/bin/sh` holds on the simulated host.
const SH: &[u8] = b"the simulated shell\n";

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// A file or directory as a lookup answered it.
#[derive(Clone, Copy)]
struct Node {
    id: u64,
    mode: u32,
}

impl Node {
    fn of(entry: &[u8]) -> Option<Node> {
        let id = le_u64(entry, offset_of!(fuse_entry_out, nodeid));
        let mode = le_u32(entry, offset_of!(fuse_entry_out, attr) + offset_of!(fuse_attr, mode));
        (id != 0).then_some(Node { id, mode })
    }

    fn is(&self, kind: u32) -> bool {
        self.mode & libc::S_IFMT == kind
    }
}

/// What a command of the simulated shell does, and the status it exits with.
type Shell = fn(&mut Proc, &str) -> i32;

/// A process in an action's sandbox: its command, and the kernel's side of its file access.
struct Proc {
    fs: Arc<SandboxFs>,
    handler: fusewire::RequestHandler,
    unique: u64,
    root: u64,
    cwd: Vec<u8>,
    argv: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    stdout: io::PipeEnd,
    stderr: io::PipeEnd,
}

impl Proc {
    /// Sends one FUSE request to the filesystem and returns the body of its reply, or its error number.
    fn request(&mut self, opcode: u32, node: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        self.unique += 2;
        let len = size_of::<fuse_in_header>() + body.len();
        let mut frame = Vec::with_capacity(len);
        frame.extend((len as u32).to_le_bytes());
        frame.extend(opcode.to_le_bytes());
        frame.extend(self.unique.to_le_bytes());
        frame.extend(node.to_le_bytes());
        frame.extend([0u8; 16]);
        frame.extend(body);
        assert_eq!(frame.len(), len);
        let reply = self.handler.handle(&*self.fs, &frame).bytes.to_vec();
        assert_eq!(le_u32(&reply, 0) as usize, reply.len(), "a reply says how long it is");
        assert_eq!(le_u64(&reply, 8), self.unique, "a reply answers its request");
        match le_u32(&reply, 4) as i32 {
            0 => Ok(reply[size_of::<fuse_out_header>()..].to_vec()),
            error => Err(-error),
        }
    }

    fn lookup(&mut self, dir: u64, name: &[u8]) -> Result<Option<Node>, i32> {
        match self.request(FUSE_LOOKUP, dir, &[name, &[0]].concat()) {
            Ok(entry) => Ok(Node::of(&entry)),
            Err(libc::ENOENT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn absolute(&self, path: &[u8]) -> Vec<u8> {
        if path.starts_with(b"/") { path.to_vec() } else { vjoin(&self.cwd, path) }
    }

    /// Looks a path up one name at a time from the root of the sandbox, as the kernel resolves it.
    fn walk(&mut self, path: &[u8]) -> Result<Option<Node>, i32> {
        let path = self.absolute(path);
        let mut at = Node { id: self.root, mode: libc::S_IFDIR };
        for name in path.split(|c| *c == b'/').filter(|name| !name.is_empty()) {
            assert!(name != b"." && name != b"..", "the simulated kernel resolves plain names only");
            if !at.is(libc::S_IFDIR) {
                return Err(libc::ENOTDIR);
            }
            match self.lookup(at.id, name)? {
                Some(node) => at = node,
                None => return Ok(None),
            }
            assert!(!at.is(libc::S_IFLNK), "the simulated kernel follows no symbolic links");
        }
        Ok(Some(at))
    }

    fn exists(&mut self, path: &[u8]) -> bool {
        matches!(self.walk(path), Ok(Some(_)))
    }

    fn open(&mut self, node: u64, flags: i32) -> Result<u64, i32> {
        let opened = self.request(FUSE_OPEN, node, &[(flags as u32).to_le_bytes(), [0; 4]].concat())?;
        Ok(le_u64(&opened, offset_of!(fuse_open_out, fh)))
    }

    fn release(&mut self, node: u64, fh: u64) {
        let body = [&fh.to_le_bytes()[..], &[0; 16]].concat();
        self.request(FUSE_RELEASE, node, &body).expect("a release is answered");
    }

    fn read_node(&mut self, node: Node) -> Result<Vec<u8>, i32> {
        if node.is(libc::S_IFDIR) {
            return Err(libc::EISDIR);
        }
        let fh = self.open(node.id, libc::O_RDONLY)?;
        let mut data = Vec::new();
        let read = loop {
            let body = [
                &fh.to_le_bytes()[..],
                &(data.len() as u64).to_le_bytes(),
                &(CHUNK as u32).to_le_bytes(),
                &[0; 20],
            ]
            .concat();
            match self.request(FUSE_READ, node.id, &body) {
                Ok(got) => {
                    data.extend_from_slice(&got);
                    if got.len() < CHUNK {
                        break Ok(());
                    }
                }
                Err(e) => break Err(e),
            }
        };
        self.release(node.id, fh);
        read.map(|()| data)
    }

    fn read(&mut self, path: &[u8]) -> Result<Vec<u8>, i32> {
        let node = self.walk(path)?.ok_or(libc::ENOENT)?;
        self.read_node(node)
    }

    /// Opens `path` for writing, created with `mode` or emptied, and writes `bytes` to it.
    fn write(&mut self, path: &[u8], bytes: &[u8], mode: u32) -> Result<(), i32> {
        let path = self.absolute(path);
        let (dir, name) = vsplit(&path).ok_or(libc::EISDIR)?;
        let dir = self.walk(dir)?.ok_or(libc::ENOENT)?;
        if !dir.is(libc::S_IFDIR) {
            return Err(libc::ENOTDIR);
        }
        let (node, fh) = match self.lookup(dir.id, name)? {
            Some(node) => (node.id, self.open(node.id, libc::O_WRONLY | libc::O_TRUNC)?),
            None => {
                let flags = (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC) as u32;
                let body = [
                    &flags.to_le_bytes()[..],
                    &mode.to_le_bytes(),
                    &0o022u32.to_le_bytes(),
                    &[0; 4],
                    name,
                    &[0],
                ]
                .concat();
                let made = self.request(FUSE_CREATE, dir.id, &body)?;
                let node = Node::of(&made).expect("a created file has a node");
                (node.id, le_u64(&made, size_of::<fuse_entry_out>() + offset_of!(fuse_open_out, fh)))
            }
        };
        let mut written = Ok(());
        for (i, chunk) in bytes.chunks(CHUNK).enumerate() {
            let body = [
                &fh.to_le_bytes()[..],
                &((i * CHUNK) as u64).to_le_bytes(),
                &(chunk.len() as u32).to_le_bytes(),
                &[0; 20],
                chunk,
            ]
            .concat();
            match self.request(FUSE_WRITE, node, &body) {
                Ok(wrote) => assert_eq!(le_u32(&wrote, 0) as usize, chunk.len(), "a write takes all it is given"),
                Err(e) => {
                    written = Err(e);
                    break;
                }
            }
        }
        self.release(node, fh);
        written
    }

    /// The names in a directory, read a page at a time from where the page before ended.
    fn list(&mut self, path: &[u8]) -> Result<Vec<Vec<u8>>, i32> {
        let dir = self.walk(path)?.ok_or(libc::ENOENT)?;
        if !dir.is(libc::S_IFDIR) {
            return Err(libc::ENOTDIR);
        }
        let mut names = Vec::new();
        let mut offset = 0u64;
        loop {
            let body = [&[0; 8][..], &offset.to_le_bytes(), &PAGE.to_le_bytes(), &[0; 20]].concat();
            let page = self.request(FUSE_READDIR, dir.id, &body)?;
            assert!(page.len() <= PAGE as usize, "a page of names fits the buffer it was asked into");
            if page.is_empty() {
                return Ok(names);
            }
            let mut at = 0;
            while at < page.len() {
                let len = le_u32(&page, at + offset_of!(fuse_dirent, namelen)) as usize;
                let next = le_u64(&page, at + offset_of!(fuse_dirent, off));
                assert!(next > offset, "each name in a directory comes after the one before it");
                offset = next;
                let name = at + size_of::<fuse_dirent>();
                names.push(page[name..name + len].to_vec());
                at = (name + len + 7) & !7;
            }
        }
    }

    fn var(&self, name: &[u8]) -> Option<Vec<u8>> {
        self.env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    fn out(&self, text: &str) {
        io::write_at(self.stdout.as_raw_fd(), 0, text.as_bytes()).expect("the engine reads the output");
    }

    fn err(&self, text: &str) {
        io::write_at(self.stderr.as_raw_fd(), 0, text.as_bytes()).expect("the engine reads the errors");
    }

    /// `cat` of one file: its bytes, or a complaint on the error output.
    fn cat(&mut self, path: &str) -> Option<Vec<u8>> {
        match self.read(path.as_bytes()) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                self.err(&format!("cat: {path}: {}\n", std::io::Error::from_raw_os_error(e)));
                None
            }
        }
    }

    /// Reads an executable file as `execve` does before it runs it.
    fn load(&mut self, path: &[u8]) -> Result<Vec<u8>, i32> {
        let node = self.walk(path)?.ok_or(libc::ENOENT)?;
        if !node.is(libc::S_IFREG) || node.mode & 0o111 == 0 {
            return Err(libc::EACCES);
        }
        self.read_node(node)
    }

    /// Runs the command: its exit status, and whether it could not be started at all.
    fn run(&mut self, candidates: &[Vec<u8>], shell: Shell) -> (i32, bool) {
        let mut loaded = Err(libc::ENOENT);
        for candidate in candidates {
            loaded = self.load(candidate);
            if !matches!(loaded, Err(libc::ENOENT | libc::ENOTDIR)) {
                break;
            }
        }
        let program = match loaded {
            Ok(program) => program,
            Err(e) => {
                self.err(&format!("exec {}: {}\n", show(&self.argv[0]), std::io::Error::from_raw_os_error(e)));
                return (1, true);
            }
        };
        if let Some((marks, exit)) = fixture::run_of(&program) {
            if let Some(path) = self.var(fn_code::MARKS_ENV) {
                let _ = self.write(&path, marks, 0o644);
            }
            return (i32::from(exit), false);
        }
        assert_eq!(program, SH, "the simulated host runs its shell and the fixture's programs");
        assert_eq!(self.argv[1], b"-c", "the shell is given one command line");
        let script = String::from_utf8(self.argv[2].clone()).expect("a command line is text");
        (shell(self, &script), false)
    }
}

/// What the machine tells the fiber that keeps its processes.
enum Note {
    Started(u64, io::JoinHandle<()>),
    Kill(u64),
}

/// Stands where the zygote does: every launch is a fiber that acts as the planned process.
struct Machine {
    fs: Arc<SandboxFs>,
    shell: Shell,
    notes: io::UnboundedSender<Note>,
}

impl Launcher for Machine {
    fn launch(&self, plan: Plan, stdout: io::PipeEnd, stderr: io::PipeEnd, _extra: &[RawFd]) -> anyhow::Result<Running> {
        let exec = plan.exec();
        let id = exec.seq;
        let mut proc = Proc {
            fs: self.fs.clone(),
            handler: fusewire::RequestHandler::new(1 << 20),
            unique: 0,
            root: FUSE_ROOT_ID,
            cwd: exec.cwd.to_vec(),
            argv: exec.argv.to_vec(),
            env: exec.env.to_vec(),
            stdout,
            stderr,
        };
        let candidates = exec.candidates.to_vec();
        let shell = self.shell;
        let (exited, done) = io::oneshot::channel();
        let running = io::spawn(move || {
            let started = io::now();
            let root = proc.lookup(FUSE_ROOT_ID, SandboxFs::dir_name(id).as_bytes());
            proc.root = root.ok().flatten().expect("the action has its root in the filesystem").id;
            let (code, setup_failed) = proc.run(&candidates, shell);
            drop(proc);
            let wall_us = io::now().saturating_duration_since(started).as_micros() as u64;
            let _ = exited.send(ExitInfo { status: (code & 0xff) << 8, setup_failed, wall_us, ..Default::default() });
        });
        self.notes.send(Note::Started(id, running)).ok().expect("the machine keeps its processes");
        Ok(Running { id, done: Some(done) })
    }

    fn kill(&self, id: u64) {
        self.notes.send(Note::Kill(id)).ok().expect("the machine keeps its processes");
    }
}

struct Fx {
    tmp: PathBuf,
    work: PathBuf,
    engine: Arc<Engine>,
    memo: Arc<HostMemo>,
    /// The processes the engine had the machine kill, in order.
    killed: Rc<RefCell<Vec<u64>>>,
}

fn fixture(shell: Shell) -> Fx {
    fixture_with(shell, Duration::from_secs(3600))
}

fn fixture_with(shell: Shell, action_timeout: Duration) -> Fx {
    let tmp = sim::tempdir();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let store = Arc::new(Store::open(&tmp.join("store"), db).unwrap());
    let content = Arc::new(Content::new(store.clone()));
    let fs = Arc::new(SandboxFs::new(content.clone()));

    let (notes, mut inbox) = io::unbounded_channel();
    let killed = Rc::new(RefCell::new(Vec::new()));
    let log = killed.clone();
    drop(io::spawn(move || {
        let mut running = BTreeMap::new();
        while let Some(note) = inbox.recv() {
            match note {
                Note::Started(id, process) => {
                    running.insert(id, process);
                }
                Note::Kill(id) => {
                    log.borrow_mut().push(id);
                    if let Some(process) = running.remove(&id) {
                        process.abort();
                    }
                }
            }
        }
    }));

    let machine = Arc::new(Machine { fs: fs.clone(), shell, notes });
    let sched = Scheduler::new(8, u64::MAX).unwrap();
    let mut engine = Engine::new(store, content, fs, tmp.join("mnt"), machine, sched);
    engine.action_timeout = action_timeout;

    let work = tmp.join("work");
    file::create_dir_all(&work).unwrap();
    file::create_dir_all("/bin").unwrap();
    file::write("/bin/sh", SH).unwrap();
    file::set_mode("/bin/sh", 0o755).unwrap();
    Fx { tmp, work, engine: Arc::new(engine), memo: Arc::new(HostMemo::default()), killed }
}

impl Fx {
    /// The host as an action sees it, read afresh: the simulated root, with `work` at `/w` and the fixture's own directory hidden.
    fn base(&self, work: &Path) -> BaseView {
        let g = self.memo.advance();
        BaseView::new(
            vec![
                Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/")), traced: true },
                Mount { at: b"/w".to_vec(), region: Region::Host(HostRegion::new(work)), traced: true },
                Mount { at: self.tmp.clone().into_os_string().into_vec(), region: Region::Empty, traced: true },
            ],
            self.memo.clone(),
            g,
        )
    }

    fn view(&self) -> Arc<dyn View> {
        self.view_of(&self.work)
    }

    fn view_of(&self, work: &Path) -> Arc<dyn View> {
        Arc::new(Hosted::new(self.base(work)))
    }

    fn req(&self, script: &str, env: &[(&str, &str)], client_env: &[(&str, &str)], declared: &[&str]) -> Request {
        let mut e: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        e.insert(b"PATH".to_vec(), b"/usr/bin:/bin".to_vec());
        for (k, v) in env {
            e.insert(k.as_bytes().to_vec(), v.as_bytes().to_vec());
        }
        let declared: Vec<String> = declared.iter().map(|s| s.to_string()).collect();
        Request {
            spec: ActionSpec {
                kind: ActionKind::Other,
                argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()],
                env: e,
                cwd: b"/w".to_vec(),
                net: NetMode::Loopback,
                mkdirs: vec![],
                root_id: Hash::ZERO,
                discard: vec![],
                label: script.to_string(),
            },
            view: self.view(),
            client_env: Arc::new(ClientEnv::new(
                client_env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                Vec::new(),
                None,
            )),
            uid: 0,
            gid: 0,
            prio: Priority::Agent,
            no_cache: false,
            seed_blobs: Default::default(),
            cacheable: Arc::new(|r| r.success()),
            declared_env: Arc::new(move |_, _| declared.clone()),
            observer: Arc::new(NullObserver),
        }
    }

    fn text(&self, h: &Hash) -> String {
        String::from_utf8(self.engine.store.read(h).unwrap()).unwrap()
    }

    fn runs(&self) -> u64 {
        self.engine.stats.runs.load(Ordering::Relaxed)
    }
}

/// The command lines the tests below give `/bin/sh -c`, each doing what a shell would do with it.
fn sh(p: &mut Proc, script: &str) -> i32 {
    let echo = |p: &Proc, name: &str, var: &str| {
        let value = p.var(var.as_bytes()).unwrap_or_default();
        p.out(&format!("{name}={}\n", show(&value)));
        0
    };
    match script {
        "cat in.txt > out.txt; echo printed; echo warned >&2; test -e missing.txt || true" => {
            let bytes = p.cat("in.txt").unwrap_or_default();
            p.write(b"out.txt", &bytes, 0o666).expect("the redirection opens its file");
            p.out("printed\n");
            p.err("warned\n");
            p.exists(b"missing.txt");
            0
        }
        "sleep 0.5; cat in.txt" => {
            io::sleep(Duration::from_millis(500));
            match p.cat("in.txt") {
                Some(bytes) => {
                    p.out(&show(&bytes));
                    0
                }
                None => 1,
            }
        }
        "echo value=$FOO" => echo(p, "value", "FOO"),
        "echo other=$BAR" => echo(p, "other", "BAR"),
        "echo oops >&2; exit 3" => {
            p.err("oops\n");
            3
        }
        "sleep 100000" => {
            io::sleep(Duration::from_secs(100_000));
            0
        }
        "echo fine" => {
            p.out("fine\n");
            0
        }
        "ls /w/big | wc -l; ls -la /w/big | grep -c rmeta; ls /w/big | sort | uniq -d | wc -l" => {
            let names = match p.list(b"/w/big") {
                Ok(names) => names,
                Err(e) => {
                    p.err(&format!("ls: /w/big: {}\n", std::io::Error::from_raw_os_error(e)));
                    return 2;
                }
            };
            p.out(&format!("{}\n", names.len()));
            let mut described = 0;
            for name in &names {
                if p.exists(&vjoin(b"/w/big", name)) && name.windows(5).any(|w| w == b"rmeta") {
                    described += 1;
                }
            }
            p.out(&format!("{described}\n"));
            let mut sorted = names.clone();
            sorted.sort();
            let mut twice: Vec<&Vec<u8>> = sorted.windows(2).filter(|w| w[0] == w[1]).map(|w| &w[0]).collect();
            twice.dedup();
            p.out(&format!("{}\n", twice.len()));
            0
        }
        _ => match script.strip_prefix("cat /w/in/libapp.rlib /w/in/libnative.a > /w/out/") {
            Some(rest) => {
                let name = rest.split('.').next().unwrap_or_default();
                assert_eq!(rest, format!("{name}.read && cp /w/in/{name} /w/out/{name}"));
                let (Some(mut read), Some(native)) = (p.cat("/w/in/libapp.rlib"), p.cat("/w/in/libnative.a")) else {
                    return 1;
                };
                read.extend_from_slice(&native);
                p.write(format!("/w/out/{name}.read").as_bytes(), &read, 0o666).expect("the redirection opens its file");
                let from = format!("/w/in/{name}");
                let Ok(Some(source)) = p.walk(from.as_bytes()) else {
                    p.err(&format!("cp: {from}: No such file or directory\n"));
                    return 1;
                };
                let bytes = p.read_node(source).expect("cp reads its source");
                p.write(format!("/w/out/{name}").as_bytes(), &bytes, source.mode & 0o777).expect("cp writes its copy");
                0
            }
            None => {
                p.err(&format!("sh: the simulated shell does not know `{script}`\n"));
                127
            }
        },
    }
}

#[test]
fn replays_until_an_observed_input_changes() {
    sim::test(|| {
        let fx = fixture(sh);
        file::write(fx.work.join("in.txt"), "one\n").unwrap();
        file::write(fx.work.join("other.txt"), "x\n").unwrap();
        let script = "cat in.txt > out.txt; echo printed; echo warned >&2; test -e missing.txt || true";

        let a = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(!a.cached && a.result.success(), "{}", fx.text(&a.result.stderr));
        assert_eq!(fx.text(&a.result.stdout), "printed\n");
        assert_eq!(fx.text(&a.result.stderr), "warned\n");
        let Some(MEntry::File { hash, .. }) = a.result.outputs.get(b"/w/out.txt") else { panic!("no output") };
        assert_eq!(fx.text(hash), "one\n");
        assert_eq!(a.result.outputs.entries.len(), 1, "{:?}", a.result.outputs);
        assert!(!file::exists(fx.work.join("out.txt")), "an output stays out of the host directory");
        assert_eq!(fx.runs(), 1);

        let b = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(b.cached);
        assert_eq!(fx.text(&b.result.stdout), "printed\n");
        assert_eq!(fx.runs(), 1);

        file::write(fx.work.join("other.txt"), "changed\n").unwrap();
        assert!(fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap().cached);

        file::write(fx.work.join("in.txt"), "two\n").unwrap();
        let c = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(!c.cached);
        let Some(MEntry::File { hash, .. }) = c.result.outputs.get(b"/w/out.txt") else { panic!() };
        assert_eq!(fx.text(hash), "two\n");
        assert_eq!(fx.runs(), 2);

        file::write(fx.work.join("missing.txt"), "now here\n").unwrap();
        assert!(!fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap().cached);
        assert_eq!(fx.runs(), 3);

        file::remove_file(fx.work.join("missing.txt")).unwrap();
        file::write(fx.work.join("in.txt"), "one\n").unwrap();
        let d = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(d.cached);
        assert_eq!(fx.runs(), 3);
    });
}

#[test]
fn waiter_runs_its_own_when_the_trace_does_not_hold() {
    sim::test(|| {
        let fx = fixture(sh);
        let other = fx.tmp.join("work2");
        file::create_dir_all(&other).unwrap();
        file::write(fx.work.join("in.txt"), "first\n").unwrap();
        file::write(other.join("in.txt"), "second\n").unwrap();
        let script = "sleep 0.5; cat in.txt";

        let e = fx.engine.clone();
        let r1 = fx.req(script, &[], &[], &[]);
        let mut first = io::spawn(move || e.obtain(r1).unwrap());
        io::sleep(Duration::from_millis(150));
        let mut r2 = fx.req(script, &[], &[], &[]);
        r2.view = fx.view_of(&other);
        let second = fx.engine.obtain(r2).unwrap();
        let first = first.join().unwrap();
        assert_eq!(fx.text(&first.result.stdout), "first\n");
        assert_eq!(fx.text(&second.result.stdout), "second\n", "the waiter must not accept a result for other inputs");
        assert_eq!(fx.runs(), 2);
        assert_eq!(fx.engine.stats.joined.load(Ordering::Relaxed), 1);
        assert_eq!(fx.engine.stats.joined_then_ran.load(Ordering::Relaxed), 1);

        file::write(fx.work.join("in.txt"), "third\n").unwrap();
        file::write(other.join("in.txt"), "fourth\n").unwrap();
        let e = fx.engine.clone();
        let r1 = fx.req(script, &[], &[], &[]);
        let mut a = io::spawn(move || e.obtain(r1).unwrap());
        io::sleep(Duration::from_millis(100));
        let mut r2 = fx.req(script, &[], &[], &[]);
        r2.view = fx.view_of(&other);
        let b = fx.engine.obtain(r2).unwrap();
        let a = a.join().unwrap();
        assert_eq!(fx.text(&a.result.stdout), "third\n");
        assert_eq!(fx.text(&b.result.stdout), "fourth\n");
        assert_eq!(fx.runs(), 4);
        assert_eq!(fx.engine.stats.joined.load(Ordering::Relaxed), 1, "different predictions, no waiting");
    });
}

#[test]
fn declared_environment_variables_become_part_of_the_key() {
    sim::test(|| {
        let fx = fixture(sh);
        let script = "echo value=$FOO";
        let a = fx.engine.obtain(fx.req(script, &[], &[("FOO", "bar")], &["FOO"])).unwrap();
        assert_eq!(fx.text(&a.result.stdout), "value=bar\n");
        assert_eq!(fx.runs(), 2);
        assert_eq!(fx.engine.stats.env_reruns.load(Ordering::Relaxed), 1);
        let b = fx.engine.obtain(fx.req(script, &[], &[("FOO", "bar")], &["FOO"])).unwrap();
        assert!(b.cached);
        assert_eq!(fx.text(&b.result.stdout), "value=bar\n");
        let c = fx.engine.obtain(fx.req(script, &[], &[("FOO", "baz")], &["FOO"])).unwrap();
        assert!(!c.cached);
        assert_eq!(fx.text(&c.result.stdout), "value=baz\n");
        let d = fx.engine.obtain(fx.req(script, &[], &[], &["FOO"])).unwrap();
        assert!(d.cached);
        assert_eq!(fx.text(&d.result.stdout), "value=\n");
        let e = fx.engine.obtain(fx.req("echo other=$BAR", &[], &[("BAR", "x")], &[])).unwrap();
        assert_eq!(fx.text(&e.result.stdout), "other=\n");
    });
}

#[test]
fn failures_are_not_cached_unless_allowed() {
    sim::test(|| {
        let fx = fixture(sh);
        let script = "echo oops >&2; exit 3";
        let a = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert_eq!(a.result.code(), Some(3));
        let b = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(!b.cached);
        assert_eq!(fx.runs(), 2);

        let mut r = fx.req("true", &[], &[], &[]);
        r.spec.argv = vec![b"/no/such/program".to_vec()];
        let err = fx.engine.obtain(r).err().expect("must fail").to_string();
        assert!(err.contains("could not start"), "{err}");
        assert_eq!(fx.runs(), 3);

        let allowed = || {
            let mut r = fx.req(script, &[], &[], &[]);
            r.cacheable = Arc::new(|_| true);
            r
        };
        let c = fx.engine.obtain(allowed()).unwrap();
        assert!(!c.cached, "nothing was recorded for it before");
        assert_eq!(fx.runs(), 4);
        let d = fx.engine.obtain(allowed()).unwrap();
        assert!(d.cached, "a failure the request allows is recorded");
        assert_eq!(d.result.code(), Some(3));
        assert_eq!(fx.text(&d.result.stderr), "oops\n");
        assert!(fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap().cached, "and it answers every later request");
        assert_eq!(fx.runs(), 4);
    });
}

#[test]
fn a_request_with_caching_turned_off_runs_and_records_what_it_ran() {
    sim::test(|| {
        let fx = fixture(sh);
        let uncached = || {
            let mut r = fx.req("echo fine", &[], &[], &[]);
            r.no_cache = true;
            r
        };
        assert!(!fx.engine.obtain(uncached()).unwrap().cached);
        assert!(!fx.engine.obtain(uncached()).unwrap().cached, "a recorded run is not used");
        assert_eq!(fx.runs(), 2);
        assert_eq!(fx.engine.explain_request(&uncached()), "caching was turned off for it");
        let kept = fx.engine.obtain(fx.req("echo fine", &[], &[], &[])).unwrap();
        assert!(kept.cached, "what it ran was recorded");
        assert_eq!(fx.text(&kept.result.stdout), "fine\n");
        assert_eq!(fx.runs(), 2);
    });
}

#[test]
fn large_directories_list_completely_through_a_build_view() {
    sim::test(|| {
        let fx = fixture(sh);
        let big = fx.work.join("big");
        file::create_dir_all(&big).unwrap();
        for i in 0..400 {
            file::write(big.join(format!("libsomething-with-a-long-name-{i:04}-5cc9b44b3cc93987.rmeta")), "x").unwrap();
        }
        let mut r = fx.req("ls /w/big | wc -l; ls -la /w/big | grep -c rmeta; ls /w/big | sort | uniq -d | wc -l", &[], &[], &[]);
        r.view = Arc::new(BuildView {
            base: Arc::new(fx.base(&fx.work)),
            layer: Arc::new(OutputsLayer::new()),
            levels: Arc::new(vec![]),
            untraced: vec![],
            seed: None,
            content: None,
        });
        let o = fx.engine.obtain(r).unwrap();
        assert_eq!(fx.text(&o.result.stdout), "400\n400\n0\n", "{}", fx.text(&o.result.stderr));
    });
}

#[test]
fn a_hung_action_is_stopped_and_its_waiters_fail_with_it() {
    sim::test(|| {
        let fx = fixture_with(sh, Duration::from_secs(1));
        let started = io::now();
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let e = fx.engine.clone();
            let r = fx.req("sleep 100000", &[], &[], &[]);
            tasks.push(io::spawn(move || e.obtain(r).map(|_| ()).map_err(|e| format!("{e:#}"))));
        }
        for t in &mut tasks {
            let e = t.join().unwrap().expect_err("a hung action fails");
            assert!(e.contains("ran longer than 1 s"), "{e}");
        }
        let waited = io::now().saturating_duration_since(started);
        assert!(waited >= Duration::from_secs(1) && waited < Duration::from_secs(2), "stopped at the timeout: {waited:?}");
        assert_eq!(fx.runs(), 1, "three identical requests, one run, and no rerun by the waiters");
        assert_eq!(fx.engine.stats.joined.load(Ordering::Relaxed), 2);
        let ok = fx.engine.obtain(fx.req("echo fine", &[], &[], &[])).unwrap();
        assert_eq!(fx.text(&ok.result.stdout), "fine\n");
        io::yield_now();
        assert_eq!(fx.killed.borrow().len(), 1, "the engine had the one hung process killed: {:?}", fx.killed.borrow());
    });
}

mod fn_code_selection {
    use super::*;

    const TESTS: [(&str, &[&str]); 4] = [
        ("a", &["_RNv3app5parse", "_RNv3app6shared", "_RNv5tests1a"]),
        ("b", &["_RNv3app6render", "_RNv3app6shared", "_RNv5tests1b"]),
        ("c", &["_RNv3app6shared", "_RNv5tests1c"]),
        ("d", &["_RNv3app5parse", "_RNv5tests1d"]),
    ];

    #[derive(Clone, Copy)]
    struct App {
        parse: u8,
        render: Option<u8>,
        shared: u8,
        unused: u8,
        sections: bool,
        exit: u8,
        pad: u8,
        native: u8,
    }

    const BASE: App = App { parse: 1, render: Some(2), shared: 3, unused: 4, sections: true, exit: 0, pad: 0, native: 0 };

    fn functions(app: &App) -> Vec<Function> {
        vec![
            Function { symbol: "_RNv3app5parse", identity: fixture::id(app.parse) },
            Function { symbol: "_RNv3app6render", identity: app.render.map(|n| [n; 32]) },
            Function { symbol: "_RNv3app6shared", identity: fixture::id(app.shared) },
            Function { symbol: "_RNv3app6unused", identity: fixture::id(app.unused) },
            Function { symbol: "_RNv5tests1a", identity: fixture::id(50) },
            Function { symbol: "_RNv5tests1b", identity: fixture::id(51) },
            Function { symbol: "_RNv5tests1c", identity: fixture::id(52) },
            Function { symbol: "_RNv5tests1d", identity: fixture::id(53) },
        ]
    }

    fn put(fx: &Fx, unit: u32, layer: &OutputsLayer, path: &str, bytes: &[u8], exec: bool) {
        let hash = fx.engine.store.put_bytes(bytes).unwrap();
        layer.add(unit, LEVEL_FULL, &[(path.as_bytes().to_vec(), MEntry::File { hash, exec, size: bytes.len() as u64 })]);
    }

    fn layered(fx: &Fx, layer: &Arc<OutputsLayer>) -> Arc<dyn View> {
        Arc::new(BuildView {
            base: Arc::new(fx.base(&fx.work)),
            layer: layer.clone(),
            levels: Arc::new(vec![LEVEL_FULL; 2]),
            untraced: vec![],
            seed: None,
            content: None,
        })
    }

    fn req(view: &Arc<dyn View>, kind: ActionKind, argv: Vec<Vec<u8>>, mkdirs: Vec<Vec<u8>>, no_cache: bool) -> Request {
        let mut env = BTreeMap::new();
        env.insert(b"PATH".to_vec(), b"/usr/bin:/bin".to_vec());
        Request {
            spec: ActionSpec {
                kind,
                label: String::from_utf8_lossy(&argv.concat()).into_owned(),
                argv,
                env,
                cwd: b"/w".to_vec(),
                net: NetMode::Loopback,
                mkdirs,
                root_id: Hash::ZERO,
                discard: vec![],
            },
            view: view.clone(),
            client_env: Arc::new(ClientEnv::new(Default::default(), Vec::new(), None)),
            uid: 0,
            gid: 0,
            prio: Priority::Agent,
            no_cache,
            seed_blobs: Default::default(),
            cacheable: Arc::new(|r| r.success()),
            declared_env: Arc::new(|_, _| Vec::new()),
            observer: Arc::new(NullObserver),
        }
    }

    fn link(fx: &Fx, app: &App, tests: &[&str]) -> (Arc<dyn View>, Vec<Hash>) {
        let fns = functions(app);
        let layer = Arc::new(OutputsLayer::new());
        let unit = fixture::ar(&[("libapp.app.1-cgu.0.rcgu.o", fixture::elf(&fns, true))]);
        put(fx, 0, &layer, "/w/in/libapp.rlib", &unit, false);
        let native = fixture::program(&[], false, &Run { ran: &[], exit: 0, pad: &[app.native] });
        put(fx, 0, &layer, "/w/in/libnative.a", &fixture::ar(&[("native.o", native)]), false);
        for (name, ran) in TESTS.iter().filter(|(n, _)| tests.contains(n)) {
            let bytes = fixture::program(&fns, app.sections, &Run { ran, exit: app.exit, pad: &[app.pad] });
            put(fx, 0, &layer, &format!("/w/in/{name}"), &bytes, true);
        }
        let inputs = layered(fx, &layer);
        let mut bins = Vec::new();
        for name in tests {
            let script = format!("cat /w/in/libapp.rlib /w/in/libnative.a > /w/out/{name}.read && cp /w/in/{name} /w/out/{name}");
            let r = req(&inputs, ActionKind::Rustc, vec![b"/bin/sh".to_vec(), b"-c".to_vec(), script.into_bytes()], vec![b"/w/out".to_vec()], false);
            let linked = fx.engine.obtain(r).unwrap();
            assert!(linked.result.success(), "{}", fx.text(&linked.result.stderr));
            layer.add_manifest(1, LEVEL_FULL, &linked.result.outputs);
            let Some(MEntry::File { hash, exec: true, .. }) = linked.result.outputs.get(format!("/w/out/{name}").as_bytes()) else {
                panic!("the link wrote no executable")
            };
            bins.push(*hash);
        }
        (layered(fx, &layer), bins)
    }

    fn test_req(view: &Arc<dyn View>, name: &str, nextest: bool, no_cache: bool) -> Request {
        let mut argv = vec![format!("/w/out/{name}").into_bytes()];
        if nextest {
            argv.extend([b"--exact".to_vec(), name.as_bytes().to_vec(), b"--nocapture".to_vec()]);
        }
        let mut r = req(view, ActionKind::Test, argv, vec![], no_cache);
        fn_code::mark(&mut r.spec);
        r
    }

    struct Round {
        ran: Vec<String>,
        why: Vec<String>,
    }

    fn round(fx: &Fx, view: &Arc<dyn View>, tests: &[&str], nextest: bool, no_cache: bool) -> Round {
        let mut out = Round { ran: Vec::new(), why: Vec::new() };
        for name in tests {
            let why = fx.engine.explain_request(&test_req(view, name, nextest, no_cache));
            let o = fx.engine.obtain(test_req(view, name, nextest, no_cache)).unwrap();
            assert!(o.result.success(), "test {name}: {}", fx.text(&o.result.stderr));
            if !o.cached {
                out.ran.push(name.to_string());
                out.why.push(why);
            }
        }
        out
    }

    fn table(nextest: bool) {
        sim::test(move || {
            let fx = fixture(sh);
            let abc = ["a", "b", "c"];
            let run = |app: App, tests: &[&str], no_cache: bool| {
                let (view, bins) = link(&fx, &app, tests);
                (round(&fx, &view, tests, nextest, no_cache), bins)
            };

            let (first, bins) = run(BASE, &abc, false);
            assert_eq!(first.ran, abc);
            let (again, _) = run(BASE, &abc, false);
            assert_eq!(again.ran, Vec::<String>::new());

            let (native, _) = run(App { native: 1, ..BASE }, &abc, false);
            assert_eq!(native.ran, abc, "a native object in the link inputs");
            assert!(native.why.iter().all(|w| w.contains("rest changed")), "{:?}", native.why);
            let base = App { native: 1, ..BASE };

            let (parse, _) = run(App { parse: 9, ..base }, &abc, false);
            assert_eq!(parse.ran, ["a"]);
            assert!(parse.why[0].contains("/w/out/a: function changed: _RNv3app5parse"), "{}", parse.why[0]);

            let (shared, _) = run(App { shared: 9, ..base }, &abc, false);
            assert_eq!(shared.ran, abc);
            assert!(shared.why[0].contains("/w/out/a: function changed: _RNv3app5parse"), "a's newest run had parse changed: {:?}", shared.why);
            assert!(shared.why[1..].iter().all(|w| w.contains("function changed: _RNv3app6shared")), "{:?}", shared.why);

            let (unused, relinked) = run(App { unused: 9, pad: 7, ..base }, &abc, false);
            assert!(relinked.iter().zip(&bins).all(|(a, b)| a != b), "every binary has different bytes");
            assert_eq!(unused.ran, Vec::<String>::new(), "a function no test executes, relinked");

            let (undone, _) = run(base, &abc, false);
            assert_eq!(undone.ran, Vec::<String>::new());

            let (new, _) = run(base, &["a", "b", "c", "d"], false);
            assert_eq!(new.ran, ["d"]);

            let (all, _) = run(base, &abc, true);
            assert_eq!(all.ran, abc, "--no-cache");
            assert!(all.why.iter().all(|w| w == "caching was turned off for it"));

            let flagged = App { render: None, native: 1, ..BASE };
            let (kept, _) = run(flagged, &abc, false);
            assert_eq!(kept.ran, ["b"], "b executed a function without identity");
            let (fallback, _) = run(App { pad: 3, ..flagged }, &abc, false);
            assert_eq!(fallback.ran, ["b"], "b keeps the binary read and reruns on any relink");
            assert!(fallback.why[0].contains("/w/out/b was File"), "{}", fallback.why[0]);

            let bare = App { sections: false, native: 1, ..BASE };
            let (plain, _) = run(bare, &abc, false);
            assert_eq!(plain.ran, abc);
            let (plain_relinked, _) = run(App { pad: 5, ..bare }, &abc, false);
            assert_eq!(plain_relinked.ran, abc, "a binary without sections reruns on its bytes");
        });
    }

    #[test]
    fn a_test_reruns_when_code_it_executed_or_the_binary_s_rest_changed() {
        table(false);
    }

    #[test]
    fn a_nextest_test_process_reruns_when_code_it_executed_or_the_binary_s_rest_changed() {
        table(true);
    }

    #[test]
    fn a_kept_pass_that_fails_for_real_with_no_identity_changed_is_reported_as_drift() {
        sim::test(|| {
            let fx = fixture(sh);
            let (view, _) = link(&fx, &BASE, &["c"]);
            assert_eq!(round(&fx, &view, &["c"], false, false).ran, ["c"]);
            assert_eq!(fx.engine.recheck_kept(test_req(&view, "c", false, false), 1), Kept::Holds);
            assert_eq!(fx.engine.recheck_kept(test_req(&view, "a", false, false), 1), Kept::NotKept);

            let (failing, _) = link(&fx, &App { exit: 1, ..BASE }, &["c"]);
            assert!(fx.engine.obtain(test_req(&failing, "c", false, false)).unwrap().cached, "the pass is kept");
            let Kept::Fails { functions, failure } = fx.engine.recheck_kept(test_req(&failing, "c", false, false), 1) else {
                panic!("the real run fails")
            };
            assert_eq!(functions, ["_RNv3app6shared", "_RNv5tests1c"]);
            assert_eq!(failure, "exit status: 1");
            let line = fn_code::drift_line("test c", &functions, &failure);
            assert_eq!(line, "test c: kept as a pass by the 2 function(s) it executed (_RNv3app6shared, _RNv5tests1c), but a real run fails (exit status: 1)");
            assert!(fx.engine.obtain(test_req(&failing, "c", false, false)).unwrap().cached, "a recheck records nothing");
        });
    }
}
