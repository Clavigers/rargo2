//! Whether this host has a working Nix, asked once per process: the answer
//! is a fact about the machine, not state any build owns, so the cell that
//! keeps it lives here in the shell.

use std::sync::OnceLock;

/// The first call runs `probe` and keeps its answer; every later call returns
/// that answer without probing again.
pub fn ready(probe: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    READY.get_or_init(probe).clone()
}
