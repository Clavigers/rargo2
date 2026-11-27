//! The thread that owns a build's live output layer. Other threads send it
//! steps and wait for each answer, so the state has one owner and no lock.
//!
//! Every call waits for its answer before it returns, so the order of steps
//! is the order of the calls: a simulation that reaches this sees the same
//! results as if the steps ran where they were asked for.

use crate::buildview::OutputState;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{channel, sync_channel, Sender};

type OutputRequest = Box<dyn FnOnce(&mut OutputState) + Send>;

pub(crate) struct OutputLoop {
    requests: Sender<OutputRequest>,
}

impl OutputLoop {
    pub(crate) fn spawn() -> Self {
        let (requests, pending) = channel::<OutputRequest>();
        super::thread::spawn("rargo-output-layer", move || {
            let mut state = OutputState::default();
            for request in pending {
                request(&mut state);
            }
        })
        .expect("spawn output layer loop");
        Self { requests }
    }

    /// Runs `step` on the owned state and returns its answer. A step that
    /// panics panics here, in the caller, and the loop goes on.
    pub(crate) fn call<A, T>(&self, step: impl FnOnce(&mut OutputState, A) -> T + Send + 'static, args: A) -> T
    where
        A: Send + 'static,
        T: Send + 'static,
    {
        let (reply, result) = sync_channel(1);
        assert!(
            self.requests
                .send(Box::new(move |state| {
                    let _ = reply.send(panic::catch_unwind(AssertUnwindSafe(|| step(state, args))));
                }))
                .is_ok(),
            "output layer loop stopped"
        );
        match result.recv().expect("output layer loop dropped its reply") {
            Ok(answer) => answer,
            Err(cause) => panic::resume_unwind(cause),
        }
    }
}
