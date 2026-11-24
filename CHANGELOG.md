# Changelog

What changed for someone who uses rargo, newest first. Versions follow
semantic versioning with the pre-1.0 reading: a change in the second number
may break something (the wire protocol, a file format, an option), a change
in the third does not. Client and server must be the same protocol; the
release notes say when a release changes it.

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
