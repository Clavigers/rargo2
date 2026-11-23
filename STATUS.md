# Status

What exists, how it was checked, what was measured, and what is weak or
missing. Machine: 32 threads, 124 GB RAM, kernel 7.0, ext4. All builds are
capped at 24 cores (`CPUQuota=2400%`, 24 scheduler tokens).

## Summary (updated 2026-11-23, after the move to remote first)

rargo was first built with the server on the client's machine. That was not
what was asked for: clients were meant to be on other machines and do
nothing but sync files and environment, with all analysis, scheduling and
building on a remote server. It has now been rebuilt that way (DESIGN.md,
DECISIONS D34 to D43). The engine (planner, sandbox, trace cache,
scheduler, shared incremental compilation, store, workers) is the same
code; the boundary between client and server is new.

What a client does now: scan its worktree into a content-addressed tree,
upload what the server lacks, pass on an allow-listed environment, receive
output, the artifacts it asked for and files the build wrote into the
worktree. What it no longer needs: a daemon, root, a toolchain, the
registry, or the server's file system.

Measured with the clients in their own network namespace, one shaped link
(1 ms each way, 1 Gbit) from a server that cannot see their files:

- **20 agents in 20 worktrees** against stock cargo with one target
  directory per worktree: rust-analyzer 3.0x less wall time, 5.2x less CPU;
  Ruff 5.5x and 7.3x; Bevy 4.6x and 8.2x. No failed steps. On the client
  machine: 11.8 GB, 2.7 GB and 2.1 GB of artifacts instead of 100 GB, 95 GB
  and 61 GB, and 29 s, 27 s and 14 s of CPU time for all twenty clients
  together; the server's store holds 43 GB, 33 GB and 25 GB.
- **One client, rust-analyzer**: nothing changed, 0.04 s (stock cargo
  0.15 s); a fresh worktree on another machine at a commit the server has
  built, 0.9 s for a ready binary; the first sync of the repository
  uploads 21.5 MiB and a cold check takes 24.1 s against stock cargo's
  22.2 s locally. Over a slow link (20 ms each way, 100 Mbit): 0.12 s,
  8.7 s and 24.8 s.
- **No drift**, client side: after rargo built and tested them through the
  server, rust-analyzer, Ruff and Bevy build and test with stock cargo in
  the same worktree and `target/`; see No-drift for the run.

Tests at the last commit: `cargo test` 8 (client) + 7 (protocol) + 47 unit
and 6 engine (server); `dev/e2e.sh` 104 checks with the server in a mount
namespace where the client's directory is empty; `dev/e2e-flake.sh` 14
checks (two servers, a registry dependency fetched by the servers). The
swarm and no-drift runs below were made before the last two protocol
changes (one delivery round trip per command, compressed control
messages); the single-client table and both e2e scripts were run after.

Not met, not done, or not re-measured since the change:

- Phase 4 (a worker shortens a cold build in proportion) was not met
  before and is not now; see Workers. The worker rig was brought up once
  with the new server and a remote client (smoke test below); its
  benchmark series was not repeated.
- The rustc-fork measurements (Phase 5) were not repeated; nothing in them
  depends on where the client is.
- The client was only run on Linux x86_64.
- Open from before: `--timings` is not reproduced; the worker link is
  authenticated but not encrypted (the client link is both).

| Area | State |
|---|---|
| Client: worktree scan with index, content-addressed tree | done, unit-tested, e2e |
| Sync: manifests and contents the server lacks, shared uploads | done, unit-tested (server), e2e (counts of files and manifests asked) |
| Stand-in for `.git` (HEAD, branch, dirty or not; no history, no contents) | done, unit-tested against real git, e2e |
| Environment: allow-list, ask-on-declare, client cargo configuration | done, e2e (declared variable, policy file, a `config.toml` in a cargo home the server cannot see); `credentials.toml` travels the same way but no private registry was tried |
| Transport: TCP, Noise `NNpsk0`, per-user keys, admin users | done, unit-tested, e2e (wrong key, unknown user, non-admin) |
| Results: artifacts by hash, client cache shared between worktrees, worktree writes | done, unit-tested, e2e |
| Fetching and toolchain installation on the server | done; fetch exercised by e2e-flake, toolchain installation checked once by hand (see No-drift) |
| Drift check on the server; no-drift on the client | done; see No-drift |
| FUSE server with passthrough (`fusewire`) | done, tested |
| Content store, GC, LRU budget, cold tier | done, tested |
| Traced sandbox filesystem with copy-on-write upper layer | done, tested |
| Process sandbox (zygote, namespaces, init), per-action cgroups | done, tested |
| Cargo fork: planning mode, parse service | done, exercised on rust-analyzer, Ruff, Bevy |
| Trace cache, in-flight dedup, declared env vars | done, tested |
| Scheduler: priorities, memory admission, shared jobserver | done, unit-tested |
| `rargo build / check / test / run / clippy / doc`, JSON messages | done, e2e |
| Shared incremental compilation | done, e2e, in the 20-agent benchmark |
| Per-repo policy file (`.rargo.toml`) | done, e2e (env, test network, sync excludes, git switch, malformed file) |
| Flake-restricted sandboxes, cache export/import between servers | done, e2e-flake |
| Workers | as before; smoke-tested with the new server |
| `cargo` shim | done, e2e (routing, passthrough, fallback without a server) |
| rustc fork (phase 5), `check` as a build | as before, not re-measured |

## Measurements

### One remote client (`dev/bench-remote.sh ra`)

rust-analyzer (commit 37721d1), server with 24 cores and a fresh store per
column, client in its own network namespace. "Stock cargo" is cargo 1.100
on the client's files with the same 24 cores, no network involved.

| | stock cargo, local | rargo, 1 ms each way, 1 Gbit | rargo, 20 ms each way, 100 Mbit |
|---|---|---|---|
| first `check` (rargo: includes the first sync, 21.5 MiB) | 22.2 s | 24.1 s | 24.8 s |
| `check`, nothing changed | 0.15 s | 0.04 s | 0.12 s |
| `check` after a one-file edit | 5.9 s | 6.1 s | 6.3 s |
| `build -p rust-analyzer` after that (rargo: binary comes over the link) | 38.3 s | 40.2 s | 47.6 s |
| the same build, nothing changed | 0.15 s | 0.05 s | 0.17 s |
| the same build from a fresh worktree on a second machine | a full build | 0.9 s | 8.7 s |
| in the worktree's `target/` afterwards | 4,607 MiB | 565 MiB | 565 MiB |

The first sync asked the client for 263 directory manifests and 2,333
files; the second machine's worktree for 4 manifests and 1 file. One run
per figure; across three runs of this table during the work the cold
figures moved by up to 0.7 s. On the slow link a command that changes
nothing costs a few 40 ms round trips: connect, request and answer, and
for a build one more to agree that the artifacts are already in place.

A single cold build is not faster than local cargo: it is the same
compiler on the same number of cores plus a link. What the client gets is
its own cores back, a 565 MiB `target/`, and everything anyone has already
built.

### 20 agents in 20 worktrees (`dev/bench-swarm.sh`)

Each agent: check; edit a file; check; test one package; edit another file;
check; test another package; build the main binary; test again. Agents
overlap (several make the same edit to the same file). The rargo agents are
20 clients in one network namespace, 1 ms each way and 1 Gbit from the
server, sharing one client cache; the server cannot see their worktrees.
Stock cargo gets one target directory per worktree and its default `-j`;
both sides are capped at 24 cores. Server CPU is the cgroup's `cpu.stat`;
client CPU is the summed process times of the `rargo` commands.

| rust-analyzer | stock cargo | rargo, remote clients | ratio |
|---|---|---|---|
| wall time | 374.5 s | 125.5 s | 3.0x |
| CPU time | 8,680 s | 1,641 s on the server + 29 s on the client | 5.2x |
| disk on the client machine | 100.1 GB | 11.8 GB | 8.5x |
| disk on the server | | 42.6 GB (40.8 GB after GC) | |
| failed steps | 0 | 0 | |
| mean step times: check / check after edit / test / check after 2nd edit / test / build / test | 109 / 13.1 / 7.9 / 31.4 / 16.3 / 173 / 1.0 s | 24.6 / 8.6 / 1.5 / 9.4 / 0.9 / 73.8 / 0.4 s | |
| crossing the link | | 21.7 MiB up (one upload of the repository; 19 clients waited for it), 1,820 MiB of results down | |

| Ruff | stock cargo (earlier run) | rargo, remote clients | ratio |
|---|---|---|---|
| wall time | 818 s | 149.3 s | 5.5x |
| CPU time | 19,385 s | 2,643 s + 27 s | 7.3x |
| disk on the client machine | 94.9 GB | 2.7 GB | |
| disk on the server | | 32.9 GB (30.2 GB after GC) | |
| failed steps | 0 | 0 | |
| mean step times (same order) | 317 / 22.2 / 5.8 / 77.6 / 22.9 / 315 / 2.0 s | 35.7 / 14.7 / 1.0 / 17.5 / 1.5 / 72.3 / 0.3 s | |
| crossing the link | | 71.7 MiB up, 604 MiB down | |

| Bevy | stock cargo (earlier run) | rargo, remote clients | ratio |
|---|---|---|---|
| wall time | 400 s | 87.4 s | 4.6x |
| CPU time | 9,112 s | 1,108 s + 14 s | 8.2x |
| disk on the client machine | 61.0 GB | 2.1 GB | |
| disk on the server | | 25.0 GB (21.2 GB after GC) | |
| failed steps | 0 | 0 | |
| mean step times (same order) | 284 / 22.8 / 10.8 / 27.9 / 9.2 / 30.2 / 0.9 s | 34.8 / 15.1 / 4.7 / 11.3 / 0.8 / 16.6 / 0.4 s | |
| crossing the link | | 84.2 MiB up, 362 MiB down | |

The stock cargo figures for Ruff and Bevy are from the run before the
change (same machine, same cap; nothing about stock cargo changed); for
rust-analyzer stock cargo was run again alongside.

Against the first implementation, where client and server shared a disk
(117 s, 146 s and 83 s wall), the link costs 4 to 9 s on these runs. The
rust-analyzer agents each end with a different 565 MiB binary, so the
client side is twenty binaries; where agents build the same thing the
client cache holds it once.

Most of the server's store is incremental state (about 23 GB of the
42.6 GB for rust-analyzer). Idle blobs go to the cold tier as chunks: the
Ruff store of the earlier run went from 28.8 GiB to 3.9 GiB.

### Tracing overhead

- A whole `cargo build` of this repository (105 crates) run as one process
  tree inside one traced sandbox: 8.4 to 8.5 s against 8.0 s native (about
  5% wall, under 1% user CPU, 16% more system time). This is the worst case
  for lock contention, since all 24 compilers share one action.
- rust-analyzer cold `check`, one sandbox per compiler invocation: faster
  than stock cargo in absolute terms (see above); the tracing share has not
  been isolated yet.

This was measured with the first implementation. The sandbox and its file
system are unchanged.

### Small synthetic workspace (`dev/e2e.sh`, 104 checks, all passing)

The server runs in a mount namespace in which the client's directory is an
empty file system, with its own cargo home, and requires a key.

- A wrong key, an unknown user and a non-administrator asking for garbage
  collection are refused.
- Second worktree at the same commit: zero actions; one file (the `HEAD`
  naming its branch) and at most four manifests uploaded; the artifact is
  the same inode as the first worktree's (or a reflink).
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

## No-drift

The server-side half and the client-side half (D42), run through a fresh
server that cannot see the repositories (`dev/nodrift-all.sh`): rargo build
and test; the server's drift check (stock cargo against rargo re-running
every cached unit, on the synced state); tracked files untouched; stock
cargo build and test in the same worktree and `target/`; rargo again.

| | rust-analyzer | Ruff | Bevy |
|---|---|---|---|
| rargo build / test | ok / ok (full suite) | ok / ok (`ruff_linter`, `ruff_python_parser`, `ruff_text_size`) | ok / ok (`bevy_ecs`, `bevy_math`, `bevy_color`) |
| drift check on the server | no drift (90 s) | no drift (231 s) | no drift (178 s) |
| tracked files untouched | ok | ok | ok |
| stock cargo build and test, same `target/` | ok | ok | ok |
| rargo again afterwards | ok | ok | ok |
| failures | 0 | 0 | 0 |

In that run the worktrees' `target/` directories still held stock cargo's
artifacts from earlier runs, so stock cargo had nothing to rebuild (0 s).
For that reason one more run was made in a fresh rust-analyzer worktree:
`rargo build -p rust-analyzer` (40.1 s) and `rargo test -p stdx --no-run`
left 6 files, 575 MB, in `target/`; stock `cargo build -p rust-analyzer`
then did its full cold build into the same directory (38.1 s) and
`cargo test -p stdx` passed; rargo build and test afterwards took 0.1 s and
0.2 s; `git status` was clean. Ruff and Bevy were not repeated that way.

Bevy leaves an untracked `Cargo.lock` (Bevy ignores it; stock cargo writes
it as well).

A difference stock cargo would not have, found here and fixed: the
server-built `rust-analyzer --version` said `37721d1` where stock cargo's
says `37721d141`, because git abbreviates by repository size and the
stand-in for `.git` is tiny. The stand-in now carries the client
repository's abbreviation length; a server-built binary prints
`rust-analyzer 0.0.0 (37721d141 2026-11-22)`, as stock cargo's does.

Also checked once by hand: a project whose `rust-toolchain.toml` names a
toolchain the server did not have (1.98.0) was built and run through the
server in 5.7 s; the server installed the toolchain, the client has no
part in it.

## Mechanisms

### Sync
`rargo/src/scan.rs` (client), `rargo-core/src/snapshot.rs` and
`session.rs` (server). Verified by: unit tests on both sides (scan skips
and reuse, sync asks only for what is missing, evicted content is asked for
again, a client that withholds content fails the sync, a file that changed
since the scan restarts the command), e2e counts.
Measured: scanning a rust-analyzer worktree (2,386 files) with a warm index
takes 2.7 to 3.2 ms.
Known weaknesses: every command scans the whole worktree (one `lstat` per
file), which will show on very large trees; there is no watcher. A large
git-ignored directory is hashed and uploaded once unless the policy file
excludes it. The whole tree must be in the server's store before the build
starts. Symlinks that point outside the worktree dangle on the server.
Sparse checkouts are synced as they are on disk.

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
`rargo-proto/src/secure.rs`, `rargo-core/src/session.rs`, `serve.rs`.
Known weaknesses: every user shares one cache and could read another's
source through it by guessing nothing more than a build command, which is
the intended trust model, not an accident; the request in the first
handshake message is not forward secret and could be replayed by someone
who recorded it (it would start a build of an already synced tree and
deliver nothing to the attacker); keys are plain lines in a file, with no
rotation or revocation other than editing it and restarting.

### Results on the client
`rargo/src/place.rs`. Verified by unit tests (sharing, own copies, parts,
forged content refused) and e2e (stock cargo in the same `target/`).
Known weaknesses: without reflinks, shared artifacts are read-only hard
links into the client's cache (as cargo's own are links into `deps/`); a
client killed while placing files can leave `.rargo-tmp-*` files in
`target/`; a client that disconnects does not stop actions already running
for it (their results are cached).

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
Known weaknesses: `--timings` is not produced; cargo's
future-incompatibility report file is not written (the warning is shown);
example scraping failures are treated as hard errors; the fork is pinned to
one cargo release.

### Trace cache, engine, scheduler
`rargo-core/src/{trace,engine,sched}.rs`. Only successes are recorded.
Verified by `tests/engine.rs` (six tests with real sandboxed processes) and
four scheduler unit tests. Not verified under a real memory-bound workload.

### Shared incremental compilation
`rargo-core/src/incr.rs`. States are the largest thing in the store.
Seeds are chosen with the commits the client names (D43). Remote (worker)
compiles run without a seed.

### Verify mode and drift check
`RARGO_VERIFY=1`, `rargo --rargo-drift-check`, background every six hours
on the server for each worktree's newest synced state (D42).
Finding from before: rustc's incremental mode is not byte-reproducible;
verify compares such rlibs by member content.

### Workers
`rargo-core/src/{net,chunk,remote}.rs`, `dev/netns.sh`, `dev/bench-worker.sh`,
`dev/bench-worker-swarm.sh`, `examples/netbench.rs`. Server and workers in
their own network namespaces; every link 1 ms each way, 0.1% loss, 1 Gbit
(`tc netem`); each daemon capped at its own core count. Links are
authenticated with a shared key (D26).

**The phase 4 criterion is not met.** What was measured (D30, D31):

| rust-analyzer cold `build` | 4 cores per machine | 8 cores per machine |
|---|---|---|
| server alone | 53.1 s | 41.9 s |
| + 1 fresh worker | 56.2 s | 44.0 s (241.9 s before hedging) |
| + 1 warm worker | 50.2 s | 43.2 s (51.8 s before the fixes) |
| + 2 warm workers | 51.5 s | 46.6 s |
| server with 2x the cores, no worker | 41.8 s | 40.6 s |
| server with 3x the cores, no worker | 41.1 s | |

| 20-agent rust-analyzer swarm | wall |
|---|---|
| server 8 cores | 171.1 s, 171.0 s |
| server 8 + worker 8 | 159.6, 173.8, 172.4, 175.5 s (one failed step in the first, not reproduced) |
| server 16 cores | 116.4 s, 115.4 s |

Warm, no-op and one-edit builds are no slower with workers (no-op 0.06 s
in every configuration; one-edit rebuild 8.8 to 10.2 s against 9.0 to
9.8 s), so that half of the criterion holds. A cold build is not shortened
in proportion. Why, as far as measured: past about 8 cores a cold build of
rust-analyzer or Bevy is bound by its critical path, and what extra local
cores buy there is code-generation parallelism inside the crates on that
path, which a worker cannot provide; in the swarm, each remote compile
needs the freshly changed metadata of many crates, which at 1 Gbit costs
about as much as waiting for a local core, and the dispatcher correctly
keeps most work local.

What the measurements fixed along the way: transport 18.8 to 72 MiB/s
(fast retransmit, gentler loss response); hedging (a dispatched action runs
locally too when a local core falls idle before the worker has said
anything), which bounds a worker's harm; compressed messages (fetch time
halved); a cost model that learns each worker's overhead and the link's
bandwidth; remote compiles without incremental state.

Known weaknesses: a cold worker first pulls the toolchain (about 300 MB);
flake-rooted and host-network actions are never sent to workers; the link
is authenticated but not encrypted.

All of the above was measured with the first implementation, with the
client on the server's machine. Since the change the rig
(`dev/netns.sh`) also has a client machine; one smoke test was run on it with the new server: a
client in its namespace built rust-analyzer through an 8-core server with
one 8-core worker (1 ms each way, 0.1% loss, 1 Gbit on both links) in
43.6 s, 10 actions were dispatched to the worker (all of them hedged
locally as well), no worker failure. The benchmark series above was not
repeated.

## Phase 5

What each item of the design's phase 5 became, and the measurement behind
it (decisions D22 to D24).

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
  store per compiler): after the first commit, 350 actions with the fork
  against 351 with stock 1.100.0. Real commits add or remove lines, which
  changes the hash by design. With the fork's slower (non-PGO) compiler the
  replay took 634 s against 375 s. In the favourable case, the 20-agent
  swarm with body-only second edits, it still loses: 139.3 s wall and
  1,749 CPU-s against 109.1 s and 1,455 CPU-s for stock 1.100.0 under
  rargo.
- **Separately cached metadata and codegen.** `check` with the fork runs the
  build's own compiler commands with `-Zno-codegen` (byte-identical
  metadata to the full build, verified), so a check after a build or test
  is free: on rust-analyzer 0.03 s instead of a 31 s cold check. The first
  check of a crate costs more (47 s against 31 s cold), and a build after a
  check gains little (55 s against 58 s). In the swarm it loses (166.8 s,
  54.2 GB against 139.3 s, 39.4 GB with cargo-style check), so it is off by
  default and opt-in with `RARGO_CHECK_AS_BUILD=1`.
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

Fork compiler cost: it is built without profile-guided optimisation, and
`cargo check` of rust-analyzer takes 91 CPU-seconds with it against 64 with
the official 1.100.0. Fork measurements compare the fork with itself or say
which compiler they used.

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
