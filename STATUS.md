# Status

What exists, how it was checked, what was measured, and what is weak or
missing. Machine: 32 threads, 124 GB RAM, kernel 7.0, ext4. All builds are
capped at 24 cores (`CPUQuota=2400%`, 24 scheduler tokens) and 90 GB.

## Summary (2026-11-23)

rargo is remote cargo: a client scans its worktree into a content-addressed
tree, uploads what the server lacks, passes on an allow-listed environment,
and receives output, the artifacts it asked for and files the build wrote
into the worktree. Analysis, scheduling and building happen on the server
(DESIGN.md, DECISIONS D34 to D54).

Everything below was measured on 2026-11-23 at commit `a92e359` (protocol
6, plan format 6) unless a line says otherwise. The commits after it
change how work is given to workers and how the drift check compares
outputs (D55), nothing a client or a server without workers runs through
in a build; the tests and both e2e scripts were run again at the final
commit. Measured with clients in their own
network namespace, one shaped link (1 ms each way, 1 Gbit) from a server
that cannot see their files.

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
- **Gaps closed since the last report**: the worker link is encrypted
  (D44); a client that goes away takes its actions with it, on the server
  and on workers (D48); `--timings` and cargo's future-incompatibility
  report file (D49); temporaries of interrupted clients (D50); Zed builds,
  through its flake (D45), and its drift check (D51).

Tests at this commit: `cargo test` 11 (client) + 8 (protocol) + 55 unit and
6 engine (server) + 1 (plan); `dev/e2e.sh` 113 checks with the server in a
mount namespace where the client's directory is empty; `dev/e2e-flake.sh`
14 checks (two servers, a registry dependency fetched by the servers).

Not met, or weak:

- **Phase 4 is met in part.** With 4 cores per machine a warm worker
  shortens a cold rust-analyzer build from 56.0 s to 49.2 s, where 4 more
  local cores give 45.2 s: about two thirds of what the cores are worth,
  not all of it. A worker that has never seen the toolchain neither helps
  nor hurts its first build (55.8 s). With 8 cores per machine there is
  2 s to gain and the worker gets 0.8 s of it. A build where nothing
  changed, and a rebuild after an edit, are no slower with a worker. The
  20-agent swarm is not shortened by a worker at all. See Workers for
  why and for what was tried.
- The rustc fork (phase 5) still buys nothing on real history; see Phase 5.
- The client was only run on Linux x86_64.
- Two mistakes of mine on the shared machine during this work are listed
  under Incidents.

| Area | State |
|---|---|
| Client: worktree scan with index, content-addressed tree | done, unit-tested, e2e |
| Sync: manifests and contents the server lacks, shared uploads | done, unit-tested (server), e2e (counts of files and manifests asked) |
| Stand-in for `.git` (HEAD, branch, dirty or not; no history, no contents) | done, unit-tested against real git, e2e |
| Environment: allow-list, ask-on-declare, client cargo configuration | done, e2e (declared variable, policy file, a `config.toml` in a cargo home the server cannot see); `credentials.toml` travels the same way but no private registry was tried |
| Transport: TCP, Noise `NNpsk0`, per-user keys, admin users | done, unit-tested, e2e (wrong key, unknown user, non-admin) |
| Results: artifacts by hash, client cache shared between worktrees, worktree writes, interrupted clients | done, unit-tested, e2e |
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
| Workers: encrypted link, dispatch, hedging, cancellation | done; benchmark series below; phase 4 criterion met in part |
| `cargo` shim | done, e2e (routing, passthrough, fallback without a server) |
| rustc fork (phase 5), `check` as a build | done, re-measured; not worth using |

## Measurements

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

### Small synthetic workspace (`dev/e2e.sh`, 113 checks, all passing)

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
- A crate with a future-incompatibility warning: the closing warning names
  the report, `cargo report future-incompatibilities --id 1` shows it, its
  text equals what stock cargo writes for the same crate, and a cached
  build reports the same warnings under the same number.
- `--timings` writes a report of what ran and what came from the cache.
- Temporaries of a client that was killed are removed by the next one;
  those of a live process are kept; an interrupted client leaves none.
- A build script that sleeps for ten minutes is gone within five seconds
  of its client being killed, and the server builds on.

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
Both links are encrypted with fresh keys per connection: the client link
(TCP, Noise `NNpsk0` with a key per user) and the worker link (the
reliable-UDP transport, the same handshake with the shared worker key,
D44).
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
A client that is interrupted removes the temporaries it was writing, and
the next client to write into a directory removes those of clients that
were killed outright (D50; unit tests and e2e). A client that goes away
takes its queued and running actions with it, on the server and on workers
(D48; e2e: a build script sleeping for ten minutes is gone within five
seconds of its client being killed).
Known weaknesses: without reflinks, shared artifacts are read-only hard
links into the client's cache (as cargo's own are links into `deps/`); an
action another build was also waiting for is killed with its owner and
started again by the waiting build; a temporary whose process id has been
reused stays until that process ends.

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

Ideas not tried, in the order I would try them: bring a remote compile's
incremental state home in the background at low priority instead of not
at all (workspace crates could then go to workers again, which is where
the swarm's work is); let workers fetch
registry crates themselves instead of through the server; a second
transport connection for bulk data so that questions do not queue behind
pushes.

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
  40.7 s. In the favourable case, the 20-agent swarm with body-only second
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
