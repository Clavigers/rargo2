use rargo_machine::StateMachine;
use crate::hash::Hash;
use crate::remote::WLooked;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

pub const ANNOUNCED_WAIT_MS: u64 = 60_000;
pub const RECONNECT_MS: u64 = 1_000;

pub type Listing = Vec<(Vec<u8>, u8)>;

pub trait Carried {
    type Run: Debug;
    type Part: Debug;
    type Serve: Debug;
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(bound(
    serialize = "C::Run: serde::Serialize, C::Part: serde::Serialize, C::Serve: serde::Serialize",
    deserialize = "C::Run: serde::Deserialize<'de>, C::Part: serde::Deserialize<'de>, C::Serve: serde::Deserialize<'de>"
))]
pub enum FromServer<C: Carried> {
    Welcome,
    Ask { rid: u64, serve: Option<C::Serve> },
    Push { blob: Hash, first: bool, of: u32, part: C::Part },
    Run(Run<C>),
    Cancel { action: u64 },
    Other,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(bound(serialize = "C::Run: serde::Serialize", deserialize = "C::Run: serde::Deserialize<'de>"))]
pub struct Run<C: Carried> {
    pub action: u64,
    pub incoming: Vec<Hash>,
    pub prefill: Vec<(Vec<u8>, Vec<u8>, WLooked)>,
    pub listings: Vec<(Vec<u8>, Listing)>,
    pub closed: Vec<Vec<u8>>,
    pub run: C::Run,
}

#[derive(Debug)]
pub enum Effect<C: Carried> {
    Connect { after_ms: u64 },
    Quit,
    Hello,
    Serve { rid: u64, serve: C::Serve },
    Refuse { rid: u64 },
    Assemble { blob: Hash, parts: Vec<C::Part> },
    Stored { blob: Hash },
    Start { token: u64, action: u64, run: C::Run },
    Abort { token: u64 },
    Report { token: u64, action: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Query {
    Lookup { dir: Vec<u8>, name: Vec<u8> },
    List { dir: Vec<u8> },
    Open { dir: Vec<u8>, name: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Looked(WLooked),
    Listing(Result<Listing, i32>),
    Opened(Result<Hash, i32>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Asked {
    Known(Reply),
    Ask { ticket: u64, action: u64 },
    Wait,
    Gone,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Need {
    Have,
    Wait,
    Fetch,
    Gone,
}

#[derive(Debug)]
struct Waiting<R> {
    missing: BTreeSet<Hash>,
    since_ms: u64,
    run: R,
}

#[derive(Debug)]
struct Act<R> {
    action: u64,
    waiting: Option<Waiting<R>>,
    entries: BTreeMap<(Vec<u8>, Vec<u8>), WLooked>,
    listings: BTreeMap<Vec<u8>, Listing>,
    closed: BTreeSet<Vec<u8>>,
    opened: BTreeMap<(Vec<u8>, Vec<u8>), Hash>,
}

#[derive(Debug)]
struct Incoming<P> {
    first: bool,
    of: u32,
    parts: Vec<P>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Down,
    Up,
}

#[derive(Debug)]
pub struct Worker<C: Carried> {
    link: Link,
    ever_up: bool,
    next_token: u64,
    acts: BTreeMap<u64, Act<C::Run>>,
    by_action: BTreeMap<u64, u64>,
    asks: BTreeMap<u64, (u64, Query)>,
    asking: BTreeMap<(u64, Query), u64>,
    fetching: BTreeSet<Hash>,
    partial: BTreeMap<Hash, Incoming<C::Part>>,
}

impl<C: Carried> Worker<C> {
    pub fn new() -> (Worker<C>, Vec<Effect<C>>) {
        let w = Worker {
            link: Link::Down,
            ever_up: false,
            next_token: 0,
            acts: BTreeMap::new(),
            by_action: BTreeMap::new(),
            asks: BTreeMap::new(),
            asking: BTreeMap::new(),
            fetching: BTreeSet::new(),
            partial: BTreeMap::new(),
        };
        (w, vec![Effect::Connect { after_ms: 0 }])
    }

    pub fn is_up(&self) -> bool {
        self.link == Link::Up
    }

    pub fn connected(&mut self) -> Vec<Effect<C>> {
        assert_eq!(self.link, Link::Down, "a link is made only while there is none");
        self.link = Link::Up;
        self.ever_up = true;
        vec![Effect::Hello]
    }

    pub fn connect_failed(&mut self, timed_out: bool) -> Vec<Effect<C>> {
        assert_eq!(self.link, Link::Down);
        if timed_out {
            vec![Effect::Connect { after_ms: 0 }]
        } else if self.ever_up {
            vec![Effect::Connect { after_ms: RECONNECT_MS }]
        } else {
            vec![Effect::Quit]
        }
    }

    pub fn link_lost(&mut self) -> Vec<Effect<C>> {
        assert_eq!(self.link, Link::Up, "only a link that was made is lost");
        self.link = Link::Down;
        let mut fx: Vec<Effect<C>> = self
            .acts
            .iter()
            .filter(|(_, a)| a.waiting.is_none())
            .map(|(token, _)| Effect::Abort { token: *token })
            .collect();
        self.acts.clear();
        self.by_action.clear();
        self.asks.clear();
        self.asking.clear();
        self.fetching.clear();
        self.partial.clear();
        fx.push(Effect::Connect { after_ms: RECONNECT_MS });
        fx
    }

    pub fn server(&mut self, msg: FromServer<C>, has: &dyn Fn(&Hash) -> bool, now_ms: u64) -> Vec<Effect<C>> {
        assert_eq!(self.link, Link::Up, "messages come over a link");
        match msg {
            FromServer::Welcome | FromServer::Other => Vec::new(),
            FromServer::Ask { rid, serve: Some(serve) } => vec![Effect::Serve { rid, serve }],
            FromServer::Ask { rid, serve: None } => vec![Effect::Refuse { rid }],
            FromServer::Push { blob, first, of, part } => {
                let inc = self.partial.entry(blob).or_insert_with(|| Incoming { first: false, of, parts: Vec::new() });
                inc.first |= first;
                inc.parts.push(part);
                if inc.parts.len() >= of.max(1) as usize && inc.first {
                    let inc = self.partial.remove(&blob).expect("present");
                    vec![Effect::Assemble { blob, parts: inc.parts }]
                } else {
                    Vec::new()
                }
            }
            FromServer::Run(run) => self.run(run, has, now_ms),
            FromServer::Cancel { action } => {
                let Some(token) = self.by_action.remove(&action) else { return Vec::new() };
                let act = self.end(token);
                if act.waiting.is_some() { Vec::new() } else { vec![Effect::Abort { token }] }
            }
        }
    }

    fn run(&mut self, run: Run<C>, has: &dyn Fn(&Hash) -> bool, now_ms: u64) -> Vec<Effect<C>> {
        assert_eq!(self.link, Link::Up, "a run comes over a link");
        let action = run.action;
        let mut fx = Vec::new();
        if let Some(old) = self.by_action.remove(&run.action) {
            let act = self.end(old);
            if act.waiting.is_none() {
                fx.push(Effect::Abort { token: old });
            }
        }
        self.next_token += 1;
        let token = self.next_token;
        let missing: BTreeSet<Hash> = run.incoming.into_iter().filter(|h| !has(h)).collect();
        let mut act = Act {
            action: run.action,
            waiting: None,
            entries: run.prefill.into_iter().map(|(d, n, w)| ((d, n), w)).collect(),
            listings: run.listings.into_iter().collect(),
            closed: run.closed.into_iter().collect(),
            opened: BTreeMap::new(),
        };
        let starts = missing.is_empty();
        if starts {
            fx.push(Effect::Start { token, action: run.action, run: run.run });
        } else {
            act.waiting = Some(Waiting { missing, since_ms: now_ms, run: run.run });
        }
        assert!(self.acts.insert(token, act).is_none(), "run {token} gets a fresh token");
        assert!(self.by_action.insert(action, token).is_none(), "action {action} has one current run");
        assert_eq!(self.acts[&token].waiting.is_none(), starts, "run {token} starts now or waits for its inputs");
        assert_eq!(
            fx.iter().filter(|e| matches!(e, Effect::Start { .. })).count(),
            usize::from(starts),
            "a run starts at most once, and only with all its inputs here"
        );
        fx
    }

    fn end(&mut self, token: u64) -> Act<C::Run> {
        let act = self.acts.remove(&token).expect("an action that is known ends");
        let stale: Vec<u64> = self.asks.iter().filter(|(_, (t, _))| *t == token).map(|(ticket, _)| *ticket).collect();
        for ticket in stale {
            let (t, q) = self.asks.remove(&ticket).expect("listed");
            assert_eq!(self.asking.remove(&(t, q)), Some(ticket), "each ask is listed both ways");
        }
        assert!(!self.acts.contains_key(&token), "run {token} is gone once it ended");
        assert!(self.asks.values().all(|(t, _)| *t != token), "an ended run {token} leaves no ask behind");
        assert_eq!(self.asks.len(), self.asking.len());
        act
    }

    fn start_ready(&mut self, ready: impl Fn(&Waiting<C::Run>) -> bool) -> Vec<Effect<C>> {
        let tokens: Vec<u64> =
            self.acts.iter().filter(|(_, a)| a.waiting.as_ref().is_some_and(&ready)).map(|(t, _)| *t).collect();
        let fx: Vec<Effect<C>> = tokens
            .iter()
            .map(|&token| {
                let act = self.acts.get_mut(&token).expect("listed");
                let w = act.waiting.take().expect("waiting");
                Effect::Start { token, action: act.action, run: w.run }
            })
            .collect();
        assert_eq!(fx.len(), tokens.len(), "each ready run starts once");
        assert!(tokens.iter().all(|t| self.acts[t].waiting.is_none()), "a started run no longer waits");
        assert!(self.acts.values().all(|a| a.waiting.as_ref().is_none_or(|w| !ready(w))), "no ready run is left waiting");
        fx
    }

    pub fn assembled(&mut self, blob: Hash) -> Vec<Effect<C>> {
        if self.link == Link::Down {
            assert!(self.acts.is_empty(), "a blob assembled after the link went has no run to start");
            return Vec::new();
        }
        for a in self.acts.values_mut() {
            if let Some(w) = &mut a.waiting {
                w.missing.remove(&blob);
            }
        }
        let mut fx = vec![Effect::Stored { blob }];
        fx.extend(self.start_ready(|w| w.missing.is_empty()));
        assert!(
            self.acts.values().all(|a| a.waiting.as_ref().is_none_or(|w| !w.missing.is_empty() && !w.missing.contains(&blob))),
            "no run waits for the stored blob, nor for nothing"
        );
        fx
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<Effect<C>> {
        if self.link == Link::Down {
            assert!(self.acts.is_empty(), "no run waits without a link");
            return Vec::new();
        }
        let fx = self.start_ready(|w| now_ms >= w.since_ms + ANNOUNCED_WAIT_MS);
        assert!(
            self.acts.values().all(|a| a.waiting.as_ref().is_none_or(|w| now_ms < w.since_ms + ANNOUNCED_WAIT_MS)),
            "no run waits past its wait"
        );
        fx
    }

    pub fn ended(&mut self, token: u64) -> Vec<Effect<C>> {
        match self.acts.get(&token) {
            Some(a) if a.waiting.is_none() => {
                let action = a.action;
                self.end(token);
                self.by_action.remove(&action);
                vec![Effect::Report { token, action }]
            }
            Some(_) => panic!("run {token} ends only after it started"),
            None => Vec::new(),
        }
    }

    pub fn query(&mut self, token: u64, q: Query) -> Asked {
        let Some(act) = self.acts.get_mut(&token) else { return Asked::Gone };
        assert!(act.waiting.is_none(), "a run asks only once it started");
        let known = match &q {
            Query::Lookup { dir, name } => {
                let key = (dir.clone(), name.clone());
                match act.entries.get(&key) {
                    Some(w) => Some(Reply::Looked(w.clone())),
                    None if act.closed.contains(dir)
                        && act.listings.get(dir).is_some_and(|l| !l.iter().any(|(n, _)| n == name)) =>
                    {
                        act.entries.insert(key, WLooked::Absent);
                        Some(Reply::Looked(WLooked::Absent))
                    }
                    None => None,
                }
            }
            Query::List { dir } => act.listings.get(dir).map(|l| Reply::Listing(Ok(l.clone()))),
            Query::Open { dir, name } => act.opened.get(&(dir.clone(), name.clone())).map(|h| Reply::Opened(Ok(*h))),
        };
        if let Some(r) = known {
            return Asked::Known(r);
        }
        let key = (token, q);
        if self.asking.contains_key(&key) {
            return Asked::Wait;
        }
        self.next_token += 1;
        let ticket = self.next_token;
        let action = act.action;
        self.asks.insert(ticket, key.clone());
        self.asking.insert(key, ticket);
        Asked::Ask { ticket, action }
    }

    pub fn answered(&mut self, ticket: u64, reply: Option<Reply>) {
        let Some((token, q)) = self.asks.remove(&ticket) else {
            assert!(self.asking.values().all(|t| *t != ticket), "an unknown ticket {ticket} is asked nowhere");
            return;
        };
        assert_eq!(self.asking.remove(&(token, q.clone())), Some(ticket), "ticket {ticket} is listed both ways");
        assert_eq!(self.asks.len(), self.asking.len());
        let act = self.acts.get_mut(&token).expect("an ask's run is known while it is asked");
        assert!(act.waiting.is_none(), "only a started run {token} is answered");
        match (q, reply) {
            (Query::Lookup { dir, name }, Some(Reply::Looked(w))) => {
                act.entries.insert((dir, name), w);
            }
            (Query::Lookup { dir, name }, None) => {
                act.entries.insert((dir, name), WLooked::Absent);
            }
            (Query::List { dir }, Some(Reply::Listing(Ok(l)))) => {
                act.listings.insert(dir, l);
            }
            (Query::Open { dir, name }, Some(Reply::Opened(Ok(h)))) => {
                act.opened.insert((dir, name), h);
            }
            _ => {}
        }
    }

    fn announced(&self, h: &Hash) -> bool {
        self.acts.values().any(|a| a.waiting.as_ref().is_some_and(|w| w.missing.contains(h)))
    }

    pub fn need(&mut self, h: Hash, has: bool) -> Need {
        if has {
            return Need::Have;
        }
        if self.link == Link::Down {
            assert!(self.fetching.is_empty(), "nothing is fetched without a link");
            return Need::Gone;
        }
        if self.fetching.contains(&h) || self.announced(&h) {
            return Need::Wait;
        }
        assert!(self.fetching.insert(h), "a blob is fetched once at a time");
        Need::Fetch
    }

    pub fn fetched(&mut self, h: Hash) {
        self.fetching.remove(&h);
        assert!(!self.fetching.contains(&h), "a fetched blob is no longer being fetched");
    }
}

impl<C: Carried> StateMachine for Worker<C> {
    fn invariants(&self) {
        if self.link == Link::Down {
            assert!(self.acts.is_empty() && self.asks.is_empty(), "no run outlives its link");
            assert!(self.fetching.is_empty() && self.partial.is_empty());
        }
        assert_eq!(self.by_action.len(), self.acts.len(), "each run is its action's current run");
        for (action, token) in &self.by_action {
            assert_eq!(self.acts.get(token).map(|a| a.action), Some(*action));
        }
        assert!(
            self.acts.keys().chain(self.asks.keys()).all(|t| *t <= self.next_token),
            "every token and ticket was handed out"
        );
        assert_eq!(self.asks.len(), self.asking.len());
        for (ticket, key) in &self.asks {
            assert_eq!(self.asking.get(key), Some(ticket), "one ask per question");
            let act = self.acts.get(&key.0).expect("an ask belongs to a run");
            assert!(act.waiting.is_none(), "only a started run asks");
        }
        for a in self.acts.values() {
            if let Some(w) = &a.waiting {
                assert!(!w.missing.is_empty(), "a run waits only for inputs that are missing");
            }
        }
        for inc in self.partial.values() {
            assert!(!(inc.first && inc.parts.len() >= inc.of.max(1) as usize), "a whole push is assembled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Transcript;
    use expect_test::expect;

    #[derive(Debug)]
    struct T;

    impl Carried for T {
        type Run = &'static str;
        type Part = u32;
        type Serve = &'static str;
    }

    fn h(i: u8) -> Hash {
        Hash([i; 32])
    }

    fn run(action: u64, incoming: Vec<Hash>, name: &'static str) -> FromServer<T> {
        FromServer::Run(Run {
            action,
            incoming,
            prefill: vec![(b"/w".to_vec(), b"known".to_vec(), WLooked::Other)],
            listings: vec![(b"/w/closed".to_vec(), vec![(b"a".to_vec(), 1)])],
            closed: vec![b"/w/closed".to_vec()],
            run: name,
        })
    }

    fn up() -> Worker<T> {
        let (mut w, fx) = Worker::<T>::new();
        assert!(matches!(fx[..], [Effect::Connect { after_ms: 0 }]));
        assert!(matches!(w.connected()[..], [Effect::Hello]));
        w.invariants();
        w
    }

    #[test]
    fn an_action_runs_once_all_of_its_inputs_are_here() {
        let mut outcomes = Transcript::default();
        let mut w = up();
        let have = |x: &Hash| *x == h(1);
        let fx = w.server(run(7, vec![h(1), h(2), h(3)], "rustc"), &have, 0);
        // inputs 2 and 3 are still coming.
        outcomes.record("effects", fx);
        w.invariants();
        outcomes.record("push tail", w.server(FromServer::Push { blob: h(2), first: false, of: 2, part: 1 }, &have, 1));
        let fx = w.server(FromServer::Push { blob: h(2), first: true, of: 2, part: 0 }, &have, 2);
        outcomes.record("effects", fx);
        let fx = w.assembled(h(2));
        // one input is still coming.
        outcomes.record("effects", fx);
        w.invariants();
        let fx = w.server(FromServer::Push { blob: h(3), first: true, of: 1, part: 0 }, &have, 3);
        outcomes.record("push only part", fx);
        let fx = w.assembled(h(3));
        outcomes.record("start", fx);
        w.invariants();
        let fx = w.server(run(8, vec![h(1)], "cc"), &have, 4);
        // an action whose inputs are all here runs at once.
        outcomes.record("start", fx);
        w.invariants();
        expect![[r#"
            effects: []
            push tail: []
            effects: [Assemble { blob: 0202020202020202, parts: [1, 0] }]
            effects: [Stored { blob: 0202020202020202 }]
            push only part: [Assemble { blob: 0303030303030303, parts: [0] }]
            start: [Stored { blob: 0303030303030303 }, Start { token: 1, action: 7, run: "rustc" }]
            start: [Start { token: 2, action: 8, run: "cc" }]
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn an_input_that_never_comes_is_fetched_after_the_wait() {
        let mut outcomes = Transcript::default();
        let mut w = up();
        outcomes.record("server", w.server(run(1, vec![h(9)], "x"), &|_| false, 100));
        // an announced input is waited for, not fetched.
        outcomes.record("need", w.need(h(9), false));
        outcomes.record("tick", w.tick(100 + ANNOUNCED_WAIT_MS - 1));
        let fx = w.tick(100 + ANNOUNCED_WAIT_MS);
        outcomes.record("start", fx);
        // after the wait the run fetches it itself.
        outcomes.record("need", w.need(h(9), false));
        // and once.
        outcomes.record("need", w.need(h(9), false));
        w.fetched(h(9));
        outcomes.record("need", w.need(h(9), true));
        w.invariants();
        expect![[r#"
            server: []
            need: Wait
            tick: []
            start: [Start { token: 1, action: 1, run: "x" }]
            need: Fetch
            need: Wait
            need: Have
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn missing_files_are_asked_for_once() {
        let mut outcomes = Transcript::default();
        let mut w = up();
        let fx = w.server(run(3, vec![], "rustc"), &|_| true, 0);
        let Effect::Start { token, .. } = fx[0] else { panic!("runs") };
        let q = Query::Lookup { dir: b"/w".to_vec(), name: b"lib.rs".to_vec() };
        outcomes.record("query", w.query(token, Query::Lookup { dir: b"/w".to_vec(), name: b"known".to_vec() }));
        // a name missing from a closed directory is absent without asking.
        outcomes.record("query", w.query(token, Query::Lookup { dir: b"/w/closed".to_vec(), name: b"b".to_vec() }));
        let Asked::Ask { ticket, action: 3 } = w.query(token, q.clone()) else { panic!("the server is asked") };
        // a second reader waits for the same answer.
        outcomes.record("query", w.query(token, q.clone()));
        w.invariants();
        let file = WLooked::File { size: 3, mode: 0o644, own: false, hash: None, sem: None };
        w.answered(ticket, Some(Reply::Looked(file.clone())));
        outcomes.record("query", w.query(token, q.clone()));
        let open = Query::Open { dir: b"/w".to_vec(), name: b"lib.rs".to_vec() };
        let Asked::Ask { ticket, .. } = w.query(token, open.clone()) else { panic!("opened once") };
        w.answered(ticket, Some(Reply::Opened(Ok(h(4)))));
        outcomes.record("query", w.query(token, open));
        let list = Query::List { dir: b"/w/src".to_vec() };
        let Asked::Ask { ticket, .. } = w.query(token, list.clone()) else { panic!("listed once") };
        w.answered(ticket, Some(Reply::Listing(Err(2))));
        // a failed listing is asked again.
        outcomes.record("query", w.query(token, list));
        outcomes.record("need", w.need(h(5), false));
        // content is fetched once at a time.
        outcomes.record("need", w.need(h(5), false));
        w.invariants();
        expect![[r#"
            query: Known(Looked(Other))
            query: Known(Looked(Absent))
            query: Wait
            query: Known(Looked(File { size: 3, mode: 420, own: false, hash: None, sem: None }))
            query: Known(Opened(Ok(0404040404040404)))
            query: Ask { ticket: 5, action: 3 }
            need: Fetch
            need: Wait
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn a_server_restart_is_survived() {
        let mut outcomes = Transcript::default();
        let mut w = up();
        let fx = w.server(run(1, vec![], "a"), &|_| true, 0);
        let Effect::Start { token: old, .. } = fx[0] else { panic!("runs") };
        w.server(run(2, vec![h(1)], "b"), &|_| false, 0);
        w.server(FromServer::Push { blob: h(1), first: true, of: 2, part: 0 }, &|_| false, 0);
        let Asked::Ask { ticket, .. } = w.query(old, Query::List { dir: b"/w".to_vec() }) else { panic!() };
        let fx = w.link_lost();
        outcomes.record("effects", fx);
        w.invariants();
        // a reader of a lost run is told so.
        outcomes.record("query", w.query(old, Query::List { dir: b"/w".to_vec() }));
        w.answered(ticket, None);
        outcomes.record("need", w.need(h(1), false));
        // it keeps trying.
        outcomes.record("connect failed", w.connect_failed(false));
        // and says hello to the new server.
        outcomes.record("connected", w.connected());
        let fx = w.server(run(1, vec![], "a again"), &|_| true, 10);
        let Effect::Start { token: new, .. } = fx[0] else { panic!("runs") };
        assert_ne!(new, old);
        // the old run's end reports nothing.
        outcomes.record("ended", w.ended(old));
        outcomes.record("ended", w.ended(new));
        // a run reports once.
        outcomes.record("ended", w.ended(new));
        w.invariants();
        let (mut first, _) = Worker::<T>::new();
        // a server never reached is an error.
        outcomes.record("connect failed", first.connect_failed(false));
        outcomes.record("connect failed", first.connect_failed(true));
        expect![[r#"
            effects: [Abort { token: 1 }, Connect { after_ms: 1000 }]
            query: Gone
            need: Gone
            connect failed: [Connect { after_ms: 1000 }]
            connected: [Hello]
            ended: []
            ended: [Report { token: 4, action: 1 }]
            ended: []
            connect failed: [Quit]
            connect failed: [Connect { after_ms: 0 }]
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn a_cancel_stops_a_running_action_and_forgets_a_waiting_one() {
        let mut outcomes = Transcript::default();
        let mut w = up();
        let fx = w.server(run(1, vec![], "a"), &|_| true, 0);
        let Effect::Start { token, .. } = fx[0] else { panic!() };
        outcomes.record("server", w.server(FromServer::Cancel { action: 1 }, &|_| true, 0));
        // a cancelled run reports nothing.
        outcomes.record("ended", w.ended(token));
        w.server(run(2, vec![h(3)], "b"), &|_| false, 0);
        outcomes.record("server", w.server(FromServer::Cancel { action: 2 }, &|_| false, 0));
        // a cancelled waiting run never starts.
        outcomes.record("assemble after cancel", w.assembled(h(3)));
        outcomes.record("server", w.server(FromServer::Ask { rid: 4, serve: None }, &|_| true, 0));
        outcomes.record("server", w.server(FromServer::Ask { rid: 5, serve: Some("blob") }, &|_| true, 0));
        w.invariants();
        expect![[r#"
            server: [Abort { token: 1 }]
            ended: []
            server: []
            assemble after cancel: [Stored { blob: 0303030303030303 }]
            server: [Refuse { rid: 4 }]
            server: [Serve { rid: 5, serve: "blob" }]
        "#]].assert_eq(&outcomes.0);
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

    mod generated_server_sessions_run_each_action_once_its_inputs_are_here_and_ask_each_question_once {
        rargo_machine::test_batches! {
            super::check_generated_server_sessions;
            seeds_1_to_150: 1..=150,
            seeds_151_to_300: 151..=300,
        }
    }

    fn check_generated_server_sessions(seeds: std::ops::RangeInclusive<u64>) {
        for seed in seeds {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let (mut w, _) = Worker::<T>::new();
            let mut store: BTreeSet<Hash> = BTreeSet::new();
            let mut up = false;
            let mut now = 0u64;
            let mut runs: BTreeMap<u64, (u64, BTreeSet<Hash>, u64)> = BTreeMap::new();
            let mut running: BTreeMap<u64, u64> = BTreeMap::new();
            let mut aborted: BTreeSet<u64> = BTreeSet::new();
            let mut open: BTreeMap<u64, (u64, u64, Query)> = BTreeMap::new();
            let mut cached: BTreeSet<(u64, Query)> = BTreeSet::new();
            let mut reported: BTreeSet<u64> = BTreeSet::new();
            let mut pushes: Vec<(Hash, u32, u32)> = Vec::new();
            let mut next_action = 1u64;
            for _ in 0..300 {
                now += rng.below(5_000);
                let fx = if !up {
                    if rng.below(3) == 0 {
                        w.connect_failed(rng.below(2) == 0)
                    } else {
                        up = true;
                        w.connected()
                    }
                } else {
                    match rng.below(10) {
                        0 | 1 => {
                            let action = if rng.below(4) == 0 { 1 + rng.below(next_action) } else { next_action };
                            next_action = next_action.max(action + 1);
                            let incoming: Vec<Hash> =
                                (0..rng.below(3)).map(|_| Hash([rng.below(12) as u8; 32])).collect();
                            for b in &incoming {
                                if !store.contains(b) && !pushes.iter().any(|(p, _, _)| p == b) && rng.below(6) != 0 {
                                    let of = 1 + rng.below(3) as u32;
                                    for p in 0..of {
                                        pushes.push((*b, p, of));
                                    }
                                }
                            }
                            let missing: BTreeSet<Hash> =
                                incoming.iter().filter(|b| !store.contains(b)).copied().collect();
                            runs.insert(action, (now, missing, 0));
                            let s = store.clone();
                            w.server(run(action, incoming, "r"), &|b| s.contains(b), now)
                        }
                        2 | 3 if !pushes.is_empty() => {
                            let i = rng.below(pushes.len() as u64) as usize;
                            let (blob, p, of) = pushes.remove(i);
                            let s = store.clone();
                            let fx = w.server(
                                FromServer::Push { blob, first: p == 0, of, part: p },
                                &|b| s.contains(b),
                                now,
                            );
                            let mut out = Vec::new();
                            for e in fx {
                                if let Effect::Assemble { blob, parts } = e {
                                    assert_eq!(parts.len() as u32, of, "seed {seed}: every part is assembled");
                                    store.insert(blob);
                                    out.extend(w.assembled(blob));
                                } else {
                                    out.push(e);
                                }
                            }
                            out
                        }
                        4 if !running.is_empty() => {
                            let tokens: Vec<u64> = running.keys().copied().collect();
                            let token = tokens[rng.below(tokens.len() as u64) as usize];
                            let dir = vec![b'd', rng.below(3) as u8];
                            let q = match rng.below(3) {
                                0 => Query::Lookup { dir, name: vec![rng.below(3) as u8] },
                                1 => Query::List { dir },
                                _ => Query::Open { dir, name: vec![rng.below(3) as u8] },
                            };
                            match w.query(token, q.clone()) {
                                Asked::Ask { ticket, .. } => {
                                    assert!(
                                        !open.values().any(|(t, _, oq)| *t == token && *oq == q),
                                        "seed {seed}: a question already asked is not asked again"
                                    );
                                    assert!(
                                        !cached.contains(&(token, q.clone())),
                                        "seed {seed}: an answered question is not asked again"
                                    );
                                    open.insert(ticket, (token, 0, q));
                                }
                                Asked::Wait => assert!(open.values().any(|(t, _, oq)| *t == token && *oq == q)),
                                Asked::Known(_) => {}
                                Asked::Gone => panic!("seed {seed}: a running run is known"),
                            }
                            Vec::new()
                        }
                        5 if !open.is_empty() => {
                            let tickets: Vec<u64> = open.keys().copied().collect();
                            let ticket = tickets[rng.below(tickets.len() as u64) as usize];
                            let (token, _, q) = open.remove(&ticket).unwrap();
                            let lost = rng.below(8) == 0;
                            let reply = match q {
                                Query::Lookup { .. } => Some(Reply::Looked(WLooked::Other)),
                                Query::List { .. } => Some(Reply::Listing(Ok(vec![]))),
                                Query::Open { .. } => Some(Reply::Opened(Ok(Hash([7; 32])))),
                            };
                            if !lost || matches!(q, Query::Lookup { .. }) {
                                cached.insert((token, q));
                            }
                            w.answered(ticket, if lost { None } else { reply });
                            Vec::new()
                        }
                        6 if !running.is_empty() => {
                            let tokens: Vec<u64> = running.keys().copied().collect();
                            let token = tokens[rng.below(tokens.len() as u64) as usize];
                            running.remove(&token);
                            w.ended(token)
                        }
                        7 => {
                            let action = 1 + rng.below(next_action);
                            runs.remove(&action);
                            w.server(FromServer::Cancel { action }, &|_| true, now)
                        }
                        8 if rng.below(4) == 0 => {
                            up = false;
                            runs.clear();
                            pushes.clear();
                            open.clear();
                            w.link_lost()
                        }
                        _ => w.tick(now),
                    }
                };
                for e in fx {
                    match e {
                        Effect::Start { token, action, .. } => {
                            let (since, missing, _) = runs
                                .get(&action)
                                .cloned()
                                .unwrap_or_else(|| panic!("seed {seed}: {action} was not asked to run"));
                            assert!(
                                missing.iter().all(|b| store.contains(b)) || now >= since + ANNOUNCED_WAIT_MS,
                                "seed {seed}: {action} runs only once its inputs are here or the wait is over"
                            );
                            assert!(running.insert(token, action).is_none());
                            runs.get_mut(&action).unwrap().2 += 1;
                            assert_eq!(runs[&action].2, 1, "seed {seed}: one run starts per request");
                        }
                        Effect::Abort { token } => {
                            assert!(running.remove(&token).is_some(), "seed {seed}: only a running run is aborted");
                            aborted.insert(token);
                        }
                        Effect::Report { token, .. } => {
                            assert!(!aborted.contains(&token), "seed {seed}: an aborted run reports nothing");
                            assert!(reported.insert(token), "seed {seed}: a run reports once");
                        }
                        Effect::Connect { .. } | Effect::Quit => assert!(!up),
                        Effect::Hello => assert!(up),
                        _ => {}
                    }
                }
                if !up {
                    assert!(running.is_empty(), "seed {seed}: nothing runs without a link");
                }
                w.invariants();
            }
        }
    }
}
