//! rargo-core's shell: everything in `rargo_net::io`, which a simulation can
//! answer from memory, plus submodules for the system calls only a real
//! kernel can carry out (namespaces, mounts, HTTP), which are not tested.

pub use rargo_net::io::*;

pub(crate) mod db;
pub mod http;
pub mod sandbox;
pub mod sys;
pub mod thread;
