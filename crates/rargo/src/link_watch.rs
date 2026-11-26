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
    Silent(Duration),
}

/// Watches one link: when it last heard the server, when to ping, and when silence means the server is gone.
#[derive(Clone, Debug)]
pub struct Watch {
    limit: Duration,
    ping_every: Duration,
    heard: Duration,
    pinged: Duration,
    pings: u64,
    silent: Option<Duration>,
}

impl Watch {
    pub fn new(now: Duration, limit: Duration) -> Watch {
        let ping_every = PING_EVERY.min(limit / 3).max(Duration::from_millis(500));
        Watch { limit, ping_every, heard: now, pinged: now, pings: 0, silent: None }
    }

    pub fn limit(&self) -> Duration {
        self.limit
    }

    /// How long the server had been silent when the link was given up, once it has been.
    pub fn silent(&self) -> Option<Duration> {
        self.silent
    }

    /// The server was heard at `at`; an earlier time than one already known changes nothing.
    pub fn heard(&mut self, at: Duration) {
        self.heard = self.heard.max(at);
    }

    pub fn tick(&mut self, now: Duration) -> Tick {
        if let Some(quiet) = self.silent {
            return Tick::Silent(quiet);
        }
        let quiet = now.saturating_sub(self.heard);
        if quiet > self.limit {
            self.silent = Some(quiet);
            return Tick::Silent(quiet);
        }
        if now.saturating_sub(self.pinged) >= self.ping_every {
            self.pinged = now;
            self.pings += 1;
            return Tick::Ping(self.pings);
        }
        Tick::Wait
    }
}

/// The line a command ends with when its server went silent: the server, how long, and what was awaited.
pub fn silent_text(server: &str, quiet: Duration, waiting_for: &str) -> String {
    format!("error: the rargo server at {server} sent nothing for {} s while rargo waited for {waiting_for}; it may be stopped or overloaded\n", quiet.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                Tick::Silent(q) => panic!("silent at {t} ms after {q:?}"),
            }
        }
        assert_eq!(pings, 3_600_000 / PING_EVERY.as_millis() as u64 - 1);
    }

    #[test]
    fn a_server_that_stops_answering_is_silent_once_the_limit_passes_and_stays_so() {
        let mut w = Watch::new(ms(0), SILENT_AFTER);
        w.heard(ms(1_000));
        let gone = (1..).map(|n| n * 200).find(|t| matches!(w.tick(ms(*t)), Tick::Silent(_))).unwrap();
        assert_eq!(gone, 1_000 + SILENT_AFTER.as_millis() as u64 + 200);
        w.heard(ms(gone + 1));
        assert_eq!(w.tick(ms(gone + 200)), Tick::Silent(SILENT_AFTER + ms(200)));
        assert_eq!(w.silent(), Some(SILENT_AFTER + ms(200)));
    }

    #[test]
    fn hearing_an_older_time_does_not_move_the_last_heard_back() {
        let mut w = Watch::new(ms(0), ms(3_000));
        w.heard(ms(2_000));
        w.heard(ms(500));
        assert_eq!(w.tick(ms(5_000)), Tick::Ping(1));
        assert_eq!(w.tick(ms(5_001)), Tick::Silent(ms(3_001)));
    }

    #[test]
    fn a_short_limit_pings_three_times_within_it() {
        let mut w = Watch::new(ms(0), ms(3_000));
        let pings: Vec<u64> = (1..=15).map(|n| n * 200).filter(|t| matches!(w.tick(ms(*t)), Tick::Ping(_))).collect();
        assert_eq!(pings, [1_000, 2_000, 3_000]);
    }

    #[test]
    fn the_silent_line_names_the_server_the_time_and_what_was_awaited() {
        assert_eq!(
            silent_text("rargo.rs:7459", ms(45_200), "the build's output"),
            "error: the rargo server at rargo.rs:7459 sent nothing for 45 s while rargo waited for the build's output; it may be stopped or overloaded\n"
        );
    }
}
