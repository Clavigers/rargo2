//! rargo-core's shell: everything in `rargo_net::io`, which a simulation can
//! answer from memory, plus submodules for the system calls only a real
//! kernel can carry out (namespaces, mounts, cgroups), which are not tested.

pub use rargo_net::io::*;

pub mod http;
pub mod map;
pub mod pipe;
pub mod thread;
