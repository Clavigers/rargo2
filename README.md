# rargo

rargo is remote cargo. A build server does your Rust builds.
`rargo build`, `check`, `test`, `run`, `clippy` and `doc` work like the cargo commands.
They do not build on your machine.
The client sends your files to the server. The server builds them. You get the output and the files you asked for.

`DESIGN.md` says how it works and why. `STATUS.md` says what is tested. `DECISIONS.md` lists the choices made.

## The server

The server runs on Linux, as root. It mounts a FUSE file system and makes namespaces.

```sh
forks/setup.sh                      # clones cargo, applies the patches, builds the planner
cargo build --release               # builds rargod and rargo

rargod token invite --state /var/lib/rargo --user ann --admin   # prints an invitation

rargod --state /var/lib/rargo \
       --planner /path/to/cargo-fork/target/release/cargo \
       --listen 0.0.0.0:7459
```

rargod uses the rustup and `PATH` of the shell that starts it.
It installs the toolchain a repository asks for.
It keeps downloaded crates in `<state>/cargo-home`.

Options: `--jobs N`, `--mem-gb N`, `--cargo-home DIR`, `--store DIR` (the content store), `--store-budget-gb N`, `--uid N --gid N` (the user that runs builds).
To add workers, use `--workers-listen ADDR --worker-key FILE`.
A worker runs `rargod --state DIR --worker SERVER:PORT --worker-key FILE`.
The key encrypts the link.

Tokens say who can connect. Use `rargod token invite|create|list|revoke`.
A server with no tokens listens on `127.0.0.1` only. It accepts anyone on that machine.

## The client

The client runs on Linux. It needs git if the worktree is a git checkout.

```sh
cargo install rargo                 # or: curl -fsSL https://rargo.rs/install.sh | sh
cd my-project
rargo @build.example                # saves the server name in Cargo.toml
rargo auth                          # opens a page in your browser: enter your invitation
rargo build
```

`rargo @host` takes a name or `name:port`. rargo asks over HTTPS where a name is.
`rargo auth status` shows the server and the user. `rargo auth remove` deletes the token.
`RARGO_SERVER` and `RARGO_TOKEN` replace the project's server and the stored token.

To make other programs use rargo, put a link named `cargo` to the `rargo` binary before the real cargo on `PATH`.
rargo passes every command that is not a build to the real cargo.
`RARGO_FALLBACK=cargo` also passes builds when the server is not reachable.

### The daemon

The first `rargo` command starts a daemon. There is one for each user, server and key.
It keeps one session to the server (UDP, or TCP if UDP gets no answer; `RARGO_TRANSPORT=udp|tcp` picks one).
It watches your worktrees and sends each change when it happens.
The server port must be open for UDP and TCP.

`rargo --rargo-daemon-status` shows what it does. `rargo --rargo-daemon-stop` stops it.
`RARGO_NO_DAEMON=1` runs a command with no daemon. The log is `daemon.log` in the cache.

### Files

rargo sends the worktree. It does not send `.git`, cargo target directories, or what the policy excludes.
You get back what cargo leaves in `target/`, and the files the build writes into the worktree.

The daemon mounts `target/` as a FUSE file system (root or `CAP_SYS_ADMIN`).
Built files show there at once, and rargo fetches a file when a program reads it.
Debug information is fetched only when a backtrace or a debugger reads it.
What you or stock cargo write there goes to the real directory under the mount.
When the daemon stops, `target/` is a plain directory.

If the mount is not possible, or `RARGO_OUTPUT=copy`, rargo copies results into `target/`.

The cache is `~/.cache/rargo` (`RARGO_CACHE_DIR`). It holds the index, the daemon's socket and log, and fetched contents.

## Policy in `Cargo.toml`

Policy is optional. The nearest Cargo workspace gives it.
A package outside a workspace gives its own, under `[package.metadata.rargo]`.
Cargo ignores `metadata` tables.

```toml
[workspace.metadata.rargo]
host = "build.example"

[workspace.metadata.rargo.env]
pass = ["DATABASE_URL", "PROTOC*"]     # given to every action, part of every key

[workspace.metadata.rargo.network]
tests = false                          # host network for all test runs
test-packages = ["integration-tests"]  # host network for the tests of these packages
build-scripts = ["some-sys"]           # build scripts that can use the network

[workspace.metadata.rargo.flake]
enabled = true                         # build in the devShell of the flake (default when flake.nix exists)

[workspace.metadata.rargo.sync]
exclude = ["node_modules"]             # never sent to the server
gitignored = false                     # do not send what git ignores (default: send it)
include = ["config/local.toml"]        # send this, although git ignores it
git = "history"                        # build scripts see every commit (default: only when they ask)
```

## The build environment

A build sees the dev shell of the project's `flake.nix`: the compiler, the linker, `pkg-config` and the libraries.
The server installs nothing for a project. The same commit builds the same way on every server.

A project with no flake uses the build shell of rargo.
It has Rust, a C and C++ compiler, `make`, `cmake`, `pkg-config`, OpenSSL, zlib and SQLite.
`rust-toolchain.toml` or `rust-toolchain` selects the Rust toolchain.

To give a project its own flake:

```sh
cp nix/default/flake.nix nix/default/flake.lock YOUR-PROJECT/
cd YOUR-PROJECT && git add flake.nix flake.lock
```

To add a library, put it in `packages` in `flake.nix`. `nix develop` gives the same environment on your machine.

## nextest

`rargo nextest run` runs [nextest](https://nexte.st) on the server. The client does not need nextest.
rargo caches each test alone. It replays a test when its binary and inputs do not change. It never caches a failure.
The nextest version is the one the repository names (`nextest = "0.9.100"` under `[workspace.metadata.rargo]`, or `nextest-version` in `.config/nextest.toml`), else the one the client has, else the newest.

## Other commands

`rargo --rargo-status`, `--rargo-drift-check` (compares stock cargo with rargo), `--rargo-incr-stats`.
`rargo test --no-cache` runs the tests and does not replay.
`RARGO_EXPLAIN=1` says why each action ran. `RARGO_VERIFY=1` runs every cached unit again and compares.
Administrators use `--rargo-gc`, `--rargo-compact`, `--rargo-export DIR` and `--rargo-import DIR`. The directories are on the server.

## rargo builds rargo

`Cargo.toml` of this repository names `rargo.rs` as its server. CI runs `rargo build` and `rargo test` there.
`dev/mount-tests.sh` runs the tests that mount file systems on the runner.
`dev/e2e.sh`, `dev/e2e-flake.sh`, `dev/nodrift.sh`, `dev/bench-remote.sh`, `dev/bench-swarm.sh` and `dev/netns.sh` are the other test and benchmark scripts.
