# Status

What exists, how it was checked, what was measured, and what is weak or
missing. Machine: 32 threads, 124 GB RAM, kernel 7.0, ext4. All builds are
capped at 24 cores (`CPUQuota=2400%`, 24 scheduler tokens) and 90 GB.

## Summary (2026-11-23)

rargo is remote cargo. On a client machine one daemon per server keeps a
long-lived session to the build server, watches the worktrees it has
seen and pushes their changes as they happen; the `rargo` command hands
its arguments and environment to the daemon and relays the output.
Results are not copied into the worktree: `target/` is a file system the
daemon serves, in which artifacts appear as the server makes them and
are fetched when something opens them. Analysis, scheduling and building
happen on the server (DESIGN.md, DECISIONS D34 to D59).

What was measured when:

- At `a92e359` (protocol 6: one TCP connection per command, results
  copied into `target/`), on the rig's LAN link (1 ms each way, 1 Gbit):
  the four 20-agent swarms against stock cargo, the single-client
  tables, tracing overhead, phase 5 and the no-drift runs. Those tables
  are kept below and say so.
- In this round (D56 to D59, protocol 7), at the commits named with each
  table: incremental state on workers and the worker series; the client
  daemon; per-command latency before and after; everything that crosses
  the client link again **with 100 ms of round-trip time** (netem,
  50 ms each way, 1 Gbit), beside LAN figures: one client, the swarms,
  the history replay, the latency of one command, the fresh worktree,
  the branch switch; no-drift with the virtual output directory.

Clients are in their own network namespace, one shaped link from a
server that cannot see their files. Every timing of this round in the
tables below was made, or made again, at the final build (`fd599de`),
after the faults the first passes found were fixed (D60, D61); where an
earlier figure is quoted beside it, the text says so.

- **One command, 100 ms round trip** (rust-analyzer, median of 7):
  nothing to do 0.16 s, was 0.36 s; after a one-line edit 1.27 s, was
  2.41 s. On a LAN 0.06 s and 1.18 s, were 0.07 s and 1.22 s. A build
  with nothing to do is one message each way; an edit is on the server
  before the build is asked for. See "One command".
- **The swarm at 100 ms round trip** (20 agents, no failed steps):
  rust-analyzer 114.9 s, Ruff 147.3 s, Bevy 87.2 s, Zed 603.3 s; on
  the LAN link rust-analyzer takes 112.7 s. With the virtual output
  directory 2.3 MiB cross the link towards the client in the
  rust-analyzer swarm and 0.01 GB end up on its disk; with results
  copied it was 1,959 MiB and 11.8 GB (and 133.3 s). The client daemon
  uses 3 to 12 s of CPU over a swarm.
- **The swarm with nextest at 100 ms round trip** (rust-analyzer, each
  test step a `nextest run`): 124.1 s against stock `cargo nextest`'s
  418.2 s; 1,974 s of server CPU against 9,816 s; 8.8 MiB over the link.
  See "The nextest variant".
- **A branch switch** (rust-analyzer; check, build and test of a branch
  another agent has been working on): in a fresh worktree 0.17, 0.14 and
  0.12 s, in a worktree just switched to the branch 0.14, 0.14 and
  0.11 s, against 0.13, 0.14 and 0.11 s for the agent already there
  (100 ms round trip). Stock cargo: 22.5, 37.8 and 3.0 s fresh; 6.1, 9.5
  and 1.1 s switched. See "A branch switch".
- **20 agents in 20 worktrees** against stock cargo with one target
  directory per worktree, no failed steps on either side:

  | | wall time | CPU time | disk on the client machine |
  |---|---|---|---|
  | rust-analyzer | 125.9 s against 380.8 s (3.0x) | 1,688 s against 8,918 s (5.3x) | 11.8 GB against 100.1 GB |
  | Ruff | 149.2 s against 797.5 s (5.3x) | 2,701 s against 19,052 s (7.1x) | 2.7 GB against 94.9 GB |
  | Bevy | 88.7 s against 385.6 s (4.3x) | 1,113 s against 8,896 s (8.0x) | 2.1 GB against 61.0 GB |
  | Zed | 623.6 s against 4,244 s (6.8x) | 12,084 s against 100,641 s (8.3x) | 35.7 GB against 813.9 GB |

  rargo's CPU time is the server's plus the clients'. The server's store
  held 42.6, 33.0, 25.1 and 275.6 GB. Zed's stock figure is twenty agents
  run as two groups of ten, because twenty at once do not fit in the
  memory cap (see the Zed table).
- **One client**: nothing changed, 0.05 s (stock cargo 0.15 s) for
  rust-analyzer and 0.28 s (1.45 s) for Zed; a fresh worktree on another
  machine at a commit the server has built gets a ready binary in 1.0 s
  (rust-analyzer) and 3.0 s (Zed, a 1.7 GB binary). A cold build is the
  same compiler on the same cores plus a link: rust-analyzer 23.6 s
  against 22.2 s for the first check, Zed 111.9 s against 99.0 s.
- **No drift**: after rargo built and tested them through the server,
  all four repositories build and test with stock cargo in the same
  worktree and `target/`, also when stock cargo starts from a fresh
  worktree and has everything to build; tracked files are untouched. The
  server-side check (stock cargo against rargo re-running every cached
  unit) finds no drift in any of them; for Zed, 25 of 1,717 re-run units
  are builds the tools themselves do not reproduce and are reported as
  such (see No-drift).
- **Workers help the swarm now** (D56), and the fault in the worker
  path is found and fixed (D61): with 8 cores per machine the 20-agent
  rust-analyzer swarm takes 150.6 s with a worker (140.4 s in a second
  run) against 176.0 s alone and 116.5 s with 16 local cores, no failed
  step and no action handed back to the server. Before, a worker made
  no difference; in between, 1 and 5 agent steps failed in two runs
  because the server refused a worker contents it had named to it. See
  Workers.
- **Gaps closed since the last report**: the worker link is encrypted
  (D44); a client that goes away takes its actions with it, on the server
  and on workers (D48); `--timings` and cargo's future-incompatibility
  report file (D49); temporaries of interrupted clients (D50); Zed builds,
  through its flake (D45), and its drift check (D51).

Tests at this commit (the one after `5f8bc7a`): `cargo test` 15
(client) + 9 (protocol) + 4 (transport) + 56 unit and 6 engine (server)
+ 1 (plan) at `6862973`, not rerun; `dev/e2e.sh` 113 of 113 (virtual
output directory) and 113 of 113 with `RARGO_OUTPUT=copy`;
`dev/e2e-flake.sh` 14 of 14 (at `5f8bc7a`); `dev/e2e-link.sh` 67 of 67;
`dev/e2e-worker.sh` 28 of 28. The link and worker suites run in a
second rig (`RIG=rargo2`) beside the history benchmark. The copy-mode
run first failed one check, which read its counters after making the
edit it counts and lost a race with the daemon pushing that edit; the
check now reads them before.

Not met, or weak:

- **Phase 4 is met in part.** At the final build a warm worker gets
  about half of what the same cores are worth locally: a cold
  rust-analyzer build goes from 54.4 s to 48.6 s with 4 cores per
  machine (8 local cores: 43.7 s) and from 43.6 s to 41.9 s with 8
  (16 local cores: 40.3 s); the 20-agent swarm from 176.0 s to 150.6 s
  (16 local cores: 116.5 s). Not "roughly in proportion". A worker that
  has seen nothing costs 1.1 s on the 8-core cold build. A build where
  nothing changed is no slower with a worker; a rebuild after an edit
  is 0.6 s slower at 4 cores. See Workers for where the rest goes and
  what was not tried.
- **The virtual output directory without root** goes through
  `fusermount3` and was exercised by one test as `nobody` (build, run,
  write, unmount); every benchmark ran as root. Without privilege the
  kernel does not read and write real files directly (no passthrough),
  so stock cargo in such a mount will be slower than measured here. The
  directory also disappears with its daemon (see "Results on the
  client").
- **Stock cargo in a mounted `target/` on Zed** takes 170.9 s for a cold
  build against 168.8 s in a plain directory, and 30 s against 29 s for
  the tests; it was 446 s and 187 s. See No-drift for what was wrong.
- **A fresh worktree that runs its binary gets it no sooner than
  before**: the binary has to cross the link either way (3.1 s at 100 ms
  round trip, both modes). What the virtual directory saves is everything
  that is built and never opened.
- The rustc fork (phase 5) still buys nothing on real history; see Phase 5.
- The client was only run on Linux x86_64.
- Two mistakes of mine on the shared machine during this work are listed
  under Incidents.

| Area | State |
|---|---|
| Client: worktree scan with index, content-addressed tree | done, unit-tested, e2e |
| Client daemon: one per machine and server, started on demand, Unix socket, in-process fallback | done, e2e-link |
| Sync: changes pushed as they happen (inotify), one message per edit; the server asks for what it still lacks | done, unit-tested (server), e2e (counts of files and manifests received and asked for), e2e-link |
| Stand-in for `.git` (HEAD, branch, dirty or not; no history, no contents) | done, unit-tested against real git, e2e |
| Environment: allow-list, ask-on-declare, client cargo configuration | done, e2e (declared variable, policy file, a `config.toml` in a cargo home the server cannot see); `credentials.toml` travels the same way but no private registry was tried |
| Transport: one session per daemon on the reliable-UDP layer (paced, follows the client's address, keepalives), TCP where UDP is blocked; Noise `NNpsk0`, per-user keys, admin users | done, unit-tested, e2e (wrong key, unknown user, non-admin), e2e-link (address change, UDP dropped) |
| Results: `target/` served by the daemon (FUSE), listed at once, fetched by chunk on open; copying as fallback; client cache shared between worktrees; worktree writes; interrupted clients | done, unit-tested (copy mode), e2e in both modes, e2e-link; as root only |
| Cancellation when a client leaves | done, e2e |
| Fetching and toolchain installation on the server | done; fetch exercised by e2e-flake and by Zed, toolchain installation checked once by hand (see No-drift) |
| Drift check on the server; no-drift on the client | done; see No-drift |
| FUSE server with passthrough (`fusewire`) | done, tested |
| Content store, GC, LRU budget, cold tier | done, tested |
| Traced sandbox filesystem with copy-on-write upper layer | done, tested |
| Process sandbox (zygote, namespaces, init), per-action cgroups | done, tested |
| Cargo fork: planning mode, parse service | done, exercised on rust-analyzer, Ruff, Bevy, Zed |
| Trace cache, in-flight dedup, declared env vars | done, tested |
| Scheduler: priorities, memory admission, shared jobserver, tokens of killed compilers | done, unit-tested |
| `rargo build / check / test / run / clippy / doc`, JSON messages, `--timings`, future-incompatibility report | done, e2e |
| Shared incremental compilation | done, e2e, in the 20-agent benchmark |
| Per-repo policy file (`.rargo.toml`) | done, e2e (env, test network, sync excludes, git switch, malformed file); `flake.files` by a unit test and by Zed |
| Flake-restricted sandboxes, cache export/import between servers | done, e2e-flake, Zed |
| Workers: encrypted link, dispatch, hedging, cancellation, incremental state left on workers | done, e2e-worker; benchmark series below; phase 4 criterion met in part |
| `cargo` shim | done, e2e (routing, passthrough, fallback without a server) |
| rustc fork (phase 5), `check` as a build | done, re-measured; not worth using |

## Installing and releasing (issues 1, 2, 4; `FACTORY.md` has the process)

What exists and how each part was checked (2026-11-24):

| | how it was checked |
|---|---|
| `cargo install rargo` (crates.io: `rargo`, `rargo-net`, `rargo-proto`, `fusewire`; 0.2.1 and 0.3.0 published by the release workflow) | in a clean `rust:1-slim` container: install, `rargo @rargo.rs`, `rargo auth`, `rargo build`, run, test: 36 s in all (`dev/new-user.sh`) |
| `curl -fsSL https://rargo.rs/install.sh \| sh` (static client for Linux x86_64, checksum verified) | in a clean `ubuntu:24.04` container |
| `curl -fsSL https://rargo.rs/install-server.sh \| sudo sh` (rargod, sandbox helper, planner; rustup; an invitation for the first user; a systemd unit) | in a clean privileged `ubuntu:24.04` container with `RARGO_NO_SERVICE=1`: installed, started by the printed command, and the printed path (`rargo @host`, `rargo auth` with the printed invitation, `rargo build`) run against it. **Not run:** the branch that writes and starts the systemd unit (the container has no systemd; the unit is the shape of the one that runs on this machine) |
| GitHub releases with the client, the server archive, both installers, checksums, the changelog section as notes | v0.2.1 and v0.3.0 made by `.github/workflows/release.yml` from a tag |
| the rargod service on this machine, `build.rargo.rs:7459` | from a GitHub runner over UDP and TCP (`service check` workflow), and by the container runs above through `rargo.rs` |
| CI on every push and pull request | build with warnings as errors, unit and engine tests, static client, packaged crates, and the new user's path against a server of the commit (`dev/e2e-auth.sh`, 36 checks) |
| the end-to-end suites that need the planner and network rigs | on this machine before a release (`dev/gate.sh`): 113 + 113 + 14 + 36 + 67 + 28 checks |

Found by trying it as a stranger and fixed: the site served the server
archive with a gzip content encoding, so it arrived unpacked and failed
its checksum (downloads are now served as bytes, and the installers turn
"latest" into a version number first so every file has an address that
never changes what it holds); `rargo run` could not start a program in a
worktree under `/tmp` whose `target/` was mounted (0.3.1).

**crates.io: what the owner has to do once, to stop needing a token.**
Releases publish with the repository secret `CARGO_REGISTRY_TOKEN` when
it is set, and with crates.io trusted publishing when it is not. All
four crates exist now, so trusted publishing can be set up:

1. On crates.io, signed in as the owner of the crates, for each of
   `rargo`, `rargo-net`, `rargo-proto` and `fusewire`: the crate's page,
   *Settings*, *Trusted Publishing*, *Add*.
2. Publisher: GitHub. Repository owner: `sadosystems`. Repository name:
   `rargo2`. Workflow filename: `release.yml`. Environment: leave empty.
3. When all four are added: delete the secret `CARGO_REGISTRY_TOKEN` in
   the repository's settings (Secrets and variables, Actions) and revoke
   the token on crates.io (Account Settings, API Tokens). The next
   release then publishes without it.

Until step 3 nothing is blocked: the token publishes.

**Not possible on this plan:** a rule on `main` that requires a pull
request and passing checks (GitHub offers branch rules for private
repositories only on a paid plan; the API answers "Upgrade to GitHub Pro
or make this repository public"). The rule is kept by `CLAUDE.md` and by
`dev/pr-land.sh`, which merges a pull request only after its checks
passed.

## Measurements

### One command (`dev/bench-latency.sh`)

rust-analyzer, server with 24 cores, one client. After a full build:
the same build again; a build right after appending a comment line to
`crates/stdx/src/lib.rs` (one crate recompiles, nothing above it); and
the same with a second between edit and build. Median of 7 each.
"Before" is commit `a0aa3f4` (one TCP connection per command, the
server walks the tree to find what changed, results copied); "after" is
the final build (`fd599de`: client daemon on its UDP session, virtual
output directory).

| | LAN (2 ms round trip) | 40 ms round trip | 100 ms round trip |
|---|---|---|---|
| nothing to do, before | 0.069 s | 0.184 s | 0.364 s |
| nothing to do, after | 0.059 s | 0.097 s | 0.159 s |
| build right after an edit, before | 1.221 s | 1.671 s | 2.406 s |
| build right after an edit, after | 1.175 s | 1.201 s | 1.270 s |
| build a second after an edit, before | 1.216 s | 1.668 s | 2.387 s |
| build a second after an edit, after | 1.172 s | 1.223 s | 1.275 s |
| first build of the worktree (sync, 42 s of compiling, results back), before | 41.9 s | 45.0 s | 49.1 s |
| first build of the worktree (the binary is listed, not sent), after | 40.1 s | 41.0 s | 41.7 s |

Before, a command with nothing to do cost about three round trips
(connection, request, the delivery's question and answer); now it costs
one, plus 60 ms of work at both ends. After an edit the old client paid
for the walk down the tree as well (about a dozen round trips in all at
100 ms); now the change travels in one message, ahead of the request or
before it. Whether the daemon had a second to push the edit first makes
no difference worth measuring (the push rides ahead of the request on
the same channel). The first build is compiling; what it gains at
100 ms is the binary that no longer comes back with it. (With results
copied, the daemon alone gave 0.060, 0.097 and 0.157 s for nothing to
do and 1.18, 1.22 and 1.29 s after an edit, measured at `32f6c5e`.)

What these figures do not include: the first command after a daemon
starts pays for its start and the key agreement (one round trip more);
over the TCP fallback the first command also waits 2.5 s for UDP to
fail (e2e-link: 2.6 s for the first command with UDP dropped, then
normal).

### The first command of a new client daemon (`dev/bench-restart.sh`, 2026-11-24)

rust-analyzer on the rig. Before each command the daemon was stopped (and
one second passed). Median of 7. "before" is 0.6.2 (0a472bc), "after" is
main at the #9 merge (PR #85), where what the server holds outlives the
daemon (D80).

| Link | No edit (before → after) | After a one-line edit (before → after) |
|---|---|---|
| LAN (2 ms RTT) | 0.100 → 0.094 s | 1.227 → 1.222 s |
| 100 ms RTT | 0.291 → 0.297 s | 1.763 → 1.432 s |

### Scanning after a change (`scan::rescan`, 2026-11-24)

One change, timed in-process (median of 5), against a whole scan with a
warm stat cache (D79, issue 10). Each result was checked to equal a
whole scan:

| Worktree | Files | Directories | Whole scan | Rescan | Rescan, scan shared |
|---|---|---|---|---|---|
| rustc | 243,970 | 21,412 | 329 ms | 12 ms | 99 ms |
| Ruff | 11,338 | 908 | 11.2 ms | 0.86 ms | 2.2 ms |
| Zed | 3,698 | 830 | 4.5 ms | 0.91 ms | 1.5 ms |

### A fresh worktree to a usable binary (`dev/bench-fresh.sh`)

100 ms round trip. The server has built rust-analyzer for one worktree.
A fresh worktree at the same commit runs `rargo build -p rust-analyzer`
and then `target/debug/rust-analyzer --version` (a 456 MiB binary, about
98 MiB on the wire). "Copy" is results copied into `target/`
(`RARGO_OUTPUT=copy`, what there was before); "virtual" is the output
directory served by the daemon. Bytes are what the client's interface
received. One run each, at the final build (`fd599de`).

| | build | run | bytes down |
|---|---|---|---|
| same machine as the first worktree, copy | 0.17 s | 0.00 s | 8 KB |
| same machine, virtual | 0.15 s | 0.02 s | 8 KB |
| a machine with an empty cache, copy | 3.07 s | 0.01 s | 97 MiB |
| a machine with an empty cache, virtual | 1.43 s | 1.71 s | 93 MiB |
| (the first worktree, which waits for the build: copy) | 41.7 s | 0.00 s | 98 MiB |
| (the first worktree: virtual) | 40.6 s | 1.80 s | 93 MiB |

Since then the build on a machine with an empty cache has gone from
1.43 s to 0.52 to 0.56 s (two fresh machines, measured with a 6-core
server beside the history benchmark, `bench/fresh-phases.sh`), in two
steps (D66):

| a machine with an empty cache, everything cached on the server | whole command | of it: the offer | of it: request to answer |
|---|---|---|---|
| before | 1.46 to 1.64 s | 0.24 to 0.32 s | 1.05 s |
| the server asks for environment variables once, not nine times in a row | 0.66 to 0.69 s | 0.24 s | 0.26 s |
| a tree the server may have is offered by its root first | 0.52 to 0.56 s | 0.10 s | 0.25 s |

Then the request stopped waiting for the offer's answer: the daemon
sends the root and the request together, and the server, which handles
them in order, answers the request if it has the tree. The offer phase
went from 0.10 s to nothing (request sent 0.004 s after the worktree
is read). If the server lacks the tree the request is withdrawn and the
tree offered whole, which costs what the probe cost before (measured on
the first worktree: 0.10 s, then the offer as it was). The whole
command was 0.66 to 0.71 s in that run because reading the worktree
took 0.28 to 0.33 s instead of 0.05 s beside the history benchmark; by
phases it is 0.10 s (link) + reading + 0.26 s. `RARGO_OFFER_WAIT=1` on
a command restores the wait.

What is left is two round trips one after the other (key agreement,
request) and one beside the planning (the environment question, asked
of a machine once in its life), plus about 0.1 s of work on the two
sides. The table above was not run again.

A worktree that runs its binary has it after 3.1 s either way: the
binary has to cross the link, at the end of the build or when it is
first run. The virtual directory moves that cost from the build to the
first use, and removes it for what is never used (see the swarm: agents
build the binary and never run it). On the same machine a fresh
worktree costs 0.15 s and nothing crosses the link in either mode,
because the client's cache is shared between worktrees.

### A branch switch (`dev/bench-branch.sh`)

rust-analyzer. One agent works on a branch B: three edits (`parser`,
`syntax`, `ide-db`, a new function each), committed one by one, with
check, build and test after each. Then check, `build -p rust-analyzer`
and `test -p stdx -p parser -p syntax` are timed (a) in that agent's
worktree, still on B; (b) in a fresh worktree checked out at B; (c) in a
worktree that had been built at the base commit and was just switched to
B with `git checkout`. Stock cargo: 24 cores, one target directory per
worktree. rargo: server with 24 cores, the final build (`fd599de`).
One run each; stock cargo's row is from the earlier run.

| check / build / test | (a) same agent, still on B | (b) fresh worktree at B | (c) worktree switched to B |
|---|---|---|---|
| stock cargo | 0.15 / 0.14 / 0.37 s | 22.5 / 37.8 / 3.0 s | 6.1 / 9.5 / 1.1 s |
| rargo, 2 ms round trip | 0.04 / 0.04 / 0.02 s | 0.08 / 0.04 / 0.02 s | 0.04 / 0.04 / 0.02 s |
| rargo, 100 ms round trip | 0.13 / 0.14 / 0.11 s | 0.17 / 0.14 / 0.12 s | 0.14 / 0.14 / 0.11 s |

Under rargo an agent that has just arrived on a branch is as fast as
the one that was working there: everything B needs was built once, for
the first agent, and the other two worktrees only have to say what they
contain. The fresh worktree's first command is 0.04 s slower (its
manifests are new to the daemon: one push). The switched worktree's
checkout is seen by the watcher and pushed before the check is typed.
Stock cargo builds B from nothing in a fresh worktree and rebuilds the
three edited crates and everything above them in the switched one.

What the rargo figures rest on: the tests are not run again (their
recorded results are replayed, because nothing they read differs), and
the binary is listed in `target/`, not fetched; a worktree that then
runs it on a machine whose cache lacks it pays for the transfer (see the
fresh worktree above; on the same machine it is already in the cache).
For comparison, the base commit's first check, build and test took
24.1 / 39.5 / 3.2 s under rargo (LAN) and 22.6 / 37.5 / 3.0 s with
stock cargo.

### History (`dev/history.py`)

The first-parent history of rust-analyzer over two years (2,672
commits, 2024-11-24 to 2026-11-22), `build -p rust-analyzer` at each
commit, four ways (D62; an earlier version of this section had my own
reading of (a) to (d) over 30 commits, which was not what the brief
defines):

- (a) stock cargo, one worktree with a persistent target directory,
  every commit;
- (b) stock cargo, an empty target directory at every commit;
- (c) stock cargo, one worktree with a persistent target directory,
  every 10th commit only;
- (d) rargo, 100 ms of round-trip time on the client link, two agents
  interleaved in commit order: one builds every commit that is not a
  10th, the other only the 10th ones.

Every build has 4 cores (stock cargo's `-j4` under a 4-core cap, a
rargo server with 4 cores), six builds side by side, because (b) is
2,672 cold builds and everything shares a cap of 24 cores. These
figures compare with each other, not with the 24-core tables. Each
commit is built with the toolchain of its time (1.82 to 1.98, nine of
them): older commits do not compile with today's.

**What was measured.** The run was stopped by the owner on 2026-11-24
before it reached the end of the history; lane (c) starts when (a)
ends and therefore never ran. The figures are for the commits each
lane built, with builds that were in progress during a pause of the
benchmark left out (8 in (a), 30 in (b), 7 in (d)).

| lane | commits built | median | p90 | max | total |
|---|---|---|---|---|---|
| (a) stock cargo, persistent target, every commit | 1,386 of 2,672 | 14.6 s | 38.8 s | 62.7 s | 23,998 s |
| (b) stock cargo, cold at every commit | 1,863 | 58.2 s | 63.0 s | 73.4 s | 108,713 s |
| (c) stock cargo, persistent target, every 10th commit | none | | | | |
| (d) rargo at 100 ms, the agent building every commit but the 10th | 821 | 23.9 s | 45.2 s | 78.1 s | 23,250 s |
| (d) rargo at 100 ms, the agent building only the 10th commits | 91 | 22.6 s | 38.6 s | 57.2 s | 2,395 s |

**The key comparison, (d)'s 10th-commit builds against (c), cannot be
made: there is no (c).** What the data does give for those builds, on
the 87 10th commits that (d), (a) and (b) all built:

| the 10th commits (87) | median | p90 | max | total |
|---|---|---|---|---|
| rargo, the agent that builds only these, arriving from ten commits back | 22.2 s | 37.7 s | 57.2 s | 2,269 s |
| stock cargo arriving from one commit back (lane (a) at the same commits) | 15.2 s | 35.2 s | 51.3 s | 1,495 s |
| stock cargo cold (lane (b) at the same commits) | 57.2 s | 62.3 s | 65.6 s | 5,005 s |

Lane (c), stock cargo arriving from ten commits back with its own
target directory, lies somewhere between the last two rows; where is
not known. What can be said without it:

- **Under rargo a hole costs nothing extra.** The agent that builds
  only every 10th commit takes 22.2 s at the median; the agent that
  builds every other commit takes 23.8 s on its 788 commits common to
  all lanes. The 10th-commit agent starts from the other agent's state
  one commit back (the choice of incremental state nearest to HEAD,
  D62), which is the design's point.
- **A single agent walking forward is slower under rargo than under
  stock cargo**: 23.8 s against 16.3 s at the median on those 788
  commits (p90 44.9 s against 39.4 s, in all 22,208 s against 14,978 s,
  48% more), on four cores with 100 ms of round-trip time for rargo and
  none for stock cargo. The breakdown below says where it goes.
- Against cold builds rargo's steps take 41% of the time (23.8 s
  against 57.9 s at the median).

Failed builds, and why: in (d), 2 of 915 failed with "the store kept
changing while the worktree was being synced" (a fault of rargo's,
issue 40, fixed in 0.3.6 and 0.3.7; the benchmark ran the earlier
binaries throughout), and the others failed because the benchmark was
paused or stopped under them. In (a) and (b), 5 commits each failed on
a nested directory an earlier checkout had left in the lane's worktree
(the benchmark's checkout step, corrected in `dev/history.py`), and 141
later commits of (b) failed because the map from commit to toolchain
names a compiler too old for them. That map was found wrong once
during the run, for commits 782 to 810 and 969 to 1278 (339 stock
builds failed there), corrected, and those builds were made again;
beyond commit 1,400 it was not checked, which is where the 141 are.

**Why one agent alone is slower under rargo than under stock cargo.**
Measured per action on single steps of this history (24 cores, LAN;
D62). It is not hashing or storing the binary or the incremental
states:

| one commit that changes one file of `hir-ty` | stock cargo | rargo |
|---|---|---|
| `hir-ty` to `ide` (ten units) | the same to 0.1 s each | the same |
| `rust-analyzer` (lib) | 1.6 s | 6.3 s (10.6 CPU-s) |
| `rust-analyzer` (bin), the link | 0.9 s | 1.1 s |
| before the first unit starts / after the last ends | 0.1 s / 0 | 0.3 s / 0.3 s |
| whole step | 7.1 s | 12.5 s |
| the same with `[sync] git = false` | | 7.9 s |

- 4.6 of the 5.4 s are the `rust-analyzer` library being compiled
  again because its build script ran again and passed it a new commit
  hash. The script asks cargo to rerun it when `.git/HEAD` changes; in
  a linked worktree there is no such file (`.git` is a file), so it
  asks for nothing and stock cargo never reruns it: on a commit that
  changes no Rust file stock cargo takes 0.14 s, and its binary
  reports the commit it was first built at. rargo saw the script read
  HEAD through git, HEAD changed, the script runs (0.02 s), and what
  embeds its answer is rebuilt: 7.7 s on such a commit. rargo's binary
  says the right commit; that is what the time buys. A repository that
  does not want it sets `[sync] git = false` in `.rargo.toml`.
- 0.8 s are rargo's own: 0.2 s before the first action (request,
  plan), 0.2 s on the link step, 0.3 s after it (the 456 MiB binary is
  hashed and stored and the delivery listed), the round trip. Not
  reduced.
- Found on the way and changed: a linked worktree is shown to the build
  as one (`.git` a file, the stand-in beside it), as on the client; and
  a compile starts from the incremental state made nearest to HEAD,
  which for an agent that has just arrived at a commit is another
  agent's state from one commit back, not its own from ten back. The
  second matters for (d)'s 10th-commit agent and was found by watching
  the first builds of this run.

### One remote client (`dev/bench-remote.sh`)

Server with 24 cores and a fresh store per column, client in its own
network namespace. "Stock cargo" is cargo on the client's files with the
same 24 cores, no network involved (for Zed: inside its flake's dev
shell, as its users run it).

| rust-analyzer (commit 37721d1) | stock cargo, local | rargo, 1 ms each way, 1 Gbit | rargo, 20 ms each way, 100 Mbit |
|---|---|---|---|
| first `check` (rargo: includes the first sync, 21.5 MiB) | 22.2 s | 23.6 s | 24.4 s |
| `check`, nothing changed | 0.15 s | 0.05 s | 0.12 s |
| `check` after a one-file edit | 5.9 s | 6.2 s | 6.4 s |
| `build -p rust-analyzer` after that (rargo: binary comes over the link) | 38.3 s | 40.4 s | 47.1 s |
| the same build, nothing changed | 0.15 s | 0.05 s | 0.17 s |
| the same build from a fresh worktree on a second machine | a full build | 1.0 s | 8.7 s |
| in the worktree's `target/` afterwards | 4,607 MiB | 565 MiB | 565 MiB |

| Zed (v0.233.10) | stock cargo, local | rargo, 1 ms each way, 1 Gbit | rargo, 20 ms each way, 100 Mbit |
|---|---|---|---|
| first `check` (rargo: includes the first sync, 65.3 MiB) | 99.0 s | 111.9 s | 113.0 s |
| `check`, nothing changed | 1.45 s | 0.28 s | 0.36 s |
| `check` after a one-file edit | 17.4 s | 19.3 s | 19.5 s |
| `build -p zed` after that (rargo: a 1.7 GB binary comes over the link) | 128.3 s | 144.6 s | 167.8 s |
| the same build, nothing changed | 5.3 s | 0.30 s | 0.41 s |
| the same build from a fresh worktree on a second machine | a full build | 3.0 s | 28.2 s |
| in the worktree's `target/` afterwards | 37,525 MiB | 1,703 MiB | 1,703 MiB |

The stock cargo columns were measured earlier the same day (nothing about
stock cargo changed); the rargo columns at `a92e359`. One run per figure;
across the runs of the rust-analyzer table during the work the cold
figures moved by up to 0.7 s. The first sync asked the client for 263
manifests and 2,333 files (rust-analyzer) and 827 manifests and 3,570
files (Zed).

Zed's environment is its flake's dev shell, evaluated on the server
(D45). The repository's policy file names the files the flake reads
(`flake.files`), lets one build script use the network (`webrtc-sys`
downloads a library), and passes `HTTPS_PROXY`, because the benchmark
server sits in a network namespace without a route out and reaches the
network through a small proxy (`dev/rigproxy.py`).

The same tables with the client daemon and the virtual output directory
at the final build (`fd599de`), on LAN and with 100 ms of round-trip
time:

| rust-analyzer | rargo, 2 ms round trip | rargo, 100 ms round trip |
|---|---|---|
| first `check` (includes the first sync, 21.5 MiB) | 24.0 s | 25.0 s |
| `check`, nothing changed | 0.04 s | 0.14 s |
| `check` after a one-file edit | 6.1 s | 6.4 s |
| `build -p rust-analyzer` after that (the binary is listed, not sent) | 38.6 s | 40.0 s |
| the same build, nothing changed | 0.04 s | 0.14 s |
| the same build from a fresh worktree on a second machine (binary listed, not fetched) | 0.12 s | 1.43 s |
| shown in the worktree's `target/` afterwards | 565 MiB | 565 MiB |

| Zed | rargo, 100 ms round trip |
|---|---|
| first `check` (includes the first sync, 65.3 MiB) | 113.1 s |
| `check`, nothing changed | 0.37 s |
| `check` after a one-file edit | 19.2 s |
| `build -p zed` after that (the 1.7 GB binary is listed, not sent) | 144.8 s |
| the same build, nothing changed | 0.38 s |
| the same build from a fresh worktree on a second machine (binary listed, not fetched) | 0.86 s (was 8.1 s, see below) |
| shown in the worktree's `target/` afterwards | 1,703 MiB |

Against the copying client over the 20 ms, 100 Mbit link in the table
above (113.0, 0.36, 19.5, 167.8, 0.41 and 28.2 s): the build no longer
waits for the binary, and neither does the second machine.

**The second machine's 8.1 s, broken down and mostly removed (D66).**
The series was run again with the phases of that command recorded on
both sides (`RARGO_SYNC_STATS=1` on the command, `RARGO_PHASES=1` on
the server), while the history benchmark was paused:

| second machine, 100 ms round trip | before | after |
|---|---|---|
| daemon started, link to the server ready (one round trip) | 0.10 s | 0.10 s |
| worktree read (3,698 files, 70.8 MiB hashed) | 0.05 s | 0.04 s |
| worktree offered, the server's answer awaited | 0.32 s | 0.32 s |
| request sent to answer complete | 7.45 s | 0.39 s |
| of that on the server: planning | 0.08 s | 0.10 s |
| of that on the server: every unit answered from the cache | 7.25 s | 0.16 s |
| whole command | 7.94 s | 0.86 s |

The 7.25 s were not work. When the server looks up an action it needs
the client's values of the environment variables that action's earlier
runs declared (`CC`, `CFLAGS` and the like, through build scripts'
`rerun-if-env-changed`); a client it had not asked before was asked at
each action, a round trip each, one after another down the dependency
chain: about 70 round trips for Zed, 9 for rust-analyzer (0.93 s of a
1.46 s command there; 0.05 s on a 1 ms link, which is why it did not
show on the LAN). The server now asks for all variables any action has
ever declared in one question, while it plans. A client that has
answered sends them with every later request, as before.

What is left of the 0.86 s: three round trips that follow one another
(the key agreement, the offer and its answer, the request and its
answer), the server's 0.27 s, and 0.2 s more in the offer: the
manifests of a whole repository do not fit into the first flight of a
link that has just been made, and took two more round trips to get
through. Since this run the daemon offers such a tree by its root
first, and a server that has it says so in one round trip (D66); on
rust-analyzer that took the offer from 0.24 s to 0.10 s. Zed was not
run again with it. Sending the request without waiting for the offer's
answer would save one more round trip and was not tried.

The other rows of the series in that run: 114.2 s, 0.36 s, 19.2 s,
145.4 s and 0.38 s.

The first sync was one push: 263 manifests offered, the server answered
with the 2,333 files it lacked, none asked for afterwards.

A single cold build is not faster than local cargo: it is the same
compiler on the same number of cores plus a link. What the client gets is
its own cores back, a small `target/`, and everything anyone has already
built.

### 20 agents in 20 worktrees (`dev/bench-swarm.sh`)

Each agent: check; edit a file; check; test one package; edit another file;
check; test another package; build the main binary; test again. Agents
overlap (several make the same edit to the same file). The rargo agents are
20 clients in one network namespace, 1 ms each way and 1 Gbit from the
server, sharing one client cache; the server cannot see their worktrees.
Stock cargo gets one target directory per worktree and its default `-j`;
both sides are capped at 24 cores and 90 GB. Server CPU is the cgroup's
`cpu.stat`; client CPU is the summed process times of the `rargo`
commands. Stock cargo was run alongside rargo for every repository.

| rust-analyzer | stock cargo | rargo, remote clients | ratio |
|---|---|---|---|
| wall time | 380.8 s | 125.9 s | 3.0x |
| CPU time | 8,918 s | 1,662 s on the server + 27 s on the client | 5.3x |
| disk on the client machine | 100.1 GB | 11.8 GB | 8.5x |
| disk on the server | | 42.6 GB (41.0 GB after GC) | |
| failed steps | 0 | 0 | |
| mean step times: check / check after edit / test / check after 2nd edit / test / build / test | 115 / 13.5 / 7.4 / 25.6 / 15.1 / 187 / 1.0 s | 24.2 / 8.7 / 1.4 / 9.2 / 0.8 / 74.5 / 0.4 s | |
| crossing the link | | 21.7 MiB up (one upload of the repository; 19 clients waited for it), 1,820 MiB of results down | |

| Ruff | stock cargo | rargo, remote clients | ratio |
|---|---|---|---|
| wall time | 797.5 s | 149.2 s | 5.3x |
| CPU time | 19,052 s | 2,674 s + 27 s | 7.1x |
| disk on the client machine | 94.9 GB | 2.7 GB | 35x |
| disk on the server | | 33.0 GB (30.3 GB after GC) | |
| failed steps | 0 | 0 | |
| mean step times (same order) | 309 / 24.8 / 6.7 / 79.0 / 18.0 / 311 / 2.4 s | 34.8 / 14.8 / 1.1 / 17.3 / 1.4 / 73.1 / 0.3 s | |
| crossing the link | | 71.7 MiB up, 604 MiB down | |

| Bevy | stock cargo | rargo, remote clients | ratio |
|---|---|---|---|
| wall time | 385.6 s | 88.7 s | 4.3x |
| CPU time | 8,896 s | 1,099 s + 14 s | 8.0x |
| disk on the client machine | 61.0 GB | 2.1 GB | 29x |
| disk on the server | | 25.1 GB (21.7 GB after GC) | |
| failed steps | 0 | 0 | |
| mean step times (same order) | 273 / 25.0 / 10.8 / 26.8 / 10.2 / 30.4 / 1.0 s | 35.1 / 15.1 / 4.8 / 11.1 / 0.7 / 17.4 / 0.4 s | |
| crossing the link | | 84.2 MiB up, 362 MiB down | |

| Zed | stock cargo, two groups of ten | rargo, remote clients | ratio |
|---|---|---|---|
| wall time | 4,244 s | 623.6 s | 6.8x |
| CPU time | 100,641 s | 12,009 s + 75 s | 8.3x |
| disk on the client machine | 813.9 GB | 35.7 GB | 23x |
| disk on the server | | 275.6 GB (275.5 GB after GC) | |
| failed steps | 0 | 0 | |
| mean step times (same order) | 764 / 49.8 / 53.8 / 116 / 126 / 917 / 4.7 s | 117 / 64.1 / 41.4 / 56.6 / 11.1 / 310 / 2.0 s | |
| crossing the link | | 90.8 MiB up, 5,620 MiB down | |

How Zed's stock figure was obtained, since it is the one that needed
care. Twenty worktrees with twenty target directories were used, as for
the other repositories; all 813.9 GB were on disk at the end (space was
freed for it; nothing is extrapolated). Run all at once, the twenty stock
agents were killed by the 90 GB memory cap after 2,817 s, having used
67,357 CPU-seconds and 382 GB of disk: stock cargo cannot run this swarm
on this machine under its rules. The twenty agents were therefore run as
two consecutive groups of ten (`swarm.py --batch 10`), all worktrees kept
until the end. Wall time and CPU time are the sums over both groups; a
single group of ten, measured separately before, took 2,128 s and
50,367 CPU-seconds, so the two halves agree. Ten stock agents at a time
is the more favourable setting for stock cargo per agent (less
contention), and twenty rargo agents all ran at once.

The rust-analyzer agents each end with a different 565 MiB binary, and
the Zed agents with a different 1.7 GB one, which is most of what is on
the client side; where agents build the same thing the client cache holds
it once. Most of the server's store is incremental state (about 23 GB of
the 42.6 GB for rust-analyzer). Idle blobs go to the cold tier as chunks:
the Ruff store of an earlier run went from 28.8 GiB to 3.9 GiB.

**The swarms with 100 ms of round-trip time**, client daemon and
virtual output directory (one daemon and one session for the twenty
agents, as on one machine), at the final build (`fd599de`). Stock cargo
does not cross the link; its figures are the ones above.

| 20 agents, 100 ms round trip | wall time | server CPU | client CPU: commands + daemon | on the client's disk | bytes down / up the link | failed steps |
|---|---|---|---|---|---|---|
| rust-analyzer, results copied (before) | 133.3 s | 1,707 s | 0.4 s + 38.9 s (from a second run: 132.2 s) | 11.83 GB | 1,959 MiB / 96 MiB | 0 |
| rust-analyzer, virtual output directory | 114.9 s | 1,636 s | 0.4 s + 3.1 s | 0.01 GB | 2.3 MiB / 6.7 MiB | 0 |
| Ruff, virtual output directory | 147.3 s | 2,657 s | 0.5 s + 11.8 s | 0.03 GB | 4.0 MiB / 22.6 MiB | 0 |
| Bevy, virtual output directory | 87.2 s | 1,090 s | 0.5 s + 6.4 s | 0.01 GB | 7.1 MiB / 65.6 MiB | 0 |
| Zed, virtual output directory | 603.3 s | 12,350 s | 1.1 s + 7.7 s | 0.01 GB | 9.3 MiB / 32.7 MiB | 0 |
| rust-analyzer on the LAN link, virtual output directory | 112.7 s | 1,653 s | 0.4 s + 3.0 s (from a second run: 119.6 s) | 0.01 GB | 2.2 MiB / 6.7 MiB | 0 |

Against stock cargo (which does not cross the link): 380.8 s, 797.5 s,
385.6 s and 4,244 s of wall time (Zed's in two groups of ten), 100.1,
94.9, 61.0 and 813.9 GB of disk. With results copied over the LAN link
(the tables above) the four swarms took 125.9, 149.2, 88.7 and 623.6 s
and left 11.8, 2.7, 2.1 and 35.7 GB on the client.

- **Bytes.** With results copied, every agent's binary and test
  executables cross the link and land in its `target/`: 1,959 MiB down
  for rust-analyzer. With the virtual directory the agents are told what
  exists (40 names in 20 worktrees at the end) and open none of it:
  2.3 MiB down. Up the link go the edits: 23 MB of worktree content in
  all, most of it the first worktree's first sync.
- **Disk.** What is left on the client is its cache (dependency
  information and other small files) and empty real `target/`
  directories: 0.01 GB instead of 11.8 GB. The figure is taken after the
  daemon has been stopped and the directories unmounted.
- **Time.** 100 ms of round-trip time costs the rust-analyzer swarm two
  seconds (114.9 s against 112.7 s on the LAN link; a second run at
  100 ms took 116.4 s); copying results costs about 18 s at 100 ms.
- **Client CPU.** The commands are the `rargo` processes; the daemon is
  a separate process, read from `/proc` before it is stopped (scanning,
  hashing edits, serving the output directories for twenty agents): 3 to
  12 s over a whole swarm with the virtual directory, and 38.9 s when
  results are copied (the daemon then receives and writes 1.9 GiB into
  twenty `target/` directories). For two rows it was not recorded in the
  run the row is from (the lookup for the daemon's process was wrong);
  those two swarms were run again at `bb2192b` during a pause of the
  history benchmark, and the daemon figure is from that second run
  (132.2 s and 2,044 MiB down with results copied; 119.6 s on the LAN
  link, against 112.7 s in the first run, a difference I did not look
  into).
- Runs one build earlier (`32f6c5e`) of Ruff and Bevy at 100 ms failed
  (2 and 20 failed steps): twenty worktrees starting at once each
  uploaded the repository, the server took longer than 45 s to take it
  all in, keepalive answers queued behind it and the daemon took the
  link for dead. Both causes are fixed (D60).

#### The nextest variant (2026-11-24, `b22ecdf`, issue 33)

`SWARM_TESTS=nextest dev/bench-swarm.sh ra`: the same swarm, with every
`test -p PKG` step run as `nextest run -p PKG` (nextest 0.9.152).
- **rargo:** 20 clients at 100 ms round trip (`CLIENT_DELAY_MS=50`),
  virtual output directory. Under rargo each test is a cached action of
  its own (D83).
- **stock:** `cargo nextest` with the same nextest on `PATH`, one target
  directory per worktree. Both sides are capped at 24 cores and 90 GB.
- No failed steps on either side.

| rust-analyzer, nextest | stock cargo nextest | rargo nextest, 100 ms RTT | ratio |
|---|---|---|---|
| wall time | 418.2 s | 124.1 s | 3.4x |
| CPU time | 9,816 s | 1,974 s on the server + 0.4 s in the commands + 3.5 s in the client daemon | 5.0x |
| disk on the client machine | 100.1 GB | 0.015 GB | |
| disk on the server | | 42.6 GB (40.6 GB after GC) | |
| crossing the link | | 6.7 MiB up, 2.1 MiB down | |
| mean step times: check / check after edit / test / check after 2nd edit / test / build / test | 125.7 / 13.0 / 7.9 / 31.9 / 19.6 / 208.4 / 1.4 s | 25.5 / 9.2 / 3.2 / 11.7 / 2.2 / 66.0 / 2.4 s | |

**Against the `test` swarm at 100 ms** (114.9 to 116.4 s, about 1,620 to
1,650 s of server CPU):
- nextest costs the rargo swarm about 8 s of wall time and about 330 s of
  server CPU.
- **Test steps take longer than with `rargo test`:** 3.2, 2.2 and 2.4 s
  against 1.4, 0.8 and 0.4 s. A rerun of the rargo side gave 3.9, 2.6
  and 2.1 s.
- **One client alone does not show that gap.** `rargo nextest run -p
  stdx` against `rargo test -p stdx`, one client at 100 ms round trip,
  medians of three:
  - everything replayed: 0.33 s against 0.11 s;
  - every test run again (`--no-cache`): 0.35 s against 0.12 s, the same
    while another worktree rebuilds rust-analyzer on the server;
  - first run after a test is added (the test binary changed): 0.60 s
    against 0.35 s.

  So nextest's rounds cost about 0.25 s per command. What makes up the
  rest of the 1.5 to 2 s in the swarm, where twenty agents share the
  server, is not measured.

### Running a debug binary fetched as it is read (`dev/bench-run.sh`, issue 115, 2026-11-25)

**Setup.**
- rust-analyzer at `37721d141`: its debug `rust-analyzer` binary is
  476,998,528 bytes.
- A server with 24 cores builds it for a first worktree.
- Then a fresh worktree of the same commit, on a machine that has never
  fetched it (an empty client cache), runs `rargo build -p rust-analyzer`
  (nothing to build), then, measured,
  `rargo run -p rust-analyzer -- --version`.
- Bytes are what the client's network interface received during the run.
  The client link is LAN, or 50 ms each way (100 ms round trip).
- **ranged** is this design (protocol 8): reading fetches the chunks under
  each read, and the first read of the code region asks for the rest of it
  behind them. **whole** is 0.6.5, which fetches the whole content (by
  chunk, compressed) at its first read.
- Three rounds; each figure is the median, and the rounds differed by at
  most 0.02 s.

| | LAN | 100 ms round trip |
|---|---|---|
| **ranged**: wall time of the run | 1.13 s | 1.99 s |
| **ranged**: bytes received during it | 28.1 MB | 27.9 MB |
| **whole**: wall time of the run | 1.69 s | 2.67 s |
| **whole**: bytes received during it | 97.1 MB | 97.1 MB |

**What the numbers say.**
- A run receives 5.9% of the binary's size. It is 0.56 s faster than
  fetching the binary whole on LAN, and 0.68 s faster at 100 ms.
- The rest of the code region is still arriving when `--version` exits:
  18–21 MB more, received during the next run. So a binary that is run
  once costs about 48 MB in all (half of what a whole fetch receives).
  The debug information, the remaining 429 MB of the file, is not
  fetched.
- The same command again takes 0.14–0.16 s on LAN and 0.19–0.21 s at
  100 ms, against 0.05 s and 0.15 s for a whole binary in the cache. The
  partly fetched file is still read through the daemon until every part
  of it is here, and the kernel reads a whole one itself (passthrough).
- `dev/e2e-link.sh`, "a binary is read by range", covers the rest:
  - A run of a small debug binary fetched 620,916 of its 4,548,168 bytes.
  - A panic with `RUST_BACKTRACE=1` printed files and lines, after
    fetching the debug sections.
  - Read through to the end, the file is byte for byte the server's
    stored content.
  - With the server's store wiped, the file reads the same and runs from
    the cache, fetching nothing.

### Chunking rule: RepMaxCDC against the mask rule (issue 117, 2026-11-25)

The question: is RepMaxCDC (`github.com/buildbarn/go-cdc`, as Bonanza uses
it) a better rule for cutting chunks inside a structural region than
`chunk.rs`'s gear hash with two masks? The structural boundaries (archive
members, ELF sections) are the same for both.

**Corpus** (`dev/cdc-corpus.sh`; 48.5 GB of files):
- Two projects, each built by rargo at 11 successive first-parent commits,
  oldest first:
  - rust-analyzer: `aa1982292` to `37721d141`;
  - rargo: `8daafa142` to `d552e2066`.
- The builds ran on a private rargod (0.6.4 with #119's change), plain
  `rargo build`.
- After each build, one version was kept:
  - everything the build leaves in `target/debug` (workspace rlibs,
    binaries, `.d` files);
  - every crate's newest incremental state, read out of the server's store
    (`crates/rargo-core/examples/incrdump.rs`).
- Per version: rust-analyzer 1.41 GB of target and 2.0–2.07 GB of
  incremental state (52 crates); rargo 0.25 GB and 0.41–0.82 GB (8, then
  16 crates, because the 0.6.4 version bump at its fourth commit gave every
  crate a new incremental directory, and the old ones stay in the store).
- Some rargo commits change only documentation; their builds are the
  same as the one before.

**Measured** (`crates/rargo-core/examples/cdcbench.rs`):
- **store:** the bytes of the distinct chunks of the whole corpus, each
  compressed with zstd level 3, as the cold tier does.
- **send:** for each version after the first, the bytes of its chunks that
  the version before it, of the same project, does not have; summed.
- **cut speed:** finding the cuts (structure and rule, not hashing the
  chunks) on one core, over 2 GiB of the first versions held in memory;
  the best of three runs.
- **Bounds:** RepMaxCDC's minimum was chosen to give the mask rule's mean
  chunk size, 64 KiB (calibrated on the first versions). Its horizon is
  eight times that, Bonanza's setting.

**Checked:**
- The benchmark's copy of the mask rule gave `rargo_core::chunk::chunk`'s
  cuts on every file of the corpus.
- RepMaxCDC is cut in one pass, and gave the cuts of the reference
  algorithm (go-cdc's `simple_rep_max_content_defined_chunker.go`, ported)
  on every file.
- No RepMaxCDC chunk reached twice the minimum.

| | mask rule (`chunk.rs`) | RepMaxCDC, min 64 KiB | change | bar to adopt |
|---|---|---|---|---|
| mean chunk | 54,409 B (892,086 chunks) | 54,718 B (887,060) | +0.6% | equal |
| store | 4,021,297,124 B | 4,023,320,960 B | +0.05% | 2% less |
| send (of 44.4 GB) | 7,528,838,651 B | 7,482,929,506 B | −0.61% | 2% less |
| send, rust-analyzer | 5,845,381,538 B | 5,822,725,593 B | −0.39% | |
| send, rargo | 1,683,457,113 B | 1,660,203,913 B | −1.38% | |
| cut speed | 3,053 MiB/s | 1,930 MiB/s | −36.8% | at most 10% slower |

RepMaxCDC meets none of the three bars, so `chunk.rs` is unchanged. Its
cuts are slower because it hashes nearly every byte, where the mask rule
skips the first 16 KiB of every chunk. Unrolling its scan by eight bytes,
as go-cdc does, did not change the speed.

### Tracing overhead (`dev/bench-trace.sh`)

The same cold command twice, both capped at 24 cores: stock cargo on the
files directly, and rargo with a fresh server and the client on the same
machine, so that no link is involved. rargo's CPU time is split by where
it was spent: the compilers themselves (the server's cgroup minus the
server process), the server's threads that answer the sandbox file system
(`fuse-*`, read from `/proc`), and the rest of the server.

| rust-analyzer, cold | `check` | `build` |
|---|---|---|
| stock cargo: wall | 22.7 s | 38.5 s |
| stock cargo: user + system CPU | 63.6 + 15.1 s | 142.6 + 28.5 s |
| rargo: wall | 23.5 s | 42.4 s |
| rargo, compilers: user + system CPU | 62.3 + 14.9 s | 141.9 + 28.0 s |
| rargo, server threads answering the sandbox file system | 1.8 s | 2.8 s |
| rargo, rest of the server (planning, hashing and storing outputs, scheduling, the client) | 3.6 s | 9.5 s |

So the tracing share is the 1.8 s and 2.8 s of the server's file system
threads: 2.3% and 1.6% of what stock cargo spends on the same command.
The compilers themselves use no more CPU in the sandbox than outside it
(system time 14.9 s against 15.1 s and 28.0 s against 28.5 s): with FUSE
passthrough their reads and writes go to the backing files, and what
reaches the server is lookups and opens. The rest of the server costs
more than tracing does (4.6% and 5.5%). Wall
time is 0.9 s and 3.9 s longer; for the build that includes bringing the
built binaries to the client. One run each.

The earlier worst-case figure stands as measured with the first
implementation (the sandbox and its file system are unchanged): a whole
`cargo build` of this repository (105 crates) as one process tree inside
one traced sandbox took 8.4 to 8.5 s against 8.0 s native (about 5% wall,
under 1% user CPU, 16% more system time), with all 24 compilers sharing
one action.

### Small synthetic workspace (`dev/e2e.sh`, 113 checks, all passing in both output modes)

The server runs in a mount namespace in which the client's directory is an
empty file system, with its own cargo home, and requires a key.

- A wrong key, an unknown user and a non-administrator asking for garbage
  collection are refused.
- Second worktree at the same commit: zero actions; one file (the `HEAD`
  naming its branch) and at most four manifests received; the artifact is
  the same content as the first worktree's (in copy mode: the same inode,
  or a reflink).
- A one-file edit uploads one file and the four manifests on its path, and
  reruns exactly the edited crate and its dependents.
- A file git ignores but the build includes is synced and built with; a
  3 MB file travels in parts; a directory the policy excludes is not sent.
- Eight concurrent cold builds of the same commit from eight worktrees: one
  build's worth of actions.
- A variable a build script declares is asked of the client and causes a
  rebuild; an unrelated one changes nothing.
- A `config.toml` in the client's cargo home, which the server cannot see,
  applies to that client's builds and to nobody else's.
- A test binary reruns only when something it read changed; a file a test
  writes appears in the client's worktree.
- A build script sees HEAD, its branch and whether the tree is dirty, and
  no history; with `sync.git = false` it sees no repository.
- `run` passes arguments and resolves compiled-in paths; `doc --open`
  starts the client's browser on the client's file.
- Stock cargo builds, tests and documents in the same `target/`
  afterwards, and rargo works again after that.
- A crate with a future-incompatibility warning: the closing warning names
  the report, `cargo report future-incompatibilities --id 1` shows it, its
  text equals what stock cargo writes for the same crate, and a cached
  build reports the same warnings under the same number.
- `--timings` writes a report of what ran and what came from the cache.
- Temporaries of a client that was killed are removed by the next one;
  those of a live process are kept; an interrupted client leaves none.
- A build script that sleeps for ten minutes is gone within five seconds
  of its client being killed, and the server builds on.

### The link and the output directory (`dev/e2e-link.sh`, 67 checks, all passing)

Server and client in separate network namespaces; the server cannot see
the client's directories.

- The first command starts a daemon; later commands use the same daemon
  and the same UDP session; a command in an unchanged worktree needs no
  scan.
- An edit reaches the server without any command; the build that
  follows finds the server with nothing to ask for; so does a build
  started in the instant of an edit; a crate added in a new directory,
  an edit inside it and checkouts back and forth are followed.
- The client's address is changed under the session: the next build
  works on the same session.
- The daemon is killed: the next command starts another. The server is
  restarted: the daemon connects again.
- An interrupted command stops its build script on the server; the
  daemon and its session stay.
- `RARGO_NO_DAEMON=1` works; with UDP dropped in the client's namespace
  a new daemon uses TCP (2.6 s for its first command) and
  `RARGO_TRANSPORT=udp` fails instead of falling back.
- `target/` is the daemon's file system; a new binary is listed with its
  size without its content crossing the link, running it fetches it,
  nothing is copied into the worktree; stock cargo builds and tests in
  the same directory and rargo replaces what stock cargo left;
  `RARGO_OUTPUT=copy` gives a plain directory with the binary in it;
  a directory listed while other processes create and remove files in
  it names every file that stays (150 listings; the build before the
  fix came up short in 60); `rargo clean` removes `target/`; a killed
  daemon's mount is unmounted at once with no rargo command run, and
  stock cargo builds and tests in the worktree (D70); a stopped daemon leaves a plain
  directory in which stock cargo builds the same program.
- As the user `nobody` (in a directory under `/tmp`, which that user
  can reach): a build mounts the output directory through
  `fusermount3`, the binary runs out of it, the user can write there,
  and stopping the daemon unmounts it and leaves what was written.

### Incremental state on workers (`dev/e2e-worker.sh`, 27 checks, all passing)

A server with one core and a worker with four, then with one: first
compiles of workspace crates run on the worker and leave their state
there; the state comes home in the background; edited crates are
compiled on the worker from the state it holds; with the background
fetch put off, a compile on the server fetches the state it starts
from; a worker killed with states on it costs nothing but time.

## No-drift

The server-side half and the client-side half (D42), run through a fresh
server that cannot see the repositories (`dev/nodrift-all.sh`), at
`a92e359`: rargo build and test; the server's drift check (stock cargo
against rargo re-running every cached unit, on the synced state); tracked
files untouched; stock cargo build and test in the same worktree and
`target/`; rargo again.

| | rust-analyzer | Ruff | Bevy | Zed |
|---|---|---|---|---|
| rargo build / test | ok (43 s) / ok (51 s, full suite) | ok (71 s) / ok (33 s; `ruff_linter`, `ruff_python_parser`, `ruff_text_size`) | ok (62 s) / ok (45 s; `bevy_ecs`, `bevy_math`, `bevy_color`) | ok (198 s) / ok (31 s; `util`, `rope`, `sum_tree`) |
| drift check on the server | no drift (92 s) | no drift (232 s) | no drift (184 s) | no drift (702 s; final commit, see below) |
| tracked files untouched | ok | ok | ok | ok |
| stock cargo build and test, same `target/` | ok | ok | ok | ok |
| rargo again afterwards | ok (1 s) | ok (3 s) | ok (0 s) | ok (0 s) |

In that run the worktrees' `target/` directories still held stock cargo's
artifacts from earlier runs, so stock cargo had little to rebuild. The
same was therefore repeated in a fresh worktree of each repository with an
empty `target/` (`FRESH=1 dev/nodrift-all.sh`; the server-side check is
left out, the run above has it), so that stock cargo has everything to
build next to what rargo placed:

| fresh worktree | rust-analyzer | Ruff | Bevy | Zed |
|---|---|---|---|---|
| rargo build / test | ok (43 s) / ok (51 s) | ok (71 s) / ok (35 s) | ok (62 s) / ok (44 s) | ok (189 s) / ok (31 s) |
| tracked files untouched; nothing left outside `target/` | ok | ok | ok (an untracked `Cargo.lock`, see below) | ok |
| stock cargo cold build into the same `target/` | ok (39 s) | ok (65 s) | ok (59 s) | ok (168 s) |
| stock cargo test | ok (46 s) | ok (33 s) | ok (43 s) | ok (29 s) |
| rargo build again afterwards | ok (0 s) | ok (0 s) | ok (0 s) | ok (0 s) |
| failures | 0 | 0 | 0 | 0 |

Bevy leaves an untracked `Cargo.lock` (Bevy ignores it; stock cargo writes
it as well). Zed's stock cargo runs inside its flake's dev shell.

**With the virtual output directory.** Both tables above were made with
results copied into `target/`. Repeated at the final build with
`target/` mounted by the client daemon, so that stock cargo reads what
rargo shows there and writes through the same mount (one client on the
server's machine over the loopback interface, as above):

| virtual output directory, in place | rust-analyzer | Ruff | Bevy | Zed |
|---|---|---|---|---|
| rargo build / test | ok (40 s) / ok (49 s) | ok (65 s) / ok (34 s) | ok (60 s) / ok (43 s) | ok (181 s) / ok (31 s) |
| drift check on the server | no drift (92 s; 314 units re-run) | no drift (233 s; 562 units) | no drift (183 s; 508 units) | no drift (701 s; 1,717 units, 25 not reproducible by the tools themselves) |
| tracked files untouched | ok | ok | ok | ok |
| stock cargo build and test, same `target/` | ok (3 s) / ok (12 s) | ok (5 s) / ok (4 s) | ok (1 s) / ok (15 s) | ok (5 s) / ok (4 s) |
| rargo again afterwards | ok (0 s) | ok (0 s) | ok (0 s) | ok (0 s) |

| virtual output directory, fresh worktree | rust-analyzer | Ruff | Bevy | Zed |
|---|---|---|---|---|
| rargo build / test | ok (40 s) / ok (51 s) | ok (67 s) / ok (33 s) | ok (60 s) / ok (43 s) | ok (186 s) / ok (31 s) |
| tracked files untouched; nothing left outside `target/` | ok | ok | ok (the untracked `Cargo.lock`) | ok |
| stock cargo cold build through the mount | ok (40 s) | ok (65 s) | ok (59 s) | ok (171 s) |
| stock cargo test | ok (48 s) | ok (33 s) | ok (43 s) | ok (30 s) |
| rargo build again afterwards | ok (0 s) | ok (0 s) | ok (0 s) | ok (0 s) |

The in-place table is from `ddf3d18`; the fresh-worktree table is from
the final commit, after the listing fix (at `ddf3d18` the same three
had passed, with 41, 67 and 56 s for stock cargo's cold build). Stock
cargo's cold build writes everything through the daemon's file system:
40, 65 and 59 s here against 39, 65 and 59 s into a plain directory in
the earlier table (one run each): no cost that single runs can show.
**Zed: 170.9 s against 168.8 s for the cold build** (measured back to
back with `bench/zed-prof.sh` while the history benchmark was paused),
**30 s against 29 s for the tests** (one build earlier; the tests were
not run again after the last change). It was 446 s and 187 s. Zed's
profile compiles incrementally with unpacked split debug info, which
puts tens of thousands of small files into `target/debug/deps`, and
every compiler looks through that directory. In the order found:

| | cold build | tests |
|---|---|---|
| one lock around every operation; misses not remembered by the kernel | 446 s | 187 s |
| no lock across system calls; misses cached (D61) | 198 s | 55 s |
| fewer system calls per operation (D63) | 203 s | 56 s |
| a listing built once per reading instead of once per kernel call (D63) | 175 s | 30 s |
| a listed directory's names kept up to date, never read again (D64) | 170.9 s | not re-run |
| plain directory | 168.8 s | 29 s |

During the cold build the daemon serves 1.3 million operations
(428,000 `getattr`, 301,000 `release`, 203,000 `open`, 145,000
`lookup`, 98,000 `create`, 52,000 `setattr`, 47,000 `readdir`, 22,000
`unlink`, ...) and uses 31 s of CPU for them, spread over its threads
(43 s before D64, of which 13 s were listings). What is left, 2 s in
169, is those round trips; nothing in it stands out any more, and it
was not pursued.
What this run does not cover: a stock `cargo clean` not going through
the shim fails at its last step (it cannot remove a mount point).

**Zed's drift check** needed work and is the part to read with care.
At `a92e359` it failed. Three things were wrong or missing, none of them
drift (D51, D55):

- The stock-cargo half ran outside the flake and could not find Zed's
  system libraries. It now runs in the flake's environment, as Zed's
  users run cargo.
- Zed's profile combines incremental compilation with unpacked split
  debug info. Every codegen unit's files then carry the compiler's
  session in their names, and the objects an identifier derived from it:
  228 units "differed". Verify now pairs files without the session
  component and accepts exactly the eight-byte identifier that a unit's
  own `.dwo` files show to have changed (and, in linked files, the
  linker's build id that follows from it).
- 25 units remain that two clean builds by stock cargo alone do not
  reproduce either, which was checked for each kind at one path with
  stock cargo: linked outputs under split debug info (a three-line build
  script's executable differs in about 10,000 places), `wasmtime-wasi-io`
  and two crates above it (their dependency information lists the same
  files in a different order, metadata and code differ with it), and
  build scripts that print the same `cargo:` lines in a different order.
  Verify recognises these three shapes and reports the units as not
  reproducible by the tools themselves, separately from drift.

At the final commit the check reads: no drift; stock cargo builds
(456 s), rargo builds (243 s); 1,717 cached units re-run, 0 differed, 25
not reproducible by the tools themselves. Zed's other rows in the table
above are from the run at `a92e359` and were the same in the final run
(build 189 s, test 31 s, everything else ok). What this does not show:
for those 25 units the check cannot tell a cached result from a wrong
one, because there is no single right answer to compare with.

A difference stock cargo would not have, found earlier and fixed: the
server-built `rust-analyzer --version` said `37721d1` where stock cargo's
says `37721d141`, because git abbreviates by repository size and the
stand-in for `.git` is tiny. The stand-in now carries the client
repository's abbreviation length.

Also checked once by hand: a project whose `rust-toolchain.toml` names a
toolchain the server did not have (1.98.0) was built and run through the
server in 5.7 s; the server installed the toolchain, the client has no
part in it.

## Mechanisms

### Client daemon and sync
`rargo/src/daemon.rs`, `watch.rs`, `scan.rs`, `mux.rs` (client),
`rargo-core/src/serve.rs`, `snapshot.rs`, `session.rs` (server); D57,
D58. Verified by: unit tests (scan skips and reuse, the watcher, frames
put back in order, sync asks only for what is missing, evicted content
is asked for again, a client that withholds content fails the sync),
e2e counts, `dev/e2e-link.sh` (one daemon and one link over many
commands, an edit on the server before any command, an edit and a build
in the same instant, new directories, checkouts, daemon killed, server
restarted, a command interrupted, no daemon).
Measured: a command in an unchanged, watched worktree needs no scan; a
scan of rust-analyzer (2,386 files) with a warm index takes about 3 ms.
Known weaknesses: the watcher is inotify, one watch per directory, and a
worktree whose directories cannot all be watched (the per-user limit) is
scanned for every command; a scan is always of the whole worktree, the
watcher only says whether one is needed; the daemon's belief about what
the server holds is per session and in memory, so a new session offers a
worktree's manifests again (and asks once what is missing); two daemons
on different machines that first sync the same new content at the same
moment both upload it (within one daemon first syncs take turns, found
by the Ruff and Bevy swarms at 100 ms, where twenty at once uploaded
the repository twenty times). A large git-ignored directory is hashed
and uploaded once unless the policy file excludes it. The whole tree
must be in the server's store before the build starts. Symlinks that
point outside the worktree dangle on the server. Sparse checkouts are
synced as they are on disk. A daemon is one more process per machine and
server; it is keyed by server, user, key, cache directory and program
version, so stale ones are never talked to, only left to expire.

### Stand-in for `.git`
`rargo/src/git.rs`. Verified by: a unit test that runs real git against
it (HEAD, log, branch, status, describe, a linked worktree, a detached
HEAD), e2e, and rust-analyzer's build script no longer warning.
Known weaknesses: no history (`describe --tags` fails as in a shallow
clone) and no contents (`git diff`, `git show`); in a repository with CRLF
files committed under `eol=lf`, `git status` in the sandbox calls them
modified (D36); submodules' own git data is not provided.

### Environment
`rargo-proto/src/lib.rs` (`env_allowed`), `rargo-core/src/engine.rs`
(`ClientEnv`), `build.rs` (`base_env`, `Home`).
Known weaknesses: the build environment is the server's, so anything a
build needs from the machine (a system library, `protoc`) has to be on the
server or come from the repository's flake; cargo configuration in
directories above the worktree is not synced; a client's `credentials.toml`
is stored on the server (mode 0600, in the state directory) for as long as
the state directory lives.

### Transport and access
`rargo-net/src/lib.rs` and `link.rs`, `rargo-proto/src/secure.rs`,
`rargo-core/src/serve.rs`; D57, D59. Both links are sessions of the
reliable-UDP transport, encrypted with fresh keys per session (Noise
`NNpsk0`): the client link under a key per user, with every datagram
after the key agreement tagged by a key of the session, and the worker
link under the shared worker key (D44). A client session follows its
client's address. Where UDP gets no answer the daemon uses one TCP
connection. Verified by unit tests (user proven, wrong key and unknown
user refused, a forged datagram neither delivered nor able to move the
session, the session following a changed address, loss in both
directions) and e2e-link (address changed under a session, UDP dropped).
Measured (`linkperf`, the rig's 1 Gbit client link): 111, 102 and
77 MiB/s at 2, 40 and 100 ms round trip, no retransmissions; before the
transport was paced it managed under 3 MiB/s at 40 ms.
Known weaknesses: the congestion control is simple (slow start until
round trips lengthen, then a twentieth more per round trip, three tenths
less on a loss) and was only run on the rig's emulated links, never
across a real WAN or against competing traffic; the rig's netem queues
had to be enlarged for the emulated link to carry what a real one would
(D59); one session per daemon means one command's large download
shares a window with another's. Every user shares one cache and could
read another's source through it by guessing nothing more than a build
command, which is the intended trust model, not an accident; keys are
plain lines in a file, with no rotation or revocation other than editing
it and restarting.

### Results on the client
`rargo/src/outfs.rs` (the virtual output directory), `place.rs` (copying),
`daemon.rs`; D59. `target/` is mounted by the daemon; what the server
built is listed there at once and fetched into the client's cache when
opened (by chunk from 1 MiB); what is written locally goes to the real
directory underneath; files up to 64 KiB come with their names.
Verified by: `dev/e2e-link.sh` (mounted, listed without content crossing
the link, fetched on run, nothing copied into the worktree, stock cargo
build and test in the same directory, rargo after stock cargo, copy mode
and back, `clean`, a daemon killed with SIGKILL leaving a plain
directory in which stock cargo builds and tests, a stopped daemon
leaving a plain directory in which stock cargo builds); `dev/e2e.sh` in
both modes; the no-drift runs below. Copying: unit tests (sharing, own
copies, parts, forged content refused).
A command that is interrupted withdraws its request; its queued and
running actions go with it, on the server and on workers (D48; e2e: a
build script sleeping for ten minutes is gone within five seconds).
Known weaknesses: without the right to mount, the directory is mounted
through `fusermount3` and file contents then go through the daemon
instead of straight to the real file (one e2e check as `nobody`; every
measurement was made as root); the directory
is only as present as its daemon: after a reboot or a stopped daemon,
or a killed one, `target/` is a plain directory that shows what was
written locally until the next rargo command mounts it again (stock
cargo works in it, and builds again what it does not find);
stock `cargo clean` cannot remove a mount point (`rargo clean` can); the
chunk index is in the daemon's memory, so chunks are only reused from
contents this daemon fetched by chunk; a file opened for writing is
fetched whole and copied first; no file system test suite was run
against it, only cargo, rustc and the tools the suites use; with the
directory mounted, stock cargo's own reads and writes go through FUSE
(passthrough for file contents, the daemon for names and attributes),
whose cost shows in the no-drift table. In copy mode: without reflinks,
shared artifacts are read-only hard links into the client's cache; a
temporary whose process id has been reused stays until that process
ends. An action another build was also waiting for is killed with its
owner and started again by the waiting build.

### Snapshots, fetching, flakes
`rargo-core/src/snapshot.rs`, `build.rs` (`fetch`), `flake.rs`.
A fetch writes the whole snapshot to a scratch directory first, which
costs a copy of the source on file systems without reflinks; it happens
only when cargo needs the network. A server without network access needs
its cargo home filled beforehand (the benchmark server had none and used
one filled earlier). A binary built in a flake sandbox needs the flake's
store paths on the client to run there.

### Planner (cargo fork)
`forks/cargo/*.patch` on cargo 1.100.0. Cargo's real pipeline runs with
process execution stubbed; build-script results appear in recorded commands
as placeholder tokens that rargod expands.
`--timings` writes rargo's own report where cargo puts its own, and the
future-incompatibility report goes into cargo's report file on the client
(D49; e2e: `cargo report future-incompatibilities` shows it, and its text
equals stock cargo's for the same crate).
Known weaknesses: the timings report is not cargo's (it describes what the
server did: runs, cache hits, queueing); the report's "newer version
available" suggestion is not reproduced; example scraping failures are
treated as hard errors; the fork is pinned to one cargo release.

### Trace cache, engine, scheduler
`rargo-core/src/{trace,engine,sched}.rs`. Only successes are recorded.
Verified by `tests/engine.rs` (six tests with real sandboxed processes) and
five scheduler unit tests. Not verified under a real memory-bound workload.
A compiler that is killed (a cancelled client, the losing copy of a hedged
action) has its helper threads' jobserver tokens put back from a count of
its code-generation threads, a lower bound, and the pipe is settled exactly
whenever nothing runs (D53); without per-action cgroups only the second
half applies.

### Shared incremental compilation
`rargo-core/src/incr.rs`. States are the largest thing in the store.
Seeds are chosen with the commits the client names (D43). Remote (worker)
compiles run without a seed.

### Verify mode and drift check
`RARGO_VERIFY=1`, `rargo --rargo-drift-check`, background every six hours
on the server for each worktree's newest synced state (D42).
Findings: rustc's incremental mode is not byte-reproducible. Verify pairs
outputs by name without the per-session component and compares contents
with session names blanked; with split debug info it also accepts the
eight-byte identifier that ties an object to its `.dwo`, when that
substitution is the one the unit's own `.dwo` files show (D51; unit tests,
and a three-crate workspace with `split-debuginfo` unpacked, packed and
off). For a repository with a flake the stock-cargo half runs in the
flake's environment. Three shapes of difference that stock cargo's own
builds show from run to run are reported as "not reproducible by the
tools themselves" instead of as drift (D55; see No-drift for Zed).
Known weakness: for such units the check cannot tell a cached result from
a wrong one.

### Workers
`rargo-core/src/{net,chunk,remote}.rs`, `dev/netns.sh`, `dev/bench-worker.sh`,
`dev/bench-worker-swarm.sh`, `dev/sim-workers.py`, `examples/netbench.rs`.
Server, workers and the client in their own network namespaces; every
worker link 1 ms each way, 0.1% loss, 1 Gbit (`tc netem`); each daemon
capped at its own core count. The link is authenticated and encrypted:
per-connection Noise keys from the shared key, every message sealed, on
top of the per-datagram tag (D26, D44). A worker without the key cannot
connect (unit test), and the series below ran over the encrypted link.

**The phase 4 criterion is met in part.** The series, with a remote
client, at `a92e359` (dispatch of D47, D53 and D54):

| rust-analyzer `build`: cold / again / after a one-file edit | 4 cores per machine | 8 cores per machine |
|---|---|---|
| server alone | 56.5 s / 0.07 s / 10.9 s | 45.2 s / 0.08 s / 10.7 s |
| + 1 worker that has seen nothing | 60.1 s / 0.07 s / 15.1 s | 49.1 s / 0.08 s / 10.5 s |
| + 1 warm worker | 48.7 s / 0.07 s / 16.5 s | 44.4 s / 0.07 s / 10.7 s |
| + 2 workers that have seen nothing | 63.9 s / 0.07 s / 11.5 s | 50.9 s / 0.08 s / 10.7 s |
| + 2 warm workers | 48.1 s / 0.07 s / 16.9 s | 44.2 s / 0.07 s / 10.5 s |
| server with 2x the cores, no worker | 44.6 s / 0.07 s / 10.7 s | 43.1 s / 0.07 s / 10.6 s |
| server with 3x the cores, no worker | 43.3 s / 0.07 s / 10.7 s | 41.6 s / 0.07 s / 10.0 s |

| 20-agent rust-analyzer swarm | wall | server CPU |
|---|---|---|
| server 8 cores | 179.8 s | 1,266 s |
| server 8 + worker 8 | 182.8 s | 1,330 s |
| server 16 cores | 126.4 s | 1,442 s |

That series showed two regressions against a server alone: a worker that
had seen nothing made the cold build slower, and after a cold build with a
warm worker the first rebuild was slower. Two rules were added for them
(D55: a crate that is compiled incrementally is compiled on the server;
an action nothing is known about runs on the server first), and the
4-core series repeated at the final commit:

| rust-analyzer `build`, 4 cores per machine, final commit | cold | again | after a one-file edit |
|---|---|---|---|
| server alone | 56.0 s | 0.07 s | 11.3 s |
| + 1 worker that has seen nothing | 55.8 s | 0.07 s | 11.4 s |
| + 1 warm worker | 49.2 s | 0.07 s | 11.1 s |
| + 2 workers that have seen nothing | 56.0 s | 0.07 s | 12.5 s |
| + 2 warm workers | 48.7 s | 0.07 s | 11.4 s |
| server with 8 cores, no worker | 45.2 s | 0.07 s | 10.5 s |
| server with 12 cores, no worker | 42.5 s | 0.07 s | 10.6 s |

The 8-core series and the swarm were not repeated with these two rules;
the rules only keep work on the server that the tables above show was
better off there.

**Incremental state left on workers (D56)**, the first idea on the list
of things not tried. A compile on a worker leaves its incremental state
there; the state is recorded at once, used in place by the next compile
of that crate on that worker, fetched in the background on idle cores,
and fetched at once when a compile on the server starts from it.
Workspace crates therefore go to workers again. Measured at `a0aa3f4`
with the client of that commit:

| rust-analyzer `build`, 4 cores per machine | cold | again | after a one-file edit |
|---|---|---|---|
| server alone | 56.5 s | 0.07 s | 11.6 s |
| + 1 worker that has seen nothing | 51.9 s | 0.07 s | 11.7 s |
| + 1 warm worker | 48.8 s | 0.07 s | 11.5 s |
| + 2 warm workers | 47.8 s | 0.07 s | 11.7 s |
| server with 8 cores, no worker | 44.5 s | 0.07 s | 11.1 s |
| server with 12 cores, no worker | 43.4 s | 0.08 s | 11.1 s |

| 20-agent rust-analyzer swarm, 8 cores per machine | wall | server CPU | failed steps |
|---|---|---|---|
| server alone | 176.0 s | 1,245 s | 0 |
| server + worker | 139.9 s | 919 s | 1 |
| server with 16 cores | 122.4 s | 1,404 s | 0 |
| again at `32f6c5e` (client daemon, virtual output directory): alone / + worker / 16 cores | 177.8 / 145.9 / 125.5 s | 1,273 / 936 / 1,442 s | 0 / 5 / 0 |
| again at `ddf3d18` (starved worker actions rerun on the server: 18 of them): alone / + worker / 16 cores | 172.8 / 147.7 / 114.2 s | 1,236 / 949 / 1,377 s | 0 / 0 / 0 |
| at the final build (`fd599de`, the fault fixed; no action handed back): alone / + worker / 16 cores | 176.0 / 150.6 / 116.5 s | 1,270 / 983 / 1,406 s | 0 / 0 / 0 |
| (a diagnostic run at the final build, with a worker only) | 140.4 s | 900 s | 0 |

The cold-build series again at the final build (`fd599de`, client
daemon, no worker failure in any configuration):

| rust-analyzer `build`: cold / again / after a one-file edit | 4 cores per machine | 8 cores per machine |
|---|---|---|
| server alone | 54.4 s / 0.07 s / 8.9 s | 43.6 s / 0.06 s / 8.8 s |
| + 1 worker that has seen nothing | 51.6 s / 0.06 s / 9.6 s | 44.7 s / 0.06 s / 8.8 s |
| + 1 warm worker | 48.6 s / 0.06 s / 9.5 s | 41.9 s / 0.06 s / 9.1 s |
| + 2 warm workers | 46.5 s / 0.07 s / 9.3 s | 42.1 s / 0.06 s / 8.5 s |
| server with 2x the cores, no worker | 43.7 s / 0.07 s / 8.3 s | 40.3 s / 0.07 s / 8.6 s |
| server with 3x the cores, no worker | 41.2 s / 0.06 s / 8.6 s | 40.6 s / 0.07 s / 8.8 s |

- **At the final build**: with 4 cores per machine a warm worker takes
  5.8 s off a cold build where four more local cores take 10.7 s (54%);
  with 8 cores, 1.7 s of 3.3 s (52%). A worker that has seen nothing
  helps at 4 cores (2.8 s) and costs 1.1 s at 8. After an edit the
  rebuild is 0.6 s slower with a worker at 4 cores (9.5 s against
  8.9 s) and within 0.3 s at 8. The swarm with a worker took 150.6 s
  against 176.0 s alone and 116.5 s on 16 local cores (43% of what the
  cores are worth), 140.4 s in another run.
- **The swarm**: a worker now takes 36 s off 176 s where eight more
  local cores take 54 s: two thirds of what the cores are worth, where
  it was nothing (182.8 s against 179.8 s). In the second run 286
  workspace compiles ran on the worker (72 first compiles, 214 from the
  state it held), 5.8 GB of state were left there, 0.2 GB came home in
  the background and 1,362 files were fetched because a compile on the
  server needed them. (The server CPU column does not include the
  worker's.)
- **A cold build, 4 cores per machine**: a worker that has seen nothing
  now helps (51.9 s against 56.5 s; before it made no difference), a
  warm one gets 7.7 s of the 12.0 s that four more local cores give
  (64%; was 63%). The rebuild after an edit is unchanged.
- **Failed steps with a worker: found and fixed (D61).** One step
  failed in the first swarm run with a worker and five in the second,
  none without a worker; in the third, 18 actions were handed back to
  the server. Logging why the server refused a content showed the
  cause. When an action goes to a worker the server sends ahead the
  answers it expects the action to need; for a file on the server's own
  disk that it had hashed before (toolchain, registry sources) the
  answer named the content, but where that content could be read from
  was only remembered for files the action was predicted to read, not
  for the others. When the action opened one of those, the worker asked
  for the content by its hash, the server had no record of it (161 and
  190 times in two diagnostic runs), and the compiler on the worker
  failed on the file ("required to be available in rlib format", a
  build script that would not compile). A second fault sat behind it:
  the messages of a pushed content can overtake each other, and the
  worker put the content together when the message sent last arrived,
  with the recipe of whichever came first, so some pushed contents
  failed their check. After both fixes a diagnostic swarm run had no
  refused content, no failed assembly and no worker failure
  (140.4 s), and neither did the runs at the final build below. This
  is very likely also what the unexplained failed step in D31's swarm
  was.

Read against the criterion (a worker shortens a cold build roughly in
proportion to the cores it adds; a warm build is no slower with one):

- **Cold, 4 cores per machine, warm worker**: 6.8 s shorter at the final
  commit, where four more local cores give 10.8 s: 63% of what the cores
  are worth (66% in the series before it: 7.8 s of 11.9 s). Before this
  round it was 26% (53.1, 50.2 and 41.8 s). A second worker adds 0.5 s,
  and four more local cores beyond eight add 2.7 s: the build has little
  more width to give (below).
- **Cold, 8 cores per machine**: doubling the server's cores is worth
  2.1 s; the worker gets 0.8 s. There is nothing here to be in proportion
  to.
- **A worker that has never seen the toolchain** made the build slower in
  the first series, by 3.6 s at 4 cores and 3.9 s at 8: it was given
  compiles before it had a compiler, and asked for the toolchain one file
  at a time. At the final commit it is sent the toolchain once and given
  work when the link has drained, and no longer costs anything (55.8 s
  against 56.0 s); it does not help in its first build either.
- **Nothing changed** (0.07 s) is unaffected.
- **After an edit, with a warm worker at 4 cores**: slower in the first
  series, 16.5 s against 10.9 s. The crates the worker compiled during
  the cold build left no incremental state on the server (D31), so their
  first rebuild was a compile from nothing. At the final commit workspace
  crates are compiled on the server and the rebuild takes 11.1 s against
  11.3 s. With that the second half of the criterion holds: nothing
  changed and after an edit, a build is no slower with a worker.
- **The swarm is not shortened** (182.8 s against 179.8 s; 16 local cores
  give 126.4 s). Of 1,476 compiles, 871 were of crates with incremental
  state on the server and stayed there; the 250 that went to the worker
  cost 1.76 s each beyond their run time, mostly waiting for the metadata
  of crates an agent had just changed.

Why a cold build stops at two thirds, as far as measured:

- `--timings` reports of the same build on 4, 8 and 24 cores and with a
  worker show where the time is. From the moment `hir-def` starts, the
  build is one chain (`hir-def`, `hir-ty`, `hir`, `ide-db`, `ide-assists`,
  `ide`, `rust-analyzer`) that takes 32.2 s on 4 cores, 31.8 s with a
  worker and 30.5 s on 8 cores: no configuration shortens it. Everything
  cores buy is in the wide phase before it: `hir-def` starts at 22.4 s on
  4 cores, 12.7 s on 8, and 17.4 s with a warm worker before D54's
  changes, 14.9 s after.
- The whole build is 152 to 173 CPU-seconds of compiling in 314 units, of
  which about 100 are serial compiler front ends. `dev/sim-workers.py`
  replays the measured units on other machine shapes with a free network
  and whole units pinned to one machine: 46.2 s for 4 cores, 40.7 s for 8,
  41.8 s for 4+4, 39.3 s for 16, 39.8 s for 8+8. It agrees with the
  measurements at 8 and 24 cores to within 6% and is 8 s too optimistic at
  4. About 3 s of that is the server's own work, which shares the four
  cores with the compilers (12 CPU-seconds per build, see Tracing
  overhead); the rest is the model, which lets code generation use every
  free core at once and treats front ends as unaffected by their
  neighbours. Its answer to "is whole-unit dispatch the limit?" is no:
  with a free network 4+4 would be within about 1 s of 8 cores. It is a
  model that is known to be optimistic where cores are scarce, so that
  answer is an indication, not a proof.
- What is left between 49 s and 45 s is therefore most likely cost, not
  structure: the server spends part of its four cores feeding the worker
  and fetching 300 to 480 MiB of outputs back; a remote compile costs
  0.16 to 0.45 s beyond its run time on units that mostly run for under a
  second; and about 2,300 questions per build still cross the link one
  round trip at a time (files a crate includes that are not Rust sources,
  the first compile of each kind).

What was done in this round, each from a measurement (D47, D53, D54):
queued actions take the first free core, local or worker, with saturation
judged by free jobserver tokens; hedged copies race and the loser is
stopped; a hedge only takes a core no compiler thread wants; a stopped or
cancelled compile is killed on the worker too (it used to keep its core
while the server counted it free), and the jobserver tokens of a killed
compiler are returned; dependencies named on the command line, the
crate's source tree and, for linking compiles, the toolchain's libraries
travel ahead of the action (questions per build 6,100 to 2,300, per-action
overhead of a traced run 0.05 s); crates with incremental state on the
server stay there; the transfer guard counts what is queued on the link;
a worker without the toolchain is sent it once instead of being given
work it cannot start.

**D65, measured** (2026-11-24, second rig, history benchmark paused;
one run each):

| rust-analyzer `build`, 4 cores per machine | cold | again | after a one-file edit |
|---|---|---|---|
| server alone | 55.1 s | 0.06 s | 9.4 s |
| + 1 worker that has seen nothing | 51.2 s | 0.06 s | 9.9 s |
| + 1 warm worker | 47.7 s | 0.06 s | 9.6 s |
| + 2 warm workers | 46.0 s | 0.06 s | 9.3 s |
| server with 8 cores | 43.4 s | 0.06 s | 8.3 s |
| server with 12 cores | 41.2 s | 0.07 s | 8.4 s |

| 20-agent rust-analyzer swarm, 8 cores per machine | wall | server CPU |
|---|---|---|
| server alone | 178.9 s | 1,281 s |
| + 1 worker, states sent ahead | 160.6 s | 1,032 s |
| + 1 worker, states not sent ahead | 154.7 s | 960 s |
| server with 16 cores | 127.9 s | 1,476 s |

- A warm worker is worth 7.4 s of the 11.7 s that four more local cores
  give: 63%, where it was (63% and 66% in the two series before). D65
  did not move the cold build. A worker that has seen nothing now helps
  in its first build (51.2 s against 55.1 s; it used to cost nothing
  and gain nothing).
- **Sending states ahead does not pay and is now off unless asked for**
  (`RARGO_STATE_AHEAD=1`). It did what it was for: compiles kept on the
  server for lack of a state on the worker fell from 500 to 85. But it
  put 5.5 GiB of states on the link in one swarm, and the scheduler
  then kept 360 actions local because the transfer was judged too
  costly (231 without) and 278 because a local core came free first
  (78 without). The swarm took 160.6 s with it and 154.7 s without.
- Contents behind questions stays: the overhead per remote action in
  the cold series is 0.12 to 0.17 s (0.16 to 0.45 s before). Its effect
  on wall time is not separable from run-to-run differences: the swarm
  with a worker was 150.6 s at `fd599de` and is 154.7 s now, and the
  16-core server alone was 116.5 s then and 127.9 s now, so sessions
  differ by more than the change could be worth.
- In the swarm a worker gives 24 s of the 51 s that eight more local
  cores give (47%).

Not tried: letting workers fetch registry crates themselves instead of
through the server. The profile of the remaining gap (where the
worker's cores idle during the wide phase) is still owed.

Known weaknesses besides the above: flake-rooted and host-network actions
are never sent to workers; a worker is trusted like the server (it sees
sources and produces results that are cached).

## Phase 5

What each item of the design's phase 5 became, and the measurement behind
it (decisions D22 to D24). The swarm, the replay and the single-client
figures were measured again at `a92e359`; where a figure is from the
first implementation it says so.

- **Metadata stable under body-only edits.** rustc 1.100 fork, patch 1. On
  a three-function library, stock 1.100 changed the `.rmeta` on every edit
  tried; with the fork, edits that keep line numbers of later code (a
  changed literal, a longer line, an edit in the last function) keep the
  crate hash, and rargo observes fork metadata by crate hash, so dependents
  are not rerun. Adding lines inside a body still changes the hash, by
  design (line numbers of inlined and generic code end up in dependents).
  A dependent compiled against the old and the new `.rmeta` produced
  byte-identical metadata; a diagnostic pointing into the edited crate
  showed the right line.
  **On real history it buys nothing.** Replaying the last 30 rust-analyzer
  commits (`dev/replay.py`, `rargo build -p rust-analyzer` at each, fresh
  store per compiler): after the first commit, 351 actions with the fork
  against 352 with stock 1.100.0. Real commits add or remove lines, which
  changes the hash by design. With the fork's slower (non-PGO) compiler the
  29 later commits took 665 s against 391 s, and the first 59.9 s against
  40.7 s. (Stock 1.100.0 again with the client daemon, 100 ms of
  round-trip time on the client link: first commit 40.8 s, the 29 later
  ones 350.7 s, 352 actions, none failed; on the LAN link 39.8 s and
  351.3 s.) In the favourable case, the 20-agent swarm with body-only second
  edits, it still loses: 146.6 s wall and 1,789 CPU-s against 114.9 s and
  1,492 CPU-s for stock 1.100.0 under rargo.
- **Separately cached metadata and codegen.** `check` with the fork runs the
  build's own compiler commands with `-Zno-codegen` (byte-identical
  metadata to the full build, verified), so a check after a build or test
  is free: on rust-analyzer 0.04 s instead of a 32.6 s cold check. The
  first check of a crate costs more (47.5 s against 32.6 s cold), and a
  build after a check gains little (56.8 s against 61.5 s). In the swarm
  it loses (166.5 s, 52.7 GB against 146.6 s, 39.5 GB with cargo-style
  check), so it is off by default and opt-in with
  `RARGO_CHECK_AS_BUILD=1`.
- **Per-codegen-unit object caching.** Not a separate mechanism: rustc's
  incremental work products plus the file-level store already keep each
  object file once (2.6 GiB logical, 0.23 GiB stored in the Ruff swarm). The
  fork's patch 2 makes incrementally built rlibs reproducible (member names
  without the per-session random string), so an unchanged library rebuilt
  from scratch no longer relinks its dependents.
- **Incremental state that deduplicates between snapshots.** Done in the
  store instead of the compiler: the cold tier. The Ruff swarm store's
  21.8 GiB of incremental state is 6.9 GiB as compressed chunks; the whole
  store went from 28.8 GiB to 3.9 GiB.
- **Persistent in-memory cargo graph.** Not built. Planning is itself a
  cached action: a no-op `build` of rust-analyzer takes 0.02 s end to end,
  and the plan is decoded once per distinct plan (`plans decoded` in the
  status). A resident graph would save the planner run after edits that
  change a manifest, which the benchmarks do not do.

Fork compiler cost: it is built without profile-guided optimisation. Under
rargo a cold `check` of rust-analyzer takes 32.6 s and 105 CPU-seconds
with it against 24.5 s and 78 with the official 1.100.0, a cold `build`
61.5 s and 211 CPU-seconds against 42.1 s and 170. Fork measurements compare the fork with itself or say
which compiler they used.

## Incidents

Two things I did wrong on the shared machine while doing this work. Both
are mine, not rargo's.

- **`/dev/null` was a regular file from about 10:11 to 10:22 on
  2026-11-23.** I probed the compiler with `rustc ... -o /dev/null` as
  root; rustc writes its output to a temporary and renames it over the
  target, which replaced the device node. For those eleven minutes every
  new process on the machine that read its standard input from
  `/dev/null` read file contents instead, and writes to it went to a
  file. I noticed through its effect on my own runs (stock cargo's
  `rustc -` probe failing; one failed step in a stock Zed run of ten
  agents, which is not used above) and restored the node
  (`mknod -m 666 ... c 1 3`, renamed into place). Services started before
  10:11 kept the real device open and were not affected; anything started
  in that window may have been. Nothing was restarted.
- **Two stray entries at the file system root**, `/slowcancel` (a
  directory with three small files) and `/cancel.log`, created the same
  day by a shell snippet that ran with unset variables. Removing them
  needs a confirmation I could not give myself; they are harmless and can
  be deleted. The 9-byte `/.gitignore` from an earlier session is in the
  same state.

In the round of 2026-11-23 evening (client daemon, output directory)
nothing outside `/root/rargo-work` and this repository was touched.
Within them: benchmark runs that I stopped by killing their drivers left
dead FUSE mounts on twenty `target/` directories under
`/root/rargo-work/swarm`, which I unmounted by hand; a benchmark script
I edited while it was running failed at its last (cleanup) lines after
its results were written; and the rig's network namespaces, `tc` rules
and UDP ports (7459 inside the namespaces, 7481 and 7999 on loopback
for two short tests) were mine. No daemon, namespace or mount of mine
is left running.

## Bugs found by running real builds (kept here because they say what to
## distrust)

- Passthrough backing ids were registered twice by concurrent first opens
  of one file; closing the duplicate let the kernel reuse the id while a
  reply still referred to it, and a compiler read the wrong file. Fixed:
  registration is once per file, closes are deferred.
- A failed compile caused by the above was cached and replayed. Fixed by
  never recording failures (DECISIONS D8).
- Linked units started before the object code of indirect dependencies was
  finished. Fixed by waiting for all transitive dependencies.
- A pipelined dependency's early `.rmeta` became unreachable once its final
  manifest listed its output directory. Fixed in the output layer.
- A view file truncated by an action was recorded with size 0, so that
  action never hit the cache again. Fixed.
- `rargo doc` never placed documentation into `target/doc`, and rustdoc's
  cross-crate files would have covered one crate each. Found by a new e2e
  check; fixed by chaining rustdoc runs per output directory (D25).
- Actions were moved into their cgroup by the parent after the fork, so a
  fast-starting action's CPU time was mostly accounted elsewhere. Fixed:
  the child moves itself before it forks anything.

- (Remote first.) Build scripts that ask git for the commit saw no
  repository once the worktree was no longer on the server's disk:
  rust-analyzer warned and would have shipped a version string without a
  commit. Found by the first remote build of it; fixed with the per-commit
  stand-in (D36).
- (Remote first.) Each step that produced something for the client cost
  its own round trip, and cargo's `.d` file for rust-analyzer (hundreds of
  kilobytes of paths) went uncompressed over a connection still in slow
  start; a no-op build over a 40 ms link took 0.33 s. Fixed: one delivery
  per command and compressed control messages (0.17 s).

## Store on XFS

With `mkfs.xfs` unavailable, the reflink check used a 20 GiB loopback btrfs
image. All 81 e2e checks passed on btrfs and on ext4, and the core suite
passed 53 tests with one ignored. For `build` and `test --no-run` in two
worktrees, `df` grew by 56.09 MiB on btrfs versus 102.95 MiB on ext4.
Aggregate `du` counts were 122.69 MiB and 104.83 MiB respectively.
`filefrag` confirmed shared extents on btrfs and independent copies when
the store was on btrfs and the worktrees were on ext4. The btrfs image
was unmounted and retained (D33).

That was the first implementation, where worktrees could share the store's
file system. The server's store can still be on a reflink file system
(`--store`); worktrees are now on other machines, and it is the client's
cache that shares artifacts between them.

## Gate

On `08bf0d3`, three gates in a row on this machine each pass every suite.
They take 127s, 104s and 103s. The scripts the gate and CI run do not wait
on time.

The gate also fails when it is slower than the budget in `dev/gate-budget`
(150s total; the file also holds each suite's last recorded time, so the
failure message says which suite grew). This ratchet is the one check in
the gate that depends on how long something took, by the owner's choice.
`dev/gate-budget.sh` refuses a higher total than `origin/main` has unless
a commit since says `Gate-budget-raise: <why>`. Not yet run in a full gate.

## Nix store trees (issue 116)

On the worker suite, run back to back on this machine, a worker asks the
server for 803 lookups under `/nix/store` in the wide phase and 624 in
the one-core phase. With each closure's trees on the worker, both phases
ask for 0. The suite takes 23s and 29s. An earlier wide run of the same
questioning server asked for 1411, and that one-core run asked for 624;
the suite took 25s.

Hashing the suite's closure on its one-core server takes 4898 ms and
5034 ms: 86844 files, 2493189332 bytes. That hash is the difference
between the two suite times. A later resolve of the same store paths
hashes nothing. The flake closure in `e2e-flake`, hashed while those
pages were already cached, is 8760 files, 2242090134 bytes, 160 ms.

A worker whose `/nix/store` holds only the libraries `rargod` itself is
linked against builds the flake. One action runs there, and it asks for
nothing under `/nix/store`. Two closures that share a store path share
its tree (`a_shared_store_path_is_manifested_once`): the second hashes
only the path the first did not have.
