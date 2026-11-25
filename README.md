# rargo

rargo is remote cargo. A build server does your Rust builds.

These commands work like the cargo commands with the same names:
`rargo build`, `check`, `test`, `run`, `clippy` and `doc`.
They do not build on your machine.
The client sends your files to the server.
The server builds them.
You get the output and the files you asked for.

Read `DESIGN.md` for how it works and why.
Read `STATUS.md` for what is tested and measured.
Read `DECISIONS.md` for the choices made.

## The server

The server runs on Linux, as root.
It mounts a FUSE file system and makes namespaces.

```sh
forks/setup.sh                      # clones cargo, applies the patches, builds the planner
cargo build --release               # builds rargod and rargo

rargod token invite --state /var/lib/rargo --user ann --admin   # prints an invitation

rargod --state /var/lib/rargo \
       --planner /path/to/cargo-fork/target/release/cargo \
       --listen 0.0.0.0:7459
```

rargod uses the rustup and `PATH` of the shell that starts it.
It installs a toolchain the first time a repository asks for it.
It downloads crates into `<state>/cargo-home`.
`--cargo-home DIR` changes that place.

Options:

- `--jobs N`: how many jobs run at once.
- `--mem-gb N`: how much memory the server uses.
- `--store DIR`: puts the content store on another file system.
- `--store-budget-gb N`: the size limit of the store.
- `--uid N --gid N`: the user that runs builds.
- `--workers-listen ADDR --worker-key FILE`: lets workers connect.

A worker runs this command:
`rargod --state DIR --worker SERVER:PORT --worker-key FILE`.
The key authenticates and encrypts the link.

Tokens say who can connect.
Use `rargod token invite|create|list|revoke`.
A change to a token takes effect at once.
A server with no tokens listens on `127.0.0.1` only.
It accepts anyone on that machine.

## The client

The client runs on Linux.
It needs git if the worktree is a git checkout.

```sh
cargo install rargo                 # or: curl -fsSL https://rargo.rs/install.sh | sh
cd my-project
rargo @build.example                # saves the server name in Cargo.toml
rargo auth                          # opens a page in your browser: enter your invitation
rargo build
```

`rargo @host` takes a name or `name:port`.
rargo asks over HTTPS where a name is.
`name:port` is the server itself.

- `rargo auth status` shows the server and the user of this project.
- `rargo auth remove` deletes the token.
- `RARGO_SERVER` and `RARGO_TOKEN` replace the project's server and the stored token.

To make other programs use rargo, put a link named `cargo` to the `rargo` binary.
Put it before the real cargo on `PATH`.
rargo passes every command that is not a build to the real cargo.
`RARGO_FALLBACK=cargo` also passes builds when the server is not reachable.

### The daemon

The first `rargo` command starts a daemon.
There is one daemon for each user, server and key on the machine.
Other commands talk to it over a Unix socket.

The daemon:

- keeps one session to the server (UDP on the server's port, or TCP if UDP gets no answer);
- watches your worktrees;
- sends each change when it happens.

The server must be reachable on its port over UDP and TCP.
`RARGO_TRANSPORT=udp` or `RARGO_TRANSPORT=tcp` picks one.

- `rargo --rargo-daemon-status` shows what the daemon does.
- `rargo --rargo-daemon-stop` stops it.
- `RARGO_NO_DAEMON=1` runs a command with no daemon.
- The log is `daemon.log` in the cache directory.

### What is sent and what comes back

rargo sends the whole worktree.
It does not send `.git`, cargo target directories, or what the project's policy excludes.

You get back what cargo leaves in `target/` for the command.
You also get the files the build writes into the worktree.

### The target directory

The daemon mounts `target/` as a FUSE file system.
This needs root or `CAP_SYS_ADMIN`.

- Built files show in `target/` at once.
- rargo fetches a file when a program reads it.
- rargo fetches the debug information of a debug binary only when a backtrace or a debugger reads it.
- What you or stock cargo write there goes to the real directory under the mount.
- When the daemon stops, `target/` is a plain directory.
- `cargo clean` removes it like any `target/`.

If the mount is not possible, or if `RARGO_OUTPUT=copy`, rargo copies results into `target/`.
Worktrees share the copies through the cache.

### The cache

The cache directory is `~/.cache/rargo`.
`RARGO_CACHE_DIR` changes it.
It holds the index, the daemon's socket and log, and the fetched contents.

## Policy in `Cargo.toml`

Policy is optional.
The nearest Cargo workspace gives the policy, even if the Git root has its own manifest.
A package that is not in a workspace gives its own policy.

Put the tables under `[workspace.metadata.rargo]`.
For a package outside a workspace, use `[package.metadata.rargo]`.
`rargo @host` writes `host` in the same place.
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

## rargo builds rargo

`Cargo.toml` of this repository names `rargo.rs` as its server.
A `rargo build` here is one build on one cache.
It can run on a laptop, for an agent, or in CI.
What one machine builds for a commit is a cache hit on every other machine.

CI installs the released client and signs in with a token.
It runs `rargo build` and `rargo test`.
The server compiles the tests that mount file systems.
The runner runs them (`dev/mount-tests.sh`).

`flake.nix` and `flake.lock` in this repository set the native build environment.
They give the Rust standard library and the C linker for `x86_64-unknown-linux-musl`.
`rargo build` and `rargo test` build on the server in that environment.
To run the native binaries or the mount tests on your machine, run `nix develop --command true`.
This makes their loader and libraries available under `/nix/store`.

## The build environment is the flake of the project

A build sees what the dev shell of the `flake.nix` of the project has.
That is the compiler, the linker, `pkg-config` and the system libraries.
The server installs nothing for a project.
The same commit builds the same way on every server.

A project with no flake uses the build shell of rargo.
It has Rust, a C and C++ compiler, `make`, `cmake`, `pkg-config`, OpenSSL, zlib and SQLite.
`rust-toolchain.toml` or `rust-toolchain` selects the Rust toolchain.

To give a workspace its own flake:

```sh
cp nix/default/flake.nix nix/default/flake.lock YOUR-PROJECT/
cd YOUR-PROJECT && git add flake.nix flake.lock
```

To add a library, put it in `packages` in `flake.nix`.
`nix develop` gives the same environment on your machine.

## nextest

`rargo nextest run` runs [nextest](https://nexte.st) on the server.
`cargo nextest run` does the same through the `cargo` link.
The client does not need nextest.

The test binaries are the same as for `rargo test`.
The output, the exit code and the JUnit files come back as for any build.

rargo caches each test alone.
If the binary and the inputs of a test do not change, rargo replays the result.
rargo never caches a failure, so a retry runs the test.

The server picks the nextest version in this order:

1. The version the repository names: `nextest = "0.9.100"` under `[workspace.metadata.rargo]`, or `nextest-version` in `.config/nextest.toml`.
2. The nextest of the client.
3. The newest version.

The server installs a version the first time it is asked.

## Other commands

- `rargo --rargo-status`
- `rargo --rargo-drift-check`: runs stock cargo and rargo on the server for this worktree and compares them.
- `rargo --rargo-incr-stats`
- `rargo test --no-cache`: runs the tests and does not replay.
- `RARGO_EXPLAIN=1`: says why each action ran.
- `RARGO_VERIFY=1`: runs every cached unit again from nothing and compares.

Administrators use these commands.
The directories are on the server.

- `--rargo-gc`
- `--rargo-compact`
- `--rargo-export DIR`
- `--rargo-import DIR`

## Development

- `dev/e2e.sh`: end-to-end test. The server cannot see the files of the client.
- `dev/e2e-flake.sh`
- `dev/nodrift.sh`
- `dev/bench-remote.sh`: one client on a shaped link.
- `dev/bench-swarm.sh`: 20 agents.
- `dev/netns.sh`: a server, workers and a client machine in network namespaces.
