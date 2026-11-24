# rargo

Remote cargo. `rargo build`, `check`, `test`, `run`, `clippy` and `doc` behave
like cargo's, but nothing is built on your machine: the client syncs your
worktree and environment to a build server, the server does all analysis,
scheduling and compiling for everyone, and you get back the output and the
artifacts you asked for.

```sh
cargo install rargo

cd my-project
rargo @build.example        # names the server in Cargo.toml
rargo auth                  # a page in your browser
rargo build
```

This crate is the client. It needs a rargo server of the same version to
talk to; installing one is described at <https://rargo.rs/reference>. Linux
only. With FUSE available, `target/` is served by the client's daemon and
artifacts are fetched when they are opened; without it they are copied.

Everything that is not a build is passed to the real cargo.
