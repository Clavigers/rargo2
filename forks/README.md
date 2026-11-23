# Forks

rargo carries its changes to cargo and rustc as patch series against pinned
upstream commits. The working checkouts live under `/root/rargo-work`.

- `forks/<name>/BASE` is the upstream commit; `forks/<name>/*.patch` apply on
  top (`<name>` is `cargo` or `rust`).
- `forks/setup.sh` clones upstream at the bases and applies the patches.
- `forks/export.sh` regenerates the patch files from the working checkouts.
- `forks/build.sh` builds the forked cargo (the planner).
- `forks/build-rust.sh` builds the forked rustc and links it into rustup as
  the toolchain `rargo`; `forks/rust/bootstrap.toml` is its configuration.

The cargo fork is required: it is rargo's planner. The rustc fork is
optional. Without it rargo works with any toolchain; with it (select it with
`RUSTUP_TOOLCHAIN=rargo` or `rargo +rargo ...`) an edit that leaves a crate's
interface alone does not rebuild the crates that depend on it, rlibs built
incrementally are reproducible, and `check` shares work with `build`.
