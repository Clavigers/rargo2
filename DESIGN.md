# rargo design

Status: proposal, nothing built yet.

## What this is for

A small team of humans each directs dozens of coding agents. The agents work
in separate git worktrees of the same repo, often on one machine, and run
overlapping `build`, `check` and `test` commands. Today that means N copies of
the same compile, N full `target/` directories, cores thrashing, and disks
filling up.

rargo makes N worktrees cost roughly one build plus their differences.

### What must be true

1. **Shared work is done once.** Across worktrees, across agents, across
   time, and across builds that are running right now.
2. **A worktree holds source and the artifacts it asked for.** Nothing else.
3. **Fast in every sense a person or agent perceives**: latency of one
   command and throughput of the whole swarm.
4. **Drop-in.** Point it at a large cargo project and it works. Setup is no
   harder than building that repo for the first time.
5. **No drift.** Using rargo for years never makes a repo depend on rargo.
   Stock cargo keeps working.
6. **Nobody ever nukes the cache.** It is never wrong, so there is never a
   reason to.

### What we do not care about

Coupling (to git, Linux, cargo, rustc, Nix), implementation complexity,
general-purpose scalability, protocol conventions. Where any of these
conflicts with the list above, the list wins.

## The model

One server process, `rargod`, owns all build work on a machine. Every
`rargo` command in every worktree is a thin request to it.

```
worktree A ─┐                    ┌─ analysis (cargo, memoized)
worktree B ─┼─ rargo ── rargod ──┼─ scheduler (all cores, all memory)
worktree C ─┘                    ├─ sandboxed, traced actions
                                 └─ content store + trace cache
```

An **action** is one process the build needs: a rustc invocation, a build
script run, a link, a test binary run. Every action runs in a sandbox whose
filesystem rargod serves, so rargod sees exactly which files it read, which
lookups failed, and which files it wrote. That record is the action's
**trace**. Nothing about an action's inputs is ever declared.

### Decisions on the open questions

- **Topology.** One `rargod` per machine, reached over a local socket, and
  builds run on that machine. Later, `rargod` can also dispatch actions to a
  fleet of workers. The server stays the single owner of analysis, the cache
  and scheduling; workers only execute. See "Workers" below.
- **Nix.** Optional. By default the environment (toolchain, linker, system
  libraries) is captured by tracing the host, which is what makes rargo
  drop-in. If the repo has a flake, the sandbox is restricted to that closure
  instead, which is stricter and stable across machines.
- **Platform.** Linux only. x86_64 first.

## Mechanisms

### Canonical paths

Inside the sandbox every worktree appears at the same path (`/w`), cargo home
at another fixed path, and so on. Identical source therefore produces
identical command lines, identical cargo metadata hashes and identical
outputs no matter where it is checked out. Without this, worktrees share
nothing.

`rargo run` and `rargo test` execute in a mount namespace where `/w` maps to
the real worktree, so paths baked in at compile time still resolve.

### Snapshots from git

A worktree's state is its HEAD tree plus a dirty overlay. Clean tracked files
already have a content hash in git, so hashing them is free. A filesystem
watcher keeps the dirty set current, so a command never scans the tree.

When a build starts, the dirty files are captured into the content store and
the sandbox is served from that immutable snapshot, never from the live
worktree. An agent can keep editing while its build runs and the build stays
consistent.

Git ancestry is used only as a hint for what to pre-warm or evict. It is
never part of correctness.

### The trace cache

Inputs are unknown until an action has run, so the cache has two levels:

- **Weak key**: canonical command line, working directory, declared
  environment variables.
- **Traces** under that key: each a list of observations (`path → content
  hash`, `path → absent`, `directory → listing hash`) plus the outputs
  produced.

A lookup finds the weak key and checks each recorded trace against the
requesting worktree's snapshot. Any trace whose observations all hold is a
hit. Because dependency outputs are observed by content, a dependency that
rebuilds to identical bytes does not rebuild its dependents.

### Deduplicating builds in flight

Two agents often need the same action at the same moment. The action's
previous trace predicts its inputs. If two requests agree on that predicted
input set, one runs and the other waits. When it finishes, the real trace is
verified against the waiter's snapshot. If it does not hold, the waiter runs
its own.

### Sandbox and tracing

Each action gets its own mount, PID and network namespace. Its root
filesystem is a FUSE mount served by rargod, with reads passed through to
backing files in the content store so file data never crosses userspace.
Each action sees a distinct subtree of the mount, so kernel caching cannot
hide one action's lookups behind another's.

Writes go to a private upper layer. Whatever an action writes is its output,
including files written into the worktree by tests (snapshot files, for
example). On a cache hit those writes are replayed.

Environment variables cannot be traced, so actions get a clean environment
plus the variables cargo and rustc declare an interest in. Network is
loopback-only by default. Both have a per-repo escape hatch kept outside the
cargo files.

### Analysis

rargod runs cargo as a library, inside the same traced view, to get the unit
graph. That is itself a cached action keyed on the manifests, lockfile and
config it read, so worktrees with the same manifests get their graph
instantly. rargo replaces cargo's fingerprinting and job queue; cargo's
resolution and command-line construction are kept as is, which is what keeps
behaviour identical.

### Scheduling

One scheduler owns every core and all memory on the machine. Human requests
outrank agent requests. Actions are admitted by measured memory use from
their previous runs, because a swarm of concurrent links is how a machine
runs out of memory. rustc's internal parallelism draws from the same pool
through a jobserver rargod owns.

### Storage

One content-addressed store per machine. Requested artifacts appear in a
worktree's `target/` as links into it, costing no extra space. A size budget
with least-recently-used eviction replaces `cargo clean`.

This machine is ext4, which has no reflinks. Artifacts will be read-only
hard links at first. A btrfs or XFS store would allow reflinks and is worth
moving to.

Chunk-level storage (content-defined chunking with format-aware boundaries:
archive members in `.rlib`, sections in objects) is used for the cold tier
and for network transfer. Hot files stay whole so they can be passed through
to the sandbox.

### Shared incremental compilation

The trace cache answers "has exactly this been built before". Incremental
compilation covers the miss: the crate changed, but mostly it did not.

rustc's incremental state for a crate is only a starting point. rustc checks
every piece of it against the current source before reusing anything, so any
state is a safe seed for any source; a closer one is just faster. That makes
it shareable:

- After an action, the crate's incremental state is stored as an immutable
  entry keyed by crate and source snapshot.
- Before an action, rargod picks a seed: the same worktree's last state for
  that crate, otherwise the nearest ancestor commit that has one, otherwise
  the most recent of any. An agent that branches off main starts warm from
  main's state.
- The action gets the seed as a private copy-on-write layer, so concurrent
  builds of the same crate never contend for rustc's session lock.

Canonical paths are what make a state from one worktree usable in another.
The seed is excluded from the trace, because it must not affect the result.

This works with stock rustc. States are large and are only an optimisation,
so they are evicted freely: recent commits on main and the latest per
worktree are kept. If rustc crashes with a seed, the action is retried
without one and that state is discarded.

### Workers

An action is already self-contained: a command, a snapshot, and a filesystem
that rargod serves on demand. Running it on another machine means serving
that filesystem over the network instead of locally. A worker is a sandbox
host that faults content in from the server, keeps a local content cache, and
streams back the trace and outputs.

The stateful session protocol lives here, between `rargod` and each worker:
the server knows what every worker already holds and sends only what is
missing, predicted from the action's previous trace. Local execution stays
the default; the scheduler sends an action to a worker when local cores are
the bottleneck and the transfer is cheaper than the wait.

### Tests

A test binary run is an action like any other. If nothing it read has
changed, it does not rerun and its recorded output is replayed. Agents rerun
whole suites after small edits, so this is expected to save more time than
compile caching. `--no-cache` forces a real run.

### Keeping cargo honest

The sandbox is stricter than a plain cargo build, so most things that pass
under rargo pass under cargo. The exceptions are canonical paths, the clean
environment and cached tests. A background drift check periodically builds a
recent commit with stock cargo in a scratch checkout and reports any
disagreement.

Agents type `cargo`. An optional `cargo` shim routes `build`, `check`,
`clippy`, `test`, `run` and `doc` to rargo and passes everything else to real
cargo.

## Phases

Ordered by how much of the stated pain each removes.

**0. Spikes.** The two assumptions everything rests on.
- FUSE view with per-action tracing: measure overhead against a native build
  of a large workspace. Target: under 5% on a cold build.
- Cargo as a library under canonical paths: build one commit from two
  worktrees and confirm byte-identical outputs.

**1. Local core.** `rargod`, `rargo build` and `rargo check`, git snapshots,
sandboxed traced actions, trace cache, in-flight dedup, content store with
linked artifacts.
Done when a second worktree at the same commit builds in about a second and
uses no extra disk, and a one-file edit rebuilds only what depends on it.

**2. Swarm.** Global scheduler with priorities and memory admission, shared
incremental compilation, cached tests, `clippy`, `run`, the cargo shim,
eviction.
Done when a 20-agent benchmark beats stock cargo with per-worktree target
directories on wall time, CPU time and disk by a wide margin.

**3. Environment.** Flake-restricted sandboxes, environment variable and
network policy, the drift check.
Done when a cache entry made on one machine is valid on another with the
same flake.

**4. Workers.** A worker daemon, a stateful UDP session between server and
worker that sends only what the worker lacks, chunked transfer with
format-aware chunkers, and dispatch in the scheduler.
Done when adding a worker machine shortens a cold build of a large workspace
roughly in proportion, and a warm build is no slower than local-only.

**5. Forks.** A persistent in-memory cargo graph in the server. In rustc:
metadata that is stable under body-only edits, separately cached metadata and
codegen, per-codegen-unit object caching, and an incremental state format
that deduplicates well between snapshots.
Each is justified by a measurement from the phase 2 benchmark.

## Risks

- **FUSE overhead** on rustc's many small reads. Spike 0 answers this. The
  fallback is a forked rustc that reports its own inputs, with FUSE tracing
  kept only for build scripts, proc macros and tests.
- **Cargo's library API is unstable** and does not route build-script
  execution through its executor hook. Expect a small fork from phase 1.
- **Untraceable inputs**: environment variables, network, time. Handled by
  policy, not observation. A build script that needs an undeclared variable
  fails under rargo until allowed.
- **Incremental compilation has a history of bugs** in rustc, and sharing
  states widely exposes more of them. That threatens the "never wrong" goal.
  Mitigations: retry without a seed on any crash, and have the drift check
  compare incremental and clean outputs. Phase 1 runs without incremental, so
  edit-rebuild of a large leaf crate may be slower than stock cargo until
  phase 2.
- **Incremental states are large** and rustc's on-disk format changes all
  over on small edits, so stored states deduplicate poorly until phase 5.
- **Proc macros and build scripts with nondeterministic output** reduce
  sharing but do not break correctness.
