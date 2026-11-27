use std::time::Duration;

/// How long a link over UDP may hear nothing at all from the server before the command gives up on it.
pub const SILENT_AFTER: Duration = Duration::from_secs(45);
/// TCP queues the server's answers behind everything already in flight, so it is given longer.
pub const SILENT_AFTER_TCP: Duration = Duration::from_secs(6 * 60);
/// How often the client pings, so a live server always has something to answer.
pub const PING_EVERY: Duration = Duration::from_secs(10);

const _: () = assert!(SILENT_AFTER.as_secs() > 2 * PING_EVERY.as_secs());

/// What the client does next on its link, by the clock it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tick {
    Wait,
    Ping(u64),
    /// Two pings went unanswered: dial the server afresh, and report with `probed` whether it answered.
    Probe,
    Silent(Duration),
}

/// Why a link was given up, and how long the server had been silent on it then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Silence {
    /// Nothing was heard for the whole limit.
    Quiet(Duration),
    /// The server answered a fresh link while this one stayed silent: it no longer knows this one, as after a restart.
    Restarted(Duration),
}

impl Silence {
    pub fn quiet(self) -> Duration {
        match self {
            Silence::Quiet(d) | Silence::Restarted(d) => d,
        }
    }
}

/// Watches one link: when it last heard the server, when to ping, and when silence means the server is gone.
#[derive(Clone, Debug)]
pub struct Watch {
    limit: Duration,
    ping_every: Duration,
    heard: Duration,
    pinged: Duration,
    pings: u64,
    probing: Option<Duration>,
    probed: Option<Duration>,
    silent: Option<Silence>,
}

impl Watch {
    pub fn new(now: Duration, limit: Duration) -> Watch {
        let ping_every = PING_EVERY.min(limit / 3).max(Duration::from_millis(500));
        Watch { limit, ping_every, heard: now, pinged: now, pings: 0, probing: None, probed: None, silent: None }
    }

    pub fn limit(&self) -> Duration {
        self.limit
    }

    pub fn ping_every(&self) -> Duration {
        self.ping_every
    }

    /// How long the server had been silent when the link was given up, once it has been.
    pub fn silent(&self) -> Option<Silence> {
        self.silent
    }

    /// The server was heard at `at`; an earlier time than one already known changes nothing.
    pub fn heard(&mut self, at: Duration) {
        self.heard = self.heard.max(at);
    }

    /// The fresh dial asked for by `Tick::Probe` ended at `now`; `answered` if the server took it.
    /// When this link heard nothing since the probe began, the server has forgotten it, and the link is given up.
    pub fn probed(&mut self, now: Duration, answered: bool) -> Option<Silence> {
        let started = self.probing.take()?;
        self.probed = Some(started);
        if self.silent.is_some() || !answered || self.heard >= started {
            return None;
        }
        let gone = Silence::Restarted(now.saturating_sub(self.heard));
        self.silent = Some(gone);
        Some(gone)
    }

    pub fn tick(&mut self, now: Duration) -> Tick {
        if let Some(gone) = self.silent {
            return Tick::Silent(gone.quiet());
        }
        let quiet = now.saturating_sub(self.heard);
        if quiet > self.limit {
            self.silent = Some(Silence::Quiet(quiet));
            return Tick::Silent(quiet);
        }
        if now.saturating_sub(self.pinged) >= self.ping_every {
            self.pinged = now;
            self.pings += 1;
            return Tick::Ping(self.pings);
        }
        let probe_again = self.probed.is_none_or(|at| now.saturating_sub(at) >= self.ping_every);
        if self.probing.is_none() && quiet >= 2 * self.ping_every && probe_again {
            self.probing = Some(now);
            return Tick::Probe;
        }
        Tick::Wait
    }
}

/// The line a command ends with when its server went silent: the server, how long, and what was awaited.
pub fn silent_text(server: &str, gone: Silence, waiting_for: &str) -> String {
    match gone {
        Silence::Quiet(quiet) => format!(
            "error: the rargo server at {server} sent nothing for {} s while rargo waited for {waiting_for}; it may be stopped or overloaded\n",
            quiet.as_secs()
        ),
        Silence::Restarted(_) => format!(
            "error: the rargo server at {server} restarted while rargo waited for {waiting_for}, and lost this command; run the command again\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_live_server_that_answers_pings_is_never_silent_however_long_the_build() {
        let mut w = Watch::new(ms(0), SILENT_AFTER);
        let mut pings = 0;
        for t in (200..3_600_000).step_by(200) {
            match w.tick(ms(t)) {
                Tick::Ping(_) => {
                    pings += 1;
                    w.heard(ms(t + 150));
                }
                Tick::Wait => {}
                Tick::Probe => panic!("probed at {t} ms, though every ping was answered"),
                Tick::Silent(q) => panic!("silent at {t} ms after {q:?}"),
            }
        }
        assert_eq!(pings, 3_600_000 / PING_EVERY.as_millis() as u64 - 1);
    }

    enum Ev {
        Heard(u64),
        Tick(u64),
        Probed(u64, bool),
    }

    /// Feeds the events to a watch made at 0 ms with `limit`, and lists what each tick decided.
    fn run(limit: Duration, events: impl IntoIterator<Item = Ev>) -> String {
        let mut w = Watch::new(ms(0), limit);
        let mut out = String::new();
        for ev in events {
            match ev {
                Ev::Heard(t) => {
                    w.heard(ms(t));
                    out += &format!("heard {t} ms\n");
                }
                Ev::Tick(t) => match w.tick(ms(t)) {
                    Tick::Wait => {}
                    tick => out += &format!("tick {t} ms => {tick:?}\n"),
                },
                Ev::Probed(t, answered) => out += &format!("probe {} at {t} ms => {:?}\n", if answered { "answered" } else { "unanswered" }, w.probed(ms(t), answered)),
            }
        }
        out + &format!("silent: {:?}\n", w.silent())
    }

    #[test]
    fn a_server_that_stops_answering_is_silent_once_the_limit_passes_and_stays_so() {
        let ticks = (1..=240).map(|n| Ev::Tick(n * 200));
        let events = std::iter::once(Ev::Heard(1_000)).chain(ticks.take_while(|e| !matches!(e, Ev::Tick(t) if *t > 46_400)));
        let events = events.chain([Ev::Heard(46_401), Ev::Tick(46_600)]);
        expect![[r#"
            heard 1000 ms
            tick 10000 ms => Ping(1)
            tick 20000 ms => Ping(2)
            tick 21000 ms => Probe
            tick 30000 ms => Ping(3)
            tick 40000 ms => Ping(4)
            tick 46200 ms => Silent(45.2s)
            tick 46400 ms => Silent(45.2s)
            heard 46401 ms
            tick 46600 ms => Silent(45.2s)
            silent: Some(Quiet(45.2s))
        "#]].assert_eq(&run(SILENT_AFTER, events));
    }

    #[test]
    fn hearing_an_older_time_does_not_move_the_last_heard_back() {
        let events = [Ev::Heard(2_000), Ev::Heard(500), Ev::Tick(5_000), Ev::Tick(5_001)];
        expect![[r#"
            heard 2000 ms
            heard 500 ms
            tick 5000 ms => Ping(1)
            tick 5001 ms => Silent(3.001s)
            silent: Some(Quiet(3.001s))
        "#]].assert_eq(&run(ms(3_000), events));
    }

    #[test]
    fn a_short_limit_pings_three_times_within_it() {
        expect![[r#"
            tick 1000 ms => Ping(1)
            tick 2000 ms => Ping(2)
            tick 2200 ms => Probe
            tick 3000 ms => Ping(3)
            silent: None
        "#]].assert_eq(&run(ms(3_000), (1..=15).map(|n| Ev::Tick(n * 200))));
    }

    fn ticks(from: u64, to: u64) -> impl Iterator<Item = Ev> {
        (from / 200..=to / 200).map(|n| Ev::Tick(n * 200))
    }

    #[test]
    fn a_server_that_restarted_is_given_up_once_a_fresh_dial_answers_while_the_link_stays_silent() {
        let events = std::iter::once(Ev::Heard(1_000)).chain(ticks(200, 21_000)).chain([Ev::Probed(21_100, true)]).chain(ticks(21_200, 21_400));
        expect![[r#"
            heard 1000 ms
            tick 10000 ms => Ping(1)
            tick 20000 ms => Ping(2)
            tick 21000 ms => Probe
            probe answered at 21100 ms => Some(Restarted(20.1s))
            tick 21200 ms => Silent(20.1s)
            tick 21400 ms => Silent(20.1s)
            silent: Some(Restarted(20.1s))
        "#]].assert_eq(&run(SILENT_AFTER, events));
    }

    #[test]
    fn a_probe_that_goes_unanswered_is_tried_again_a_ping_later_and_the_limit_still_holds() {
        let events = ticks(200, 20_200).chain([Ev::Probed(22_000, false)]).chain(ticks(22_200, 45_200));
        expect![[r#"
            tick 10000 ms => Ping(1)
            tick 20000 ms => Ping(2)
            tick 20200 ms => Probe
            probe unanswered at 22000 ms => None
            tick 30000 ms => Ping(3)
            tick 30200 ms => Probe
            tick 40000 ms => Ping(4)
            tick 45200 ms => Silent(45.2s)
            silent: Some(Quiet(45.2s))
        "#]].assert_eq(&run(SILENT_AFTER, events));
    }

    #[test]
    fn a_probe_answered_after_the_link_spoke_again_changes_nothing() {
        let events = ticks(200, 20_200).chain([Ev::Heard(20_300), Ev::Probed(20_500, true)]).chain(ticks(20_600, 41_000));
        expect![[r#"
            tick 10000 ms => Ping(1)
            tick 20000 ms => Ping(2)
            tick 20200 ms => Probe
            heard 20300 ms
            probe answered at 20500 ms => None
            tick 30000 ms => Ping(3)
            tick 40000 ms => Ping(4)
            tick 40400 ms => Probe
            silent: None
        "#]].assert_eq(&run(SILENT_AFTER, events));
    }

    #[test]
    fn the_restarted_line_says_to_run_the_command_again() {
        expect![[r#"
            error: the rargo server at rargo.rs:7459 restarted while rargo waited for the build's progress, and lost this command; run the command again
        "#]].assert_eq(&silent_text("rargo.rs:7459", Silence::Restarted(ms(20_100)), "the build's progress"));
    }

    #[test]
    fn the_silent_line_names_the_server_the_time_and_what_was_awaited() {
        expect![[r#"
            error: the rargo server at rargo.rs:7459 sent nothing for 45 s while rargo waited for the build's output; it may be stopped or overloaded
        "#]].assert_eq(&silent_text("rargo.rs:7459", Silence::Quiet(ms(45_200)), "the build's output"));
    }
}
