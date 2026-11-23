# Status

What exists, how it was checked, what was measured, and what is weak or
missing. Updated as the run proceeds; the newest state is at the top of each
section. Machine: 32 threads, 124 GB RAM, kernel 7.0, ext4. All builds are
capped at 24 cores (`CPUQuota=2400%`, `-j24` / 24 scheduler tokens).

## Summary (final report; updated 2026-11-23)

All phases (1 to 5) are implemented and tested. Measured against stock
cargo with one target directory per worktree:

- **20 agents in 20 worktrees** (24 cores): rust-analyzer 3.3x less wall
  time, 5.3x less CPU, 2.4x less disk; Ruff 5.6x / 7.2x / 2.9x; Bevy
  4.8x / 8.4x / 2.5x. No failed steps.
- **Phase 1** (second worktree ~1 s, no extra disk; one edit rebuilds only
  dependents): met (e2e). **Phase 2** (swarm wins by a wide margin): met.
  **Phase 3** (cache entry valid on another machine with the same flake):
  met, `dev/e2e-flake.sh`, with both "machines" on one host.
- **Phase 4 is not met**: warm and no-op builds are no slower with a worker,
  but a cold build is not shortened in proportion (see Workers for the
  numbers and why).
- **Phase 5**: rustc fork (4 patches) built and checked (`dev/fork-check.sh`,
  13/13). Measured, it does not pay for itself: over 30 real rust-analyzer
  commits it saves 1 action of 351, and its non-PGO compiler is slower.
  It stays optional. Check-as-build is opt-in (slower in the swarm). The
  incremental-state dedup item was done in the store instead (cold tier:
  28.8 GiB to 3.9 GiB).
- **No drift**: the first no-drift run found that stock `cargo test` failed
  after rargo in all three repos (read-only store links in `target/`);
  fixed (D32). The verification rerun with the fix: see "No-drift" below.

Tests: `cargo test -p rargo-core` (47 unit + 6 engine), `dev/e2e.sh` 81
checks, `dev/e2e-flake.sh` 14 checks, `dev/fork-check.sh` 13 checks, all
passing at the last commit.

Open, known: phase 4 criterion; one unexplained failed step in one of four
swarm runs with a worker (not reproduced); `--timings` not reproduced; the
worker link is not encrypted.

| Area | State |
|---|---|
| FUSE server with passthrough (`fusewire`) | done, tested |
| Content store, reflink / hard-link placement, GC, LRU budget | done, tested |
| Traced sandbox filesystem with copy-on-write upper layer | done, tested |
| Process sandbox (zygote, namespaces, init) | done, tested |
| Git snapshots with inotify-maintained overlay | done, tested |
| Cargo fork: planning mode, parse service | done, exercised on rust-analyzer, Ruff, Bevy |
| Trace cache, in-flight dedup, declared env vars | done, tested |
| Scheduler: priorities, memory admission, shared jobserver | done, unit-tested |
| Per-action cgroups (exact CPU, memory of all processes) | done, active in e2e and benchmarks when the daemon's cgroup is delegated |
| `rargo build / check / test / run / clippy / doc`, JSON messages | done, e2e-tested (`dev/e2e.sh`, 72 checks) |
| Shared incremental compilation | done, e2e-tested, in the 20-agent benchmark |
| Per-repo policy file (`.rargo.toml`) | done, e2e-tested (env pass-through, test network, malformed file) |
| Flake-restricted sandboxes | done, e2e-tested (`dev/e2e-flake.sh`, 14 checks) |
| Cache export/import between machines | done, e2e-tested |
| Verify mode and drift check | done, exercised on the test workspace |
| Workers: UDP session, chunked transfer, dispatch, hedging, keyed authentication | done and benchmarked; warm builds no slower, cold builds not shortened in proportion (criterion not met, see Workers) |
| `cargo` shim | done, e2e-tested (routing, passthrough, fallback without a daemon) |
| Cold tier (chunk-level storage of idle blobs) | done, unit- and e2e-tested, measured on the Ruff swarm store |
| rustc fork (phase 5) | three patches, built and linked as toolchain `rargo`; see "Phase 5" |
| `check` as a build that stops at metadata (fork only) | done, measured on rust-analyzer (D23) |

## Measurements

### rust-analyzer (commit 37721d1, 324 check units, 602 more for build)

| | stock cargo 1.100 | rargo |
|---|---|---|
| `check`, cold | 22.5 s wall, 76 s CPU | 20.3 s wall, 71 s CPU |
| `check`, nothing changed | 0.12 s | 0.02 s |
| `build` after `check`, cold | 37.9 s wall, 160 s CPU | 32.1 s wall |
| `build`, nothing changed | not measured yet | 0.09 s |
| disk after check + build | 4.7 GB target dir | 2.4 GB store; worktree `target/` holds 48 hard links |

Stock cargo here compiles workspace members incrementally (its default for
the dev profile); rargo does not yet, which is part of why its cold numbers
are lower. A like-for-like comparison with `CARGO_INCREMENTAL=0` is still to
be done. The rargo-built `rust-analyzer` binary runs and reports its version.

### 20 agents in 20 worktrees (`dev/swarm.py`)

Each agent: check; edit a file; check; test one package; edit another file;
check; test another package; build the main binary; test again. Agents
overlap (several make the same edit to the same file). Stock cargo gets one
target directory per worktree and its default `-j`; both sides are capped at
24 cores. CPU time is cgroup `cpu.stat` of everything that ran.

| rust-analyzer | stock cargo | rargo | ratio |
|---|---|---|---|
| wall time | 380 s | 117 s | 3.3x |
| CPU time | 8,846 s | 1,659 s | 5.3x |
| disk | 100.1 GB | 42.6 GB (41.1 GB after GC) | 2.4x |
| failed steps | 0 | 0 | |
| mean step times: check / check after edit / test / check after 2nd edit / test / build / test | 117 / 13.6 / 6.8 / 28.8 / 13.9 / 178 / 1.1 s | 23.7 / 8.8 / 1.4 / 11.0 / 1.1 / 67.5 / 0.7 s | |

Without shared incremental compilation the same rargo run took 127 s wall,
2,134 s CPU and 18.2 GB: incremental state costs about 23 GB of store here
and saves about 10 s of wall time and 470 s of CPU. That trade is what the
design's "incremental states are large" risk predicted; see known weaknesses.

| Ruff (commit as cloned) | stock cargo | rargo | ratio |
|---|---|---|---|
| wall time | 818 s | 146 s | 5.6x |
| CPU time | 19,385 s | 2,703 s | 7.2x |
| disk | 94.9 GB | 32.8 GB (30.0 GB after GC) | 2.9x |
| failed steps | 0 | 0 | |
| mean step times (same order) | 317 / 22.2 / 5.8 / 77.6 / 22.9 / 315 / 2.0 s | 35.3 / 14.6 / 1.0 / 16.8 / 1.6 / 71.3 / 0.3 s | |

The Ruff rargo run was done twice because the first lost its CPU figure to a
measurement bug. Wall and disk are from the first run (146 s, nothing else
on the box); CPU is from the second (155 s wall), which shared the machine
with a compiler build, so 2,703 s is if anything too high.

| Bevy | stock cargo | rargo | ratio |
|---|---|---|---|
| wall time | 400 s | 83 s | 4.8x |
| CPU time | 9,112 s | 1,086 s | 8.4x |
| disk | 61.0 GB | 24.9 GB (21.3 GB after GC) | 2.5x |
| failed steps | 0 | 0 | |
| mean step times (same order) | 284 / 22.8 / 10.8 / 27.9 / 9.2 / 30.2 / 0.9 s | 32.6 / 14.7 / 4.4 / 11.1 / 0.7 / 15.7 / 0.4 s | |

Zed was dropped from the set at the user's request (DECISIONS D9).

### Tracing overhead

- A whole `cargo build` of this repository (105 crates) run as one process
  tree inside one traced sandbox: 8.4 to 8.5 s against 8.0 s native (about
  5% wall, under 1% user CPU, 16% more system time). This is the worst case
  for lock contention, since all 24 compilers share one action.
- rust-analyzer cold `check`, one sandbox per compiler invocation: faster
  than stock cargo in absolute terms (see above); the tracing share has not
  been isolated yet.

### Small synthetic workspace (`dev/e2e.sh`, 48 checks, all passing)

- Second worktree at the same commit: zero actions run, about 7 ms, artifact
  shares an inode with the first worktree's.
- Nothing-changed build: about 5 ms end to end.
- One-file edit: exactly the edited crate and its dependents rerun.
- Eight concurrent cold builds of the same commit from eight worktrees: one
  build's worth of actions (5), the rest joined in flight or hit the cache.
- A test binary reruns only when something it read changed; a passing run
  is replayed otherwise.
- Stock cargo builds the same worktree afterwards, including into the same
  `target/` that rargo linked artifacts into.

## No-drift

`dev/nodrift.sh REPO`: rargo build and test, rargo's drift check, tracked
files untouched, then stock cargo build and test in the same worktree and
`target/`, then rargo again. First run: every rargo step and stock build
passed in all three repos; stock `cargo test` failed in all three (fixed,
D32); Ruff's drift check flagged an unread autoconf log (no longer counted
as drift, D32). Rerun after the fix: pending at the time of writing.

## Mechanisms

### Sandbox filesystem and tracing
Implemented in `crates/fusewire` and `rargo-core/src/sandboxfs.rs`. One FUSE
mount; each action is pivoted into its own top-level directory. Reads and
writes go through kernel passthrough to backing files; only lookups,
opens, readdirs and metadata changes reach the daemon.
Verified by: unit and integration tests (`cargo test -p rargo-core`),
`dev/e2e.sh`, rust-analyzer builds.
Known weaknesses: FUSE-over-io_uring is not used; a file opened read-only
and then for writing while the first handle is still open falls back to
direct I/O for the second handle; `O_TMPFILE` is not supported inside the
sandbox.

### Snapshots
`rargo-core/src/snapshot.rs`. HEAD tree plus overlay; modified, untracked and
filter-affected files are captured into the store when a snapshot is taken;
ignored directories and submodules are shown live.
Verified by: five unit tests (clean tree, edits via events, HEAD moving
without file changes, ignored directories, two worktrees sharing identity).
Known weaknesses: sparse checkouts and `assume-unchanged` are not handled
specially; a full rescan runs `git status` (tens of milliseconds on a large
repository) after any change to a `.gitignore` or `.gitattributes`.

### Planner (cargo fork)
`forks/cargo/*.patch` on cargo 1.100.0. Cargo's real pipeline runs with
process execution stubbed; build-script results appear in recorded commands
as placeholder tokens that rargod expands.
Verified by: expansion unit tests, e2e script, rust-analyzer.
Known weaknesses: cargo's own post-build lints (`unused_dependencies`),
future-incompatibility reports, `--timings`, and the `target/debug/*.d`
dep-info files are not produced; `cargo doc --open` does not open anything;
example scraping failures are treated as hard errors.

### Trace cache and engine
`rargo-core/src/{trace,engine}.rs`. Only successes are recorded.
Verified by: `tests/engine.rs` (six tests with real sandboxed processes).

### Scheduler
`rargo-core/src/sched.rs`. Tokens are a real jobserver pipe shared with
rustc's worker threads; requests are ordered by priority and admitted by
expected memory.
Verified by: four unit tests. Not yet verified under a real memory-bound
workload. Memory estimates come from `ru_maxrss` of the largest process of a
previous run, not from a cgroup.

### Shared incremental compilation
`rargo-core/src/incr.rs`. In the 20-agent rust-analyzer run: 968 states
saved; of 4,116 seeded compiles 373 took the same worktree's state and 2,480
an ancestor commit's; 1,263 compiles had no state to start from.
Known weaknesses: states are the largest thing in the store (about 23 GB of
the 42.6 GB above); remote (worker) compiles run without a seed.

### Flake environments, export/import
`rargo-core/src/flake.rs`, `dev/e2e-flake.sh`. Two daemons with separate
stores, worktrees at different paths and different cargo homes: after
exporting from the first and importing into the second, the second built a
byte-identical binary and replayed the test while running one action (the
offline planning attempt). A third store without the import ran seven.
Known weaknesses: both "machines" are the same host, so planning (which
observes host files) also hit; on a different host it would run once. The
flake key covers `flake.nix`, `flake.lock` and `.nix` files at the top level
and under `nix/`, not arbitrary files a flake might import.

### Verify mode and drift check
`RARGO_VERIFY=1`, `rargo --rargo-drift-check`, background every six hours.
On the test workspace: "no drift", 5 cached units re-run cleanly, 0 differed.
Finding: rustc's incremental mode is not byte-reproducible; the object
members of an rlib are named after a random per-invocation string. Verify
compares such rlibs by member content. Not yet run on the large workspaces.

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

## Store on XFS

With `mkfs.xfs` unavailable, the reflink check used a 20 GiB loopback btrfs
image. All 81 e2e checks passed on btrfs and on ext4, and the core suite
passed 53 tests with one ignored. For `build` and `test --no-run` in two
worktrees, `df` grew by 56.09 MiB on btrfs versus 102.95 MiB on ext4.
Aggregate `du` counts were 122.69 MiB and 104.83 MiB respectively.
`filefrag` confirmed shared extents on btrfs and independent copies when
the store was on btrfs and the worktrees were on ext4. The btrfs image
was unmounted and retained (D33).
