use rargo_core::buildview::*;
use rargo_core::engine::*;
use rargo_core::hash::Hash;
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
        fusewire::Session::mount(&mnt, &fusewire::MountOptions { fsname: "rargo-test".into(), ..Default::default() })
            .unwrap(),
    );
    fs.attach(session.clone());
    session.spawn_workers(fs.clone(), 4).unwrap();
    let zygote = Zygote::start(&sandbox_helper()).unwrap();
    let sched = Scheduler::new(8, u64::MAX).unwrap();
    let engine = Arc::new(Engine::new(store, content, fs, mnt, zygote, sched));
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    Fx { work, engine, memo: Arc::new(HostMemo::default()), session, _tmp: tmp }
}

impl Fx {
    fn view(&self) -> Arc<dyn View> {
        self.view_of(&self.work)
    }

    fn view_of(&self, work: &Path) -> Arc<dyn View> {
        let g = self.memo.advance();
        Arc::new(BaseView::new(
            vec![
                Mount { at: b"/".to_vec(), region: Region::Host(HostRegion::new("/")), traced: true },
                Mount { at: b"/w".to_vec(), region: Region::Host(HostRegion::new(work)), traced: true },
                Mount { at: test_tmp().into_os_string().into_vec(), region: Region::Empty, traced: true },
            ],
            self.memo.clone(),
            g,
        ))
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

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap()
}

#[test]
fn replays_until_an_observed_input_changes() {
    let fx = fixture();
    std::fs::write(fx.work.join("in.txt"), "one\n").unwrap();
    std::fs::write(fx.work.join("other.txt"), "x\n").unwrap();
    let script = "cat in.txt > out.txt; echo printed; echo warned >&2; test -e missing.txt || true";
    rt().block_on(async {
        let a = fx.engine.obtain(fx.req(script, &[], &[], &[])).await.unwrap();
        assert!(!a.cached && a.result.success());
        assert_eq!(fx.text(&a.result.stdout), "printed\n");
        assert_eq!(fx.text(&a.result.stderr), "warned\n");
        let Some(MEntry::File { hash, .. }) = a.result.outputs.get(b"/w/out.txt") else { panic!("no output") };
        assert_eq!(fx.text(hash), "one\n");
        assert_eq!(a.result.outputs.entries.len(), 1, "{:?}", a.result.outputs);
        assert!(!fx.work.join("out.txt").exists());
        assert_eq!(fx.runs(), 1);

        let b = fx.engine.obtain(fx.req(script, &[], &[], &[])).await.unwrap();
        assert!(b.cached);
        assert_eq!(fx.runs(), 1);

        std::fs::write(fx.work.join("other.txt"), "changed\n").unwrap();
        assert!(fx.engine.obtain(fx.req(script, &[], &[], &[])).await.unwrap().cached);

        std::fs::write(fx.work.join("in.txt"), "two\n").unwrap();
        let c = fx.engine.obtain(fx.req(script, &[], &[], &[])).await.unwrap();
        assert!(!c.cached);
        let Some(MEntry::File { hash, .. }) = c.result.outputs.get(b"/w/out.txt") else { panic!() };
        assert_eq!(fx.text(hash), "two\n");
        assert_eq!(fx.runs(), 2);

        std::fs::write(fx.work.join("missing.txt"), "now here\n").unwrap();
        assert!(!fx.engine.obtain(fx.req(script, &[], &[], &[])).await.unwrap().cached);
        assert_eq!(fx.runs(), 3);

        std::fs::remove_file(fx.work.join("missing.txt")).unwrap();
        std::fs::write(fx.work.join("in.txt"), "one\n").unwrap();
        let d = fx.engine.obtain(fx.req(script, &[], &[], &[])).await.unwrap();
        assert!(d.cached);
        assert_eq!(fx.runs(), 3);
    });
}

#[test]
fn identical_requests_in_flight_run_once() {
    let fx = fixture();
    std::fs::write(fx.work.join("in.txt"), "data\n").unwrap();
    let script = "sleep 0.5; cat in.txt";
    rt().block_on(async {
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let e = fx.engine.clone();
            let r = fx.req(script, &[], &[], &[]);
            tasks.push(tokio::spawn(async move { e.obtain(r).await.unwrap() }));
        }
        for t in tasks {
            let o = t.await.unwrap();
            assert_eq!(fx.text(&o.result.stdout), "data\n");
        }
        assert_eq!(fx.runs(), 1, "six identical requests, one run");
        assert_eq!(fx.engine.stats.joined.load(Ordering::Relaxed), 5);
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
    rt().block_on(async {
        let e = fx.engine.clone();
        let r1 = fx.req(script, &[], &[], &[]);
        let first = tokio::spawn(async move { e.obtain(r1).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let mut r2 = fx.req(script, &[], &[], &[]);
        r2.view = fx.view_of(&other);
        let second = fx.engine.obtain(r2).await.unwrap();
        let first = first.await.unwrap();
        assert_eq!(fx.text(&first.result.stdout), "first\n");
        assert_eq!(fx.text(&second.result.stdout), "second\n", "the waiter must not accept a result for other inputs");
        assert_eq!(fx.runs(), 2);
        assert_eq!(fx.engine.stats.joined.load(Ordering::Relaxed), 1);
        assert_eq!(fx.engine.stats.joined_then_ran.load(Ordering::Relaxed), 1);

        std::fs::write(fx.work.join("in.txt"), "third\n").unwrap();
        std::fs::write(other.join("in.txt"), "fourth\n").unwrap();
        let e = fx.engine.clone();
        let r1 = fx.req(script, &[], &[], &[]);
        let a = tokio::spawn(async move { e.obtain(r1).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut r2 = fx.req(script, &[], &[], &[]);
        r2.view = fx.view_of(&other);
        let b = fx.engine.obtain(r2).await.unwrap();
        let a = a.await.unwrap();
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
    rt().block_on(async {
        let a = fx.engine.obtain(fx.req(script, &[], &[("FOO", "bar")], &["FOO"])).await.unwrap();
        assert_eq!(fx.text(&a.result.stdout), "value=bar\n");
        assert_eq!(fx.runs(), 2);
        let b = fx.engine.obtain(fx.req(script, &[], &[("FOO", "bar")], &["FOO"])).await.unwrap();
        assert!(b.cached);
        assert_eq!(fx.text(&b.result.stdout), "value=bar\n");
        let c = fx.engine.obtain(fx.req(script, &[], &[("FOO", "baz")], &["FOO"])).await.unwrap();
        assert!(!c.cached);
        assert_eq!(fx.text(&c.result.stdout), "value=baz\n");
        let d = fx.engine.obtain(fx.req(script, &[], &[], &["FOO"])).await.unwrap();
        assert!(d.cached);
        assert_eq!(fx.text(&d.result.stdout), "value=\n");
        let e = fx.engine.obtain(fx.req("echo other=$BAR", &[], &[("BAR", "x")], &[])).await.unwrap();
        assert_eq!(fx.text(&e.result.stdout), "other=\n");
    });
}

#[test]
fn failures_are_not_cached_unless_allowed() {
    let fx = fixture();
    rt().block_on(async {
        let a = fx.engine.obtain(fx.req("echo oops >&2; exit 3", &[], &[], &[])).await.unwrap();
        assert_eq!(a.result.code(), Some(3));
        let b = fx.engine.obtain(fx.req("echo oops >&2; exit 3", &[], &[], &[])).await.unwrap();
        assert!(!b.cached);
        assert_eq!(fx.runs(), 2);
        let mut r = fx.req("true", &[], &[], &[]);
        r.spec.argv = vec![b"/no/such/program".to_vec()];
        let err = fx.engine.obtain(r).await.err().expect("must fail").to_string();
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
    rt().block_on(async {
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
        let o = fx.engine.obtain(r).await.unwrap();
        assert_eq!(fx.text(&o.result.stdout), "400\n400\n0\n");
    });
}
