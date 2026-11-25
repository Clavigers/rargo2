use crate::proto::{CONTROL, ClientMsg, Digest, KEEPALIVE, Offer, PROTO_VERSION, RunRequest, ServerMsg};
use crate::sched::Priority;
use std::collections::{BTreeSet, HashMap};

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
                vec![Route::Run { stream, req }]
            }
            ClientMsg::Cancel => self.end(stream),
            m if for_a_session(&m) && self.running.contains(&stream) => vec![Route::Stream { stream, msg: m }],
            m => unknown(stream, &m),
        }
    }

    fn end(&mut self, stream: u32) -> Vec<Route> {
        if self.running.remove(&stream) { vec![Route::Cancel { stream }] } else { Vec::new() }
    }

    pub fn stream_failed(&mut self, stream: u32) -> Vec<Route> {
        self.end(stream)
    }

    pub fn finished(&mut self, stream: u32) {
        self.running.remove(&stream);
    }

    pub fn key_checked(&mut self, valid: bool) -> Vec<Route> {
        if valid { Vec::new() } else { self.close() }
    }

    pub fn control_failed(&mut self) -> Vec<Route> {
        self.close()
    }

    pub fn close(&mut self) -> Vec<Route> {
        if self.closed {
            return Vec::new();
        }
        self.closed = true;
        let mut out: Vec<Route> =
            std::mem::take(&mut self.running).into_iter().map(|stream| Route::Cancel { stream }).collect();
        out.push(Route::Close);
        out
    }

    pub fn invariants(&self) {
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

pub fn protocol_mismatch(client: u32) -> String {
    let server = env!("CARGO_PKG_VERSION");
    let install = format!("curl -fsSL https://rargo.rs/install.sh | sh -s -- {server}");
    let (age, verb) = if client < PROTO_VERSION { ("older", "update it") } else { ("newer", "install the matching client") };
    format!(
        "this rargo (protocol {client}) is {age} than the build server (rargod {server}, protocol {PROTO_VERSION}); {verb} with: {install}"
    )
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

pub fn priority(env: &HashMap<String, String>, stderr_tty: bool) -> Priority {
    match env.get("RARGO_PRIORITY").map(|s| s.as_str()) {
        Some("human") => Priority::Human,
        Some("agent") => Priority::Agent,
        Some("background") => Priority::Background,
        _ if stderr_tty => Priority::Human,
        _ => Priority::Agent,
    }
}

pub fn wants_color(args: &[String], env: &HashMap<String, String>, tty: bool) -> bool {
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
                Route::Reply(_) => "reply".into(),
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

    fn msg(c: &mut ConnectionState, stream: u32, m: ClientMsg, key_valid: bool) -> Vec<Route> {
        let routes = c.message(stream, m, key_valid);
        c.invariants();
        routes
    }

    fn fin(c: &mut ConnectionState, stream: u32) {
        c.finished(stream);
        c.invariants();
    }

    fn key(c: &mut ConnectionState, valid: bool) -> Vec<Route> {
        let routes = c.key_checked(valid);
        c.invariants();
        routes
    }

    fn close(c: &mut ConnectionState) -> Vec<Route> {
        let routes = c.close();
        c.invariants();
        routes
    }

    fn failed(c: &mut ConnectionState, stream: u32) -> Vec<Route> {
        let routes = c.stream_failed(stream);
        c.invariants();
        routes
    }

    fn control_failed(c: &mut ConnectionState) -> Vec<Route> {
        let routes = c.control_failed();
        c.invariants();
        routes
    }

    #[test]
    fn a_failed_stream_ends_only_itself_and_a_failed_control_stream_ends_the_connection() {
        let mut c = ConnectionState::new();
        msg(&mut c, 2, run(&["build"]), true);
        msg(&mut c, 4, run(&["build"]), true);
        assert_eq!(names(&failed(&mut c, 2)), ["cancel 2"]);
        assert_eq!(names(&failed(&mut c, 2)), Vec::<String>::new(), "a stream ends once");
        assert!(c.is_running(4) && !c.is_closed());
        assert_eq!(names(&control_failed(&mut c)), ["cancel 4", "close"]);
        assert!(control_failed(&mut c).is_empty());
        assert!(failed(&mut c, 4).is_empty());
    }

    #[test]
    fn streams_are_independent_and_a_cancel_reaches_only_its_stream() {
        let mut c = ConnectionState::new();
        assert_eq!(names(&msg(&mut c, 3, run(&["build"]), true)), ["run 3"]);
        assert_eq!(names(&msg(&mut c, 5, run(&["test"]), true)), ["run 5"]);
        assert_eq!(names(&msg(&mut c, 5, ClientMsg::Changed, true)), ["to 5"]);
        assert_eq!(names(&msg(&mut c, 3, ClientMsg::Cancel, true)), ["cancel 3"]);
        assert_eq!(names(&msg(&mut c, 3, ClientMsg::Cancel, true)), Vec::<String>::new(), "a stream ends once");
        assert!(c.is_running(5) && !c.is_running(3));
        assert_eq!(names(&msg(&mut c, 5, ClientMsg::Env(Vec::new()), true)), ["to 5"]);
        assert_eq!(
            names(&msg(&mut c, 3, ClientMsg::Changed, true)),
            ["report 3: unexpected a worktree change on stream 3"]
        );
        fin(&mut c, 5);
        assert!(!c.is_running(5));
    }

    #[test]
    fn a_closed_connection_cancels_all_of_its_streams() {
        let mut c = ConnectionState::new();
        for s in [2, 4, 6] {
            msg(&mut c, s, run(&["build"]), true);
        }
        fin(&mut c, 4);
        assert_eq!(names(&close(&mut c)), ["cancel 2", "cancel 6", "close"]);
        assert!(close(&mut c).is_empty());
        assert!(msg(&mut c, 8, run(&["build"]), true).is_empty(), "a closed connection takes no request");
    }

    #[test]
    fn a_request_on_a_key_no_longer_valid_is_refused_and_ends_the_connection() {
        let mut c = ConnectionState::new();
        msg(&mut c, 2, run(&["build"]), true);
        assert_eq!(names(&msg(&mut c, 4, run(&["build"]), false)), ["refuse 4", "cancel 2", "close"]);
        let mut d = ConnectionState::new();
        msg(&mut d, 2, run(&["build"]), true);
        assert_eq!(names(&key(&mut d, true)), Vec::<String>::new());
        assert_eq!(names(&key(&mut d, false)), ["cancel 2", "close"]);
    }

    #[test]
    fn unknown_messages_are_reported_not_fatal() {
        let mut c = ConnectionState::new();
        let r = msg(&mut c, CONTROL, run(&["build"]), true);
        assert_eq!(names(&r), ["report 0: unexpected a build request on the control stream"]);
        assert_eq!(
            names(&msg(&mut c, CONTROL, ClientMsg::Cancel, true)),
            ["report 0: unexpected a cancel on the control stream"]
        );
        assert_eq!(names(&msg(&mut c, CONTROL, ClientMsg::Ping(7), true)), ["reply"]);
        assert_eq!(names(&msg(&mut c, CONTROL, ClientMsg::Changed, true)), ["control"]);
        msg(&mut c, 3, run(&["build"]), true);
        assert_eq!(names(&msg(&mut c, 3, ClientMsg::Ping(1), true)), ["report 3: unexpected a ping on stream 3"]);
        assert_eq!(names(&msg(&mut c, 3, run(&["build"]), true)), ["report 3: unexpected a build request on stream 3"]);
        assert!(c.is_running(3) && !c.is_closed(), "the stream and the connection go on");
        assert_eq!(names(&msg(&mut c, 9, ClientMsg::Fetch { id: 1, hashes: vec![] }, true)), ["fetch 9"]);
        assert!(!c.is_running(9), "a fetch is not a request that runs");
    }

    #[test]
    fn an_admin_command_from_a_non_admin_is_refused() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for cmd in ["__gc", "__compact", "__export", "__import"] {
            assert!(
                matches!(command(PROTO_VERSION, &a(&[cmd, "/d"]), false, "bob"), Command::Fail(t) if t.contains("may not administer"))
            );
        }
        assert_eq!(command(PROTO_VERSION, &a(&["__gc"]), true, "ann"), Command::Gc);
        let install = format!("curl -fsSL https://rargo.rs/install.sh | sh -s -- {}", env!("CARGO_PKG_VERSION"));
        let Command::Fail(old) = command(PROTO_VERSION - 1, &a(&["build"]), false, "bob") else { panic!("an old client") };
        assert!(old.contains("is older than the build server") && old.ends_with(&format!("update it with: {install}")), "{old}");
        let Command::Fail(new) = command(PROTO_VERSION + 1, &a(&["build"]), false, "bob") else { panic!("a new client") };
        assert!(new.contains("is newer than the build server") && new.ends_with(&install), "{new}");
        assert_eq!(command(PROTO_VERSION, &a(&["__export", "/d"]), true, "ann"), Command::Export("/d".into()));
        assert_eq!(
            command(PROTO_VERSION, &a(&["__import"]), true, "ann"),
            Command::Fail("a directory is required".into())
        );
        assert_eq!(command(PROTO_VERSION, &a(&["__status"]), false, "bob"), Command::Status);
        assert_eq!(command(PROTO_VERSION, &a(&["build"]), false, "bob"), Command::Build);
        assert!(
            matches!(command(PROTO_VERSION - 1, &a(&["build"]), true, "ann"), Command::Fail(t) if t.contains("is older than the build server"))
        );
    }

    #[test]
    fn color_and_priority_follow_the_request() {
        let env =
            |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
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
        let t = status_text(&f);
        assert!(t.starts_with("requests            7\n"));
        assert!(t.contains("store               0 blobs, 3.0 MiB\n"));
        assert!(t.contains("actions by kind     rustc 4, build-script 1\n"));
        assert!(t.contains("scheduler           2 running, 0 waiting, peak 0, 0 granted, 8 tokens\n"));
        assert!(t.contains("drift               not checked yet\n"));
        assert!(t.ends_with("workers             not accepting workers\n"));
        f.workers = Some(WorkerFigures { count: 3, overhead: "12 ms".into(), ..Default::default() });
        assert!(status_text(&f).contains("workers             3 connected; 0 actions dispatched"));
        assert_eq!(t.lines().count(), 16);
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
