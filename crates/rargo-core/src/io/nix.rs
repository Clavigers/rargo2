//! Whether this host has a working Nix, asked once per process: the answer
//! is a fact about the machine, not state any build owns, so the cell that
//! keeps it lives here in the shell.

use std::sync::OnceLock;

/// The first call runs `probe` and keeps its answer; every later call returns
/// that answer without probing again.
pub fn ready(probe: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    static READY: Answer<Result<(), String>> = Answer::new();
    READY.get(probe)
}

/// A fact found once, kept for the rest of the process.
struct Answer<T>(OnceLock<T>);

impl<T: Clone> Answer<T> {
    const fn new() -> Self {
        Answer(OnceLock::new())
    }

    /// The probe runs a child process, so its fiber is suspended while it runs and
    /// another request's fiber on the same loop thread may ask too. It therefore
    /// runs outside the cell's lock: inside `get_or_init` the second fiber would
    /// block the loop thread on the lock the suspended first one holds, for good
    /// (#805). Two first callers may each probe; they ask the same machine, and
    /// the first answer is the one kept.
    fn get(&self, probe: impl FnOnce() -> T) -> T {
        if let Some(known) = self.0.get() {
            return known.clone();
        }
        let fresh = probe();
        self.0.get_or_init(|| fresh).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_second_fiber_asking_while_the_first_probes_does_not_stop_the_loop() {
        if crate::io::unavailable().is_some() {
            return;
        }
        let done = crate::io::within(Duration::from_secs(20), || {
            crate::io::run(1, || {
                let cell: Arc<Answer<u32>> = Arc::new(Answer::new());
                let mut first = {
                    let cell = cell.clone();
                    crate::io::spawn(move || {
                        cell.get(|| {
                            crate::io::sleep(Duration::from_millis(200));
                            1
                        })
                    })
                };
                crate::io::sleep(Duration::from_millis(20));
                let mut second = {
                    let cell = cell.clone();
                    crate::io::spawn(move || cell.get(|| 2))
                };
                let second = second.join().expect("second fiber");
                let first = first.join().expect("first fiber");
                (first, second, cell.get(|| 3))
            })
        });
        let (first, second, later) = done.expect("the loop thread blocked while a fiber probed");
        assert_eq!(second, 2, "the second asker did not wait for the first probe");
        assert_eq!(first, 2, "the first answer kept is the one every caller gets");
        assert_eq!(later, 2);
    }
}
