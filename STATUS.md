# Status

What exists, how it was checked, what was measured, and what is weak or
missing. Updated as the run proceeds; the newest state is at the top of each
section. Machine: 32 threads, 124 GB RAM, kernel 7.0, ext4. All builds are
capped at 24 cores (`CPUQuota=2400%`, `-j24` / 24 scheduler tokens).

## Summary so far

Phase 1 (local core) works end to end on a synthetic workspace and on
rust-analyzer. Phases 2 to 5 are in progress or not started; see the table.

| Area | State |
|---|---|
| FUSE server with passthrough (`fusewire`) | done, tested |
| Content store, reflink / hard-link placement | done, tested |
| Traced sandbox filesystem with copy-on-write upper layer | done, tested |
| Process sandbox (zygote, namespaces, init) | done, tested |
| Git snapshots with inotify-maintained overlay | done, tested |
| Cargo fork: planning mode, parse service | done, exercised on rust-analyzer |
| Trace cache, in-flight dedup, declared env vars | done, tested |
| Scheduler: priorities, memory admission, shared jobserver | done, unit-tested; memory figures are rusage, not cgroup |
| `rargo build / check / test / run / clippy`, JSON messages | done, e2e-tested |
| Shared incremental compilation | not started |
| Eviction policy wired to a budget | store has LRU eviction; not yet driven by a budget |
| `cargo` shim | client supports being installed as `cargo`; not yet e2e-tested |
| Flake-restricted sandboxes, drift check, policy file | not started |
| Workers | not started |
| rustc fork work (phase 5) | not started |

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
