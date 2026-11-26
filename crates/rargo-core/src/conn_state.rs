use rargo_machine::StateMachine;
use crate::proto::{CONTROL, ClientMsg, Digest, KEEPALIVE, Offer, PROTO_VERSION, RunRequest, ServerMsg, protocol_mismatch};
use crate::sched::Priority;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub enum Route {
    Reply(ServerMsg),
    Offer(Box<Offer>),
    Control(ClientMsg),
    Fetch { stream: u32, id: u64, hashes: Vec<Digest> },
    Recipe { stream: u32, id: u64, hash: Digest },
    Chunks { stream: u32, id: u64, hash: Digest, idx: Vec<u32> },
    Run { stream: u32, req: Box<RunRequest> },
    Refuse { stream: u32 },
    Stream { stream: u32, msg: ClientMsg },
    Cancel { stream: u32 },
    Report { stream: u32, text: String },
    Close,
}

#[derive(Debug, Default)]
pub struct ConnectionState {
    running: BTreeSet<u32>,
    closed: bool,
}

fn kind(m: &ClientMsg) -> &'static str {
    match m {
        ClientMsg::Run(_) => "a build request",
        ClientMsg::Trees(_) => "directory manifests",
        ClientMsg::Blobs(_) => "file content",
        ClientMsg::BlobPart { .. } => "part of a file",
        ClientMsg::Changed => "a worktree change",
        ClientMsg::Env(_) => "environment variables",
        ClientMsg::Fetch { .. } => "a fetch",
        ClientMsg::Recipe { .. } => "a recipe request",
        ClientMsg::Chunks { .. } => "a chunk request",
        ClientMsg::Offer(_) => "an offer",
        ClientMsg::Cancel => "a cancel",
        ClientMsg::Ping(_) => "a ping",
        ClientMsg::Packed(_) => "a packed message inside a packed message",
        ClientMsg::ReportId(_) => "a report id",
    }
}

fn for_a_session(m: &ClientMsg) -> bool {
    matches!(
        m,
        ClientMsg::Trees(_)
            | ClientMsg::Blobs(_)
            | ClientMsg::BlobPart { .. }
            | ClientMsg::Changed
            | ClientMsg::Env(_)
            | ClientMsg::ReportId(_)
    )
}

impl ConnectionState {
    pub fn new() -> ConnectionState {
        ConnectionState::default()
    }

    pub fn is_running(&self, stream: u32) -> bool {
        self.running.contains(&stream)
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn message(&mut self, stream: u32, msg: ClientMsg, key_valid: bool) -> Vec<Route> {
        assert_ne!(stream, KEEPALIVE, "keepalives are answered by the link");
        self.invariants();
        let was_closed = self.closed;
        let routes = self.route_message(stream, msg, key_valid);
        assert!(!was_closed || (self.closed && routes.is_empty()), "a closed connection routes nothing");
        self.invariants();
        routes
    }

    fn route_message(&mut self, stream: u32, msg: ClientMsg, key_valid: bool) -> Vec<Route> {
        if self.closed {
            return Vec::new();
        }
        let unknown = |stream: u32, m: &ClientMsg| {
            let whose = if stream == CONTROL { "the control stream".to_string() } else { format!("stream {stream}") };
            vec![Route::Report { stream, text: format!("unexpected {} on {whose}", kind(m)) }]
        };
        if stream == CONTROL {
            return match msg {
                ClientMsg::Ping(n) => vec![Route::Reply(ServerMsg::Pong(n))],
                ClientMsg::Offer(o) => vec![Route::Offer(o)],
                m if for_a_session(&m) => vec![Route::Control(m)],
                m => unknown(stream, &m),
            };
        }
        match msg {
            ClientMsg::Fetch { id, hashes } => vec![Route::Fetch { stream, id, hashes }],
            ClientMsg::Recipe { id, hash } => vec![Route::Recipe { stream, id, hash }],
            ClientMsg::Chunks { id, hash, idx } => vec![Route::Chunks { stream, id, hash, idx }],
            ClientMsg::Run(req) => {
                if !key_valid {
                    let mut out = vec![Route::Refuse { stream }];
                    out.extend(self.close());
                    return out;
                }
                if !self.running.insert(stream) {
                    return unknown(stream, &ClientMsg::Run(req));
                }
                assert!(self.is_running(stream));
                vec![Route::Run { stream, req }]
            }
            ClientMsg::Cancel => self.stream_failed(stream),
            m if for_a_session(&m) && self.running.contains(&stream) => vec![Route::Stream { stream, msg: m }],
            m => unknown(stream, &m),
        }
    }

    pub fn stream_failed(&mut self, stream: u32) -> Vec<Route> {
        self.invariants();
        let routes = if self.running.remove(&stream) {
            vec![Route::Cancel { stream }]
        } else {
            Vec::new()
        };
        assert!(!self.is_running(stream));
        self.invariants();
        routes
    }

    pub fn finished(&mut self, stream: u32) {
        self.invariants();
        self.running.remove(&stream);
        assert!(!self.is_running(stream));
        self.invariants();
    }

    pub fn key_checked(&mut self, valid: bool) -> Vec<Route> {
        self.invariants();
        let was_closed = self.closed;
        let routes = if valid { Vec::new() } else { self.close() };
        assert_eq!(self.closed, was_closed || !valid);
        self.invariants();
        routes
    }

    pub fn control_failed(&mut self) -> Vec<Route> {
        self.invariants();
        let routes = self.close();
        assert!(self.closed);
        self.invariants();
        routes
    }

    pub fn close(&mut self) -> Vec<Route> {
        self.invariants();
        let routes = if self.closed {
            Vec::new()
        } else {
            self.closed = true;
            let count = self.running.len();
            let mut out: Vec<Route> =
                std::mem::take(&mut self.running).into_iter().map(|stream| Route::Cancel { stream }).collect();
            assert_eq!(out.len(), count);
            out.push(Route::Close);
            out
        };
        assert!(self.closed);
        self.invariants();
        routes
    }
}

impl StateMachine for ConnectionState {
    fn invariants(&self) {
        assert!(!self.running.contains(&CONTROL), "the control stream runs no request");
        assert!(!self.running.contains(&KEEPALIVE), "keepalives run no request");
        if self.closed {
            assert!(self.running.is_empty(), "a closed connection runs nothing");
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Fail(String),
    Status,
    Settle,
    Token,
    Incr,
    Gc,
    Compact,
    Export(String),
    Import(String),
    Drift,
    Build,
}

pub fn command(version: u32, args: &[String], admin: bool, user: &str) -> Command {
    if version != PROTO_VERSION {
        return Command::Fail(protocol_mismatch(version));
    }
    let name = args.first().map(|s| s.as_str()).unwrap_or("");
    if matches!(name, "__gc" | "__compact" | "__export" | "__import") && !admin {
        return Command::Fail(format!(
            "user `{user}` may not administer this server (an administrator's token is made on the server with `rargod token create --admin`, `token invite --admin` or `token allow --admin`)"
        ));
    }
    let dir = || args.get(1).cloned();
    match name {
        "__status" => Command::Status,
        "__settle" => Command::Settle,
        "__token" => Command::Token,
        "__incr" => Command::Incr,
        "__gc" => Command::Gc,
        "__compact" => Command::Compact,
        "__export" => dir().map_or(Command::Fail("a directory is required".into()), Command::Export),
        "__import" => dir().map_or(Command::Fail("a directory is required".into()), Command::Import),
        "__drift" => Command::Drift,
        _ => Command::Build,
    }
}

pub fn priority(env: &BTreeMap<String, String>, stderr_tty: bool) -> Priority {
    match env.get("RARGO_PRIORITY").map(|s| s.as_str()) {
        Some("human") => Priority::Human,
        Some("agent") => Priority::Agent,
        Some("background") => Priority::Background,
        _ if stderr_tty => Priority::Human,
        _ => Priority::Agent,
    }
}

pub fn wants_color(args: &[String], env: &BTreeMap<String, String>, tty: bool) -> bool {
    let mut choice: Option<&str> = None;
    let mut it = args.iter().take_while(|a| *a != "--");
    while let Some(a) = it.next() {
        if a == "--color" {
            choice = it.next().map(|s| s.as_str());
        } else if let Some(v) = a.strip_prefix("--color=") {
            choice = Some(v);
        }
    }
    let choice = choice.or(env.get("CARGO_TERM_COLOR").map(|s| s.as_str()));
    match choice {
        Some("always") => true,
        Some("never") => false,
        _ => tty && !env.contains_key("NO_COLOR") && env.get("TERM").map(|t| t != "dumb").unwrap_or(false),
    }
}

#[derive(Clone, Debug, Default)]
pub struct WorkerFigures {
    pub count: u64,
    pub dispatched: u64,
    pub hedged: u64,
    pub hedge_won: u64,
    pub hedge_stopped: u64,
    pub kept_local_no_wait: u64,
    pub kept_local_first: u64,
    pub kept_local_transfer: u64,
    pub kept_local_incremental: u64,
    pub worker_failures: u64,
    pub overhead: String,
    pub unseeded_remote: u64,
    pub seeded_in_place: u64,
    pub state_left_bytes: u64,
    pub state_left_blobs: u64,
    pub state_homed_bytes: u64,
    pub state_homed_blobs: u64,
    pub state_needed_blobs: u64,
    pub states_sent_ahead: u64,
    pub state_ahead_bytes: u64,
    pub pushed: u64,
    pub skipped: u64,
    pub fetched: u64,
    pub reused: u64,
    pub prefilled: u64,
    pub lookups_served: u64,
    pub opens_served: u64,
    pub nix_lookups: u64,
}

#[derive(Clone, Debug, Default)]
pub struct StatusFigures {
    pub requests: u64,
    pub snapshots: u64,
    pub trees_received: u64,
    pub files_in: u64,
    pub offers: u64,
    pub blobs_requested: u64,
    pub shared_uploads: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub links: u64,
    pub blob_count: u64,
    pub store_bytes: u64,
    pub cold_blobs: u64,
    pub cold_logical: u64,
    pub cold_disk: u64,
    pub hits: u64,
    pub misses: u64,
    pub traces_stored: u64,
    pub runs: u64,
    pub run_cpu_us: u64,
    pub run_wall_us: u64,
    pub runs_by_kind: Vec<(&'static str, u64)>,
    pub joined: u64,
    pub joined_then_ran: u64,
    pub tainted: u64,
    pub env_reruns: u64,
    pub running: u64,
    pub waiting: u64,
    pub peak: u64,
    pub granted: u64,
    pub tokens: u64,
    pub lookups: u64,
    pub opens: u64,
    pub reads: u64,
    pub writes: u64,
    pub plans_decoded: u64,
    pub incr_saved: u64,
    pub seeded_same_worktree: u64,
    pub seeded_ancestor: u64,
    pub seeded_other: u64,
    pub unseeded: u64,
    pub discarded_after_crash: u64,
    pub verify_units: u64,
    pub verify_mismatches: u64,
    pub drift: Option<String>,
    pub workers: Option<WorkerFigures>,
}

fn mib(b: u64) -> f64 {
    b as f64 / (1 << 20) as f64
}

pub fn status_text(f: &StatusFigures) -> String {
    let workers = match &f.workers {
        None => "not accepting workers".to_string(),
        Some(w) => format!(
            "{} connected; {} actions dispatched ({} of them also started here on an idle core: {} finished here first, {} were stopped because the worker was ahead), {} kept local (core free), {} kept local (a local core came free first), {} kept local (transfer too costly), {} kept local (no worker has the incremental state), {} worker failures; overhead per remote action {}; \
             incremental compiles on workers: {} without state, {} with the state the worker held; state left on workers {:.1} MiB in {} files, of which {:.1} MiB in {} files came home in the background and {} files were fetched for a compile here; {} states made here sent to workers ahead of need ({:.1} MiB); \
             sent ahead {:.1} MiB, not sent because already there {:.1} MiB; outputs fetched {:.1} MiB, reused locally {:.1} MiB; \
             {} answers sent ahead, {} lookups and {} opens asked, {} nix store lookups",
            w.count,
            w.dispatched,
            w.hedged,
            w.hedge_won,
            w.hedge_stopped,
            w.kept_local_no_wait,
            w.kept_local_first,
            w.kept_local_transfer,
            w.kept_local_incremental,
            w.worker_failures,
            w.overhead,
            w.unseeded_remote,
            w.seeded_in_place,
            mib(w.state_left_bytes),
            w.state_left_blobs,
            mib(w.state_homed_bytes),
            w.state_homed_blobs,
            w.state_needed_blobs,
            w.states_sent_ahead,
            mib(w.state_ahead_bytes),
            mib(w.pushed),
            mib(w.skipped),
            mib(w.fetched),
            mib(w.reused),
            w.prefilled,
            w.lookups_served,
            w.opens_served,
            w.nix_lookups,
        ),
    };
    format!(
        "requests            {}\n\
         sync                {} worktree states, {} manifests and {} files received from clients ({} changes pushed unasked, {} files asked for, {} awaited from another client's upload), {:.1} MiB received, {:.1} MiB of results sent; {} client daemons have connected\n\
         store               {} blobs, {:.1} MiB\n\
         cold tier           {} blobs of {:.1} MiB kept as {:.1} MiB of chunks\n\
         cache               {} hits, {} misses, {} traces stored\n\
         actions run         {} ({:.1}s cpu, {:.1}s wall)\n\
         actions by kind     {}\n\
         in-flight dedup     {} joined, {} of those ran anyway\n\
         reruns              {} changed-input, {} environment\n\
         scheduler           {} running, {} waiting, peak {}, {} granted, {} tokens\n\
         fuse                {} lookups, {} opens, {} userspace reads, {} userspace writes\n\
         plans decoded       {}\n\
         incremental         {} states saved; seeds: {} same worktree, {} ancestor, {} other, {} none; {} dropped after a crash\n\
         verify              {} cached units re-run cleanly, {} differed\n\
         drift               {}\n\
         workers             {}\n",
        f.requests,
        f.snapshots,
        f.trees_received,
        f.files_in,
        f.offers,
        f.blobs_requested,
        f.shared_uploads,
        mib(f.bytes_in),
        mib(f.bytes_out),
        f.links,
        f.blob_count,
        mib(f.store_bytes),
        f.cold_blobs,
        mib(f.cold_logical),
        mib(f.cold_disk),
        f.hits,
        f.misses,
        f.traces_stored,
        f.runs,
        f.run_cpu_us as f64 / 1e6,
        f.run_wall_us as f64 / 1e6,
        f.runs_by_kind.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "),
        f.joined,
        f.joined_then_ran,
        f.tainted,
        f.env_reruns,
        f.running,
        f.waiting,
        f.peak,
        f.granted,
        f.tokens,
        f.lookups,
        f.opens,
        f.reads,
        f.writes,
        f.plans_decoded,
        f.incr_saved,
        f.seeded_same_worktree,
        f.seeded_ancestor,
        f.seeded_other,
        f.unseeded,
        f.discarded_after_crash,
        f.verify_units,
        f.verify_mismatches,
        f.drift.as_deref().unwrap_or("not checked yet").trim_end(),
        workers,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Transcript;
    use expect_test::expect;

    fn run(args: &[&str]) -> ClientMsg {
        ClientMsg::Run(Box::new(RunRequest {
            version: PROTO_VERSION,
            args: args.iter().map(|s| s.to_string()).collect(),
            root: String::new(),
            cwd: String::new(),
            cargo_home: String::new(),
            env: Vec::new(),
            env_checked: Vec::new(),
            stdout_tty: false,
            stderr_tty: false,
            worktree: String::new(),
            commits: Vec::new(),
            tree: [0; 32],
            home_files: Vec::new(),
            who: String::new(),
            delivered: 0,
            lazy: false,
        }))
    }

    fn names(routes: &[Route]) -> Vec<String> {
        routes
            .iter()
            .map(|r| match r {
                Route::Reply(msg) => format!("reply {msg:?}"),
                Route::Offer(_) => "offer".into(),
                Route::Control(_) => "control".into(),
                Route::Fetch { stream, .. } => format!("fetch {stream}"),
                Route::Recipe { stream, .. } => format!("recipe {stream}"),
                Route::Chunks { stream, .. } => format!("chunks {stream}"),
                Route::Run { stream, .. } => format!("run {stream}"),
                Route::Refuse { stream } => format!("refuse {stream}"),
                Route::Stream { stream, .. } => format!("to {stream}"),
                Route::Cancel { stream } => format!("cancel {stream}"),
                Route::Report { stream, text } => format!("report {stream}: {text}"),
                Route::Close => "close".into(),
            })
            .collect()
    }

    enum Event {
        Message(u32, ClientMsg, bool),
        Finish(u32),
        StreamFailed(u32),
        ControlFailed,
        KeyChecked(bool),
        Close,
    }

    impl Event {
        fn message(stream: u32, msg: ClientMsg) -> Self {
            Self::Message(stream, msg, true)
        }
    }

    fn replay(events: impl IntoIterator<Item = Event>) -> String {
        let mut state = ConnectionState::new();
        let mut outcomes = Transcript::default();
        for event in events {
            let (name, routes) = match event {
                Event::Message(stream, msg, valid) => {
                    let name = format!("{stream}: {}", kind(&msg));
                    (name, state.message(stream, msg, valid))
                }
                Event::Finish(stream) => {
                    state.finished(stream);
                    (format!("{stream}: finish"), vec![])
                }
                Event::StreamFailed(stream) => (format!("{stream}: fail"), state.stream_failed(stream)),
                Event::ControlFailed => ("control failed".into(), state.control_failed()),
                Event::KeyChecked(valid) => (format!("key valid={valid}"), state.key_checked(valid)),
                Event::Close => ("close".into(), state.close()),
            };
            outcomes.record(&name, names(&routes));
            state.invariants();
        }
        outcomes.0
    }

    #[test]
    fn a_failed_stream_ends_only_itself_and_a_failed_control_stream_ends_the_connection() {
        use Event::*;
        expect![[r#"
            2: a build request: ["run 2"]
            4: a build request: ["run 4"]
            2: fail: ["cancel 2"]
            2: fail: []
            control failed: ["cancel 4", "close"]
            control failed: []
            4: fail: []
        "#]].assert_eq(&replay([
            Event::message(2, run(&["build"])),
            Event::message(4, run(&["build"])),
            StreamFailed(2),
            StreamFailed(2),
            ControlFailed,
            ControlFailed,
            StreamFailed(4),
        ]));
    }

    #[test]
    fn streams_are_independent_and_a_cancel_reaches_only_its_stream() {
        let message = Event::message;
        expect![[r#"
            3: a build request: ["run 3"]
            5: a build request: ["run 5"]
            5: a worktree change: ["to 5"]
            3: a cancel: ["cancel 3"]
            3: a cancel: []
            5: environment variables: ["to 5"]
            3: a worktree change: ["report 3: unexpected a worktree change on stream 3"]
            5: finish: []
            5: a cancel: []
        "#]].assert_eq(&replay([
            message(3, run(&["build"])),
            message(5, run(&["test"])),
            message(5, ClientMsg::Changed),
            message(3, ClientMsg::Cancel),
            message(3, ClientMsg::Cancel),
            message(5, ClientMsg::Env(vec![])),
            message(3, ClientMsg::Changed),
            Event::Finish(5),
            message(5, ClientMsg::Cancel),
        ]));
    }

    #[test]
    fn a_closed_connection_cancels_all_of_its_streams() {
        use Event::{Close, Finish};
        expect![[r#"
            2: a build request: ["run 2"]
            4: a build request: ["run 4"]
            6: a build request: ["run 6"]
            4: finish: []
            close: ["cancel 2", "cancel 6", "close"]
            close: []
            8: a build request: []
        "#]].assert_eq(&replay([
            Event::message(2, run(&["build"])),
            Event::message(4, run(&["build"])),
            Event::message(6, run(&["build"])),
            Finish(4),
            Close,
            Close,
            Event::message(8, run(&["build"])),
        ]));
    }

    #[test]
    fn a_request_on_a_key_no_longer_valid_is_refused_and_ends_the_connection() {
        expect![[r#"
            2: a build request: ["run 2"]
            4: a build request: ["refuse 4", "cancel 2", "close"]
        "#]].assert_eq(&replay([
            Event::message(2, run(&["build"])),
            Event::Message(4, run(&["build"]), false),
        ]));
        expect![[r#"
            2: a build request: ["run 2"]
            key valid=true: []
            key valid=false: ["cancel 2", "close"]
        "#]].assert_eq(&replay([
            Event::message(2, run(&["build"])),
            Event::KeyChecked(true),
            Event::KeyChecked(false),
        ]));
    }

    #[test]
    fn unknown_messages_are_reported_not_fatal() {
        let msg = Event::message;
        expect![[r#"
            0: a build request: ["report 0: unexpected a build request on the control stream"]
            0: a cancel: ["report 0: unexpected a cancel on the control stream"]
            0: a ping: ["reply Pong(7)"]
            0: a worktree change: ["control"]
            3: a build request: ["run 3"]
            3: a ping: ["report 3: unexpected a ping on stream 3"]
            3: a build request: ["report 3: unexpected a build request on stream 3"]
            9: a fetch: ["fetch 9"]
            close: ["cancel 3", "close"]
        "#]].assert_eq(&replay([
            msg(CONTROL, run(&["build"])),
            msg(CONTROL, ClientMsg::Cancel),
            msg(CONTROL, ClientMsg::Ping(7)),
            msg(CONTROL, ClientMsg::Changed),
            msg(3, run(&["build"])),
            msg(3, ClientMsg::Ping(1)),
            msg(3, run(&["build"])),
            msg(9, ClientMsg::Fetch { id: 1, hashes: vec![] }),
            Event::Close,
        ]));
    }

    #[test]
    fn an_admin_command_from_a_non_admin_is_refused() {
        let cases: &[(bool, &[&str])] = &[
            (false, &["__gc"]),
            (false, &["__compact"]),
            (false, &["__export", "/d"]),
            (false, &["__import", "/d"]),
            (true, &["__gc"]),
            (true, &["__export", "/d"]),
            (true, &["__import"]),
            (false, &["__status"]),
            (false, &["build"]),
        ];
        let mut outcomes = Transcript::default();
        for &(admin, words) in cases {
            let args = words.iter().map(ToString::to_string).collect::<Vec<_>>();
            let user = if admin { "ann" } else { "bob" };
            outcomes.record(&format!("{user} runs {}", words.join(" ")), command(PROTO_VERSION, &args, admin, user));
        }
        expect![[r#"
            bob runs __gc: Fail("user `bob` may not administer this server (an administrator's token is made on the server with `rargod token create --admin`, `token invite --admin` or `token allow --admin`)")
            bob runs __compact: Fail("user `bob` may not administer this server (an administrator's token is made on the server with `rargod token create --admin`, `token invite --admin` or `token allow --admin`)")
            bob runs __export /d: Fail("user `bob` may not administer this server (an administrator's token is made on the server with `rargod token create --admin`, `token invite --admin` or `token allow --admin`)")
            bob runs __import /d: Fail("user `bob` may not administer this server (an administrator's token is made on the server with `rargod token create --admin`, `token invite --admin` or `token allow --admin`)")
            ann runs __gc: Gc
            ann runs __export /d: Export("/d")
            ann runs __import: Fail("a directory is required")
            bob runs __status: Status
            bob runs build: Build
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn a_mismatched_client_is_told_how_to_install_the_server_version() {
        let args = ["build".into()];
        let denied = |client, admin| {
            let Command::Fail(message) = command(client, &args, admin, "bob") else {
                panic!("a client with a mismatched version cannot run");
            };
            message.replace(env!("CARGO_PKG_VERSION"), "<version>")
        };
        let mut outcomes = Transcript::default();
        for (label, version, admin) in [
            ("old", PROTO_VERSION - 1, false),
            ("legacy", 7, false),
            ("new", PROTO_VERSION + 1, false),
            ("old admin", PROTO_VERSION - 1, true),
        ] {
            outcomes.record(label, denied(version, admin));
        }
        expect![[r#"
            old: "this rargo is older than the build server, which runs rargo <version>; update it with: cargo install rargo --version <version>"
            legacy: "this rargo is older than the build server, which runs rargo <version>; update it with: cargo install rargo --version <version>; then name the server once more with `rargo @HOST`, as you did before: the new rargo keeps it in your own configuration, not in Cargo.toml"
            new: "this rargo is newer than the build server, which runs rargo <version>; install the matching version with: cargo install rargo --version <version>"
            old admin: "this rargo is older than the build server, which runs rargo <version>; update it with: cargo install rargo --version <version>"
        "#]].assert_eq(&outcomes.0);
        for client in 1..PROTO_VERSION {
            assert_eq!(denied(client, false).contains("`rargo @HOST`"), client <= 7, "client {client}");
        }
    }

    #[test]
    fn color_and_priority_follow_the_request() {
        let env =
            |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let term = env(&[("TERM", "xterm")]);
        assert!(wants_color(&a(&["build"]), &term, true));
        assert!(!wants_color(&a(&["build"]), &term, false));
        assert!(!wants_color(&a(&["build", "--color", "never"]), &term, true));
        assert!(wants_color(&a(&["build", "--color=always"]), &env(&[]), false));
        assert!(
            !wants_color(&a(&["run", "--", "--color=always"]), &env(&[]), false),
            "arguments after -- are the program's"
        );
        assert!(!wants_color(&a(&["build"]), &env(&[("TERM", "xterm"), ("NO_COLOR", "1")]), true));
        assert!(wants_color(&a(&["build"]), &env(&[("CARGO_TERM_COLOR", "always")]), false));
        assert_eq!(priority(&env(&[]), true), Priority::Human);
        assert_eq!(priority(&env(&[]), false), Priority::Agent);
        assert_eq!(priority(&env(&[("RARGO_PRIORITY", "background")]), true), Priority::Background);
    }

    #[test]
    fn status_text_names_every_figure() {
        let mut f = StatusFigures { requests: 7, running: 2, tokens: 8, store_bytes: 3 << 20, ..Default::default() };
        f.runs_by_kind = vec![("rustc", 4), ("build-script", 1)];
        expect![[r#"
            requests            7
            sync                0 worktree states, 0 manifests and 0 files received from clients (0 changes pushed unasked, 0 files asked for, 0 awaited from another client's upload), 0.0 MiB received, 0.0 MiB of results sent; 0 client daemons have connected
            store               0 blobs, 3.0 MiB
            cold tier           0 blobs of 0.0 MiB kept as 0.0 MiB of chunks
            cache               0 hits, 0 misses, 0 traces stored
            actions run         0 (0.0s cpu, 0.0s wall)
            actions by kind     rustc 4, build-script 1
            in-flight dedup     0 joined, 0 of those ran anyway
            reruns              0 changed-input, 0 environment
            scheduler           2 running, 0 waiting, peak 0, 0 granted, 8 tokens
            fuse                0 lookups, 0 opens, 0 userspace reads, 0 userspace writes
            plans decoded       0
            incremental         0 states saved; seeds: 0 same worktree, 0 ancestor, 0 other, 0 none; 0 dropped after a crash
            verify              0 cached units re-run cleanly, 0 differed
            drift               not checked yet
            workers             not accepting workers
        "#]].assert_eq(&status_text(&f));
        f.workers = Some(WorkerFigures { count: 3, overhead: "12 ms".into(), ..Default::default() });
        expect![[r#"
            requests            7
            sync                0 worktree states, 0 manifests and 0 files received from clients (0 changes pushed unasked, 0 files asked for, 0 awaited from another client's upload), 0.0 MiB received, 0.0 MiB of results sent; 0 client daemons have connected
            store               0 blobs, 3.0 MiB
            cold tier           0 blobs of 0.0 MiB kept as 0.0 MiB of chunks
            cache               0 hits, 0 misses, 0 traces stored
            actions run         0 (0.0s cpu, 0.0s wall)
            actions by kind     rustc 4, build-script 1
            in-flight dedup     0 joined, 0 of those ran anyway
            reruns              0 changed-input, 0 environment
            scheduler           2 running, 0 waiting, peak 0, 0 granted, 8 tokens
            fuse                0 lookups, 0 opens, 0 userspace reads, 0 userspace writes
            plans decoded       0
            incremental         0 states saved; seeds: 0 same worktree, 0 ancestor, 0 other, 0 none; 0 dropped after a crash
            verify              0 cached units re-run cleanly, 0 differed
            drift               not checked yet
            workers             3 connected; 0 actions dispatched (0 of them also started here on an idle core: 0 finished here first, 0 were stopped because the worker was ahead), 0 kept local (core free), 0 kept local (a local core came free first), 0 kept local (transfer too costly), 0 kept local (no worker has the incremental state), 0 worker failures; overhead per remote action 12 ms; incremental compiles on workers: 0 without state, 0 with the state the worker held; state left on workers 0.0 MiB in 0 files, of which 0.0 MiB in 0 files came home in the background and 0 files were fetched for a compile here; 0 states made here sent to workers ahead of need (0.0 MiB); sent ahead 0.0 MiB, not sent because already there 0.0 MiB; outputs fetched 0.0 MiB, reused locally 0.0 MiB; 0 answers sent ahead, 0 lookups and 0 opens asked, 0 nix store lookups
        "#]].assert_eq(&status_text(&f));
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    #[test]
    fn generated_messages_cancel_each_stream_at_most_once_and_close_cancels_the_rest() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut c = ConnectionState::new();
            let mut model: BTreeSet<u32> = BTreeSet::new();
            let mut ended = false;
            for _ in 0..200 {
                let stream = rng.below(6) as u32;
                let msg = match rng.below(9) {
                    0 | 1 => run(&["build"]),
                    2 => ClientMsg::Cancel,
                    3 => ClientMsg::Changed,
                    4 => ClientMsg::Ping(1),
                    5 => ClientMsg::Fetch { id: 1, hashes: vec![] },
                    6 => ClientMsg::Env(vec![]),
                    7 => {
                        if rng.below(3) == 0 {
                            c.finished(stream);
                            model.remove(&stream);
                        } else {
                            let r = c.stream_failed(stream);
                            assert_eq!(r.len(), usize::from(model.remove(&stream)));
                        }
                        c.invariants();
                        continue;
                    }
                    _ => ClientMsg::Trees(vec![]),
                };
                let was_run = matches!(msg, ClientMsg::Run(_));
                let was_cancel = matches!(msg, ClientMsg::Cancel);
                let valid = rng.below(40) != 0;
                let routes = c.message(stream, msg, valid);
                if ended {
                    assert!(routes.is_empty(), "seed {seed}: a closed connection routes nothing");
                    assert!(c.running.is_empty(), "seed {seed}: a closed connection runs nothing");
                    c.invariants();
                    continue;
                }
                for r in &routes {
                    match r {
                        Route::Cancel { stream: s } => {
                            assert!(model.remove(s), "seed {seed}: cancelled {s}, which was not running")
                        }
                        Route::Run { stream: s, .. } => assert!(model.insert(*s), "seed {seed}: {s} ran twice"),
                        Route::Stream { stream: s, .. } => assert!(model.contains(s)),
                        Route::Close => ended = true,
                        Route::Refuse { .. } => assert!(was_run && !valid),
                        _ => {}
                    }
                }
                if was_cancel && stream != CONTROL {
                    assert!(!c.is_running(stream));
                }
                if ended {
                    assert!(model.is_empty(), "seed {seed}: closing cancelled every stream");
                }
                assert_eq!(c.running, model, "seed {seed}");
                c.invariants();
            }
            let rest = c.close();
            assert_eq!(rest.iter().filter(|r| matches!(r, Route::Cancel { .. })).count(), model.len());
            c.invariants();
        }
    }
}
