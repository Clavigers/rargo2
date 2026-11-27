use super::*;

fn found(file: &str, text: &str) -> Vec<(Check, String)> {
    scan(&[(file, text)]).into_iter().map(|f| (f.check, f.what)).collect()
}

fn of(check: Check, list: &[(Check, String)]) -> Vec<String> {
    let mut out: Vec<String> = list.iter().filter(|(c, _)| *c == check).map(|(_, w)| w.clone()).collect();
    out.sort();
    out
}

pub(super) fn workspace_purity(files: &[&str]) {
    let sources: Vec<_> = SOURCES.iter().copied().filter(|(file, _)| files.contains(file)).collect();
    let mut allow = parse_allow(ALLOW).unwrap_or_else(|e| panic!("{e}"));
    allow.retain(|e| files.contains(&e.file.as_str()));
    let findings = scan(&sources);
    let reports: Vec<String> = Check::ALL.iter().filter_map(|c| compare(*c, &findings, &allow).err()).collect();
    assert!(reports.is_empty(), "{}", reports.join("\n"));
}

#[test]
fn every_allowlist_entry_names_a_source_file_once_with_a_count_and_a_reason() {
    let allow = parse_allow(ALLOW).unwrap();
    let files: BTreeSet<&str> = SOURCES.iter().map(|(f, _)| *f).collect();
    let mut seen = BTreeSet::new();
    let mut last = None;
    for e in &allow {
        assert!(files.contains(e.file.as_str()), "allow.txt names {}, which is not a source file", e.file);
        assert!(e.count > 0, "allow.txt gives {} {} a count of 0; remove the entry", e.check.name(), e.file);
        assert!(e.reason.len() > 10, "allow.txt gives {} {} no reason", e.check.name(), e.file);
        assert!(seen.insert((e.check, e.file.clone())), "allow.txt lists {} {} twice", e.check.name(), e.file);
        let key = (e.check, e.file.clone());
        assert!(last.as_ref().is_none_or(|l| l < &key), "allow.txt is out of order at {} {}", e.check.name(), e.file);
        last = Some(key);
    }
}

#[test]
fn async_code_and_async_dependencies_are_found() {
    let list = found("crates/a/src/lib.rs", "async fn f() { g().await; let c = async || 1; } fn h() -> impl Future<Output = ()> { todo!() }");
    assert_eq!(of(Check::Async, &list), [".await", "Future", "async closure", "async fn f"]);
    let toml = "[package]\nname = \"a\"\n[dependencies]\ntokio = { workspace = true }\nfutures-util = \"0.3\"\nserde = \"1\"\n[dev-dependencies.async-std]\nversion = \"1\"\n";
    let list = found("crates/a/Cargo.toml", toml);
    assert_eq!(of(Check::Async, &list), ["depends on async-std", "depends on futures-util", "depends on tokio"]);
}

#[test]
fn ambient_io_is_found_through_imports_and_macros_but_not_in_the_shell() {
    let text = "use std::fs; use std::time::Instant; use std::env::var as v;\n\
        fn f() { fs::read(\"x\"); let t = Instant::now(); v(\"A\"); println!(\"{:?}\", std::process::id()); unsafe { libc::getpid(); } }";
    let list = found("crates/a/src/lib.rs", text);
    assert_eq!(
        of(Check::AmbientIo, &list),
        ["libc::getpid", "std::env::var", "std::fs::read", "std::process::id", "std::time::Instant::now"]
    );
    assert!(found("crates/a/src/io.rs", text).is_empty());
    assert!(found("crates/a/src/io/disk.rs", text).is_empty());
    for shell in ["crates/a/build.rs", "crates/a/examples/bench.rs", "crates/a/examples/bench/main.rs"] {
        assert!(of(Check::AmbientIo, &found(shell, text)).is_empty(), "{shell}");
    }
    for not_shell in ["crates/a/src/build.rs", "crates/a/src/examples/bench.rs", "crates/a/src/build/build.rs"] {
        assert_eq!(of(Check::AmbientIo, &found(not_shell, text)).len(), 5, "{not_shell}");
    }
    assert!(found("crates/a/src/lib.rs", "fn f() -> std::time::Duration { let x = libc::EINTR; todo!() }").is_empty());
    let addresses = "use core::net::SocketAddr; fn f() -> SocketAddr { SocketAddr::from((core::net::Ipv6Addr::LOCALHOST, 1)) }";
    assert!(found("crates/a/src/lib.rs", addresses).is_empty());
    let socket = "use std::net::UdpSocket; fn f() { let _ = UdpSocket::bind(\"[::1]:0\"); }";
    assert_eq!(of(Check::AmbientIo, &found("crates/a/src/lib.rs", socket)), ["std::net::UdpSocket::bind"]);
    let shadowed = "use std::fs; use std::sync::atomic::{AtomicU8, Ordering};\n\
        fn f(fs: &dyn Store, a: &AtomicU8) { fs.stat(); a.load(Ordering::Relaxed); }";
    assert_eq!(of(Check::AmbientIo, &found("crates/a/src/lib.rs", shadowed)), Vec::<String>::new());
    assert_eq!(of(Check::Locks, &found("crates/a/src/lib.rs", shadowed)), ["std::sync::atomic::AtomicU8"]);
}

#[test]
fn locks_and_shared_mutability_are_found_but_arc_alone_is_not() {
    let text = "use std::sync::{Arc, Mutex}; use std::sync::atomic::*;\n\
        static mut N: u8 = 0; struct S { m: Arc<Mutex<u8>>, a: AtomicU64, r: Arc<Vec<u8>>, p: parking_lot::RwLock<()> }";
    let list = found("crates/a/src/lib.rs", text);
    assert_eq!(
        of(Check::Locks, &list),
        ["parking_lot::RwLock", "static mut N", "std::sync::Mutex", "std::sync::atomic::AtomicU64"]
    );
}

#[test]
fn hash_maps_with_the_default_hasher_are_found_and_fixed_hashers_are_not() {
    let text = "use std::collections::{HashMap, HashSet};\n\
        fn f() -> HashMap<u8, u8> { HashMap::new() }\n\
        fn g(s: HashSet<u8>) {}\n\
        fn h() -> HashMap<u8, u8, Fixed> { HashMap::with_hasher(Fixed) }\n\
        fn k(s: HashSet<u8, Fixed>) -> std::collections::BTreeMap<u8, u8> { todo!() }";
    let list = found("crates/a/src/lib.rs", text);
    assert_eq!(
        of(Check::HashOrder, &list),
        ["std::collections::HashMap", "std::collections::HashMap::new", "std::collections::HashSet"]
    );
}

#[test]
fn test_code_gets_the_io_and_lock_denials_with_its_own_check() {
    let text = "fn f() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    #[ignore]\n    fn t() { std::thread::sleep(std::time::Duration::ZERO); let m = std::sync::Mutex::new(1); }\n}";
    let list = found("crates/a/src/lib.rs", text);
    assert!(of(Check::AmbientIo, &list).is_empty() && of(Check::Locks, &list).is_empty());
    assert_eq!(
        of(Check::PureTests, &list),
        ["#[ignore]", "std::sync::Mutex::new (std::sync::Mutex)", "std::thread::sleep (std::thread::sleep)"]
    );
    let list = found("crates/a/tests/it.rs", "fn t() { std::fs::read(\"x\"); }");
    assert_eq!(of(Check::PureTests, &list), ["std::fs::read (std::fs)"]);
    let toml = "[dev-dependencies]\ntempfile = \"3\"\n";
    assert_eq!(of(Check::PureTests, &found("crates/a/Cargo.toml", toml)), ["tempfile is a dev-dependency"]);
}

#[test]
fn an_external_test_module_is_test_code_of_the_file_that_declares_it() {
    let lib = "pub struct S(u8);\nimpl StateMachine for S {\n    fn invariants(&self) { assert!(self.0 < 9); }\n}\nimpl S {\n    pub fn step(&mut self) { self.0 += 1; self.invariants(); }\n}\n#[cfg(test)]\nmod tests;";
    let tests = "use super::*;\n#[test]\nfn t() { let mut s = S(0); s.step(); s.invariants(); std::fs::read(\"x\"); }";
    let list: Vec<_> =
        scan(&[("crates/a/src/lib.rs", lib), ("crates/a/src/tests.rs", tests)]).into_iter().map(|f| (f.check, f.what)).collect();
    assert_eq!(list, [(Check::PureTests, "std::fs::read (std::fs)".to_string())]);
}

#[test]
fn a_state_machine_is_found_through_its_trait_and_without_assertions_or_tests_is_found() {
    let text = "pub struct S(u8);\nimpl rargo_machine::StateMachine for S {\n    fn invariants(&self) {}\n}\nimpl S {\n    pub fn step(&mut self) { self.0 += 1; }\n    pub fn peek(&self) -> u8 { self.0 }\n}\nimpl Other { pub fn step(&mut self) {} }";
    let list = found("crates/a/src/lib.rs", text);
    assert_eq!(
        of(Check::SelfCheck, &list),
        ["S::invariants asserts nothing", "S::step changes the state and asserts nothing", "no test calls invariants() on S"]
    );
    let text = "pub struct S(u8);\nimpl StateMachine for S {\n    fn invariants(&self) { debug_assert!(self.0 < 9); }\n}\nimpl S {\n    pub fn step(&mut self) { self.0 += 1; self.invariants(); }\n}\n#[cfg(test)]\nmod tests { #[test] fn t() { super::S(0).invariants(); } }";
    assert!(of(Check::SelfCheck, &found("crates/a/src/lib.rs", text)).is_empty());
}

#[test]
fn a_machine_whose_trait_impl_is_test_only_is_counted() {
    let text = "pub struct S(u8);\nimpl S {\n    pub fn step(&mut self) { self.0 += 1; }\n}\n#[cfg(test)]\nimpl StateMachine for S {\n    fn invariants(&self) { assert!(self.0 < 9); }\n}";
    assert_eq!(of(Check::SelfCheck, &found("crates/a/src/lib.rs", text)), ["S::step changes the state and asserts nothing", "no test calls invariants() on S"]);
}

#[test]
fn a_test_checks_invariants_through_the_trait_helpers_too() {
    for call in ["rargo_machine::check(&S(0), \"here\")", "rargo_machine::check_all([&S(0) as &dyn StateMachine])", "Checked::new(S(0)).step(|s| s.step())"] {
        let text = format!(
            "pub struct S(u8);\nimpl StateMachine for S {{\n    fn invariants(&self) {{ assert!(self.0 < 9); }}\n}}\nimpl S {{\n    pub fn step(&mut self) {{ self.0 += 1; self.invariants(); }}\n}}\n#[cfg(test)]\nmod tests {{ #[test] fn t() {{ {call}; }} }}"
        );
        assert!(of(Check::SelfCheck, &found("crates/a/src/lib.rs", &text)).is_empty(), "{call}");
    }
}

#[test]
fn an_invariants_method_outside_the_trait_is_found() {
    let text = "pub struct S(u8);\nimpl S {\n    pub fn invariants(&self) -> bool { self.0 < 9 }\n    pub fn step(&mut self) { self.0 += 1; assert!(self.invariants()); }\n}";
    assert_eq!(of(Check::SelfCheck, &found("crates/a/src/lib.rs", text)), ["S::invariants is not StateMachine::invariants"]);
}

#[test]
fn an_added_finding_and_a_stale_entry_both_fail_and_say_what_to_do() {
    let f = |file: &str, line| Finding { check: Check::Locks, file: file.into(), line, what: "std::sync::Mutex".into() };
    let entry = |file: &str, count| Entry { check: Check::Locks, file: file.into(), count, reason: "a reason long enough".into() };
    let findings = [f("a.rs", 1), f("a.rs", 2), f("b.rs", 3)];
    assert!(compare(Check::Locks, &findings, &[entry("a.rs", 2), entry("b.rs", 1)]).is_ok());
    assert!(compare(Check::Async, &findings, &[]).is_ok());
    let added = compare(Check::Locks, &findings, &[entry("a.rs", 1), entry("b.rs", 1)]).unwrap_err();
    assert!(added.contains("a.rs: 2 found where allow.txt permits 1") && added.contains("a.rs:2: std::sync::Mutex"), "{added}");
    let stale = compare(Check::Locks, &findings, &[entry("a.rs", 2), entry("b.rs", 1), entry("c.rs", 4)]).unwrap_err();
    assert!(stale.contains("c.rs: allow.txt permits 4 but the code has 0; lower the entry to 0 by removing it"), "{stale}");
    let fewer = compare(Check::Locks, &findings, &[entry("a.rs", 3), entry("b.rs", 1)]).unwrap_err();
    assert!(fewer.contains("lower the entry to 2"), "{fewer}");
}

#[test]
fn the_allowlist_format_is_strict() {
    assert!(parse_allow("locks\ta.rs\t2\tthe shell's lock\n").is_ok());
    assert!(parse_allow("locks a.rs 2 spaces\n").is_err());
    assert!(parse_allow("lock\ta.rs\t2\tno such check\n").is_err());
    assert!(parse_allow("locks\ta.rs\ttwo\tnot a number\n").is_err());
}

#[test]
fn an_address_is_a_value_and_a_socket_is_io() {
    let list = found("crates/a/src/lib.rs", "fn f(a: std::net::SocketAddr, b: std::net::Ipv4Addr) { let _ = std::net::UdpSocket::bind(a); }");
    assert_eq!(of(Check::AmbientIo, &list), ["std::net::UdpSocket::bind"]);
}
