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

cat >/etc/rargo.keys <<EOF          # one user per line: name, secret (16+ characters), optionally `admin`
ann  $(head -c 24 /dev/urandom | base64)  admin
bob  $(head -c 24 /dev/urandom | base64)
EOF

rargod --state /var/lib/rargo \
       --planner /path/to/cargo-fork/target/release/cargo \
       --listen 0.0.0.0:7459 --keys /etc/rargo.keys
```

rargod uses the rustup and `PATH` it is started with; toolchains a repo
asks for are installed on first use, crates are downloaded into
`<state>/cargo-home` (or `--cargo-home DIR`). Useful options: `--jobs N`,
`--mem-gb N`, `--store DIR` (put the content store on another file system),
`--store-budget-gb N`, `--uid N --gid N` (the user builds run as),
`--workers-listen ADDR --worker-key FILE` (accept workers; a worker is
`rargod --state DIR --worker SERVER:PORT --worker-key FILE`; the link is
authenticated and encrypted with that key).

Without `--keys`, rargod listens on `127.0.0.1` only and accepts anyone on
the machine.

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

The older way still works: `~/.config/rargo/config.toml` with `server`,
`user` and `key` (or `key-file`) for a user of the server's key file.

`RARGO_SERVER`, `RARGO_TOKEN`, `RARGO_USER`, `RARGO_KEY` and `RARGO_KEY_FILE`
override all of it. To make agents that type `cargo` use rargo, put a link named `cargo`
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
and what `.rargo.toml` excludes. What comes back: what cargo would have left
in `target/` for the command, and files the build wrote into the worktree.

`target/` is a file system the daemon mounts (FUSE; as root or with
`CAP_SYS_ADMIN`): what the server built is listed there at once and
fetched when something opens it; what you or stock cargo write there
goes to the real directory underneath. Stopping the daemon leaves that
plain directory. Use `rargo clean` (or `cargo clean` through the shim),
not a stock `cargo clean`, while it is mounted. Where it cannot be
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
git = true                             # show build scripts a history-free .git for HEAD
```

The older place, a `.rargo.toml` at the top of the repository with the
same tables unprefixed, is still read. A table the manifest has wins over
the same table of the file, and the build says so.

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
