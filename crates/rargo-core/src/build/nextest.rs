use super::nextest_run::{Asked, Effect as NextEffect, NextestRun, TestAsked, exit_code};
use super::*;
use std::io::Read;

const AT: &[u8] = b"/rargo/nextest";
const NEXTEST_AT: &[u8] = b"/rargo/cargo-nextest";
const ASKED: &str = "/.rargo-nextest-asked";
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

const RUNNER: &str = r#"#!/bin/sh
# A test binary, as nextest starts it in rargo's sandbox (D83): rargod's
# recorded run of that process, or, while rargod has none, a question for
# it (the process then does nothing).
q=$(printf '%s\n' "$PWD" "${NEXTEST_ATTEMPT:-1}" "$@")
read sum len <<EOF
$(printf '%s' "$q" | cksum)
EOF
for a in /rargo/nextest/t/$sum-$len/*; do
    [ -f "$a/q" ] && [ "$(cat "$a/q")" = "$q" ] || continue
    cat "$a/out" && cat "$a/err" >&2 || exit 101
    read how n <"$a/status"
    [ "$how" = signal ] && kill -"$n" $$
    exit "$n"
done
mkdir -p /.rargo-nextest-q
{ printf '%s\0' "$PWD" "${NEXTEST_ATTEMPT:-1}" "$#" "$@"; /usr/bin/env -0; } >/.rargo-nextest-q/$sum-$len-$$
"#;

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

struct TestRuns(Vec<crate::io::JoinHandle<(TestQuestion, Result<(Arc<ActionResult>, bool)>)>>);

impl Drop for TestRuns {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

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

pub(super) fn run(d: &Arc<Daemon>, c: &Arc<Client>) -> Result<i32> {
    let mut args = c.args.clone();
    let mut no_cache = false;
    if let Some(pos) = args.iter().take_while(|a| *a != "--").position(|a| a == "--no-cache") {
        args.remove(pos);
        no_cache = true;
    }
    let at = subcommand_pos(&args).context("no subcommand")?;
    let mut p = prepare(d, c, c.snapshot.clone())?;
    let nextest = installed(d, c, wanted(c, &p)?)?;
    let mut argv = vec![NEXTEST_AT.to_vec(), b"nextest".to_vec()];
    for a in &args[at + 1..] {
        argv.push(bytes(&canonical_arg(&p.tr, a)));
    }

    let mut answers = Answers::default();
    let (mut run, first) = NextestRun::new();
    let mut todo: std::collections::VecDeque<NextEffect> = first.into();
    let mut built: Option<Built> = None;
    let mut rounds: Option<Rounds> = None;
    let mut ran: HashMap<String, Ran> = HashMap::new();
    let mut last: Option<Arc<ActionResult>> = None;
    while let Some(effect) = todo.pop_front() {
        let next = match effect {
            NextEffect::RunNextest => match &rounds {
                None => {
                    let r = ask(d, c, &p, &nextest, &argv, &answers)?;
                    let question = match r.outputs.get(ASKED.as_bytes()) {
                        Some(MEntry::File { hash, .. }) => parse_question(&d.store().read(hash)?),
                        _ => None,
                    };
                    let asked = match question {
                        Some(Question::Metadata(m)) => Asked::Metadata(m),
                        Some(Question::Build(b)) => Asked::Build(b),
                        None => Asked::Nothing,
                    };
                    let code = r.code();
                    last = Some(r);
                    run.asked(asked, code)
                }
                Some(rounds) => {
                    let r = rounds.nextest(d, c, &p, &argv, &answers, &ran)?;
                    let asked: Vec<TestAsked> = rounds
                        .asked
                        .lock()
                        .iter()
                        .map(|q| TestAsked { key: q.key.clone(), listing: q.listing })
                        .collect();
                    last = Some(r);
                    run.round_ended(asked)
                }
            },
            NextEffect::Metadata(m) => {
                let a = metadata(d, c, &p, m)?;
                let fx = run.metadata_answered(&a.args, a.code, &a.err);
                answers.metadata = Some(a);
                fx
            }
            NextEffect::Fetch { args, components, offline } => {
                answers.metadata = None;
                match fetch(d, c, &p, &args, components, offline)? {
                    Some(snapshot) => {
                        p = prepare(d, c, snapshot)?;
                        run.fetched(true)
                    }
                    None => run.fetched(false),
                }
            }
            NextEffect::Prepare => Vec::new(),
            NextEffect::Build(build_args) => {
                let host = answers.metadata.as_ref().and_then(|m| {
                    m.args.iter().find_map(|a| a.strip_prefix("--filter-platform=").map(String::from))
                });
                let target = arg_value(&build_args, "--target").map(String::from);
                let (answer, b) = build(d, c, build_args)?;
                answers.build = Some(answer);
                built = b;
                rounds = Some(Rounds::new(d, c, &p, &nextest, built.as_ref(), host.iter().chain(target.iter())));
                run.built()
            }
            NextEffect::RunTests(keys) => {
                let rounds = rounds.as_ref().expect("tests run in the rounds");
                let asked = std::mem::take(&mut *rounds.asked.lock());
                let mut runs = TestRuns(Vec::new());
                for key in keys {
                    let q = asked.iter().find(|q| q.key == key).expect("the core runs what nextest asked").clone();
                    let (d, c, p_policy, view) = (d.clone(), c.clone(), p.policy.clone(), rounds.tests_view.clone());
                    let (root_id, no_cache) = (p.flake.id, no_cache);
                    runs.0.push(crate::io::spawn(move || {
                        let ran = run_test(&d, &c, &p_policy, view, root_id, no_cache, &q);
                        (q, ran)
                    }));
                }
                let mut fx = Vec::new();
                for done in runs.0.iter_mut() {
                    let (q, outcome) = done.join().map_err(|e| anyhow!("a test task panicked: {e}"))?;
                    let (result, cached) = outcome?;
                    deliver_writes(c, &p, built.as_ref(), &result.outputs);
                    let store = d.store();
                    ran.insert(
                        q.key.clone(),
                        Ran {
                            q: q.key.clone(),
                            out: store.read(&result.stdout)?,
                            err: store.read(&result.stderr)?,
                            status: match result.signal() {
                                Some(sig) => format!("signal {sig}"),
                                None => format!("exit {}", result.code().unwrap_or(101)),
                            },
                        },
                    );
                    fx.extend(run.test_ran(&q.key, cached));
                }
                fx
            }
            NextEffect::ShowNextest => {
                let r = last.as_ref().expect("nextest ran");
                let store = d.store();
                c.out.raw_out(p.tr.text(&String::from_utf8_lossy(&store.read(&r.stdout)?)).as_bytes());
                if rounds.is_some() {
                    c.out.err_text(&without_runner_note(&String::from_utf8_lossy(&store.read(&r.stderr)?)));
                } else {
                    c.out.err_text(&String::from_utf8_lossy(&store.read(&r.stderr)?));
                }
                Vec::new()
            }
            NextEffect::Finish => {
                let r = last.as_ref().expect("nextest ran");
                if let Some(text) = run.summary() {
                    c.out.status("Replayed", &text);
                }
                deliver_writes(c, &p, built.as_ref(), &r.outputs);
                let (code, why) = exit_code(r.code(), &describe_status(r.status));
                if let Some(why) = why {
                    c.out.error(&why);
                }
                return Ok(code);
            }
            NextEffect::End(code) => return Ok(code),
        };
        run.invariants();
        todo.extend(next);
    }
    unreachable!("a nextest run ends with an end or a finish, not in {:?}", run.phase())
}

fn ask(
    d: &Arc<Daemon>,
    c: &Arc<Client>,
    p: &Prepared,
    nextest: &Path,
    argv: &[Vec<u8>],
    answers: &Answers,
) -> Result<Arc<ActionResult>> {
    let base = with_nextest(d, p, nextest);
    let view: Arc<dyn View> = Arc::new(BuildView {
        base,
        layer: Arc::new(OutputsLayer::new()),
        levels: Arc::new(Vec::new()),
        untraced: Vec::new(),
        seed: Some(answer_layer(d, answers, &HashMap::new())?),
        content: Some(d.engine.content.clone()),
    });
    let env = action_env(&p.env, None, c);
    Ok(d
        .engine
        .obtain(Request {
            spec: spec(p, argv.to_vec(), env, NetMode::Loopback),
            view,
            client_env: c.env.clone(),
            uid: c.uid,
            gid: c.gid,
            prio: c.prio,
            no_cache: false,
            seed_blobs: Default::default(),
            cacheable: Arc::new(|r| r.outputs.get(ASKED.as_bytes()).is_some()),
            declared_env: Arc::new(|_, _| Vec::new()),
            observer: Arc::new(NullObserver),
        })?
        .result)
}

struct Rounds {
    base: Arc<BaseView>,
    layer: Arc<OutputsLayer>,
    levels: Arc<Vec<u8>>,
    env: BTreeMap<Vec<u8>, Vec<u8>>,
    tests_view: Arc<dyn View>,
    asked: Mutex<Vec<TestQuestion>>,
}

impl Rounds {
    fn new<'a>(
        d: &Arc<Daemon>,
        c: &Arc<Client>,
        p: &Prepared,
        nextest: &Path,
        built: Option<&Built>,
        triples: impl Iterator<Item = &'a String>,
    ) -> Rounds {
        let base = with_nextest(d, p, nextest);
        let (layer, levels) = match built {
            Some(b) => (b.layer.clone(), Arc::new(vec![LEVEL_FULL; b.units])),
            None => (Arc::new(OutputsLayer::new()), Arc::new(Vec::new())),
        };
        let mut env = match built {
            Some(b) => action_env(&b.env, b.cargo_override.as_deref(), c),
            None => action_env(&p.env, None, c),
        };
        let runner = vjoin(AT, b"runner");
        for triple in triples {
            let var = format!("CARGO_TARGET_{}_RUNNER", triple.to_uppercase().replace(['-', '.'], "_"));
            env.insert(bytes(&var), runner.clone());
        }
        if !c.env.contains_key("NEXTEST_TEST_THREADS") {
            env.insert(bytes("NEXTEST_TEST_THREADS"), bytes(&d.engine.sched.tokens().to_string()));
        }
        let tests_view: Arc<dyn View> = Arc::new(BuildView {
            base: p.base.clone(),
            layer: layer.clone(),
            levels: levels.clone(),
            untraced: Vec::new(),
            seed: None,
            content: Some(d.engine.content.clone()),
        });
        Rounds { base, layer, levels, env, tests_view, asked: Mutex::new(Vec::new()) }
    }

    fn nextest(
        &self,
        d: &Arc<Daemon>,
        c: &Arc<Client>,
        p: &Prepared,
        argv: &[Vec<u8>],
        answers: &Answers,
        ran: &HashMap<String, Ran>,
    ) -> Result<Arc<ActionResult>> {
        let view: Arc<dyn View> = Arc::new(BuildView {
            base: self.base.clone(),
            layer: self.layer.clone(),
            levels: self.levels.clone(),
            untraced: Vec::new(),
            seed: Some(answer_layer(d, answers, ran)?),
            content: Some(d.engine.content.clone()),
        });
        let r = d
            .engine
            .obtain(Request {
                spec: spec(p, argv.to_vec(), self.env.clone(), NetMode::Loopback),
                view,
                client_env: c.env.clone(),
                uid: c.uid,
                gid: c.gid,
                prio: c.prio,
                no_cache: false,
                seed_blobs: Default::default(),
                cacheable: Arc::new(|r| r.success() || r.outputs.entries.iter().any(|(p, _)| is_test_question(p))),
                declared_env: Arc::new(|_, _| Vec::new()),
                observer: Arc::new(Explaining { c: c.clone(), label: "nextest".into() }),
            })?
            .result;
        let store = d.store();
        let mut asked = self.asked.lock();
        asked.clear();
        for (path, e) in &r.outputs.entries {
            if let (true, MEntry::File { hash, .. }) = (is_test_question(path), e)
                && let Some(q) = TestQuestion::parse(&store.read(hash)?)
                && !asked.iter().any(|o: &TestQuestion| o.key == q.key)
            {
                asked.push(q);
            }
        }
        Ok(r)
    }
}

fn without_runner_note(err: &str) -> String {
    err.split_inclusive('\n').filter(|l| !l.contains("using target runner `/rargo/nextest/runner`")).collect()
}

const TEST_QUESTIONS: &[u8] = b"/.rargo-nextest-q";

fn is_test_question(path: &[u8]) -> bool {
    vpath_starts_with(path, TEST_QUESTIONS) && path != TEST_QUESTIONS
}

#[derive(Debug, Clone, PartialEq)]
struct TestQuestion {
    key: String,
    cwd: String,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    listing: bool,
}

impl TestQuestion {
    fn parse(raw: &[u8]) -> Option<TestQuestion> {
        let mut fields = raw.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
        let cwd = fields.next()?;
        let attempt = fields.next()?;
        let argc: usize = fields.next()?.parse().ok()?;
        let argv: Vec<String> = (0..argc).map(|_| fields.next()).collect::<Option<_>>()?;
        if argv.is_empty() {
            return None;
        }
        let env: Vec<(String, String)> = fields
            .filter(|f| !f.is_empty())
            .filter_map(|f| f.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect();
        let mut key = format!("{cwd}\n{attempt}");
        for a in &argv {
            key.push('\n');
            key.push_str(a);
        }
        let listing = argv.iter().any(|a| a == "--list");
        Some(TestQuestion { key, cwd, argv, env, listing })
    }
}

struct Ran {
    q: String,
    out: Vec<u8>,
    err: Vec<u8>,
    status: String,
}

const PER_RUN: &[&str] =
    &["NEXTEST_RUN_ID", "NEXTEST_ATTEMPT_ID", "NEXTEST_TEST_GLOBAL_SLOT", "NEXTEST_TEST_GROUP_SLOT"];

fn run_test(
    d: &Arc<Daemon>,
    c: &Arc<Client>,
    policy: &crate::policy::Policy,
    view: Arc<dyn View>,
    root_id: Hash,
    no_cache: bool,
    q: &TestQuestion,
) -> Result<(Arc<ActionResult>, bool)> {
    let real_cargo = q.env.iter().find(|(k, _)| k == REAL_CARGO_VAR).map(|(_, v)| v.clone());
    let mut env: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (k, v) in &q.env {
        if PER_RUN.contains(&k.as_str()) || k == REAL_CARGO_VAR || (k.starts_with("CARGO_TARGET_") && k.ends_with("_RUNNER")) {
            continue;
        }
        let v = if k == "CARGO" { real_cargo.clone().unwrap_or_else(|| v.clone()) } else { v.clone() };
        env.insert(bytes(k), bytes(&v));
    }
    let package = q.env.iter().find(|(k, _)| k == "CARGO_PKG_NAME").map(|(_, v)| v.as_str()).unwrap_or_default();
    let name = q.env.iter().find(|(k, _)| k == "NEXTEST_TEST_NAME").map(|(_, v)| v.as_str());
    let binary = q.env.iter().find(|(k, _)| k == "NEXTEST_BINARY_ID").map(|(_, v)| v.as_str()).unwrap_or(&q.argv[0]);
    let label = match name {
        Some(n) if !q.listing => format!("test {binary} {n}"),
        _ => format!("test list of {binary}"),
    };
    let spec = ActionSpec {
        kind: ActionKind::Test,
        argv: q.argv.iter().map(|a| bytes(a)).collect(),
        env,
        cwd: bytes(&q.cwd),
        net: if policy.test_network(package) { NetMode::Host } else { NetMode::Loopback },
        mkdirs: vec![],
        root_id,
        discard: vec![],
        label: label.clone(),
    };
    let outcome = d
        .engine
        .obtain(Request {
            spec,
            view,
            client_env: c.env.clone(),
            uid: c.uid,
            gid: c.gid,
            prio: c.prio,
            no_cache: no_cache && !q.listing,
            seed_blobs: Default::default(),
            cacheable: Arc::new(|r| r.success()),
            declared_env: Arc::new(|_, _| Vec::new()),
            observer: Arc::new(Explaining { c: c.clone(), label }),
        })?;
    Ok((outcome.result, outcome.cached))
}

struct Explaining {
    c: Arc<Client>,
    label: String,
}

impl Observer for Explaining {
    fn wants_explanation(&self) -> bool {
        self.c.env.contains_key("RARGO_EXPLAIN")
    }
    fn explain(&self, why: &str) {
        self.c.out.note(&format!("{} runs because {why}", self.label));
    }
}

fn deliver_writes(c: &Client, p: &Prepared, built: Option<&Built>, outputs: &Manifest) {
    let target_dirs: Vec<String> = match built {
        Some(b) => b.target_dirs.clone(),
        None => vec![String::from_utf8_lossy(&vjoin(WORKTREE_AT, b"target")).into_owned()],
    };
    let dirs: Vec<&String> = target_dirs.iter().collect();
    apply_writes(c, &p.tr, &dirs, outputs);
    let files: Vec<Delivery> = outputs
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
    if !files.is_empty() {
        c.deliver(files, Vec::new());
    }
}

fn cksum(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    let mut feed = |b: u8| {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    };
    for &b in data {
        feed(b);
    }
    let mut len = data.len();
    while len > 0 {
        feed((len & 0xff) as u8);
        len >>= 8;
    }
    !crc
}

fn canonical_arg(tr: &Translator, a: &str) -> String {
    match a.split_once('=') {
        Some((flag, val)) if flag.starts_with("--") && val.starts_with('/') => format!("{flag}={}", tr.to_canonical(val)),
        _ if a.starts_with('/') => tr.to_canonical(a),
        _ => a.to_string(),
    }
}

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
        root_id: p.flake.id,
        discard: vec![],
        label: "nextest".into(),
    }
}

fn action_env(base: &BTreeMap<String, String>, cargo: Option<&str>, c: &Client) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut env: BTreeMap<Vec<u8>, Vec<u8>> = base.iter().map(|(k, v)| (bytes(k), bytes(v))).collect();
    env.insert(bytes("CARGO"), vjoin(AT, b"cargo"));
    let real = cargo.map(bytes).unwrap_or_else(|| PLANNER_AT.to_vec());
    env.insert(bytes(REAL_CARGO_VAR), real);
    if c.out.color_err {
        env.insert(bytes("CARGO_TERM_COLOR"), bytes("always"));
    }
    env
}

fn answer_layer(d: &Daemon, answers: &Answers, ran: &HashMap<String, Ran>) -> Result<Arc<OutputsLayer>> {
    let store = d.store();
    let mut entries = Vec::new();
    let mut file = |name: &str, content: &[u8], exec: bool| -> Result<()> {
        let hash = store.put_bytes(content)?;
        entries.push((vjoin(AT, name.as_bytes()), MEntry::File { hash, exec, size: content.len() as u64 }));
        Ok(())
    };
    file("cargo", STAND_IN.as_bytes(), true)?;
    file("runner", RUNNER.as_bytes(), true)?;
    let mut slots: HashMap<String, usize> = HashMap::new();
    for r in ran.values() {
        let dir = format!("t/{}-{}", cksum(r.q.as_bytes()), r.q.len());
        let n = slots.entry(dir.clone()).or_default();
        *n += 1;
        file(&format!("{dir}/{n}/q"), r.q.as_bytes(), false)?;
        file(&format!("{dir}/{n}/out"), &r.out, false)?;
        file(&format!("{dir}/{n}/err"), &r.err, false)?;
        file(&format!("{dir}/{n}/status"), format!("{}\n", r.status).as_bytes(), false)?;
    }
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

fn metadata(d: &Arc<Daemon>, c: &Arc<Client>, p: &Prepared, args: Vec<String>) -> Result<Answer> {
    let mut env: BTreeMap<Vec<u8>, Vec<u8>> = p.env.iter().map(|(k, v)| (bytes(k), bytes(v))).collect();
    if let Some(channel) = p.channel {
        env.insert(bytes(super::CHANNEL_VAR), bytes(channel));
    }
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
        root_id: p.flake.id,
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
        })?
        .result;
    let store = d.store();
    Ok(Answer { args, out: store.read(&r.stdout)?, err: store.read(&r.stderr)?, code: r.code().unwrap_or(101) })
}

fn build(d: &Arc<Daemon>, c: &Arc<Client>, args: Vec<String>) -> Result<(Answer, Option<Built>)> {
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
        targets: c.targets.clone(),
    });
    let keep = Mutex::new(None);
    let mut code = 101;
    for attempt in 0..3 {
        kept.lock().clear();
        match run_request(d, &cb, Some(&keep)) {
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
    let back = Translator::new(vec![
        (c.root.clone(), String::from_utf8_lossy(WORKTREE_AT).into_owned()),
        (c.cargo_home.clone(), String::from_utf8_lossy(CARGO_HOME_AT).into_owned()),
    ]);
    let out = back.text(&String::from_utf8_lossy(&kept.lock())).into_bytes();
    Ok((Answer { args, out, err: Vec::new(), code }, keep.into_inner()))
}


const RELEASES: &str = "https://github.com/nextest-rs/nextest/releases";
pub const CLIENT_VERSION_VAR: &str = "RARGO_NEXTEST_VERSION";

#[derive(Debug, PartialEq)]
struct Wanted {
    version: String,
    by: String,
}

fn wanted(c: &Client, p: &Prepared) -> Result<Option<Wanted>> {
    let check = |version: &str, by: String| -> Result<Option<Wanted>> {
        if version_of(version).is_none() {
            bail!("{by} names nextest `{version}`; rargo installs nextest by its exact version (such as `0.9.100`)");
        }
        Ok(Some(Wanted { version: version.to_string(), by }))
    };
    if let Some(v) = &p.policy.nextest {
        return check(v, "the `nextest` key of rargo's metadata in Cargo.toml".into());
    }
    let dir = c.policy_dir()?;
    let read = |path: &Path| {
        use std::os::unix::ffi::OsStrExt;
        p.snapshot.read(path.as_os_str().as_bytes()).map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let root = rargo_proto::workspace::manifest_for(&dir, read)
        .and_then(|(manifest, _)| manifest.parent().map(Path::to_path_buf))
        .unwrap_or(dir);
    let config = root.join(".config/nextest.toml");
    if let Some(v) = read(&config).as_deref().and_then(config_version) {
        return check(&v, format!("`nextest-version` in {}", config.display()));
    }
    match c.env.get(CLIENT_VERSION_VAR) {
        Some(v) => check(&v, "the client's own nextest".into()),
        None => Ok(None),
    }
}

fn config_version(text: &str) -> Option<String> {
    let table = text.parse::<toml::Table>().ok()?;
    match table.get("nextest-version")? {
        toml::Value::String(v) => Some(v.clone()),
        toml::Value::Table(t) => t.get("recommended").or_else(|| t.get("required"))?.as_str().map(String::from),
        _ => None,
    }
}

fn installed(d: &Daemon, c: &Client, wanted: Option<Wanted>) -> Result<PathBuf> {
    static INSTALLING: crate::io::Mutex<()> = crate::io::Mutex::new(());
    let dir = d.cfg.state_dir.join("nextest");
    let wanted = match wanted {
        Some(w) => w,
        None => {
            let dir2 = dir.clone();
            let newest = crate::io::cpu(move || newest_release(&dir2));
            match newest {
                Ok(version) => Wanted { version, by: "nobody (the newest release)".into() },
                Err(e) => return newest_installed(&dir).ok_or(e).context("nextest is not installed on this server"),
            }
        }
    };
    let program = dir.join(&wanted.version).join("cargo-nextest");
    if program.is_file() {
        return Ok(program);
    }
    let _one_at_a_time = INSTALLING.lock();
    if program.is_file() {
        return Ok(program);
    }
    c.out.status("Installing", &format!("nextest {} on the server", wanted.version));
    let (dir2, version) = (dir.clone(), wanted.version.clone());
    crate::io::cpu(move || install(&dir2, &version))
        .with_context(|| format!("nextest {}, which {} asks for, cannot be installed on this server", wanted.version, wanted.by))
}

fn newest_release(dir: &Path) -> Result<String> {
    let note = dir.join("newest-release");
    let fresh = std::fs::metadata(&note)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < std::time::Duration::from_secs(24 * 3600)));
    if fresh
        && let Some(v) = std::fs::read_to_string(&note).ok().map(|s| s.trim().to_string()).filter(|v| version_of(v).is_some())
    {
        return Ok(v);
    }
    let url = format!("{RELEASES}/latest");
    let r = match agent(false).get(&url).call() {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => bail!("asking {url} for the newest nextest release: {e}"),
    };
    let tag = r
        .header("location")
        .and_then(|l| l.rsplit('/').next())
        .map(str::to_string)
        .with_context(|| format!("{url} did not name the newest release (HTTP {})", r.status()))?;
    let version = tag
        .strip_prefix("cargo-nextest-")
        .filter(|v| version_of(v).is_some())
        .with_context(|| format!("{url} names `{tag}`, which is not a nextest release"))?
        .to_string();
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".newest-release.{}", std::process::id()));
    std::fs::write(&tmp, &version)?;
    std::fs::rename(&tmp, &note)?;
    Ok(version)
}

fn version_of(name: &str) -> Option<Vec<u64>> {
    let v: Option<Vec<u64>> = name.split('.').map(|n| n.parse().ok()).collect();
    v.filter(|v| v.len() == 3)
}

fn newest_installed(dir: &Path) -> Option<PathBuf> {
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
        assert_eq!(parse_question(b"build\n--color=auto\0te"), None);
        assert_eq!(parse_question(b"install\nx\0"), None);
    }

    #[test]
    fn nextest_s_word_about_rargo_s_runner_is_not_shown() {
        let err = "\x1b[32minfo\x1b[0m: using target runner `/rargo/nextest/runner` defined by environment variable `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER`\n    Starting 1 test\ninfo: using target runner `valgrind` defined by `target.x.runner`\n";
        assert_eq!(without_runner_note(err), "    Starting 1 test\ninfo: using target runner `valgrind` defined by `target.x.runner`\n");
    }

    #[test]
    fn the_runner_s_checksum_is_posix_cksum() {
        assert_eq!(cksum(b""), 4294967295);
        assert_eq!(cksum(b"hello"), 3287646509);
        assert_eq!(cksum(b"/w/crates/app\n1\n/w/target/x\n--exact\nt"), 414427659);
    }

    #[test]
    fn the_runner_s_question_is_read_back() {
        let mut raw = Vec::new();
        for f in ["/w/crates/app", "2", "3", "/w/target/debug/it-1", "--exact", "reads a fixture", "NEXTEST_ATTEMPT=2", "A=b=c"] {
            raw.extend_from_slice(f.as_bytes());
            raw.push(0);
        }
        let q = TestQuestion::parse(&raw).unwrap();
        assert_eq!(q.key, "/w/crates/app\n2\n/w/target/debug/it-1\n--exact\nreads a fixture");
        assert_eq!(q.argv, vec!["/w/target/debug/it-1", "--exact", "reads a fixture"]);
        assert_eq!(q.env, vec![("NEXTEST_ATTEMPT".into(), "2".into()), ("A".into(), "b=c".into())]);
        assert!(!q.listing);
        assert_eq!(TestQuestion::parse(b"/w\x001\x003\x00/w/x\x00"), None);
    }

    #[test]
    fn nextest_s_own_version_setting_is_read() {
        assert_eq!(config_version("nextest-version = \"0.9.100\"\n").as_deref(), Some("0.9.100"));
        assert_eq!(
            config_version("nextest-version = { required = \"0.9.90\", recommended = \"0.9.100\" }\n").as_deref(),
            Some("0.9.100")
        );
        assert_eq!(config_version("[profile.ci]\nnextest-version = { required = \"0.9.90\" }\n"), None);
        assert_eq!(config_version("nextest-version = { required = \"0.9.90\" }\n").as_deref(), Some("0.9.90"));
        assert_eq!(config_version("[profile.default]\nretries = 2\n"), None);
    }

    #[test]
    fn the_newest_installed_version_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(newest_installed(tmp.path()), None);
        for v in ["0.9.9", "0.9.152", "0.9.100", "1.0", ".0.9.200.123"] {
            std::fs::create_dir_all(tmp.path().join(v)).unwrap();
            std::fs::write(tmp.path().join(v).join("cargo-nextest"), b"").unwrap();
        }
        std::fs::create_dir_all(tmp.path().join("0.10.0")).unwrap();
        assert_eq!(newest_installed(tmp.path()), Some(tmp.path().join("0.9.152/cargo-nextest")));
    }
}
