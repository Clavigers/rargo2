use crate::buildview::{CARGO_HOME_AT, LEVEL_FULL, LEVEL_META, LEVEL_NONE, WORKTREE_AT};
use crate::present::Translator;
use rargo_plan as rp;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct PlanFacts {
    pub linking: Vec<bool>,
    pub compile: Vec<bool>,
    pub rmeta_out: Vec<Vec<Vec<u8>>>,
    pub doc_dir: Vec<Option<Vec<u8>>>,
    pub doc_order: Vec<usize>,
    pub doc_pos: Vec<Option<usize>>,
}

impl PlanFacts {
    pub fn can_meta(&self, j: usize) -> bool {
        self.compile[j] && !self.rmeta_out[j].is_empty()
    }
}

pub fn plan_facts(units: &[rp::Unit], roots: &[u32]) -> PlanFacts {
    let n = units.len();
    let linking: Vec<bool> = units.iter().map(|u| u.needs_objects).collect();
    let compile: Vec<bool> = units.iter().map(|u| u.kind == rp::UnitKind::Compile).collect();
    let rmeta_out: Vec<Vec<Vec<u8>>> = units
        .iter()
        .map(|u| u.outputs.iter().filter(|o| o.flavor == "Rmeta").map(|o| o.path.clone().into_bytes()).collect())
        .collect();
    let doc_dir: Vec<Option<Vec<u8>>> = units
        .iter()
        .map(|u| match (&u.kind, &u.cmd) {
            (rp::UnitKind::Doc, Some(cmd)) => {
                ["-o", "--out-dir"].iter().find_map(|f| arg_value(&cmd.args, f)).map(|d| d.as_bytes().to_vec())
            }
            _ => None,
        })
        .collect();
    let mut doc_order: Vec<usize> = Vec::new();
    {
        let mut seen = vec![false; n];
        fn visit(
            i: usize,
            units: &[rp::Unit],
            is_doc: &dyn Fn(usize) -> bool,
            seen: &mut [bool],
            out: &mut Vec<usize>,
        ) {
            if seen[i] {
                return;
            }
            seen[i] = true;
            assert!(is_doc(i));
            let mut deps: Vec<usize> = units[i].deps.iter().map(|d| d.unit as usize).filter(|&j| is_doc(j)).collect();
            deps.sort_unstable();
            for j in deps {
                visit(j, units, is_doc, seen, out);
            }
            assert!(!out.contains(&i), "a documentation unit is ordered once");
            out.push(i);
        }
        let is_doc = |j: usize| doc_dir[j].is_some();
        for i in 0..n {
            if is_doc(i) {
                visit(i, units, &is_doc, &mut seen, &mut doc_order);
            }
        }
    }
    assert_eq!(doc_order.len(), doc_dir.iter().filter(|d| d.is_some()).count());
    let mut doc_pos = vec![None; n];
    for (pos, &i) in doc_order.iter().enumerate() {
        assert!(doc_pos[i].is_none());
        doc_pos[i] = Some(pos);
    }
    for u in units {
        for d in &u.deps {
            assert!((d.unit as usize) < n, "a dependency names a unit of the plan");
        }
    }
    for &r in roots {
        assert!((r as usize) < n, "a root names a unit of the plan");
    }
    assert_eq!(linking.len(), n);
    assert_eq!(rmeta_out.len(), n);
    PlanFacts { linking, compile, rmeta_out, doc_dir, doc_order, doc_pos }
}

pub fn meta_levels(units: &[rp::Unit], facts: &PlanFacts, i: usize) -> Vec<u8> {
    let mut lv = vec![LEVEL_NONE; units.len()];
    let mut stack: Vec<usize> = units[i].deps.iter().map(|d| d.unit as usize).collect();
    while let Some(j) = stack.pop() {
        if lv[j] != LEVEL_NONE {
            continue;
        }
        lv[j] = if facts.can_meta(j) { LEVEL_META } else { LEVEL_FULL };
        for d in &units[j].deps {
            stack.push(d.unit as usize);
        }
    }
    assert_eq!(lv[i], LEVEL_NONE, "a unit does not depend on itself");
    assert_eq!(lv.len(), units.len());
    lv
}

pub fn levels(units: &[rp::Unit], facts: &PlanFacts, i: usize) -> Vec<u8> {
    let linking = facts.linking[i];
    let mut lv = vec![LEVEL_NONE; units.len()];
    let mut stack: Vec<usize> = Vec::new();
    for d in &units[i].deps {
        let j = d.unit as usize;
        let direct = if linking || !(d.only_rmeta && units[j].rmeta_early) { LEVEL_FULL } else { LEVEL_META };
        if lv[j] < direct {
            if lv[j] == LEVEL_NONE {
                stack.push(j);
            }
            lv[j] = direct;
        }
    }
    while let Some(j) = stack.pop() {
        for d in &units[j].deps {
            let k = d.unit as usize;
            let want = if linking || !units[k].rmeta_early { LEVEL_FULL } else { LEVEL_META };
            if lv[k] == LEVEL_NONE {
                stack.push(k);
            }
            if lv[k] < want {
                lv[k] = want;
            }
        }
    }
    assert_eq!(lv[i], LEVEL_NONE, "a unit does not depend on itself");
    if linking {
        assert!(lv.iter().all(|&l| l != LEVEL_META), "a linking unit sees its dependencies whole");
    }
    if let Some(pos) = facts.doc_pos[i] {
        assert_eq!(facts.doc_order[pos], i);
        for &j in &facts.doc_order[..pos] {
            assert_ne!(j, i);
            if facts.doc_dir[j] == facts.doc_dir[i] {
                lv[j] = LEVEL_FULL;
            }
        }
    }
    lv
}

pub fn needs(
    units: &[rp::Unit],
    roots: &[u32],
    meta_levels: &dyn Fn(usize) -> Arc<Vec<u8>>,
    levels: &dyn Fn(usize) -> Arc<Vec<u8>>,
) -> Vec<u8> {
    let mut need = vec![LEVEL_NONE; units.len()];
    let mut work: Vec<usize> = Vec::new();
    for &r in roots {
        let r = r as usize;
        need[r] = if units[r].kind == rp::UnitKind::Compile { LEVEL_META } else { LEVEL_FULL };
        work.push(r);
    }
    while let Some(i) = work.pop() {
        let sees = if need[i] == LEVEL_META { meta_levels(i) } else { levels(i) };
        for (k, &lv) in sees.iter().enumerate() {
            if lv == LEVEL_NONE {
                continue;
            }
            let lv = if units[k].kind == rp::UnitKind::Compile { lv } else { LEVEL_FULL };
            assert!(lv == LEVEL_META || lv == LEVEL_FULL);
            if lv > need[k] {
                need[k] = lv;
                work.push(k);
            }
        }
    }
    for &r in roots {
        assert_ne!(need[r as usize], LEVEL_NONE, "a root is needed");
    }
    for (k, &lv) in need.iter().enumerate() {
        if lv == LEVEL_META {
            assert_eq!(units[k].kind, rp::UnitKind::Compile, "only a compile stops at metadata");
        }
    }
    need
}

pub fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().map(|s| s.as_str());
        }
        if let Some(rest) = a.strip_prefix(flag)
            && let Some(v) = rest.strip_prefix('=')
        {
            return Some(v);
        }
    }
    None
}

pub fn subcommand_pos(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return None;
        }
        if !a.starts_with('-') {
            assert!(i < args.len());
            return Some(i);
        }
        if matches!(a.as_str(), "--config" | "-Z" | "--color" | "-C" | "--explain") {
            i += 1;
        }
        i += 1;
    }
    None
}

pub fn offline_args(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => break,
            "--manifest-path" | "--config" | "--lockfile-path" => {
                it.next();
            }
            "--locked" | "--frozen" | "--offline" => return true,
            _ => {}
        }
    }
    false
}

/// The value of the `--target-dir` flag, if the command line names one.
pub fn target_dir_flag(args: &[String]) -> Option<&str> {
    let mut it = args.iter().take_while(|a| *a != "--");
    while let Some(a) = it.next() {
        if a == "--target-dir" {
            return it.next().map(String::as_str);
        }
        if let Some(dir) = a.strip_prefix("--target-dir=") {
            return Some(dir);
        }
    }
    None
}

/// `path` made absolute against `cwd`, with `.` and `..` resolved lexically.
pub fn absolute_in(cwd: &str, path: &str) -> String {
    let mut out = PathBuf::from("/");
    for c in Path::new(cwd).join(path).components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out.to_string_lossy().into_owned()
}

/// A `--target-dir` flag takes the place of `CARGO_TARGET_DIR`, as in cargo,
/// so that [`build_paths`] maps it into the sandbox the same way.
pub fn flag_target_dir(args: &[String], cwd: &str, env: &mut BTreeMap<String, String>) -> bool {
    let Some(dir) = target_dir_flag(args) else { return false };
    env.insert("CARGO_TARGET_DIR".into(), absolute_in(cwd, dir));
    true
}

/// The planner's command line with a `--target-dir` value replaced by `dir`.
pub fn with_target_dir(args: &[String], dir: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter();
    let mut done = false;
    while let Some(a) = it.next() {
        if done || a == "--" {
            done = true;
            out.push(a.clone());
        } else if a == "--target-dir" {
            out.push(a.clone());
            if it.next().is_some() {
                out.push(dir.into());
            }
            done = true;
        } else if a.starts_with("--target-dir=") {
            out.push(format!("--target-dir={dir}"));
            done = true;
        } else {
            out.push(a.clone());
        }
    }
    out
}

/// Where a target directory outside the worktree lives inside the action filesystem.
pub const EXTERNAL_TARGET_AT: &str = "/w/target/rargo-external";

fn in_external_target(p: &str) -> bool {
    p.strip_prefix(EXTERNAL_TARGET_AT).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The program `rargo run` starts, as the client names it: one under an external target directory
/// exists only at the client's own path, which the run's mount namespace does not remap.
pub fn client_program(program: &str, sandbox_cwd: &str, tr: &Translator) -> String {
    let abs = Path::new(sandbox_cwd).join(program).components().collect::<PathBuf>();
    let abs = abs.to_string_lossy();
    if in_external_target(&abs) { tr.path(&abs) } else { program.into() }
}

/// A path list (such as the dynamic library search path) with each entry under an external target
/// directory given its client path.
pub fn client_paths(value: &str, tr: &Translator) -> String {
    let client = |part: &str| if in_external_target(part) { tr.path(part) } else { part.into() };
    value.split(':').map(client).collect::<Vec<_>>().join(":")
}

/// A status line from the plan (such as `Executable unittests src/main.rs (target/...)`) with
/// each path under an external target directory given its client path, as `rargo run` names its
/// program: the client has no `target/rargo-external`.
pub fn client_text(msg: &str, sandbox_cwd: &str, tr: &Translator) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while !rest.is_empty() {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, after) = rest.split_at(end);
        let core = word.trim_start_matches(['(', '`', '\'', '"']);
        let core = core.trim_end_matches([')', '`', '\'', '"', ',']);
        if core.is_empty() {
            out.push_str(word);
        } else {
            let start = word.find(core).expect("core is a part of word");
            out.push_str(&word[..start]);
            out.push_str(&client_program(core, sandbox_cwd, tr));
            out.push_str(&word[start + core.len()..]);
        }
        let space = after.len() - after.trim_start().len();
        out.push_str(&after[..space]);
        rest = &after[space..];
    }
    out
}

pub fn build_paths(root: &str, cargo_home: &str, env: &mut BTreeMap<String, String>) -> Translator {
    let worktree_at = std::str::from_utf8(WORKTREE_AT).expect("worktree mount is ASCII");
    let mut pairs = vec![
        (worktree_at.into(), root.into()),
        (String::from_utf8_lossy(CARGO_HOME_AT).into_owned(), cargo_home.into()),
    ];
    for (var, sandbox) in [("CARGO_TARGET_DIR", EXTERNAL_TARGET_AT), ("CARGO_BUILD_BUILD_DIR", "/w/target/rargo-external-build")] {
        let Some(dir) = env.get_mut(var) else { continue };
        if !Path::new(dir).is_absolute() {
            continue;
        }
        let local = Path::new(dir).components().collect::<PathBuf>();
        if let Ok(relative) = local.strip_prefix(root) {
            *dir = Path::new(worktree_at).join(relative).to_string_lossy().into_owned();
        } else {
            // Keep compiler outputs in the action filesystem even for client paths under /tmp.
            pairs.push((sandbox.into(), local.to_string_lossy().into_owned()));
            *dir = sandbox.into();
        }
    }
    Translator::new(pairs)
}

/// The target and build directories a plan chose from cargo configuration
/// (`build.target-dir`, `build.build-dir`) that lie outside the sandbox's
/// worktree and cargo home, as the variables and client paths that make
/// [`build_paths`] map them into the sandbox. Outside those, a directory is
/// not in the action filesystem (`/tmp` is each action's own tmpfs), so the
/// plan must be made again with these set. A variable already in `env` was
/// mapped before planning and is left alone.
pub fn configured_dirs(
    target_dir: &str,
    build_dir: &str,
    tr: &Translator,
    env: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let inside = |dir: &str| {
        let dir = absolute_in("/", dir);
        [WORKTREE_AT, CARGO_HOME_AT].iter().any(|at| crate::view::vpath_starts_with(dir.as_bytes(), at))
    };
    let mut out = Vec::new();
    for (var, dir) in [("CARGO_TARGET_DIR", target_dir), ("CARGO_BUILD_BUILD_DIR", build_dir)] {
        if env.contains_key(var) || !dir.starts_with('/') || inside(dir) {
            continue;
        }
        if var == "CARGO_BUILD_BUILD_DIR" && dir == target_dir {
            continue;
        }
        out.push((var.to_string(), absolute_in("/", &tr.path(dir))));
    }
    out
}

/// The environment and translator for planning again with `dirs` added to
/// `unmapped`, the environment as it was before [`build_paths`]. Mapping the
/// already-mapped environment again would map its sandbox paths to themselves.
pub fn map_again(
    root: &str,
    cargo_home: &str,
    unmapped: &mut BTreeMap<String, String>,
    dirs: Vec<(String, String)>,
) -> (BTreeMap<String, String>, Translator) {
    unmapped.extend(dirs);
    let mut env = unmapped.clone();
    let tr = build_paths(root, cargo_home, &mut env);
    (env, tr)
}

/// Whether a variable of the client stays out of a compile's (rustc,
/// rustdoc, build script) environment, and so out of its key. These are read
/// by the program or test being run (`RUST_LOG`, `RUST_BACKTRACE`), by
/// cargo while planning (its `CARGO_*` settings, which reach a compile only
/// through the plan's arguments), or are the locale. A compile that does read
/// one (`env!`, `rerun-if-env-changed`) declares it and gets it back from the
/// client, as any undeclared variable does.
pub fn compile_env_skips(name: &str) -> bool {
    const RUST_KEPT: &[&str] = &["RUST_MIN_STACK", "RUST_TARGET_PATH"];
    const CARGO_KEPT: &[&str] = &["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR", "CARGO_HOME"];
    match name {
        "LANG" | "LANGUAGE" => true,
        _ if name.starts_with("LC_") => true,
        _ if name.starts_with("RUST_") => !RUST_KEPT.contains(&name),
        _ if name.starts_with("CARGO_") => !CARGO_KEPT.contains(&name),
        _ => false,
    }
}

pub fn base_env<'a>(
    server: impl IntoIterator<Item = (&'a String, &'a String)>,
    client: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> BTreeMap<String, String> {
    const SERVER: &[&str] =
        &["PATH", "HOME", "USER", "LOGNAME", "SHELL", "RUSTUP_HOME", "SSL_CERT_FILE", "SSL_CERT_DIR"];
    const NOT_FOR_ACTIONS: &[&str] = &["TERM", "NO_COLOR", "BROWSER", "CLICOLOR", "CLICOLOR_FORCE", "COLORTERM"];
    const DROP_PREFIXES: &[&str] = &[
        "CARGO_PKG_",
        "CARGO_MANIFEST_",
        "CARGO_CRATE_",
        "CARGO_BIN_",
        "CARGO_CFG_",
        "CARGO_FEATURE_",
        "CARGO_TERM_",
        "CARGO_DEP_",
        "RARGO_",
    ];
    const DROP: &[&str] = &[
        "CARGO_HOME",
        "CARGO_MAKEFLAGS",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_TARGET_TMPDIR",
        "CARGO_SBOM_PATH",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_NET_OFFLINE",
        "CARGO_CACHE_RUSTC_INFO",
        "CARGO_RUSTC_CURRENT_DIR",
    ];
    let mut out = BTreeMap::new();
    for (k, v) in server {
        if SERVER.contains(&k.as_str()) || k.starts_with("NIX_") {
            out.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in client {
        let dropped = DROP.contains(&k.as_str())
            || NOT_FOR_ACTIONS.contains(&k.as_str())
            || DROP_PREFIXES.iter().any(|p| k.starts_with(p));
        if rargo_proto::env_allowed(k) && !dropped {
            out.insert(k.clone(), v.clone());
        }
    }
    out.insert("CARGO_HOME".into(), String::from_utf8_lossy(CARGO_HOME_AT).into_owned());
    out.insert("RUSTC_WRAPPER".into(), String::new());
    out.insert("RUSTC_WORKSPACE_WRAPPER".into(), String::new());
    out
}

pub fn missing_components(stderr: &[u8]) -> Vec<(String, String)> {
    let s = String::from_utf8_lossy(stderr);
    let name = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    let mut out: Vec<(String, String)> = Vec::new();
    let mut toolchain = None;
    for line in s.lines() {
        if let Some(rest) = line.split_once("is not installed for the toolchain '").map(|x| x.1) {
            toolchain = rest.split_once('\'').map(|x| x.0.to_string()).filter(|t| name(t));
        } else if let (Some(tc), Some(rest)) = (&toolchain, line.split_once("rustup component add ").map(|x| x.1)) {
            let words: Vec<&str> = rest.split('`').next().unwrap_or("").split_whitespace().collect();
            let mut i = 0;
            let mut component = "";
            while i < words.len() {
                match words[i] {
                    "--toolchain" | "--target" => i += 2,
                    w if w.starts_with('-') => i += 1,
                    w => {
                        component = w;
                        break;
                    }
                }
            }
            let pair = (component.to_string(), tc.clone());
            if name(component) && !component.starts_with('-') && !out.contains(&pair) {
                out.push(pair);
            }
            toolchain = None;
        }
    }
    out
}

pub fn needs_network(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr);
    ["--offline", "offline mode", "is not installed", "could not download", "syncing channel updates"]
        .iter()
        .any(|m| s.contains(m))
        || s.lines().any(|line| line.starts_with("error: could not find `") && line.contains(" in registry `"))
}

pub fn script_warning_message(line: &str) -> Option<&str> {
    let line = line.trim();
    line.strip_prefix("cargo:warning=").or_else(|| line.strip_prefix("cargo::warning=")).map(|msg| msg.trim_end())
}

pub fn rustup_adds(components: &[(String, String)]) -> Vec<Vec<String>> {
    components
        .iter()
        .map(|(component, toolchain)| {
            ["component", "add", component, "--toolchain", toolchain].map(String::from).to_vec()
        })
        .collect()
}

pub fn rustup_failure(ran: Result<(bool, &[u8]), &str>) -> Option<Vec<u8>> {
    match ran {
        Ok((true, _)) => None,
        Ok((false, stderr)) => Some(stderr.to_vec()),
        Err(e) => Some(format!("error: running rustup: {e}\n").into_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_env_skips_what_only_runs_and_planning_read() {
        for skipped in [
            "RUST_LOG", "RUST_BACKTRACE", "RUST_LIB_BACKTRACE", "LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE",
            "CARGO_NET_RETRY", "CARGO_INCREMENTAL", "CARGO_MAKE_TIMESTAMP", "CARGO_BUILD_JOBS",
        ] {
            assert!(compile_env_skips(skipped), "{skipped}");
        }
        for kept in [
            "RUSTFLAGS", "RUSTDOCFLAGS", "RUSTC", "RUSTC_BOOTSTRAP", "RUSTUP_TOOLCHAIN", "RUST_MIN_STACK",
            "RUST_TARGET_PATH", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR", "CARGO_HOME",
            "__CARGO_DEFAULT_LIB_METADATA", "TZ", "PATH", "HOME",
        ] {
            assert!(!compile_env_skips(kept), "{kept}");
        }
    }

    #[test]
    fn external_target_dir_uses_the_action_fs_and_translates_for_delivery() {
        let mut env = BTreeMap::from([("CARGO_TARGET_DIR".into(), "/tmp/outside/target/".into())]);
        let tr = build_paths("/tmp/worktree", "/home/user/.cargo", &mut env);
        let sandbox = &env["CARGO_TARGET_DIR"];
        assert!(sandbox.starts_with("/w/target/"));
        let binary = format!("{sandbox}/debug/second");
        assert_eq!(tr.path(&binary), "/tmp/outside/target/debug/second");
        assert_eq!(tr.text(&format!("output: {binary}")), "output: /tmp/outside/target/debug/second");
    }

    #[test]
    fn target_dir_flag_is_mapped_like_the_environment_variable() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (given, client) in [
            (args(&["build", "--target-dir", "/tmp/elsewhere/t"]), "/tmp/elsewhere/t"),
            (args(&["build", "--target-dir=/tmp/elsewhere/eq"]), "/tmp/elsewhere/eq"),
            (args(&["build", "--target-dir", "../sibling"]), "/var/tmp/sibling"),
            (args(&["build", "--target-dir=./out/../t2"]), "/var/tmp/hello/t2"),
        ] {
            let mut env = BTreeMap::from([("CARGO_TARGET_DIR".into(), "/ignored".into())]);
            assert!(flag_target_dir(&given, "/var/tmp/hello", &mut env));
            let tr = build_paths("/var/tmp/hello", "/home/user/.cargo", &mut env);
            let sandbox = env["CARGO_TARGET_DIR"].clone();
            assert!(sandbox.starts_with("/w/"), "{sandbox}");
            assert_eq!(tr.path(&format!("{sandbox}/debug/hello")), format!("{client}/debug/hello"));
            let planned = with_target_dir(&given, &sandbox);
            assert_eq!(target_dir_flag(&planned), Some(sandbox.as_str()));
            assert_eq!(planned.len(), given.len());
        }
    }

    #[test]
    fn target_dir_flag_after_the_separator_is_not_cargos() {
        let given: Vec<String> = ["run", "--", "--target-dir", "x"].iter().map(|s| s.to_string()).collect();
        let mut env = BTreeMap::new();
        assert!(!flag_target_dir(&given, "/w", &mut env));
        assert!(env.is_empty());
        assert_eq!(with_target_dir(&given, "/w/y"), given);
    }

    #[test]
    fn a_configured_target_dir_outside_the_sandbox_is_planned_again_as_the_variable() {
        // `R` under /tmp, the project at `R/two`; each case is a planned
        // target dir and where cargo puts it on the client.
        let root = "/tmp/R/two";
        for (planned, client) in [
            ("/tmp/R/two-out", "/tmp/R/two-out"),
            ("/w/../x", "/tmp/R/x"),
            ("/w/.cargo/../../x/", "/tmp/R/x"),
            ("/var/tmp/out", "/var/tmp/out"),
        ] {
            let mut env = BTreeMap::new();
            let tr = build_paths(root, "/home/user/.cargo", &mut env);
            let dirs = configured_dirs(planned, planned, &tr, &env);
            assert_eq!(dirs, vec![("CARGO_TARGET_DIR".to_string(), client.to_string())], "{planned}");
            env.extend(dirs);
            let tr = build_paths(root, "/home/user/.cargo", &mut env);
            let sandbox = env["CARGO_TARGET_DIR"].clone();
            assert_eq!(sandbox, "/w/target/rargo-external");
            assert_eq!(tr.path(&format!("{sandbox}/debug/two")), format!("{client}/debug/two"));
            assert_eq!(tr.to_canonical("/tmp/R/two/src"), "/w/src");
            // Planned again, the directory is in the sandbox: no third plan.
            assert!(configured_dirs(&sandbox, &sandbox, &tr, &env).is_empty());
        }
    }

    #[test]
    fn a_configured_target_dir_inside_the_worktree_or_given_already_is_kept() {
        let mut env = BTreeMap::new();
        let tr = build_paths("/tmp/R/two", "/home/user/.cargo", &mut env);
        for planned in ["/w/target", "/w/out", "/w/sub/../out", "/c/target"] {
            assert!(configured_dirs(planned, planned, &tr, &env).is_empty(), "{planned}");
        }
        let given = BTreeMap::from([("CARGO_TARGET_DIR".to_string(), "/w/target/rargo-external".to_string())]);
        assert!(configured_dirs("/tmp/R/two-out", "/w/target/rargo-external", &tr, &given).is_empty());
    }

    #[test]
    fn a_configured_build_dir_outside_the_sandbox_is_mapped_apart_from_the_target_dir() {
        let mut env = BTreeMap::new();
        let tr = build_paths("/tmp/R/two", "/home/user/.cargo", &mut env);
        let dirs = configured_dirs("/w/target", "/tmp/R/build", &tr, &env);
        assert_eq!(dirs, vec![("CARGO_BUILD_BUILD_DIR".to_string(), "/tmp/R/build".to_string())]);
        env.extend(dirs);
        let tr = build_paths("/tmp/R/two", "/home/user/.cargo", &mut env);
        assert_eq!(env["CARGO_BUILD_BUILD_DIR"], "/w/target/rargo-external-build");
        assert_eq!(tr.path("/w/target/rargo-external-build/debug/deps"), "/tmp/R/build/debug/deps");
        assert_eq!(tr.path("/w/target/debug/two"), "/tmp/R/two/target/debug/two");
        let both = configured_dirs("/tmp/R/t", "/tmp/R/b", &tr, &BTreeMap::new());
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn a_configured_build_dir_keeps_a_target_dir_the_client_gave() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (given, env_dir, client) in [
            (args(&["build"]), Some("/tmp/out"), "/tmp/out"),
            (args(&["build", "--target-dir", "mytarget"]), None, "/tmp/R/two/mytarget"),
        ] {
            let mut env: BTreeMap<String, String> =
                env_dir.map(|d| ("CARGO_TARGET_DIR".to_string(), d.to_string())).into_iter().collect();
            flag_target_dir(&given, "/tmp/R/two", &mut env);
            let mut unmapped = env.clone();
            let tr = build_paths("/tmp/R/two", "/home/user/.cargo", &mut env);
            let target = env["CARGO_TARGET_DIR"].clone();
            let dirs = configured_dirs(&target, "/tmp/R/build", &tr, &env);
            assert_eq!(dirs, vec![("CARGO_BUILD_BUILD_DIR".to_string(), "/tmp/R/build".to_string())]);
            let (env, tr) = map_again("/tmp/R/two", "/home/user/.cargo", &mut unmapped, dirs);
            assert_eq!(env["CARGO_TARGET_DIR"], target);
            assert_eq!(tr.path(&format!("{target}/debug/two")), format!("{client}/debug/two"));
            assert_eq!(tr.path("/w/target/rargo-external-build/debug/deps"), "/tmp/R/build/debug/deps");
            assert!(configured_dirs(&target, &env["CARGO_BUILD_BUILD_DIR"], &tr, &env).is_empty());
        }
    }

    #[test]
    fn run_names_a_program_in_an_external_target_dir_by_its_client_path() {
        let mut env = BTreeMap::from([("CARGO_TARGET_DIR".into(), "/tmp/x/one-out".into())]);
        let tr = build_paths("/root/one", "/root/.cargo", &mut env);
        for program in ["target/rargo-external/debug/one", "./target/rargo-external/debug/one", "/w/target/rargo-external/debug/one"] {
            assert_eq!(client_program(program, "/w", &tr), "/tmp/x/one-out/debug/one", "{program}");
        }
        assert_eq!(client_program("rargo-external/debug/one", "/w/target", &tr), "/tmp/x/one-out/debug/one");
        for inside in ["target/debug/one", "/w/target/debug/one", "/usr/bin/env", "target/rargo-externals/x"] {
            assert_eq!(client_program(inside, "/w", &tr), inside);
        }
        assert_eq!(
            client_paths("/w/target/rargo-external/debug/deps:/w/target/debug/deps:/usr/lib", &tr),
            "/tmp/x/one-out/debug/deps:/w/target/debug/deps:/usr/lib"
        );
    }

    #[test]
    fn test_status_lines_name_binaries_in_an_external_target_dir_by_their_client_path() {
        let mut env = BTreeMap::from([("CARGO_TARGET_DIR".into(), "/tmp/x/td-out".into())]);
        let tr = build_paths("/root/td", "/root/.cargo", &mut env);
        for (msg, expected) in [
            (
                "unittests src/main.rs (target/rargo-external/debug/build/td/3f/out/td-3f)",
                "unittests src/main.rs (/tmp/x/td-out/debug/build/td/3f/out/td-3f)",
            ),
            ("`/w/target/rargo-external/debug/deps/td-3f --nocapture`", "`/tmp/x/td-out/debug/deps/td-3f --nocapture`"),
            ("unittests src/main.rs (target/debug/deps/td-3f)", "unittests src/main.rs (target/debug/deps/td-3f)"),
            ("td", "td"),
            ("a  (b) ``", "a  (b) ``"),
        ] {
            assert_eq!(client_text(msg, "/w", &tr), expected, "{msg}");
        }
    }

    #[test]
    fn target_dir_does_not_change_the_workspace_mapping() {
        for (target, expected) in [
            ("relative", "relative"),
            ("/tmp/worktree/output", "/w/output"),
            ("/tmp", "/w/target/rargo-external"),
        ] {
            let mut env = BTreeMap::from([("CARGO_TARGET_DIR".into(), target.into())]);
            let tr = build_paths("/tmp/worktree", "/home/user/.cargo", &mut env);
            assert_eq!(env["CARGO_TARGET_DIR"], expected);
            assert_eq!(tr.to_canonical("/tmp/worktree/src"), "/w/src");
        }
    }

    const BUILD_SCRIPTS_PLAN: &str = "\
roots 0 1
Compile lib netprobe deps=2 rmeta=/w/target/debug/build/netprobe/330904e69fbcd93e/out/libnetprobe-330904e69fbcd93e.rmeta
Compile lib netprobe2 deps=4 rmeta=/w/target/debug/build/netprobe2/ead368810fd1997a/out/libnetprobe2-ead368810fd1997a.rmeta
RunScript link netprobe deps=3
Compile link netprobe-build-script
RunScript link netprobe2 deps=5
Compile link netprobe2-build-script
";

    const DOC_PLAN: &str = "\
roots 0
Doc link app deps=1,2 doc=/w/target/doc
Compile lib util deps=3,4 rmeta=/w/target/debug/build/util/ef22b685782687f6/out/libutil-ef22b685782687f6.rmeta
Doc lib util deps=3,6,4 doc=/w/target/doc
Compile link mac
RunScript link util deps=5
Compile link util-build-script
Doc link mac doc=/w/target/doc
";

    fn plan(text: &str) -> (Vec<rp::Unit>, Vec<u32>) {
        let mut lines = text.lines();
        let roots =
            lines.next().unwrap().strip_prefix("roots ").unwrap().split(' ').map(|r| r.parse().unwrap()).collect();
        let units = lines
            .map(|line| {
                let mut words = line.split(' ');
                let kind = match words.next().unwrap() {
                    "Compile" => rp::UnitKind::Compile,
                    "Doc" => rp::UnitKind::Doc,
                    "RunScript" => rp::UnitKind::RunScript,
                    _ => rp::UnitKind::Noop,
                };
                let needs_objects = words.next().unwrap() == "link";
                let name = words.next().unwrap().to_string();
                let (mut deps, mut outputs, mut cmd) = (Vec::new(), Vec::new(), None);
                for w in words {
                    let (k, v) = w.split_once('=').unwrap();
                    match k {
                        "deps" => {
                            deps = v
                                .split(',')
                                .map(|d| rp::Dep {
                                    unit: d.trim_end_matches('m').parse().unwrap(),
                                    only_rmeta: d.ends_with('m'),
                                })
                                .collect()
                        }
                        "rmeta" => outputs.push(rp::OutFile {
                            path: v.into(),
                            hardlink: None,
                            export_path: None,
                            flavor: "Rmeta".into(),
                        }),
                        "doc" => {
                            cmd = Some(rp::Cmd {
                                program: "rustdoc".into(),
                                args: vec!["--crate-name".into(), name.clone(), "-o".into(), v.into()],
                                env: Vec::new(),
                                cwd: None,
                            })
                        }
                        _ => panic!("unknown field {k}"),
                    }
                }
                rp::Unit {
                    kind,
                    cmd,
                    deps,
                    outputs,
                    rmeta_early: false,
                    needs_objects,
                    pkg: name.clone(),
                    pkg_id: name.clone(),
                    pkg_name: name.clone(),
                    pkg_version: "0.1.0".into(),
                    desc: name,
                    verb: rp::Verb::Compiling,
                    is_local: true,
                    repository: None,
                    show_warnings: true,
                    fix_hint: None,
                    pkg_spec: String::new(),
                    manifest_path: String::new(),
                    target_json: String::new(),
                    profile_json: String::new(),
                    features: Vec::new(),
                    executable: None,
                    script: None,
                    script_metas: Vec::new(),
                    scrape_can_fail: false,
                }
            })
            .collect();
        (units, roots)
    }

    fn shown(lv: &[u8]) -> String {
        lv.iter().map(|&l| ['.', 'm', 'F'][l as usize]).collect()
    }

    fn render(text: &str) -> String {
        let (units, roots) = plan(text);
        let facts = plan_facts(&units, &roots);
        let mut out = format!("doc order {:?}\n", facts.doc_order);
        for (i, u) in units.iter().enumerate() {
            out += &format!(
                "{i} {} meta={} levels={} meta_levels={}\n",
                u.desc,
                facts.can_meta(i),
                shown(&levels(&units, &facts, i)),
                shown(&meta_levels(&units, &facts, i)),
            );
        }
        let need = needs(&units, &roots, &|i| Arc::new(meta_levels(&units, &facts, i)), &|i| {
            Arc::new(levels(&units, &facts, i))
        });
        out += &format!("needs {}\n", shown(&need));
        out
    }

    #[track_caller]
    fn snapshot(actual: &str, expected: &str) {
        assert!(actual == expected, "snapshot differs; actual:\n{actual}");
    }

    #[test]
    fn a_plan_of_two_libraries_with_build_scripts() {
        snapshot(
            &render(BUILD_SCRIPTS_PLAN),
            "\
doc order []
0 netprobe meta=true levels=..FF.. meta_levels=..FF..
1 netprobe2 meta=true levels=....FF meta_levels=....FF
2 netprobe meta=false levels=...F.. meta_levels=...F..
3 netprobe-build-script meta=false levels=...... meta_levels=......
4 netprobe2 meta=false levels=.....F meta_levels=.....F
5 netprobe2-build-script meta=false levels=...... meta_levels=......
needs mmFFFF
",
        );
    }

    #[test]
    fn a_plan_documenting_a_workspace() {
        snapshot(
            &render(DOC_PLAN),
            "\
doc order [6, 2, 0]
0 app meta=false levels=.FFFFFF meta_levels=.mFFFFF
1 util meta=true levels=...FFF. meta_levels=...FFF.
2 util meta=false levels=...FFFF meta_levels=...FFFF
3 mac meta=false levels=....... meta_levels=.......
4 util meta=false levels=.....F. meta_levels=.....F.
5 util-build-script meta=false levels=....... meta_levels=.......
6 mac meta=false levels=....... meta_levels=.......
needs FFFFFFF
",
        );
    }

    #[test]
    fn an_early_metadata_dependency_is_seen_at_metadata_unless_the_unit_links() {
        let text =
            "roots 0 1\nCompile lib top deps=2m\nCompile link bin deps=2m\nCompile lib dep deps=3\nCompile lib leaf\n";
        let (mut units, roots) = plan(text);
        units[2].rmeta_early = true;
        units[3].rmeta_early = true;
        units[2].outputs.push(rp::OutFile {
            path: "/w/d.rmeta".into(),
            hardlink: None,
            export_path: None,
            flavor: "Rmeta".into(),
        });
        let facts = plan_facts(&units, &roots);
        let seen = |i| shown(&levels(&units, &facts, i));
        snapshot(&format!("{} {}", seen(0), seen(1)), "..mm ..FF");
    }

    const RUSTUP_MISSING_MIRI: &[u8] =
        b"error: 'cargo-miri' is not installed for the toolchain 'nightly-x86_64-unknown-linux-gnu'.\n\
help: run `rustup component add --toolchain nightly-x86_64-unknown-linux-gnu miri` to install it\n";

    const RUSTUP_MISSING_CLIPPY: &[u8] =
        b"error: 'cargo-clippy' is not installed for the toolchain 'stable-x86_64-unknown-linux-gnu'.\n\
help: run `rustup component add clippy` to install it\n";

    const RUSTUP_NO_SUCH_COMPONENT: &[u8] = b"error: toolchain 'stable-x86_64-unknown-linux-gnu' does not contain component 'no-such-component' for target 'x86_64-unknown-linux-gnu'\n";

    const RUSTUP_UNKNOWN_BINARY: &[u8] =
        b"error: Unknown binary 'rust-analyzer' in official toolchain 'stable-x86_64-unknown-linux-gnu'.\n";

    const CARGO_OFFLINE_RESOLUTION: &[u8] = b"error: no matching package named `anyhow` found\n\
location searched: crates.io index\n\
required by package `flaky v0.1.0 (/w)`\n\
note: offline mode (via `--offline`) can sometimes cause surprising resolution failures\n\
help: if this error is too confusing you may wish to retry without `--offline`\n";

    const RUSTC_ERROR: &[u8] = b"error[E0425]: cannot find value `x` in this scope\n --> src/main.rs:2:13\n  |\n2 |     let y = x;\n  |             ^ not found in this scope\n\n\
For more information about this error, try `rustc --explain E0425`.\n\
error: could not compile `app` (bin \"app\") due to 1 previous error\n";

    const SHADY_RUSTUP: &[u8] =
        b"error: 'x' is not installed for the toolchain 'x;rm -rf'\nhelp: run `rustup component add a` to install it\n";

    #[test]
    fn what_a_failure_s_stderr_asks_for() {
        let cases: [(&str, &[u8]); 8] = [
            ("rustup, miri on nightly", RUSTUP_MISSING_MIRI),
            ("rustup, clippy and rustfmt", &[RUSTUP_MISSING_CLIPPY, RUSTUP_MISSING_MIRI].concat()),
            ("rustup, no such component", RUSTUP_NO_SUCH_COMPONENT),
            ("rustup, unknown binary", RUSTUP_UNKNOWN_BINARY),
            ("cargo, offline resolution", CARGO_OFFLINE_RESOLUTION),
            ("cargo, an uncached install", b"error: could not find `ripgrep` in registry `crates-io` with version `=14.1.1`\n"),
            ("rustc, a compile error", RUSTC_ERROR),
            ("a toolchain name that is not a name", SHADY_RUSTUP),
        ];
        let mut out = String::new();
        for (what, stderr) in cases {
            out += &format!("{what}: components {:?}, network {}\n", missing_components(stderr), needs_network(stderr));
        }
        snapshot(&out, "\
rustup, miri on nightly: components [(\"miri\", \"nightly-x86_64-unknown-linux-gnu\")], network true
rustup, clippy and rustfmt: components [(\"clippy\", \"stable-x86_64-unknown-linux-gnu\"), (\"miri\", \"nightly-x86_64-unknown-linux-gnu\")], network true
rustup, no such component: components [], network false
rustup, unknown binary: components [], network false
cargo, offline resolution: components [], network true
cargo, an uncached install: components [], network true
rustc, a compile error: components [], network false
a toolchain name that is not a name: components [], network true
");
    }

    #[test]
    fn the_rustup_calls_that_add_components_and_what_their_failure_says() {
        let pair = |c: &str, t: &str| (c.to_string(), t.to_string());
        let adds = rustup_adds(&[pair("miri", "nightly-x86_64-unknown-linux-gnu"), pair("clippy", "stable")]);
        let mut out: String = adds.iter().map(|a| format!("rustup {}\n", a.join(" "))).collect();
        for ran in
            [Ok((true, &b""[..])), Ok((false, RUSTUP_NO_SUCH_COMPONENT)), Err("No such file or directory (os error 2)")]
        {
            out += &format!("{:?}\n", rustup_failure(ran).map(|s| String::from_utf8(s).unwrap()));
        }
        snapshot(&out, "\
rustup component add miri --toolchain nightly-x86_64-unknown-linux-gnu
rustup component add clippy --toolchain stable
None
Some(\"error: toolchain 'stable-x86_64-unknown-linux-gnu' does not contain component 'no-such-component' for target 'x86_64-unknown-linux-gnu'\\n\")
Some(\"error: running rustup: No such file or directory (os error 2)\\n\")
");
    }

    #[test]
    fn build_script_lines_that_are_warnings() {
        let stdout = "cargo:rerun-if-changed=build.rs\n\
cargo:warning=script-started\n\
  cargo::warning=hello  \r\n\
cargo:warning=\n\
cargo:rustc-cfg=has_generated\n\
cargo::rustc-env=GENERATED=1\n\
not a directive\n";
        let out: String = stdout.lines().map(|l| format!("{:?}\n", script_warning_message(l))).collect();
        snapshot(
            &out,
            "\
None
Some(\"script-started\")
Some(\"hello\")
Some(\"\")
None
None
None
",
        );
    }

    #[test]
    fn where_the_subcommand_is_in_cargo_s_arguments() {
        let cases = [
            "build --release",
            "+nightly build",
            "--config build.jobs=2 -Z unstable-options check",
            "--color always -C sub nextest run",
            "-v --explain E0425",
            "--locked -- build",
            "--frozen",
            "",
        ];
        let mut out = String::new();
        for c in cases {
            let args: Vec<String> = c.split_whitespace().map(String::from).collect();
            out += &format!("{c:?}: {:?}\n", subcommand_pos(&args).map(|i| &args[i]));
        }
        snapshot(
            &out,
            "\
\"build --release\": Some(\"build\")
\"+nightly build\": Some(\"+nightly\")
\"--config build.jobs=2 -Z unstable-options check\": Some(\"check\")
\"--color always -C sub nextest run\": Some(\"nextest\")
\"-v --explain E0425\": None
\"--locked -- build\": None
\"--frozen\": None
\"\": None
",
        );
    }

    #[test]
    fn the_environment_actions_start_from() {
        let map = |kv: &[(&str, &str)]| -> BTreeMap<String, String> {
            kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let server = map(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/rargod"),
            ("NIX_PATH", "nixpkgs=/n"),
            ("RUSTUP_HOME", "/r"),
            ("SECRET", "s"),
            ("LANG", "C"),
        ]);
        let client = map(&[
            ("RUSTFLAGS", "-Dwarnings"),
            ("CARGO_INCREMENTAL", "0"),
            ("CARGO_PKG_NAME", "app"),
            ("CARGO_HOME", "/home/me/.cargo"),
            ("RUSTC_WRAPPER", "sccache"),
            ("TERM", "xterm"),
            ("RARGO_SERVER", "rargo.rs"),
            ("RARGO_ANYTHING", "x"),
            ("LC_ALL", "C.UTF-8"),
            ("EDITOR", "vi"),
            ("CARGO_REGISTRY_TOKEN", "t"),
            ("PATH", "/home/me/bin"),
        ]);
        let out: String = base_env(&server, &client).iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        snapshot(
            &out,
            "\
CARGO_HOME=/c
CARGO_INCREMENTAL=0
HOME=/home/rargod
LC_ALL=C.UTF-8
NIX_PATH=nixpkgs=/n
PATH=/usr/bin
RUSTC_WORKSPACE_WRAPPER=
RUSTC_WRAPPER=
RUSTFLAGS=-Dwarnings
RUSTUP_HOME=/r
",
        );
    }
}
