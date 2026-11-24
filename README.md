# rargo

Remote cargo. `rargo build`, `check`, `test`, `run`, `clippy` and `doc` behave
like cargo's, but nothing is built on your machine: the client syncs your
worktree and environment to a build server, the server does all analysis,
scheduling and compiling for everyone, and you get back output and the
artifacts you asked for.

`DESIGN.md` says why and how, `STATUS.md` what was verified and measured,
`DECISIONS.md` how each open end was closed.

## The server

Linux, root (it mounts a FUSE filesystem and creates namespaces).

```sh
forks/setup.sh                      # clones cargo at the pinned commit, applies the patches, builds the planner
cargo build --release               # rargod and rargo

rargod token invite --state /var/lib/rargo --user ann --admin   # prints an invitation for the first user

rargod --state /var/lib/rargo \
       --planner /path/to/cargo-fork/target/release/cargo \
       --listen 0.0.0.0:7459
```

rargod uses the rustup and `PATH` it is started with; toolchains a repo
asks for are installed on first use, crates are downloaded into
`<state>/cargo-home` (or `--cargo-home DIR`). Useful options: `--jobs N`,
`--mem-gb N`, `--store DIR` (put the content store on another file system),
`--store-budget-gb N`, `--uid N --gid N` (the user builds run as),
`--workers-listen ADDR --worker-key FILE` (accept workers; a worker is
`rargod --state DIR --worker SERVER:PORT --worker-key FILE`; the link is
authenticated and encrypted with that key).

Who may connect is a matter of tokens (`rargod token invite|create|list|
revoke`, in effect while the server runs). A server that has issued none
listens on `127.0.0.1` only and accepts anyone on the machine.

## The client

Linux; git if the worktree is a git checkout.

```sh
cargo install rargo                 # or: curl -fsSL https://rargo.rs/install.sh | sh
cd my-project
rargo @build.example                # names the server in Cargo.toml ([workspace.metadata.rargo])
rargo auth                          # a page in your browser; enter the invitation the server's admin gave you
rargo build
```

`rargo @host` takes a name (asked over HTTPS where its server is) or
`name:port` (the server itself). `rargo auth status` shows which server a
project builds on and as whom; `rargo auth remove` forgets the token. On
the server, `rargod token invite|create|list|revoke --state DIR` manage
who may connect, without a restart.

`RARGO_SERVER` and `RARGO_TOKEN` override the project's server and the
stored token. To make agents that type `cargo` use rargo, put a link named `cargo`
to the `rargo` binary ahead of the real cargo on `PATH`; everything that is
not a build is passed to the real cargo, and `RARGO_FALLBACK=cargo` passes
builds too when the server cannot be reached.

The first `rargo` command starts a daemon for that server (one per user,
server and key on the machine); later commands talk to it over a Unix
socket. The daemon keeps one session to the server (UDP on the server's
port, TCP if UDP gets no answer; `RARGO_TRANSPORT=udp|tcp` forces one),
watches the worktrees it has seen and sends their changes as they
happen. The server must be reachable on its port over UDP and TCP.
`rargo --rargo-daemon-status` shows what the daemon is doing,
`rargo --rargo-daemon-stop` ends it, `RARGO_NO_DAEMON=1` runs a command
without one. Its log is `daemon.log` in the cache directory.

What is synced: the whole worktree except `.git`, cargo target directories,
and what the project's policy excludes. What comes back: what cargo would have left
in `target/` for the command, and files the build wrote into the worktree.

`target/` is a file system the daemon mounts (FUSE; as root or with
`CAP_SYS_ADMIN`): what the server built is listed there at once and
fetched when something opens it; what you or stock cargo write there
goes to the real directory underneath. Stopping the daemon leaves that
plain directory. `cargo clean`, stock or through rargo, removes it as it
removes any `target/`. Where it cannot be
mounted, or with `RARGO_OUTPUT=copy`, results are copied into `target/`
and shared between worktrees through the cache.

The client keeps its index, the daemon's socket and log, and fetched
contents in `~/.cache/rargo` (`RARGO_CACHE_DIR`).

## Per-repository policy: in `Cargo.toml`

Optional. The nearest enclosing Cargo workspace supplies the policy, even when
the Git root has its own manifest. A standalone package supplies its own policy.
Place the tables under
`[workspace.metadata.rargo]` (`[package.metadata.rargo]` for a package
outside a workspace), beside the `host` that `rargo @host` writes. Cargo
ignores `metadata` tables.

```toml
[workspace.metadata.rargo]
host = "build.example"

[workspace.metadata.rargo.env]
pass = ["DATABASE_URL", "PROTOC*"]     # given to every action, part of every key

[workspace.metadata.rargo.network]
tests = false                          # host networking for all test runs
test-packages = ["integration-tests"]  # ... or for these packages' tests
build-scripts = ["some-sys"]           # build scripts that may use the network

[workspace.metadata.rargo.flake]
enabled = true                         # build in the repo's flake devShell (default when flake.nix exists)

[workspace.metadata.rargo.sync]
exclude = ["node_modules"]             # never sent to the server
gitignored = false                     # nor anything git ignores (default: synced)
include = ["config/local.toml"]        # ... but this, although git ignores it
git = "history"                        # build scripts see every commit (default: where they ask)
```

## rargo builds rargo

This repository's `Cargo.toml` names `rargo.rs` as its server, so `rargo
build` here, on a laptop, for an agent or in CI, is one build on one
cache: what was built for a commit anywhere is a cache hit everywhere
else. CI installs the released client, signs in with a token and runs
`rargo build` and `rargo test`; the tests that mount file systems are
compiled by the server and run on the runner (`dev/mount-tests.sh`).

The repository's `flake.nix` and `flake.lock` select the native build
environment and provide the Rust standard library and C linker for
`x86_64-unknown-linux-musl`. Use `rargo build` or `rargo test` to build
on the server in that environment. Run `nix develop --command true` on
a machine that runs the native binaries or mount tests, so their loader
and libraries are available under `/nix/store`.

## The build environment is the project's flake

A build sees what the dev shell of the project's `flake.nix` has: the
compiler, the linker, `pkg-config`, the system libraries. Nothing is
installed on a server for a project, and the same commit builds the same
way on every server.

A project without a flake starts from the default one, the build
essentials (Rust, a C and C++ compiler, `make`, `cmake`, `pkg-config`,
openssl, zlib, sqlite):

```sh
cp install/flake/flake.nix install/flake/flake.lock YOUR-PROJECT/
cd YOUR-PROJECT && git add flake.nix flake.lock
```

A project that needs another library adds it to `packages` in its
`flake.nix`. `nix develop` gives the same environment on your machine.

## nextest

`rargo nextest run` (or `cargo nextest run` through the cargo link) runs
[nextest](https://nexte.st) on the server; the client needs no nextest.
The test binaries are rargo's build, shared with `rargo test`, and
nextest's output, exit code and JUnit files come back as from any build.
Every test is cached on its own: a test whose binary and inputs did not
change is replayed, and a failure is never cached, so retries run.
The version is the one the repository names (`nextest = "0.9.100"` under
`[workspace.metadata.rargo]`, or `nextest-version` in
`.config/nextest.toml`), else the client's own nextest's, else the newest;
the server installs each version the first time it is asked.

## Other commands

`rargo --rargo-status`, `--rargo-drift-check` (stock cargo against rargo on
the server, for this worktree), `--rargo-incr-stats`; for administrators
`--rargo-gc`, `--rargo-compact`, `--rargo-export DIR`, `--rargo-import DIR`
(directories on the server). `rargo test --no-cache` really runs the tests;
`RARGO_EXPLAIN=1` says why each action ran; `RARGO_VERIFY=1` re-runs every
cached unit from nothing and compares.

## Development

`dev/e2e.sh` (end to end, the server unable to see the client's files),
`dev/e2e-flake.sh`, `dev/nodrift.sh`, `dev/bench-remote.sh` (one client over
a shaped link), `dev/bench-swarm.sh` (20 agents), `dev/netns.sh` (server,
workers and a client machine in network namespaces).
