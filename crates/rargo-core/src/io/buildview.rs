use crate::buildview::OutputState;
use std::sync::mpsc::{channel, sync_channel, Sender};
use std::thread;

type OutputRequest = Box<dyn FnOnce(&mut OutputState) + Send>;

pub(crate) struct OutputLoop {
    requests: Sender<OutputRequest>,
}

impl OutputLoop {
    pub(crate) fn spawn() -> Self {
        let (requests, pending) = channel::<OutputRequest>();
        thread::Builder::new()
            .name("rargo-output-layer".into())
            .spawn(move || {
                let mut state = OutputState::default();
                for request in pending {
                    request(&mut state);
                }
            })
            .expect("spawn output layer loop");
        Self { requests }
    }

    pub(crate) fn call<A, T>(&self, step: impl FnOnce(&mut OutputState, A) -> T + Send + 'static, args: A) -> T
    where
        A: Send + 'static,
        T: Send + 'static,
    {
        let (reply, result) = sync_channel(1);
        assert!(self.requests.send(Box::new(move |state| {
            let _ = reply.send(step(state, args));
        })).is_ok(), "output layer loop stopped");
        result.recv().expect("output layer loop dropped its reply")
    }
}
