use rargo_core::buildview::*;
use rargo_core::engine::*;
use rargo_core::hash::Hash;
use rargo_core::io;
use rargo_core::sandbox::{NetMode, Zygote};
use rargo_core::sandboxfs::SandboxFs;
use rargo_core::sched::{Priority, Scheduler};
use rargo_core::store::Store;
use rargo_core::trace::*;
use rargo_core::view::*;
use std::collections::{BTreeMap, HashMap};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct Fx {
    _tmp: tempfile::TempDir,
    work: std::path::PathBuf,
    engine: Arc<Engine>,
    memo: Arc<HostMemo>,
    fs: Arc<SandboxFs>,
    session: Arc<fusewire::Session>,
}

impl Drop for Fx {
    fn drop(&mut self) {
        self.session.unmount();
    }
}

fn sandbox_helper() -> std::path::PathBuf {
    let named = Path::new(env!("CARGO_BIN_EXE_rargo-sandbox"));
    if named.exists() {
        return named.to_path_buf();
    }
    let me = std::env::current_exe().unwrap();
    me.ancestors().map(|d| d.join("rargo-sandbox")).find(|p| p.is_file()).unwrap_or_else(|| named.to_path_buf())
}

fn test_tmp() -> PathBuf {
    std::env::var_os("RARGO_TEST_TMP").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root/rargo-work/test-tmp"))
}

fn fixture() -> Fx {
    fixture_with(std::time::Duration::from_secs(3600))
}

fn fixture_with(action_timeout: std::time::Duration) -> Fx {
    let base = test_tmp();
    let base = base.as_path();
    std::fs::create_dir_all(base).unwrap();
    let tmp = tempfile::tempdir_in(base).unwrap();
    let db = Arc::new(rargo_core::db::Db::open(&tmp.path().join("db.redb")).unwrap());
    let store = Arc::new(Store::open(&tmp.path().join("store"), db).unwrap());
    let content = Arc::new(Content::new(store.clone()));
    let fs = Arc::new(SandboxFs::new(content.clone()));
    let mnt = tmp.path().join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let session = Arc::new(
        fusewire::Session::mount_uring(&mnt, &fusewire::MountOptions { fsname: "rargo-test".into(), ..Default::default() })
            .unwrap(),
    );
    fs.attach(session.clone());
    let zygote = Zygote::start(&sandbox_helper()).unwrap();
    let sched = Scheduler::new(8, u64::MAX).unwrap();
    let mut engine = Engine::new(store, content, fs.clone(), mnt, zygote, sched);
    engine.action_timeout = action_timeout;
    let engine = Arc::new(engine);
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    Fx { work, engine, memo: Arc::new(HostMemo::default()), fs, session, _tmp: tmp }
}

impl Fx {
    fn view(&self) -> Arc<dyn View> {
        self.view_of(&self.work)
    }

    fn view_of(&self, work: &Path) -> Arc<dyn View> {
        let g = self.memo.advance();
        Arc::new(Hosted::new(BaseView::new(
            vec![
                Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/")), traced: true },
                Mount { at: b"/w".to_vec(), region: Region::Host(HostRegion::new(work)), traced: true },
                Mount { at: test_tmp().into_os_string().into_vec(), region: Region::Empty, traced: true },
            ],
            self.memo.clone(),
            g,
        )))
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
                client_env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>(),
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

#[test]
fn replays_until_an_observed_input_changes() {
    let fx = fixture();
    std::fs::write(fx.work.join("in.txt"), "one\n").unwrap();
    std::fs::write(fx.work.join("other.txt"), "x\n").unwrap();
    let script = "cat in.txt > out.txt; echo printed; echo warned >&2; test -e missing.txt || true";
    io::drive(1, move || {
        let _fuse = rargo_core::io::FuseDrive::spawn(&fx.session, &fx.fs).unwrap();
        let a = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(!a.cached && a.result.success());
        assert_eq!(fx.text(&a.result.stdout), "printed\n");
        assert_eq!(fx.text(&a.result.stderr), "warned\n");
        let Some(MEntry::File { hash, .. }) = a.result.outputs.get(b"/w/out.txt") else { panic!("no output") };
        assert_eq!(fx.text(hash), "one\n");
        assert_eq!(a.result.outputs.entries.len(), 1, "{:?}", a.result.outputs);
        assert!(!fx.work.join("out.txt").exists());
        assert_eq!(fx.runs(), 1);

        let b = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(b.cached);
        assert_eq!(fx.runs(), 1);

        std::fs::write(fx.work.join("other.txt"), "changed\n").unwrap();
        assert!(fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap().cached);

        std::fs::write(fx.work.join("in.txt"), "two\n").unwrap();
        let c = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(!c.cached);
        let Some(MEntry::File { hash, .. }) = c.result.outputs.get(b"/w/out.txt") else { panic!() };
        assert_eq!(fx.text(hash), "two\n");
        assert_eq!(fx.runs(), 2);

        std::fs::write(fx.work.join("missing.txt"), "now here\n").unwrap();
        assert!(!fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap().cached);
        assert_eq!(fx.runs(), 3);

        std::fs::remove_file(fx.work.join("missing.txt")).unwrap();
        std::fs::write(fx.work.join("in.txt"), "one\n").unwrap();
        let d = fx.engine.obtain(fx.req(script, &[], &[], &[])).unwrap();
        assert!(d.cached);
        assert_eq!(fx.runs(), 3);
    });
}

#[test]
fn waiter_runs_its_own_when_the_trace_does_not_hold() {
    let fx = fixture();
    let other = fx.work.parent().unwrap().join("work2");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(fx.work.join("in.txt"), "first\n").unwrap();
    std::fs::write(other.join("in.txt"), "second\n").unwrap();
    let script = "sleep 0.5; cat in.txt";
    io::drive(1, move || {
        let _fuse = rargo_core::io::FuseDrive::spawn(&fx.session, &fx.fs).unwrap();
        let e = fx.engine.clone();
        let r1 = fx.req(script, &[], &[], &[]);
        let mut first = io::spawn(move || e.obtain(r1).unwrap());
        io::sleep(std::time::Duration::from_millis(150));
        let mut r2 = fx.req(script, &[], &[], &[]);
        r2.view = fx.view_of(&other);
        let second = fx.engine.obtain(r2).unwrap();
        let first = first.join().unwrap();
        assert_eq!(fx.text(&first.result.stdout), "first\n");
        assert_eq!(fx.text(&second.result.stdout), "second\n", "the waiter must not accept a result for other inputs");
        assert_eq!(fx.runs(), 2);
        assert_eq!(fx.engine.stats.joined.load(Ordering::Relaxed), 1);
        assert_eq!(fx.engine.stats.joined_then_ran.load(Ordering::Relaxed), 1);

        std::fs::write(fx.work.join("in.txt"), "third\n").unwrap();
        std::fs::write(other.join("in.txt"), "fourth\n").unwrap();
        let e = fx.engine.clone();
        let r1 = fx.req(script, &[], &[], &[]);
        let mut a = io::spawn(move || e.obtain(r1).unwrap());
        io::sleep(std::time::Duration::from_millis(100));
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
    let fx = fixture();
    let script = "echo value=$FOO";
    io::drive(1, move || {
        let _fuse = rargo_core::io::FuseDrive::spawn(&fx.session, &fx.fs).unwrap();
        let a = fx.engine.obtain(fx.req(script, &[], &[("FOO", "bar")], &["FOO"])).unwrap();
        assert_eq!(fx.text(&a.result.stdout), "value=bar\n");
        assert_eq!(fx.runs(), 2);
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
    let fx = fixture();
    io::drive(1, move || {
        let _fuse = rargo_core::io::FuseDrive::spawn(&fx.session, &fx.fs).unwrap();
        let a = fx.engine.obtain(fx.req("echo oops >&2; exit 3", &[], &[], &[])).unwrap();
        assert_eq!(a.result.code(), Some(3));
        let b = fx.engine.obtain(fx.req("echo oops >&2; exit 3", &[], &[], &[])).unwrap();
        assert!(!b.cached);
        assert_eq!(fx.runs(), 2);
        let mut r = fx.req("true", &[], &[], &[]);
        r.spec.argv = vec![b"/no/such/program".to_vec()];
        let err = fx.engine.obtain(r).err().expect("must fail").to_string();
        assert!(err.contains("could not start"), "{err}");
    });
}

#[test]
fn large_directories_list_completely_through_a_build_view() {
    let fx = fixture();
    let big = fx.work.join("big");
    std::fs::create_dir_all(&big).unwrap();
    for i in 0..400 {
        std::fs::write(big.join(format!("libsomething-with-a-long-name-{i:04}-5cc9b44b3cc93987.rmeta")), "x").unwrap();
    }
    io::drive(1, move || {
        let _fuse = rargo_core::io::FuseDrive::spawn(&fx.session, &fx.fs).unwrap();
        let mut r = fx.req("ls /w/big | wc -l; ls -la /w/big | grep -c rmeta; ls /w/big | sort | uniq -d | wc -l", &[], &[], &[]);
        let base = {
            let g = fx.memo.advance();
            Arc::new(BaseView::new(
                vec![
                    Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/")), traced: true },
                    Mount { at: b"/w".to_vec(), region: Region::Host(HostRegion::new(&fx.work)), traced: true },
                    Mount { at: test_tmp().into_os_string().into_vec(), region: Region::Empty, traced: true },
                ],
                fx.memo.clone(),
                g,
            ))
        };
        r.view = Arc::new(BuildView {
            base,
            layer: Arc::new(OutputsLayer::new()),
            levels: Arc::new(vec![]),
            untraced: vec![],
            seed: None,
            content: None,
        });
        let o = fx.engine.obtain(r).unwrap();
        assert_eq!(fx.text(&o.result.stdout), "400\n400\n0\n");
    });
}

#[test]
fn a_hung_action_is_stopped_and_its_waiters_fail_with_it() {
    let fx = fixture_with(std::time::Duration::from_secs(1));
    io::drive(1, move || {
        let _fuse = rargo_core::io::FuseDrive::spawn(&fx.session, &fx.fs).unwrap();
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let e = fx.engine.clone();
            let r = fx.req("sleep 100000", &[], &[], &[]);
            tasks.push(io::spawn(move || e.obtain(r)));
        }
        for t in &mut tasks {
            let e = t.join().unwrap().err().expect("a hung action fails");
            assert!(e.to_string().contains("ran longer than 1 s"), "{e:#}");
        }
        assert_eq!(fx.runs(), 1, "three identical requests, one run, and no rerun by the waiters");
        let ok = fx.engine.obtain(fx.req("echo fine", &[], &[], &[])).unwrap();
        assert_eq!(fx.text(&ok.result.stdout), "fine\n");
    });
}

mod fn_code_selection {
    use super::*;
    use rargo_core::fn_code::{
        self,
        fixture::{self, Function, Run},
    };

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
        let g = fx.memo.advance();
        let base = Arc::new(BaseView::new(
            vec![
                Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/")), traced: true },
                Mount { at: b"/w".to_vec(), region: Region::Host(HostRegion::new(&fx.work)), traced: true },
                Mount { at: test_tmp().into_os_string().into_vec(), region: Region::Empty, traced: true },
            ],
            fx.memo.clone(),
            g,
        ));
        Arc::new(BuildView { base, layer: layer.clone(), levels: Arc::new(vec![LEVEL_FULL; 2]), untraced: vec![], seed: None, content: None })
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
            client_env: Arc::new(ClientEnv::new(HashMap::new(), Vec::new(), None)),
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
        let fx = fixture();
        io::drive(1, move || {
            let _fuse = rargo_core::io::FuseDrive::spawn(&fx.session, &fx.fs).unwrap();
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
        let fx = fixture();
        io::drive(1, move || {
            let _fuse = rargo_core::io::FuseDrive::spawn(&fx.session, &fx.fs).unwrap();
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
