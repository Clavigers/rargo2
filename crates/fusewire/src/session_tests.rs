use super::request_tests::{unhex, RECORDED};
use super::*;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};

const DEVICE: RawFd = 7;
const HANDED_OVER: RawFd = 8;
const UNDERNEATH: u64 = 1;
const STAT: &str = "4242 (rargo (d) x) S 1 4242 4242 0 -1 4194304 100 0 0 0 1 2 0 0 20 0 3 0 5000 1000000 200 \
    18446744073709551615 4096 8192 140000000 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0 12288 16384 20480 140001000 140001064 \
    140002000 140002100 0";

struct Device(RawFd);

impl AsRawFd for Device {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// A kernel in memory. It records what a session asks of it in `log`, keeps
/// the mount at each path in `mounts`, and answers reads of the device from
/// `requests`.
struct Fake {
    log: RefCell<Vec<String>>,
    mounts: RefCell<BTreeMap<Vec<u8>, u64>>,
    next_mount: Cell<u64>,
    names_mounts: Cell<bool>,
    directory: Cell<bool>,
    mount_errno: Cell<Option<i32>>,
    helper_stderr: RefCell<Option<String>>,
    helper_hands_over: Cell<bool>,
    stat: RefCell<Option<String>>,
    guard_starts: Cell<bool>,
    plans: RefCell<Vec<GuardPlan>>,
    requests: RefCell<VecDeque<Result<Vec<u8>, i32>>>,
    written: RefCell<Vec<Vec<u8>>>,
    write_errno: Cell<Option<i32>>,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            log: RefCell::default(),
            mounts: RefCell::default(),
            next_mount: Cell::new(100),
            names_mounts: Cell::new(true),
            directory: Cell::new(true),
            mount_errno: Cell::new(None),
            helper_stderr: RefCell::new(None),
            helper_hands_over: Cell::new(true),
            stat: RefCell::new(Some(STAT.to_string())),
            guard_starts: Cell::new(true),
            plans: RefCell::default(),
            requests: RefCell::new(VecDeque::from([Ok(init())])),
            written: RefCell::default(),
            write_errno: Cell::new(None),
        }
    }

    fn said(&self, line: String) {
        self.log.borrow_mut().push(line);
    }

    fn took_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.borrow_mut())
    }

    fn put_mount(&self, path: &[u8]) -> u64 {
        let id = self.next_mount.replace(self.next_mount.get() + 1);
        self.mounts.borrow_mut().insert(path.to_vec(), id);
        id
    }

    fn mounted(&self, path: &str) -> Option<u64> {
        self.mounts.borrow().get(path.as_bytes()).copied()
    }

    fn next_request(&self, request: Vec<u8>) {
        self.requests.borrow_mut().push_back(Ok(request));
    }
}

fn words(args: &[OsString]) -> String {
    args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ")
}

impl Kernel for &Fake {
    type Device = Device;
    type Socket = ();
    type Guard = usize;

    fn open_device(&self) -> std::io::Result<Device> {
        self.said("open".into());
        Ok(Device(DEVICE))
    }

    fn is_dir(&self, _: &Path) -> std::io::Result<bool> {
        Ok(self.directory.get())
    }

    fn euid(&self) -> u32 {
        1000
    }

    fn egid(&self) -> u32 {
        100
    }

    fn mount(&self, source: &CStr, target: &CStr, fstype: &CStr, flags: libc::c_ulong, data: &CStr) -> std::io::Result<()> {
        self.said(format!("mount {source:?} {target:?} {fstype:?} {flags:#x} {data:?}"));
        if let Some(errno) = self.mount_errno.get() {
            return Err(std::io::Error::from_raw_os_error(errno));
        }
        self.put_mount(target.to_bytes());
        Ok(())
    }

    fn run_with_socket(&self, program: &str, args: &[OsString], fd_env: &str) -> std::io::Result<Helped<()>> {
        self.said(format!("run {program} {} with {fd_env}", words(args)));
        match &*self.helper_stderr.borrow() {
            Some(stderr) => Ok(Helped { success: false, stderr: stderr.clone().into_bytes(), socket: () }),
            None => {
                self.put_mount(args.last().unwrap().as_encoded_bytes());
                Ok(Helped { success: true, stderr: Vec::new(), socket: () })
            }
        }
    }

    fn receive_device(&self, _: ()) -> std::io::Result<Option<Device>> {
        self.said("receive".into());
        Ok(self.helper_hands_over.get().then_some(Device(HANDED_OVER)))
    }

    fn run_quiet(&self, program: &str, args: &[OsString]) {
        self.said(format!("run {program} {}", words(args)));
        self.mounts.borrow_mut().remove(args.last().unwrap().as_encoded_bytes());
    }

    fn statx_mount_id(&self, path: &CStr, mask: u32) -> Option<(u32, u64)> {
        assert_eq!(mask, 0x5000, "asks for the mount id and the unique mount id");
        let id = self.mounts.borrow().get(path.to_bytes()).copied().unwrap_or(UNDERNEATH);
        Some((if self.names_mounts.get() { 0x4000 } else { 0x7ff }, id))
    }

    fn read_to_string(&self, path: &str) -> std::io::Result<String> {
        assert_eq!(path, "/proc/self/stat");
        self.stat.borrow().clone().ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn spawn_guard(&self, plan: &GuardPlan) -> std::io::Result<usize> {
        if !self.guard_starts.get() {
            return Err(std::io::Error::from_raw_os_error(libc::EAGAIN));
        }
        self.plans.borrow_mut().push(plan.clone());
        self.said(format!("guard {}", self.plans.borrow().len()));
        Ok(self.plans.borrow().len())
    }

    fn stand_down(&self, guard: usize) {
        self.said(format!("stand down {guard}"));
    }

    fn detach(&self, target: &CStr) {
        self.said(format!("detach {target:?}"));
        self.mounts.borrow_mut().remove(target.to_bytes());
    }

    fn read(&self, device: &Device, buf: &mut [u8]) -> std::io::Result<usize> {
        assert_eq!(buf.len(), BUF_SIZE, "a read offers the kernel a whole request buffer");
        self.said(format!("read {}", device.0));
        match self.requests.borrow_mut().pop_front().expect("a request to read") {
            Ok(request) => {
                buf[..request.len()].copy_from_slice(&request);
                Ok(request.len())
            }
            Err(errno) => Err(std::io::Error::from_raw_os_error(errno)),
        }
    }

    fn write(&self, device: &Device, bytes: &[u8]) -> std::io::Result<usize> {
        self.said(format!("write {}", device.0));
        if let Some(errno) = self.write_errno.get() {
            return Err(std::io::Error::from_raw_os_error(errno));
        }
        self.written.borrow_mut().push(bytes.to_vec());
        Ok(bytes.len())
    }

    fn backing_open(&self, device: &Device, fd: RawFd) -> std::io::Result<i32> {
        self.said(format!("backing open {fd} on {}", device.0));
        if fd < 0 { Err(std::io::Error::from_raw_os_error(libc::EBADF)) } else { Ok(fd + 1) }
    }

    fn backing_close(&self, device: &Device, id: u32) -> std::io::Result<i32> {
        self.said(format!("backing close {id} on {}", device.0));
        Ok(0)
    }
}

/// The INIT request recorded from a kernel, which offers passthrough and
/// does not offer io_uring.
fn init() -> Vec<u8> {
    let (name, hex) = RECORDED[0];
    assert_eq!(name, "INIT");
    unhex(hex)
}

const HEADER: usize = std::mem::size_of::<fuse_in_header>();
const OUT: usize = std::mem::size_of::<fuse_out_header>();

fn init_with(change: impl FnOnce(&mut fuse_init_in)) -> Vec<u8> {
    let request = init();
    let mut body: fuse_init_in = read_struct(&request[HEADER..]).unwrap();
    change(&mut body);
    let mut changed = request[..HEADER].to_vec();
    changed.extend_from_slice(as_bytes(&body));
    changed.extend_from_slice(&request[changed.len()..]);
    changed
}

fn offering_uring() -> Vec<u8> {
    init_with(|init| init.flags2 |= (FUSE_OVER_IO_URING >> 32) as u32)
}

fn opts(auto_unmount: bool) -> MountOptions {
    MountOptions { fsname: "fwtest".into(), allow_other: false, default_permissions: true, passthrough: false, auto_unmount }
}

fn mount<'a>(fake: &'a Fake, at: &str, auto_unmount: bool) -> Session<&'a Fake> {
    Session::mount_on(fake, Path::new(at), &opts(auto_unmount), false).expect("mount")
}

fn failure(mounted: std::io::Result<Session<&Fake>>) -> std::io::Error {
    match mounted {
        Ok(_) => panic!("the mount succeeded"),
        Err(e) => e,
    }
}

fn init_out(reply: &[u8]) -> (fuse_out_header, fuse_init_out) {
    (read_struct(reply).unwrap(), read_struct(&reply[OUT..]).unwrap())
}

#[test]
fn the_reply_to_a_recorded_init_takes_only_what_the_kernel_offers() {
    let request = init();
    let asked: fuse_init_in = read_struct(&request[HEADER..]).unwrap();
    let offered = asked.flags as u64 | (asked.flags2 as u64) << 32;
    assert!(offered & FUSE_PASSTHROUGH != 0 && offered & FUSE_OVER_IO_URING == 0);

    let (reply, caps) = init_reply(&request, true, false).unwrap();
    let (header, out) = init_out(&reply);
    assert_eq!((header.len as usize, header.error, header.unique), (reply.len(), 0, 2));
    assert_eq!(reply.len(), OUT + std::mem::size_of::<fuse_init_out>());
    assert_eq!((out.major, out.minor), (FUSE_KERNEL_VERSION, FUSE_KERNEL_MINOR_VERSION.min(asked.minor)));
    assert_eq!((out.max_write, out.max_pages, out.max_readahead), (MAX_WRITE, 256, asked.max_readahead));
    let taken = out.flags as u64 | (out.flags2 as u64) << 32;
    assert_eq!(taken & !offered, 0);
    assert!(taken & FUSE_PASSTHROUGH != 0 && taken & FUSE_WRITEBACK_CACHE == 0 && taken & FUSE_INIT_EXT != 0);
    assert_eq!(out.max_stack_depth, 2);
    assert!(caps.passthrough);
    assert_eq!((caps.minor, caps.max_write), (out.minor, MAX_WRITE));

    let (reply, caps) = init_reply(&request, false, false).unwrap();
    let (_, out) = init_out(&reply);
    let taken = out.flags as u64 | (out.flags2 as u64) << 32;
    assert!(taken & FUSE_PASSTHROUGH == 0 && taken & FUSE_WRITEBACK_CACHE != 0, "without passthrough, writes are cached");
    assert_eq!(out.max_stack_depth, 0);
    assert!(!caps.passthrough);

    let unoffered = init_with(|init| init.flags2 &= !((FUSE_PASSTHROUGH >> 32) as u32));
    let (_, caps) = init_reply(&unoffered, true, false).unwrap();
    assert!(!caps.passthrough, "passthrough the kernel does not offer is not taken");
}

#[test]
fn an_init_that_cannot_be_served_is_refused_with_the_reason() {
    let refused = |request: &[u8], uring| init_reply(request, true, uring).unwrap_err().to_string();
    assert_eq!(refused(&init()[..HEADER - 1], false), "short INIT");
    assert_eq!(refused(&unhex(RECORDED[1].1), false), "first FUSE request was not INIT");
    assert_eq!(refused(&init()[..HEADER + 8], false), "short INIT body");
    assert_eq!(refused(&init_with(|init| init.minor = 30), false), "unsupported FUSE protocol 7.30");
    assert_eq!(refused(&init_with(|init| init.major = 8), false), "unsupported FUSE protocol 8.45");

    let unavailable = init_reply(&init(), true, true).unwrap_err();
    assert_eq!(unavailable.kind(), std::io::ErrorKind::Unsupported);
    assert!(unavailable.to_string().contains("fuse.enable_uring=Y"));
    assert!(refused(&init()[..HEADER + 8], true).contains("fuse.enable_uring=Y"), "a short body offers nothing");

    let (reply, _) = init_reply(&offering_uring(), true, true).unwrap();
    let (_, out) = init_out(&reply);
    assert!((out.flags2 as u64) << 32 & FUSE_OVER_IO_URING != 0);
    let (reply, _) = init_reply(&offering_uring(), true, false).unwrap();
    let (_, out) = init_out(&reply);
    assert!((out.flags2 as u64) << 32 & FUSE_OVER_IO_URING == 0, "io_uring is taken only when asked for");
}

#[test]
fn mounting_asks_the_kernel_directly_and_answers_its_init() {
    let fake = Fake::new();
    let session = mount(&fake, "/mnt/a", false);
    assert_eq!(
        fake.took_log(),
        [
            "open",
            "mount \"fwtest\" \"/mnt/a\" \"fuse.fwtest\" 0x406 \"fd=7,rootmode=40000,user_id=1000,group_id=100,default_permissions\"",
            "read 7",
            "write 7",
        ]
    );
    assert_eq!(*fake.written.borrow(), [init_reply(&init(), false, false).unwrap().0]);
    assert_eq!((session.device_fd(), session.mountpoint()), (DEVICE, Path::new("/mnt/a")));
    assert_eq!((session.caps().max_write, session.caps().passthrough), (MAX_WRITE, false));
    assert!(!session.auto_unmount_active(), "no guard is started unless asked for");
    assert!(fake.plans.borrow().is_empty());
    assert!(session.shown());

    let all = MountOptions { fsname: "rargo".into(), ..MountOptions::default() };
    assert_eq!(mount_data(3, 0, 0, &all), "fd=3,rootmode=40000,user_id=0,group_id=0,allow_other,default_permissions");
    assert_eq!(
        words(&helper_mount_args(Path::new("/mnt/b"), &all)),
        "-o rw,nosuid,nodev,noatime,fsname=rargo,subtype=rargo,default_permissions,allow_other -- /mnt/b"
    );
}

#[test]
fn mounting_retries_an_interrupted_read_of_the_init() {
    let fake = Fake::new();
    fake.requests.borrow_mut().push_front(Err(libc::EINTR));
    let session = mount(&fake, "/mnt/a", false);
    assert_eq!(fake.took_log()[2..], ["read 7", "read 7", "write 7"]);
    drop(session);
}

#[test]
fn a_mount_the_kernel_forbids_goes_through_the_helper() {
    let fake = Fake::new();
    fake.mount_errno.set(Some(libc::EPERM));
    let session = mount(&fake, "/mnt/a", true);
    let log = fake.took_log();
    assert_eq!(
        log[2..],
        [
            "run fusermount3 -o rw,nosuid,nodev,noatime,fsname=fwtest,subtype=fwtest,default_permissions -- /mnt/a with _FUSE_COMMFD",
            "receive",
            "guard 1",
            "read 8",
            "write 8",
        ]
    );
    assert_eq!(session.device_fd(), HANDED_OVER, "the session serves the connection the helper handed over");
    let plan = fake.plans.borrow()[0].clone();
    let command: Vec<&str> = plan.helper.as_ref().unwrap().iter().map(|word| word.to_str().unwrap()).collect();
    assert_eq!(command, ["fusermount3", "-u", "-z", "-q", "--", "/mnt/a"], "the guard unmounts the way the mount was made");

    session.unmount();
    assert_eq!(fake.took_log(), ["stand down 1", "run fusermount3 -u -z -q -- /mnt/a"]);
    assert_eq!(fake.mounted("/mnt/a"), None);
}

#[test]
fn a_mount_that_fails_says_why() {
    let fake = Fake::new();
    fake.mount_errno.set(Some(libc::ENODEV));
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/a"), &opts(true), false));
    assert_eq!(e.raw_os_error(), Some(libc::ENODEV));
    assert_eq!(fake.took_log().len(), 2, "an error other than a refusal is not retried through the helper");

    fake.mount_errno.set(Some(libc::EPERM));
    *fake.helper_stderr.borrow_mut() = Some("fusermount3: user has no write access to mountpoint /mnt/a\n".into());
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/a"), &opts(true), false));
    let direct = std::io::Error::from_raw_os_error(libc::EPERM);
    assert_eq!(
        e.to_string(),
        format!("mount: {direct}; fusermount3: fusermount3: user has no write access to mountpoint /mnt/a")
    );
    assert!(!fake.took_log().contains(&"receive".to_string()), "a helper that failed handed nothing over");

    *fake.helper_stderr.borrow_mut() = None;
    fake.helper_hands_over.set(false);
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/a"), &opts(true), false));
    assert!(e.to_string().ends_with("fusermount3: fusermount3 did not hand over the connection"), "{e}");

    fake.directory.set(false);
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/file"), &opts(true), false));
    assert_eq!((e.kind(), e.to_string().as_str()), (std::io::ErrorKind::InvalidInput, "mountpoint is not a directory"));
    assert!(fake.plans.borrow().is_empty(), "no guard is started for a mount that was not made");
}

#[test]
fn a_failed_handshake_unmounts_what_was_mounted() {
    let fake = Fake::new();
    *fake.requests.borrow_mut() = VecDeque::from([Ok(unhex(RECORDED[1].1))]);
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/a"), &opts(true), false));
    assert_eq!(e.to_string(), "first FUSE request was not INIT");
    assert_eq!(fake.took_log()[2..], ["guard 1", "read 7", "stand down 1", "detach \"/mnt/a\""]);
    assert_eq!(fake.mounted("/mnt/a"), None);

    fake.next_request(init());
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/a"), &opts(false), true));
    assert_eq!(e.kind(), std::io::ErrorKind::Unsupported, "io_uring was asked of a kernel that does not offer it");
    assert_eq!(fake.took_log()[2..], ["read 7", "detach \"/mnt/a\""]);

    fake.next_request(vec![0; 4]);
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/a"), &opts(false), false));
    assert_eq!(e.to_string(), "short INIT");
    fake.took_log();

    fake.requests.borrow_mut().push_back(Err(libc::ENODEV));
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/a"), &opts(false), false));
    assert_eq!(e.raw_os_error(), Some(libc::ENODEV), "a read error other than an interruption ends the mount");
    fake.took_log();

    fake.next_request(init());
    fake.write_errno.set(Some(libc::EIO));
    let e = failure(Session::mount_on(&fake, Path::new("/mnt/a"), &opts(false), false));
    assert_eq!(e.raw_os_error(), Some(libc::EIO));
    assert_eq!(fake.took_log()[2..], ["read 7", "write 7", "detach \"/mnt/a\""]);

    fake.write_errno.set(None);
    fake.next_request(offering_uring());
    let session = Session::mount_on(&fake, Path::new("/mnt/a"), &opts(false), true).expect("mount over io_uring");
    assert!(session.shown());
}

#[test]
fn the_guard_is_told_what_to_unmount_and_how_to_show_itself() {
    let fake = Fake::new();
    let session = mount(&fake, "/mnt/guarded", true);
    assert!(session.auto_unmount_active());
    let mine = fake.mounted("/mnt/guarded");
    assert!(mine.is_some());
    let plans = fake.plans.borrow().clone();
    assert_eq!(
        plans,
        [GuardPlan {
            name: c"unmount-guard".to_owned(),
            title: b"unmount-guard: /mnt/guarded".to_vec(),
            title_area: Some((140001000, 140001064)),
            ignored_signals: vec![libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGPIPE],
            target: c"/mnt/guarded".to_owned(),
            mount_id: mine,
            statx_mask: 0x5000,
            helper: None,
        }]
    );

    let plan = &plans[0];
    let there = |fake: &Fake| mount_id_in((&fake).statx_mount_id(&plan.target, plan.statx_mask));
    assert_eq!(
        unmount_action(plan.mount_id, there(&fake), plan.helper.is_some()),
        Unmount::Detach,
        "a guard whose process died unmounts its mount"
    );
    (&fake).detach(&plan.target);
    assert_eq!(
        unmount_action(plan.mount_id, there(&fake), false),
        Unmount::Leave,
        "a guard whose mount is already gone leaves the directory underneath alone"
    );
    fake.put_mount(b"/mnt/guarded");
    assert_eq!(
        unmount_action(plan.mount_id, there(&fake), false),
        Unmount::Leave,
        "a guard leaves alone a mount made there after its own went"
    );
}

#[test]
fn the_unmount_decision_follows_the_mount_ids() {
    assert_eq!(unmount_action(Some(5), Some(5), false), Unmount::Detach);
    assert_eq!(unmount_action(Some(5), Some(5), true), Unmount::Helper);
    assert_eq!(unmount_action(Some(5), Some(6), false), Unmount::Leave);
    assert_eq!(unmount_action(Some(5), Some(6), true), Unmount::Leave);
    for helper in [false, true] {
        let acts = if helper { Unmount::Helper } else { Unmount::Detach };
        assert_eq!(unmount_action(None, Some(6), helper), acts, "a mount that was never named is unmounted by path");
        assert_eq!(unmount_action(Some(5), None, helper), acts);
        assert_eq!(unmount_action(None, None, helper), acts);
    }
    assert_eq!(mount_id_in(None), None);
    assert_eq!(mount_id_in(Some((0x7ff, 9))), None, "a kernel that does not fill in a mount id names no mount");
    assert_eq!(mount_id_in(Some((0x1000, 9))), Some(9));
    assert_eq!(mount_id_in(Some((0x4000, 9))), Some(9));
}

#[test]
fn the_guard_title_goes_where_the_command_line_is() {
    assert_eq!(arg_area(STAT), Some((140001000, 140001064)));
    assert_eq!(arg_area("1 (a) b) S 1"), None, "a stat line without the fields");
    assert_eq!(arg_area("no command name"), None);
    let zeroed = STAT.replace("140001000 140001064", "0 0");
    assert_eq!(arg_area(&zeroed), None, "a process whose command line is not mapped");
    let backwards = STAT.replace("140001000 140001064", "140001064 140001000");
    assert_eq!(arg_area(&backwards), None);

    let fake = Fake::new();
    *fake.stat.borrow_mut() = None;
    let session = mount(&fake, "/mnt/a", true);
    assert!(session.auto_unmount_active(), "the guard runs without a title when the command line cannot be found");
    assert_eq!(fake.plans.borrow()[0].title_area, None);
}

#[test]
fn a_guard_that_cannot_start_leaves_the_mount_unguarded() {
    let fake = Fake::new();
    fake.guard_starts.set(false);
    let session = mount(&fake, "/mnt/a", true);
    assert!(!session.auto_unmount_active());
    assert!(session.shown());
    fake.took_log();
    session.unmount();
    assert_eq!(fake.took_log(), ["detach \"/mnt/a\""]);
}

#[test]
fn unmounting_stands_the_guard_down() {
    let fake = Fake::new();
    let first = mount(&fake, "/mnt/a", true);
    assert!(first.auto_unmount_active() && first.shown());
    fake.took_log();
    first.unmount();
    assert_eq!(fake.took_log(), ["stand down 1", "detach \"/mnt/a\""], "the guard is gone before the mount is");
    assert!(!first.auto_unmount_active());
    assert!(!first.shown());
    assert_eq!(fake.mounted("/mnt/a"), None);
    first.unmount();
    assert!(fake.took_log().is_empty(), "a second unmount does nothing");

    fake.next_request(init());
    let second = mount(&fake, "/mnt/a", true);
    let there = fake.mounted("/mnt/a");
    fake.took_log();
    drop(first);
    assert!(fake.took_log().is_empty(), "dropping an unmounted session leaves a later mount alone");
    assert_eq!(fake.mounted("/mnt/a"), there);
    assert!(second.shown());
    drop(second);
    assert_eq!(fake.took_log(), ["stand down 2", "detach \"/mnt/a\""]);
    assert_eq!(fake.mounted("/mnt/a"), None);
}

#[test]
fn a_session_whose_mount_was_replaced_leaves_the_new_one() {
    let fake = Fake::new();
    let old = mount(&fake, "/mnt/a", true);
    (&fake).detach(c"/mnt/a");
    assert!(!old.shown(), "a session whose mount is gone is not shown");
    fake.next_request(init());
    let new = mount(&fake, "/mnt/a", false);
    assert!(new.shown() && !old.shown());
    let there = fake.mounted("/mnt/a");
    fake.took_log();
    drop(old);
    assert_eq!(fake.took_log(), ["stand down 1"], "the old session stands its guard down and unmounts nothing");
    assert_eq!(fake.mounted("/mnt/a"), there);
    drop(new);
    assert_eq!(fake.mounted("/mnt/a"), None);
}

#[test]
fn a_kernel_that_names_no_mounts_is_unmounted_by_path() {
    let fake = Fake::new();
    fake.names_mounts.set(false);
    let session = mount(&fake, "/mnt/a", false);
    assert!(session.shown());
    session.ended();
    assert!(!session.shown(), "a session whose connection ended is not shown");
    fake.took_log();
    drop(session);
    assert_eq!(fake.took_log(), ["detach \"/mnt/a\""]);
}

#[test]
fn effects_are_carried_out_on_the_device() {
    let fake = Fake::new();
    let session = mount(&fake, "/mnt/a", false);
    fake.took_log();
    fake.written.borrow_mut().clear();
    assert_eq!(session.backing_open(41).unwrap(), 42);
    assert_eq!(session.backing_open(-1).unwrap_err().raw_os_error(), Some(libc::EBADF));
    session.backing_close(42).unwrap();
    session.notify_inval_entry(1, b"file").unwrap();
    assert_eq!(fake.took_log(), ["backing open 41 on 7", "backing open -1 on 7", "backing close 42 on 7", "write 7"]);
    assert_eq!(*fake.written.borrow(), [inval_entry_bytes(1, b"file")]);

    fake.write_errno.set(Some(libc::ENOENT));
    assert!(session.notify_inval_entry(1, b"file").is_ok(), "the kernel not knowing the entry is not an error");
    fake.write_errno.set(Some(libc::EBADF));
    assert_eq!(session.notify_inval_entry(1, b"file").unwrap_err().raw_os_error(), Some(libc::EBADF));
}

/// A filesystem with one passthrough file, enough to drive a session's
/// request loop.
struct OneFile;

impl Filesystem for OneFile {
    fn lookup(&self, _: &Ctx, _: u64, _: &[u8]) -> Res<Entry> {
        Err(Errno::ENOENT)
    }

    fn getattr(&self, _: &Ctx, _: u64, _: Option<u64>) -> Res<(fuse_attr, Duration)> {
        Err(Errno::ENOENT)
    }

    fn open(&self, ctx: &Ctx, _: u64, _: u32) -> Res<Opened> {
        ctx.effects.register(41, 9);
        Ok(Opened { fh: 9, flags: FOPEN_PASSTHROUGH, backing_id: 0 })
    }

    fn registered(&self, _: &Ctx, token: u64, id: Res<i32>) -> Res<Opened> {
        Ok(Opened { fh: token, flags: FOPEN_PASSTHROUGH, backing_id: id? })
    }

    fn read(&self, _: &Ctx, _: u64, _: u64, _: u64, _: u32, _: &mut Vec<u8>) -> Res<()> {
        Err(Errno::EIO)
    }

    fn readdir(&self, _: &Ctx, _: u64, _: u64, _: u64, _: &mut DirSink<'_>) -> Res<()> {
        Err(Errno::EIO)
    }
}

fn frame(opcode: u32, unique: u64, body: &[u8]) -> Vec<u8> {
    let header = fuse_in_header {
        len: (HEADER + body.len()) as u32,
        opcode,
        unique,
        nodeid: 2,
        uid: 1000,
        gid: 100,
        pid: 1,
        total_extlen: 0,
        padding: 0,
    };
    [as_bytes(&header), body].concat()
}

#[test]
fn serving_registers_a_passthrough_open_before_answering_it_and_stops_at_destroy() {
    let fake = Fake::new();
    let session = mount(&fake, "/mnt/a", false);
    fake.took_log();
    let open = frame(FUSE_OPEN, 4, as_bytes(&fuse_open_in { flags: 0, open_flags: 0 }));
    let requests = RefCell::new(VecDeque::from([
        Err(libc::EINTR),
        Ok(open),
        Err(libc::ENOENT),
        Ok(frame(FUSE_DESTROY, 6, &[])),
        Ok(frame(FUSE_STATFS, 8, &[])),
    ]));
    let replies = RefCell::new(Vec::new());
    session.serve(
        &OneFile,
        |buf| match requests.borrow_mut().pop_front().expect("a request") {
            Ok(request) => {
                buf[..request.len()].copy_from_slice(&request);
                Ok(request.len())
            }
            Err(errno) => Err(std::io::Error::from_raw_os_error(errno)),
        },
        |bytes| {
            replies.borrow_mut().push(bytes.to_vec());
            Ok(())
        },
    );
    assert_eq!(fake.took_log(), ["backing open 41 on 7"]);
    let replies = replies.into_inner();
    assert_eq!(replies.len(), 2);
    let opened: fuse_open_out = read_struct(&replies[0][OUT..]).unwrap();
    assert_eq!((opened.fh, opened.open_flags, opened.backing_id), (9, FOPEN_PASSTHROUGH, 42));
    let destroyed: fuse_out_header = read_struct(&replies[1]).unwrap();
    assert_eq!((destroyed.unique, destroyed.error), (6, 0));
    assert_eq!(requests.borrow().len(), 1, "nothing is read after the kernel destroys the session");
    assert!(!session.shown());
}

#[test]
fn serving_ends_when_the_device_does() {
    for end in [Ok(0), Err(libc::ENODEV)] {
        let fake = Fake::new();
        let session = mount(&fake, "/mnt/a", false);
        let mut reads = 0;
        session.serve(
            &OneFile,
            |_| {
                reads += 1;
                end.map_err(std::io::Error::from_raw_os_error)
            },
            |_| panic!("nothing to answer"),
        );
        assert_eq!(reads, 1);
        assert!(!session.shown());
    }

    let fake = Fake::new();
    let session = mount(&fake, "/mnt/a", false);
    let mut handler = session.handler();
    let statfs = frame(FUSE_STATFS, 8, &[]);
    assert!(session.respond(&mut handler, &OneFile, &statfs, |_| Ok(())));
    assert!(session.shown());
    assert!(!session.respond(&mut handler, &OneFile, &statfs, |_| Err(std::io::Error::from_raw_os_error(libc::ENODEV))));
    assert!(!session.shown(), "a reply the device refuses ends the session");
}
