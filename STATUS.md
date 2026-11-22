# Status

What exists, how it was checked, what was measured, and what is weak or
missing. Updated as the run proceeds; the newest state is at the top of each
section. Machine: 32 threads, 124 GB RAM, kernel 7.0, ext4. All builds are
capped at 24 cores (`CPUQuota=2400%`, `-j24` / 24 scheduler tokens).

## Summary so far

Phases 1 to 4 are implemented and exercised; phase 5 (rustc fork work) has
measurements and a plan but no compiler changes yet. Benchmarks on Ruff and
Bevy and the worker benchmark are still running or pending.

| Area | State |
|---|---|
| FUSE server with passthrough (`fusewire`) | done, tested |
| Content store, reflink / hard-link placement, GC, LRU budget | done, tested |
| Traced sandbox filesystem with copy-on-write upper layer | done, tested |
| Process sandbox (zygote, namespaces, init) | done, tested |
| Git snapshots with inotify-maintained overlay | done, tested |
| Cargo fork: planning mode, parse service | done, exercised on rust-analyzer, Ruff, Bevy |
| Trace cache, in-flight dedup, declared env vars | done, tested |
| Scheduler: priorities, memory admission, shared jobserver | done, unit-tested; memory figures are rusage, not cgroup |
| `rargo build / check / test / run / clippy`, JSON messages | done, e2e-tested (52 checks) |
| Shared incremental compilation | done, e2e-tested, in the 20-agent benchmark |
| Per-repo policy file (`.rargo.toml`) | done, unit-tested; not yet in an e2e test |
| Flake-restricted sandboxes | done, e2e-tested (`dev/e2e-flake.sh`, 14 checks) |
| Cache export/import between machines | done, e2e-tested |
| Verify mode and drift check | done, exercised on the test workspace |
| Workers: UDP session, chunked transfer, dispatch | done, functional over a netem link; not benchmarked yet |
| `cargo` shim | client supports being installed as `cargo`; not yet e2e-tested |
| Cold tier (chunk-level storage of old blobs) | not started |
| rustc fork work (phase 5) | not started; see "Phase 5 evidence" |

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

Ruff (rargo side so far): 146 s wall, 32.8 GB (30.0 GB after GC), no failed
steps; CPU figure lost to a measurement bug (fixed) and to be re-run. Stock
Ruff and both Bevy runs are still in progress.

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
`rargo-core/src/{net,chunk,remote}.rs`, `dev/netns.sh`. Transport: 118 MiB/s
on loopback, correct under 5% loss in both directions (unit tests). Chunker:
unit tests for size bounds, insertion locality and archive member
boundaries. End to end: a worker in its own network namespace behind a veth
link with 1 ms delay each way, 0.1% loss and a 1 Gbit rate limit joined,
ran actions, returned traces and outputs; killing a worker mid-action made
the server rerun the action locally.
Not yet measured: the phase 4 criterion (cold build speed-up with a worker,
warm build no slower). Known weaknesses: a cold worker first has to receive
the toolchain (about nine seconds on loopback); remote actions have no
incremental seed; flake-rooted and host-network actions are never sent to
workers; there is no authentication on the worker port.

## Phase 5 evidence

Measurements that justify (or not) each phase 5 item, collected so far:

- Stable object names: rlibs built incrementally differ between identical
  compilations only in member names (see verify). A recompiled-but-unchanged
  library therefore relinks every binary that uses it.
- Metadata and codegen as separate cached actions: in the swarm, `check`
  (23.7 s) followed later by `build` (67.5 s) compiles every dependency's
  front end twice, once per mode.
- Body-only-edit-stable metadata, object caching per codegen unit, and a
  deduplicating incremental format: not yet measured.

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
