//! `MountOptions::auto_unmount`: a process that dies without unmounting
//! (killed outright) must not leave its mount behind, and a process that
//! unmounts normally must not leave a guard behind that could take a later
//! mount of the same directory for its own.
//!
//! Runs as whoever runs it: as root the mount is made and unmounted
//! directly, otherwise through `fusermount3`.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fusewire::{MountOptions, Session};

/// How many mounts `/proc/self/mountinfo` lists at `dir`. Does not touch
/// the directory: nothing serves these mounts, so a stat would never return.
fn mounts_at(dir: &Path) -> usize {
    let here = dir.to_string_lossy();
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .lines()
        .filter(|l| l.split(' ').nth(4) == Some(here.as_ref()))
        .count()
}

fn wait_unmounted(dir: &Path, within: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < within {
        if mounts_at(dir) == 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn opts(auto_unmount: bool) -> MountOptions {
    MountOptions { fsname: "fwtest".into(), allow_other: false, default_permissions: true, passthrough: false, auto_unmount }
}

/// Whether this environment can mount at all (a container without
/// `/dev/fuse` cannot, and there is nothing to test there).
fn can_mount() -> bool {
    let tmp = tempfile::tempdir().unwrap();
    match Session::mount(tmp.path(), &opts(false)) {
        Ok(s) => {
            s.unmount();
            true
        }
        Err(e) => {
            eprintln!("skipping: cannot mount here: {e}");
            false
        }
    }
}

fn force_unmount(dir: &Path) {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid C string.
    if unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) } != 0 {
        fusewire::unmount_with_helper(dir);
    }
}

/// Not a test of its own: the process the test below kills. Started as
/// this test binary with FUSEWIRE_HOLD set, it mounts there, says so, and
/// then does nothing until it is killed.
#[test]
fn holder() {
    let Some(dir) = std::env::var_os("FUSEWIRE_HOLD") else { return };
    let auto = std::env::var_os("FUSEWIRE_HOLD_AUTO").is_some();
    let session = Session::mount(Path::new(&dir), &opts(auto)).expect("mount");
    println!("mounted auto_unmount_active={}", session.auto_unmount_active());
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Starts a holder at `dir`, waits until it has mounted, kills it with
/// SIGKILL, and returns what it said about its guard.
fn mount_and_kill(dir: &Path, auto: bool) -> String {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "holder", "--nocapture", "--test-threads=1"]).env("FUSEWIRE_HOLD", dir);
    if auto {
        cmd.env("FUSEWIRE_HOLD_AUTO", "1");
    }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let said = loop {
        let mut line = String::new();
        assert!(out.read_line(&mut line).unwrap() > 0, "the holder exited without mounting");
        // The harness puts its own "test holder ... " in front.
        if let Some(at) = line.find("mounted ") {
            break line[at..].trim().to_string();
        }
    };
    assert_eq!(mounts_at(dir), 1, "the holder says it mounted");
    // SAFETY: our own child's pid.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
    child.wait().unwrap();
    said
}

#[test]
fn a_killed_process_leaves_no_mount_behind() {
    if !can_mount() {
        return;
    }
    // Without auto_unmount the mount outlives its process: the bug this
    // option exists for, and the proof that this test can tell.
    let plain = tempfile::tempdir().unwrap();
    mount_and_kill(plain.path(), false);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(mounts_at(plain.path()), 1, "a killed process without auto_unmount leaves its mount");
    force_unmount(plain.path());
    assert!(wait_unmounted(plain.path(), Duration::from_secs(5)));

    // With it, the directory is an ordinary directory again at once.
    let guarded = tempfile::tempdir().unwrap();
    std::fs::write(guarded.path().join("underneath"), b"x").unwrap();
    let said = mount_and_kill(guarded.path(), true);
    assert_eq!(said, "mounted auto_unmount_active=true");
    assert!(wait_unmounted(guarded.path(), Duration::from_secs(5)), "still mounted after its process was killed");
    // What was there before the mount is there again, and usable.
    assert_eq!(std::fs::read(guarded.path().join("underneath")).unwrap(), b"x");
    std::fs::write(guarded.path().join("new"), b"y").unwrap();
    assert_eq!(std::fs::read_dir(guarded.path()).unwrap().count(), 2);
}

/// Children of this process that are guards, with their command lines.
fn guards_of_this_process() -> Vec<String> {
    let me = std::process::id().to_string();
    let mut found = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // pid (comm) state ppid ...
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { continue };
        let comm = &stat[open + 1..close];
        let mut rest = stat[close + 1..].split_whitespace();
        let (state, ppid) = (rest.next(), rest.next());
        if ppid == Some(me.as_str()) && state != Some("Z") && comm == "unmount-guard" {
            let cmdline = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
            found.push(String::from_utf8_lossy(&cmdline).trim_end_matches('\0').to_string());
        }
    }
    found
}

#[test]
fn unmounting_stands_the_guard_down() {
    if !can_mount() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let first = Session::mount(tmp.path(), &opts(true)).unwrap();
    assert!(first.auto_unmount_active());
    // One guard while mounted, and not under this program's name: whoever
    // kills this program by name or command line must not kill it too.
    // (It has the room this program's own command line took, so the
    // directory may be cut short.)
    let (guards, title) = (guards_of_this_process(), format!("unmount-guard: {}", tmp.path().display()));
    assert_eq!(guards.len(), 1, "{guards:?}");
    assert!(!guards[0].is_empty() && title.starts_with(&guards[0]), "{guards:?} is not the start of {title:?}");
    first.unmount();
    assert!(!first.auto_unmount_active());
    assert!(wait_unmounted(tmp.path(), Duration::from_secs(5)));
    assert!(guards_of_this_process().is_empty(), "the guard is gone when unmount returns");

    // A second mount of the same directory while the first session object
    // is still alive: nothing of the first may unmount it, not now and not
    // when the first is dropped.
    let second = Session::mount(tmp.path(), &opts(true)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(mounts_at(tmp.path()), 1);
    drop(first);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(mounts_at(tmp.path()), 1, "dropping an unmounted session leaves a later mount alone");
    drop(second);
    assert!(wait_unmounted(tmp.path(), Duration::from_secs(5)));
    assert!(guards_of_this_process().is_empty());
}
