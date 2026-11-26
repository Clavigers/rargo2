/// Who says a line: this machine, or the server, as abrasive's `[LOCAL]` and `[REMOTE]` tags did.
pub const LOCAL: &str = "rargo:";
pub const SERVER: &str = "rargo server:";

/// A reason rargo or its server waits before cargo has anything to say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wait {
    Connecting(String),
    Worktree,
    Queued { running: usize, waiting: usize },
    Toolchain,
    Planning,
}

/// The one line that says what is waited for, tagged with the side that waits.
pub fn line(wait: &Wait) -> String {
    match wait {
        Wait::Connecting(server) => format!("{LOCAL} connecting to {server}...\n"),
        Wait::Worktree => format!("{LOCAL} waiting for another command in this worktree to finish...\n"),
        Wait::Queued { running, waiting } => {
            format!("{SERVER} all build slots are busy ({running} running, {waiting} waiting), queued...\n")
        }
        Wait::Toolchain => format!("{SERVER} preparing the toolchain...\n"),
        Wait::Planning => format!("{SERVER} planning the build...\n"),
    }
}

/// Whether a command shows these lines: `-q` and `RARGO_SYNC_STATS=0` silence them, on both sides.
pub fn shown(args: &[String], sync_stats: Option<&str>) -> bool {
    let quiet = args.iter().take_while(|a| *a != "--").any(|a| a == "-q" || a == "--quiet");
    !quiet && sync_stats != Some("0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn each_reason_has_its_own_line_tagged_with_the_side_that_waits() {
        let cases = [
            (Wait::Connecting("rargo.rs:7459".into()), "rargo: connecting to rargo.rs:7459...\n"),
            (Wait::Worktree, "rargo: waiting for another command in this worktree to finish...\n"),
            (Wait::Queued { running: 30, waiting: 4 }, "rargo server: all build slots are busy (30 running, 4 waiting), queued...\n"),
            (Wait::Toolchain, "rargo server: preparing the toolchain...\n"),
            (Wait::Planning, "rargo server: planning the build...\n"),
        ];
        for (wait, text) in cases {
            assert_eq!(line(&wait), text);
        }
    }

    #[test]
    fn quiet_and_sync_stats_off_silence_the_lines_and_a_program_s_own_q_does_not() {
        assert!(shown(&args(&["build"]), None));
        assert!(shown(&args(&["build"]), Some("1")));
        assert!(!shown(&args(&["build", "-q"]), None));
        assert!(!shown(&args(&["--quiet", "test"]), None));
        assert!(!shown(&args(&["build"]), Some("0")));
        assert!(shown(&args(&["run", "--", "-q"]), None));
    }
}
