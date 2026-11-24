# Changelog

What changed for someone who uses rargo, newest first. Versions follow
semantic versioning with the pre-1.0 reading: a change in the second number
may break something (the wire protocol, a file format, an option), a change
in the third does not. Client and server must be the same protocol; the
release notes say when a release changes it.

## 0.6.3

Protocol 7 (unchanged).

- `rargo nextest` (and `cargo nextest` through the cargo link) runs
  nextest on the server. The client needs no nextest. The test binaries
  come from rargo's build and share its cache with `rargo test`, and
  nextest's output, exit code and the files it writes (a JUnit report)
  come back. The server installs the newest nextest on first use. The
  client passes `NEXTEST_*` variables. (#30)
- After stock cargo built a test binary and rargo then put its own at the
  same path (`rargo test --no-run`, `rargo nextest`), stock `cargo test`
  ran rargo's executable and failed. rargo now removes stock cargo's
  fingerprint of that unit, so stock cargo compiles it again.
- `--rargo-status` counts the actions run by kind.

- What the server holds, and where the cache holds chunks, outlive the
  client daemon: the first command of a new daemon (after a reboot, an
  idle timeout, or `--rargo-daemon-stop`) offers only what changed. The
  server still asks for anything it lacks, so a record that is out of
  date costs a round trip, never a wrong build. (#9)
- A command right after `--rargo-daemon-stop` starts a new daemon; it
  waits for the old one to let go instead of running without one.

## 0.6.2

Protocol 7 (unchanged).

- A command after a change looks only at the directories that changed
  (and those without a watch), not at the whole worktree: 12 ms instead
  of 329 ms on a rustc checkout of 244,000 files. With more directories
  than the kernel lets one user watch, the daemon watches as many as it
  can and says once which `fs.inotify.max_user_watches` value would cover
  the rest. A directory removed and made again is watched again. (#10)
- A build step that needs a toolchain component the server lacks (a build
  script that runs `rustfmt`) gets it installed there and runs again; the
  client sees the build succeed. A component that does not exist for the
  toolchain fails the build with rustup's own message. (#69)
- Build scripts that ask git about history get the client's answers:
  `git describe --tags`, `rev-list --count HEAD` and
  `%(describe:tags)` agree with stock cargo where a `build.rs` mentions
  `describe` or `rev-list`, or `Cargo.lock` names vergen, shadow-rs,
  git-version, built or git-testament. Then the server is sent the
  commits (not their contents) and the tags. `[sync] git = "history"`
  asks for that always, and `git = "head"` never. The tags on HEAD come
  along in any case. (#11)
- `[workspace.metadata.rargo.sync] gitignored = false` leaves out
  everything git ignores (not scanned, watched or sent); `include` names
  what is sent all the same. A failed build that reads a file left out
  this way ends with a note naming the file and the option. By default
  what git ignores is synced. (#62)
- After a command for another server in a worktree (`RARGO_SERVER=...`),
  `target/` shows this server's builds: a daemon notices that another
  one replaced its mount, also when the kernel gave the new mount the
  same device number and mount id, and mounts again. (#76)

## 0.6.1

Protocol 7 (unchanged).

- A command that needs a toolchain component the server lacks (clippy for
  `rargo clippy`) gets it installed on the server on first use, also with
  `--locked`, `--frozen` or `--offline`. (#69)
- An administrator manages tokens from any machine authenticated to the
  server: `rargo auth invite|create|tokens|revoke|allow|disallow`, with
  the options of `rargod token`. Anyone may list and revoke their own
  tokens. (#24)
- A token that is not an administrator's, refused an administrator's
  command, is told how one is made (`rargod token create --admin`, `token
  invite --admin`, `token allow --admin`). (#56)

## 0.6.0

Protocol 7 (unchanged).

- `~/.config/rargo/credentials.toml` holds a token as `id`, `secret` and
  `name`.
- `rargo auth --token T` asks the server whose token it is before keeping
  it: it stores the holder's name, which `rargo auth status` shows, and
  refuses a token the server does not admit. (#68)
- A server refuses a client it does not admit with "the server does not
  know this token, or it was revoked or has expired", over UDP and TCP.
  (#68)

## 0.5.0

Protocol 7 (unchanged).

- Tokens are the one way to say who may connect to a server: `rargod
  token invite|create|list|revoke`, `rargo auth`, `RARGO_TOKEN`. A server
  that has issued no token listens on the loopback interface and admits
  anyone on its machine. (#56)
- `RARGO_TOKEN` stays on the client: it is not among the variables a
  build on the server sees.

## 0.4.1

Protocol 7 (unchanged).

- Stock `cargo clean` works in a worktree whose `target/` the daemon
  serves: when the directory's last entry is removed the daemon gives the
  directory back, and cargo removes it. Stock cargo and rargo both build
  there afterwards. (#7)

## 0.4.0

Protocol 7 (unchanged).

- A project's server and policy are in its `Cargo.toml`
  (`[workspace.metadata.rargo]`); a machine's token is in
  `~/.config/rargo/credentials.toml` or `RARGO_TOKEN`. Those are the only
  places rargo reads them from. (#55)

## 0.3.7

Protocol 7 (unchanged).

- A worktree that was synced for a request is pinned in the server's
  store for as long as the request holds it: a store at its budget
  evicts other content, never the sources of a build that is running.
  (#40)

## 0.3.6

Protocol 7 (unchanged).

- Fixed: on a server whose store is at its budget and evicting, a build
  could fail at once with "the store kept changing while the worktree was
  being synced", though nothing of that worktree was missing. A sync now
  starts over only when content of its own tree was evicted. (#40)

## 0.3.5

Protocol 7 (unchanged).

- Fixed: a client daemon that was killed (SIGKILL, a crash, the
  out-of-memory killer) left `target/` as a dead mount, and everything
  that touched it (an editor, rust-analyzer, stock cargo, `ls`) got
  "Transport endpoint is not connected" until the next rargo command.
  The directory is now unmounted the moment its daemon is gone, with no
  rargo command run, and is an ordinary directory stock cargo builds in.
  (#6)

## 0.3.4

Protocol 7 (unchanged).

- A project's policy (`env`, `network`, `flake`, `sync`) is read from its
  `Cargo.toml`, under `[workspace.metadata.rargo]` (`[package.metadata.rargo]`
  for a package outside a workspace), so a repository needs no rargo file
  of its own. (#20)

## 0.3.3

Protocol 7 (unchanged).

- `rargo auth remove` revokes the token on the server before forgetting
  it, so a copy kept anywhere stops working and its open sessions end. If
  the server cannot be reached it says so and names what an administrator
  can run. (#22)

## 0.3.2

Protocol 7 (unchanged).

- `rargo auth` signs in with GitHub where the server has a GitHub
  application: the page sends the browser to GitHub and back, and an
  account on the server's allow list gets the token. On the server:
  `rargod token allow LOGIN [--admin]`, `rargod token disallow LOGIN`
  (which also revokes the tokens the account signed in for) and
  `rargod token github` to set the application up, all without a
  restart. A server without an application keeps asking for an
  invitation. (#13)
- The server installer invites its first user instead of writing a key
  file; both installers resolve "latest" to a version first. (#2)

## 0.3.1

Protocol 7 (unchanged).

- Fixed: `rargo run` and `rargo test` could not start the program of a
  worktree that lies under `/tmp` while its `target/` was served by the
  daemon ("could not execute process ... No such file or directory").

## 0.3.0

Protocol 7 (unchanged).

- `rargo @host` names a project's server in its `Cargo.toml`
  (`[workspace.metadata.rargo]`); a bare name such as `rargo.rs` is asked
  over HTTPS where its server is. (#14)
- `rargo auth` gets a machine a token through a page in the browser, with
  an invitation from the server's administrator; `rargo auth status` and
  `rargo auth remove`. No file is edited by hand. (#13)
- Tokens on the server: `rargod token invite|create|list|revoke`, in
  effect at once without a restart, with optional expiry; a revoked
  token's open sessions are closed. A server can run with tokens only.
  (#5)
- rargod answers HTTP on its TCP port for the lookup and the auth pages.
- A build against a server that does not know the machine says to run
  `rargo auth`.
- Fixed: a rargod that refused to start (no keys) left its file system
  mounted in the state directory.

## 0.2.1

Protocol 7 (unchanged: works with a 0.2.0 server or client).

- Installers: `curl -fsSL https://rargo.rs/install.sh | sh` for the client
  and `install-server.sh` for a server with a systemd service. Releases
  carry a server archive (rargod, its sandbox helper, the planner) next to
  the static client.
- The client's crates are prepared for crates.io (`cargo install rargo`).
- A machine that has never built anything gets its first answer from a
  server that has the commit in about 0.5 s at 100 ms of round-trip time
  (was 1.5 s; Zed: 0.9 s, was 8 s): the server asks for the client's
  environment once instead of at every action, a tree the server may have
  is offered by its root, and the request no longer waits for the answer
  to that offer.
- `rargo --rargo-version` and `rargod --version`.
- The client builds for musl.

## 0.2.0

The first version run as a service. Client daemon with one session over
UDP (TCP where UDP is blocked), `target/` served by the daemon through
FUSE with a copying fallback, incremental state shared across worktrees,
workers. MIT licence.
