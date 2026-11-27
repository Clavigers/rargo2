//! The threads that own a build view's live state: its output layer and its
//! host memo. Other threads send a loop steps and wait for each answer, so the
//! state has one owner and no lock.
//!
//! Every call waits for its answer before it returns, so the order of steps
//! is the order of the calls: a simulation that reaches this sees the same
//! results as if the steps ran where they were asked for.

use rargo_machine::StateMachine;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{channel, sync_channel, Sender};

type Request<S> = Box<dyn FnOnce(&mut S) + Send>;

pub(crate) struct StateLoop<S> {
    requests: Sender<Request<S>>,
}

impl<S: StateMachine + Send + 'static> StateLoop<S> {
    pub(crate) fn spawn(name: &str, mut state: S) -> Self {
        let (requests, pending) = channel::<Request<S>>();
        super::thread::spawn(name, move || {
            for request in pending {
                request(&mut state);
            }
        })
        .expect("spawn build view state loop");
        Self { requests }
    }

    /// Runs `step` on the owned state, checks the state's invariants, and
    /// returns the step's answer. A step that panics panics here, in the
    /// caller, and the loop goes on.
    pub(crate) fn call<A, T>(&self, step: impl FnOnce(&mut S, A) -> T + Send + 'static, args: A) -> T
    where
        A: Send + 'static,
        T: Send + 'static,
    {
        let (reply, result) = sync_channel(1);
        assert!(
            self.requests
                .send(Box::new(move |state: &mut S| {
                    let _ = reply.send(panic::catch_unwind(AssertUnwindSafe(|| {
                        let answer = step(state, args);
                        state.invariants();
                        answer
                    })));
                }))
                .is_ok(),
            "build view state loop stopped"
        );
        match result.recv().expect("build view state loop dropped its reply") {
            Ok(answer) => answer,
            Err(cause) => panic::resume_unwind(cause),
        }
    }

    /// Sends `step` without waiting for it. A loop that has stopped drops it.
    pub(crate) fn tell(&self, step: impl FnOnce(&mut S) + Send + 'static) {
        let _ = self.requests.send(Box::new(move |state: &mut S| {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                step(state);
                state.invariants();
            }));
        }));
    }
}
