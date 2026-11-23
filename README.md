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

Any machine with the `rargo` binary; git if the worktree is a git checkout.

```sh
mkdir -p ~/.config/rargo
cat >~/.config/rargo/config.toml <<EOF
server = "build.example:7459"
user = "ann"
key = "the secret from the server's key file"     # or: key-file = "/path"
EOF

cd my-project && rargo build
```

`RARGO_SERVER`, `RARGO_USER`, `RARGO_KEY` and `RARGO_KEY_FILE` override the
file. To make agents that type `cargo` use rargo, put a link named `cargo`
to the `rargo` binary ahead of the real cargo on `PATH`; everything that is
not a build is passed to the real cargo, and `RARGO_FALLBACK=cargo` passes
builds too when the server cannot be reached.

What is synced: the whole worktree except `.git`, cargo target directories,
and what `.rargo.toml` excludes. What comes back: what cargo would have left
in `target/` for the command, and files the build wrote into the worktree.
The client keeps its index and a shared copy of artifacts in
`~/.cache/rargo` (`RARGO_CACHE_DIR`).

## Per-repository policy: `.rargo.toml`

Optional; cargo ignores it.

```toml
[env]
pass = ["DATABASE_URL", "PROTOC*"]     # given to every action, part of every key

[network]
tests = false                          # host networking for all test runs
test-packages = ["integration-tests"]  # ... or for these packages' tests
build-scripts = ["some-sys"]           # build scripts that may use the network

[flake]
enabled = true                         # build in the repo's flake devShell (default when flake.nix exists)

[sync]
exclude = ["node_modules"]             # never sent to the server
git = true                             # show build scripts a history-free .git for HEAD
```

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
