//! `rargo nextest`: nextest runs on the server, against a build of rargo's
//! own (D81).
//!
//! nextest asks cargo two things: the workspace's metadata, and a build of
//! the test binaries (`cargo test --no-run --message-format
//! json-render-diagnostics ...`), whose messages name the binaries to run.
//! In rargo's sandbox the cargo nextest asks is a stand-in that gives the
//! answers rargod has. An answer it does not have yet is a question: the
//! stand-in writes it down and the run stops there. rargod answers (the
//! metadata from cargo, the build from its own pipeline, so every compile
//! is shared with `rargo test` and `rargo build`) and runs nextest again.
//! The run with every answer at hand is the one the user sees. From there
//! nextest decides everything: filters, retries, partitions, profiles and
//! the files it writes are its own.

use super::*;
use std::io::Read;

/// The stand-in and its answers, in the sandbox.
const AT: &[u8] = b"/rargo/nextest";
/// nextest itself, in the sandbox.
const NEXTEST_AT: &[u8] = b"/rargo/cargo-nextest";
/// Where the stand-in writes a question it has no answer to.
const ASKED: &str = "/.rargo-nextest-asked";
/// The real cargo, for whatever else asks the stand-in (a test that runs
/// cargo itself).
const REAL_CARGO_VAR: &str = "__RARGO_NEXTEST_CARGO";

const STAND_IN: &str = r#"#!/bin/sh
# cargo, as nextest sees it in rargo's sandbox (`rargo nextest`, D81):
# rargod's answers to what nextest asks cargo (the workspace's metadata and
# a build of the test binaries), and the real cargo for anything else.
what=
for a in "$@"; do
    case $a in
        -*) ;;
        metadata) what=metadata; break ;;
        test) what=build; break ;;
        *) break ;;
    esac
done
at=/rargo/nextest/$what
if [ -n "$what" ] && [ -f "$at.args" ]; then
    if [ "$(printf '%s\n' "$@")" = "$(cat "$at.args")" ]; then
        cat "$at.out" && cat "$at.err" >&2 || exit 101
        read code <"$at.code"
        exit "$code"
    fi
elif [ -n "$what" ]; then
    { printf '%s\n' "$what"; printf '%s\0' "$@"; } >/.rargo-nextest-asked
    exit 101
fi
exec "$__RARGO_NEXTEST_CARGO" "$@"
"#;

/// What cargo said, or what rargod says in its place.
#[derive(Clone, Debug)]
struct Answer {
    args: Vec<String>,
    out: Vec<u8>,
    err: Vec<u8>,
    code: i32,
}

#[derive(Default)]
struct Answers {
    metadata: Option<Answer>,
    build: Option<Answer>,
}

#[derive(Debug, PartialEq)]
enum Question {
    Metadata(Vec<String>),
    Build(Vec<String>),
}

/// The stand-in's note: `metadata` or `build`, a newline, then the
/// arguments, each ended by a NUL.
fn parse_question(raw: &[u8]) -> Option<Question> {
    let nl = raw.iter().position(|&b| b == b'\n')?;
    let args: Vec<String> = raw[nl + 1..]
        .split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let args = match args.split_last() {
        Some((last, rest)) if last.is_empty() => rest.to_vec(),
        _ => return None,
    };
    match &raw[..nl] {
        b"metadata" => Some(Question::Metadata(args)),
        b"build" => Some(Question::Build(args)),
        _ => None,
    }
}

pub(super) async fn run(d: &Arc<Daemon>, c: &Arc<Client>) -> Result<i32> {
    let mut args = c.args.clone();
    // rargo's own flag; nextest never sees it.
    let mut no_cache = false;
    if let Some(pos) = args.iter().take_while(|a| *a != "--").position(|a| a == "--no-cache") {
        args.remove(pos);
        no_cache = true;
    }
    // What cargo would hand the `cargo-nextest` program: everything after
    // the subcommand's name.
    let at = subcommand_pos(&args).context("no subcommand")?;
    let nextest = installed(d).await?;
    let mut p = prepare(d, c, c.snapshot.clone()).await?;
    let mut argv = vec![NEXTEST_AT.to_vec(), b"nextest".to_vec()];
    for a in &args[at + 1..] {
        argv.push(bytes(&canonical_arg(&p.tr, a)));
    }

    let mut answers = Answers::default();
    let mut fetched = false;
    let build_args = loop {
        let base = with_nextest(d, &p, &nextest);
        let view: Arc<dyn View> = Arc::new(BuildView {
            base,
            layer: Arc::new(OutputsLayer::new()),
            levels: Arc::new(Vec::new()),
            untraced: Vec::new(),
            seed: Some(answer_layer(d, &answers)?),
            content: Some(d.engine.content.clone()),
        });
        let env = action_env(&p.env, None, c);
        let r = d
            .engine
            .obtain(Request {
                spec: spec(&p, argv.clone(), env, NetMode::Loopback),
                view,
                client_env: c.env.clone(),
                uid: c.uid,
                gid: c.gid,
                prio: c.prio,
                no_cache: false,
                seed_blobs: Default::default(),
                // A run that stopped at a question asks it again on the
                // same inputs.
                cacheable: Arc::new(|r| r.outputs.get(ASKED.as_bytes()).is_some()),
                declared_env: Arc::new(|_, _| Vec::new()),
                observer: Arc::new(NullObserver),
            })
            .await?
            .result;
        let question = match r.outputs.get(ASKED.as_bytes()) {
            Some(MEntry::File { hash, .. }) => parse_question(&d.store().read(hash)?),
            _ => None,
        };
        match question {
            Some(Question::Metadata(m)) => {
                let a = metadata(d, c, &p, m).await?;
                if a.code != 0 && !fetched && needs_network(&a.err) {
                    fetched = true;
                    let Some(snapshot) = fetch(d, c, &p, &a.args, &a.err).await? else {
                        return Ok(101);
                    };
                    p = prepare(d, c, snapshot).await?;
                    continue;
                }
                answers.metadata = Some(a);
            }
            Some(Question::Build(b)) => break b,
            None => {
                // nextest finished without needing a build (`--version`,
                // `show-config`, a mistake on the command line, cargo's
                // metadata refused): what it said is the answer.
                let store = d.store();
                c.out.raw_out(p.tr.text(&String::from_utf8_lossy(&store.read(&r.stdout)?)).as_bytes());
                c.out.err_text(&String::from_utf8_lossy(&store.read(&r.stderr)?));
                return Ok(r.code().unwrap_or(101));
            }
        }
    };

    let (answer, built) = build(d, c, build_args).await?;
    answers.build = Some(answer);

    // The run the user sees: against the build's results.
    let base = with_nextest(d, &p, &nextest);
    let (layer, levels) = match &built {
        Some(b) => (b.layer.clone(), Arc::new(vec![LEVEL_FULL; b.units])),
        None => (Arc::new(OutputsLayer::new()), Arc::new(Vec::new())),
    };
    let view: Arc<dyn View> = Arc::new(BuildView {
        base,
        layer,
        levels,
        untraced: Vec::new(),
        seed: Some(answer_layer(d, &answers)?),
        content: Some(d.engine.content.clone()),
    });
    let env = match &built {
        Some(b) => action_env(&b.env, b.cargo_override.as_deref(), c),
        None => action_env(&p.env, None, c),
    };
    let net = match &built {
        Some(b) if b.local_packages.iter().any(|pkg| p.policy.test_network(pkg)) => NetMode::Host,
        _ => NetMode::Loopback,
    };
    let outcome = d
        .engine
        .obtain(Request {
            spec: spec(&p, argv, env, net),
            view,
            client_env: c.env.clone(),
            uid: c.uid,
            gid: c.gid,
            prio: c.prio,
            no_cache,
            seed_blobs: Default::default(),
            // A passing run is a fact about its inputs; a failing one may
            // be a flake and is run again.
            cacheable: Arc::new(|r| r.success()),
            declared_env: Arc::new(|_, _| Vec::new()),
            observer: Arc::new(TestObserver {
                out: c.clone(),
                explain: c.env.contains_key("RARGO_EXPLAIN"),
                label: "nextest".into(),
            }),
        })
        .await?;
    let r = &outcome.result;

    // What nextest and the tests wrote: the worktree part as tests' writes
    // go, the target directory's files (a JUnit report) as artifacts do.
    let target_dirs: Vec<String> = match &built {
        Some(b) => b.target_dirs.clone(),
        None => vec![String::from_utf8_lossy(&vjoin(WORKTREE_AT, b"target")).into_owned()],
    };
    let dirs: Vec<&String> = target_dirs.iter().collect();
    apply_writes(c, &p.tr, &dirs, &r.outputs);
    let files: Vec<Delivery> = r
        .outputs
        .entries
        .iter()
        .filter(|(path, _)| target_dirs.iter().any(|t| vpath_starts_with(path, t.as_bytes())))
        .filter_map(|(path, e)| match e {
            MEntry::File { hash, exec, size } => Some(Delivery {
                path: p.tr.path(&String::from_utf8_lossy(path)),
                hash: hash.0,
                size: *size,
                exec: *exec,
                own: true,
            }),
            _ => None,
        })
        .collect();
    c.deliver(files, Vec::new());
    match r.code() {
        Some(code) => Ok(code),
        None => {
            c.out.error(&format!("nextest did not exit normally ({})", describe_status(r.status)));
            Ok(101)
        }
    }
}

/// An argument as the sandbox sees it: paths the user typed refer to the
/// real worktree.
fn canonical_arg(tr: &Translator, a: &str) -> String {
    match a.split_once('=') {
        Some((flag, val)) if flag.starts_with("--") && val.starts_with('/') => format!("{flag}={}", tr.to_canonical(val)),
        _ if a.starts_with('/') => tr.to_canonical(a),
        _ => a.to_string(),
    }
}

/// The views' base, with nextest in it.
fn with_nextest(d: &Daemon, p: &Prepared, nextest: &Path) -> Arc<BaseView> {
    let mut parts = p.parts.clone();
    parts.tools.push((NEXTEST_AT.to_vec(), nextest.to_path_buf()));
    Arc::new(parts.build(d.memo.clone(), p.generation, false))
}

fn spec(p: &Prepared, argv: Vec<Vec<u8>>, env: BTreeMap<Vec<u8>, Vec<u8>>, net: NetMode) -> ActionSpec {
    ActionSpec {
        kind: ActionKind::Nextest,
        argv,
        env,
        cwd: bytes(&p.sandbox_cwd),
        net,
        mkdirs: vec![],
        root_id: p.flake.as_ref().map(|f| f.id).unwrap_or(Hash::ZERO),
        discard: vec![],
        label: "nextest".into(),
    }
}

/// nextest's environment: the build's, with the stand-in as its cargo.
fn action_env(base: &BTreeMap<String, String>, cargo: Option<&str>, c: &Client) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut env: BTreeMap<Vec<u8>, Vec<u8>> = base.iter().map(|(k, v)| (bytes(k), bytes(v))).collect();
    env.insert(bytes("CARGO"), vjoin(AT, b"cargo"));
    let real = cargo.map(bytes).unwrap_or_else(|| PLANNER_AT.to_vec());
    env.insert(bytes(REAL_CARGO_VAR), real);
    // nextest colours what it prints when the client's terminal would show
    // colour; the sandbox is no terminal.
    if c.out.color_err {
        env.insert(bytes("CARGO_TERM_COLOR"), bytes("always"));
    }
    env
}

/// The stand-in, and the answers rargod has, as files for the sandbox.
fn answer_layer(d: &Daemon, answers: &Answers) -> Result<Arc<OutputsLayer>> {
    let store = d.store();
    let mut entries = Vec::new();
    let mut file = |name: &str, content: &[u8], exec: bool| -> Result<()> {
        let hash = store.put_bytes(content)?;
        entries.push((vjoin(AT, name.as_bytes()), MEntry::File { hash, exec, size: content.len() as u64 }));
        Ok(())
    };
    file("cargo", STAND_IN.as_bytes(), true)?;
    for (what, a) in [("metadata", &answers.metadata), ("build", &answers.build)] {
        let Some(a) = a else { continue };
        let mut args = a.args.join("\n");
        args.push('\n');
        file(&format!("{what}.args"), args.as_bytes(), false)?;
        file(&format!("{what}.out"), &a.out, false)?;
        file(&format!("{what}.err"), &a.err, false)?;
        file(&format!("{what}.code"), format!("{}\n", a.code).as_bytes(), false)?;
    }
    let layer = OutputsLayer::new();
    layer.add(0, LEVEL_FULL, &entries);
    Ok(Arc::new(layer))
}

/// `cargo metadata` as nextest asked for it, from the cargo rargo plans
/// with (it sees the host, like planning).
async fn metadata(d: &Arc<Daemon>, c: &Arc<Client>, p: &Prepared, args: Vec<String>) -> Result<Answer> {
    let mut env: BTreeMap<Vec<u8>, Vec<u8>> = p.env.iter().map(|(k, v)| (bytes(k), bytes(v))).collect();
    env.insert(bytes("CARGO_NET_OFFLINE"), bytes("true"));
    let mut argv = vec![PLANNER_AT.to_vec()];
    argv.extend(args.iter().map(|a| bytes(a)));
    let spec = ActionSpec {
        kind: ActionKind::Plan,
        argv,
        env,
        cwd: bytes(&p.sandbox_cwd),
        net: NetMode::Loopback,
        mkdirs: vec![],
        root_id: p.flake.as_ref().map(|f| f.id).unwrap_or(Hash::ZERO),
        discard: vec![],
        label: "cargo metadata (for nextest)".into(),
    };
    let r = d
        .engine
        .obtain(Request {
            spec,
            view: p.plan_base.clone(),
            client_env: c.env.clone(),
            uid: c.uid,
            gid: c.gid,
            prio: c.prio,
            no_cache: false,
            seed_blobs: Default::default(),
            cacheable: Arc::new(|r| r.success()),
            declared_env: Arc::new(|_, _| Vec::new()),
            observer: Arc::new(PlanObserver { client: c.clone() }),
        })
        .await?
        .result;
    let store = d.store();
    Ok(Answer { args, out: store.read(&r.stdout)?, err: store.read(&r.stderr)?, code: r.code().unwrap_or(101) })
}

/// The build nextest asked for, by rargo's pipeline. What it prints on
/// standard error goes to the client as it happens; its messages are the
/// answer.
async fn build(d: &Arc<Daemon>, c: &Arc<Client>, args: Vec<String>) -> Result<(Answer, Option<Built>)> {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let cb = Arc::new(Client {
        args: args.clone(),
        root: c.root.clone(),
        cwd: c.cwd.clone(),
        cargo_home: c.cargo_home.clone(),
        env: c.env.clone(),
        uid: c.uid,
        gid: c.gid,
        prio: c.prio,
        out: c.out.keeping_stdout(kept.clone()),
        session: c.session.clone(),
        snapshot: c.snapshot.clone(),
        worktree: c.worktree.clone(),
        commits: c.commits.clone(),
        home: c.home.clone(),
        outbox: c.outbox.clone(),
    });
    let keep = Mutex::new(None);
    let mut code = 101;
    for attempt in 0..3 {
        kept.lock().clear();
        match run_request(d, &cb, Some(&keep)).await {
            Ok(Finish::Code(k)) => code = k,
            Ok(Finish::Retry) if attempt < 2 => {
                cb.outbox.lock().0.clear();
                continue;
            }
            Ok(Finish::Retry) => c.out.error("the build kept producing inconsistent intermediate artifacts"),
            Ok(Finish::Exec(_)) => bail!("the build nextest asked for wants to run a program on the client"),
            Err(e) => c.out.error(&format!("{e:#}")),
        }
        break;
    }
    // The messages name files as the client knows them; nextest reads them
    // in the sandbox.
    let back = Translator::new(vec![
        (c.root.clone(), String::from_utf8_lossy(WORKTREE_AT).into_owned()),
        (c.cargo_home.clone(), String::from_utf8_lossy(CARGO_HOME_AT).into_owned()),
    ]);
    let out = back.text(&String::from_utf8_lossy(&kept.lock())).into_bytes();
    Ok((Answer { args, out, err: Vec::new(), code }, keep.into_inner()))
}

// nextest on the server.

const RELEASES: &str = "https://github.com/nextest-rs/nextest/releases";

/// nextest as the server has it: the newest release it has installed, or,
/// on first use, the newest there is. It is the statically linked build,
/// which runs in any root (a Nix shell's too), from nextest's releases on
/// GitHub, checked against the release's SHA-256 sum.
async fn installed(d: &Daemon) -> Result<PathBuf> {
    static INSTALLING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let dir = d.cfg.state_dir.join("nextest");
    if let Some(p) = newest(&dir) {
        return Ok(p);
    }
    let _one_at_a_time = INSTALLING.lock().await;
    if let Some(p) = newest(&dir) {
        return Ok(p);
    }
    tokio::task::spawn_blocking(move || install_latest(&dir))
        .await?
        .context("nextest is not installed on this server, and installing it failed")
}

fn version_of(name: &str) -> Option<Vec<u64>> {
    let v: Option<Vec<u64>> = name.split('.').map(|n| n.parse().ok()).collect();
    v.filter(|v| v.len() == 3)
}

/// `DIR/VERSION/cargo-nextest` of the highest version.
fn newest(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let bin = e.path().join("cargo-nextest");
            Some((version_of(&name)?, bin)).filter(|(_, b)| b.is_file())
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, b)| b)
}

fn agent(follow: bool) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(if follow { 5 } else { 0 })
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(concat!("rargod/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn install_latest(dir: &Path) -> Result<PathBuf> {
    // The newest release's page answers with a redirect to its tag.
    let url = format!("{RELEASES}/latest");
    let r = match agent(false).get(&url).call() {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => bail!("asking {url} for the newest release: {e}"),
    };
    let tag = r
        .header("location")
        .and_then(|l| l.rsplit('/').next())
        .map(str::to_string)
        .with_context(|| format!("{url} did not name the newest release (HTTP {})", r.status()))?;
    let version = tag
        .strip_prefix("cargo-nextest-")
        .filter(|v| version_of(v).is_some())
        .with_context(|| format!("{url} names `{tag}`, which is not a nextest release"))?;
    install(dir, version)
}

fn install(dir: &Path, version: &str) -> Result<PathBuf> {
    let triple = format!("{}-unknown-linux-musl", std::env::consts::ARCH);
    let base = format!("{RELEASES}/download/cargo-nextest-{version}/cargo-nextest-{version}-{triple}");
    let get = |url: &str| -> Result<Vec<u8>> {
        let mut body = Vec::new();
        agent(true)
            .get(url)
            .call()
            .with_context(|| format!("downloading {url}"))?
            .into_reader()
            .take(512 << 20)
            .read_to_end(&mut body)
            .with_context(|| format!("downloading {url}"))?;
        Ok(body)
    };
    let sums = String::from_utf8(get(&format!("{base}.sha256"))?).unwrap_or_default();
    let want = sums.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    let archive = get(&format!("{base}.tar.gz"))?;
    let got = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(&archive))
    };
    if got != want {
        bail!("{base}.tar.gz does not match the release's SHA-256 sum: it is {got}, {base}.sha256 says {want:?}");
    }
    let mut program = None;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&archive[..]));
    for e in ar.entries().with_context(|| format!("unpacking {base}.tar.gz"))? {
        let mut e = e?;
        if e.path()?.file_name().is_some_and(|n| n == "cargo-nextest") {
            let mut b = Vec::new();
            e.read_to_end(&mut b)?;
            program = Some(b);
        }
    }
    let program = program.with_context(|| format!("{base}.tar.gz holds no `cargo-nextest`"))?;
    let tmp = dir.join(format!(".{version}.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = tmp.join("cargo-nextest");
        std::fs::write(&bin, &program)?;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    }
    let to = dir.join(version);
    if let Err(e) = std::fs::rename(&tmp, &to) {
        let _ = std::fs::remove_dir_all(&tmp);
        if !to.join("cargo-nextest").is_file() {
            return Err(e).with_context(|| format!("installing nextest {version} at {}", to.display()));
        }
    }
    Ok(to.join("cargo-nextest"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stand_in_s_question_is_read_back() {
        let mut raw = b"build\n".to_vec();
        for a in ["--color=auto", "test", "--no-run", "--message-format=json-render-diagnostics", "-p", "a b"] {
            raw.extend_from_slice(a.as_bytes());
            raw.push(0);
        }
        assert_eq!(
            parse_question(&raw),
            Some(Question::Build(
                ["--color=auto", "test", "--no-run", "--message-format=json-render-diagnostics", "-p", "a b"]
                    .map(String::from)
                    .to_vec()
            ))
        );
        assert_eq!(parse_question(b"metadata\nmetadata\0"), Some(Question::Metadata(vec!["metadata".into()])));
        // Cut short, or about something else: no question.
        assert_eq!(parse_question(b"build\n--color=auto\0te"), None);
        assert_eq!(parse_question(b"install\nx\0"), None);
    }

    #[test]
    fn the_newest_installed_version_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(newest(tmp.path()), None);
        for v in ["0.9.9", "0.9.152", "0.9.100", "1.0", ".0.9.200.123"] {
            std::fs::create_dir_all(tmp.path().join(v)).unwrap();
            std::fs::write(tmp.path().join(v).join("cargo-nextest"), b"").unwrap();
        }
        // A version without the program is not installed.
        std::fs::create_dir_all(tmp.path().join("0.10.0")).unwrap();
        assert_eq!(newest(tmp.path()), Some(tmp.path().join("0.9.152/cargo-nextest")));
    }
}
