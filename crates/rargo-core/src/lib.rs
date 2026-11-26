pub mod action_table;
pub mod ar;
pub mod build;
pub mod buildview;
pub mod chunk;
pub mod conn_state;
pub mod db;
pub mod db_writer;
pub mod drift;
pub mod engine;
#[cfg(test)]
mod engine_tests;
pub mod expand;
pub mod flake;
pub mod fn_code;
pub mod hash;
pub mod nixmanifest;
pub mod http;
pub mod io;
pub mod incr;
pub mod cgroup;
pub use rargo_net as net;
pub mod parse;
pub mod policy;
pub mod remote;
pub mod rng;
pub mod present;
pub use rargo_proto as proto;
pub mod sandbox;
pub mod serve;
pub mod session;
pub mod pool_state;
pub mod send_ahead;
pub mod session_state;
pub mod worker_state;
pub mod snapshot;
pub mod sandboxfs;
pub mod sched;
pub mod sched_state;
pub mod store;
pub mod store_index;
pub mod timings;
pub mod tokens;
pub mod trace;
pub mod trees_state;
pub mod view;

#[cfg(test)]
mod test_support {
    use std::fmt::{Debug, Write};

    #[derive(Default)]
    pub struct Transcript(pub String);

    impl Transcript {
        pub fn record(&mut self, event: &str, effect: impl Debug) {
            writeln!(self.0, "{event}: {effect:?}").unwrap();
        }
    }
}
