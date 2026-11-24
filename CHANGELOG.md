# Changelog

What changed for someone who uses rargo, newest first. Versions follow
semantic versioning with the pre-1.0 reading: a change in the second number
may break something (the wire protocol, a file format, an option), a change
in the third does not. Client and server must be the same protocol; the
release notes say when a release changes it.

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
