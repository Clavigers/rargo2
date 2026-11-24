# Decisions

Every decision made during the implementation run, with the reason. Newest
sections are appended at the bottom. `DESIGN.md` is the spec; this file records
how its open ends were closed.

## D1. Toolchain and scratch layout

- All development toolchains live in `/root/rargo-work` (`RUSTUP_HOME`,
  `CARGO_HOME` there; `source /root/rargo-work/env.sh`). Reason: `CLAUDE.md`
  forbids touching other directories under `/root`, and the pre-installed
  `~/.cargo` / `~/.rustup` are such directories.
- Rust 1.100.0 stable is the implementation toolchain.

## D2. Language and repo layout

- Everything is Rust, one cargo workspace in this repo (`crates/*`).
- The cargo fork lives in `/root/rargo-work/cargo` (branch `rargo`, based on
  the commit that shipped as cargo 1.100.0, `3acc79c0c`). This repo carries the
  fork as a patch series in `forks/cargo/` plus `forks/setup.sh`, and depends
  on the checkout by path. Reason: the brief says to clone forks under
  `/root/rargo-work`; the patch series keeps this repo self-contained.

## D3. How cargo is used: a static plan, executed by rargod

- The fork adds a planner that runs cargo's real resolution, unit graph and
  command-line construction and serialises the result: one record per unit
  with the exact process (program, args, env, cwd), declared outputs, uplift
  links, the JSON artifact message, and a declarative description of the
  parts cargo only knows at build time (which build-script outputs feed which
  flags). rargod executes that plan with its own scheduler and cache.
- Rejected: running cargo's job queue per request with a remote executor.
  It would keep more cargo code, but the plan could not be memoised, a no-op
  build would pay cargo's start-up every time, and DESIGN says rargo replaces
  the job queue.
- The planner runs as a sandboxed, traced action like any other (DESIGN:
  "itself a cached action"), so its cache key is whatever it actually read:
  manifests, lockfile, config, directory listings used for target discovery,
  the toolchain files behind `rustc -vV`.
- The planner always asks rustc for ANSI-coloured diagnostics. Colour is
  stripped at replay time for clients that do not want it (cargo does the
  same for its own output cache). Reason: colour would otherwise be part of
  every command line and split the cache between terminals and agents.
- rargod links the fork as a library so build-script output is parsed by
  cargo's own `BuildOutput::parse`, not a reimplementation.

## D4. Sandbox filesystem: our own FUSE server

- rargod speaks the kernel FUSE protocol directly (crate `fusewire`) rather
  than through libfuse or `fuser`. Reasons: tracing overhead is the main risk
  in the design, and control over worker threads (one cloned `/dev/fuse` fd
  per thread), entry/attr timeouts, negative-dentry caching, backing-id
  reuse and readdirplus suppression is what keeps it low.
- One long-lived mount. Each action gets a top-level directory in it and is
  `pivot_root`ed into that directory, so every action has its own inodes and
  dentries and the kernel cannot hide one action's lookups behind another's.
- Kernel entry and attribute caches are given the longest timeouts, including
  negative entries. Within one action a path is therefore looked up once; the
  view under an action never changes except through that action's own writes.
- READDIRPLUS is not negotiated. It would prime dentries without lookups and
  force us to treat every listed entry as observed.
- File data: `FOPEN_PASSTHROUGH` with one backing id per backing file, shared
  by all actions and kept in an LRU. Without kernel support (or without
  CAP_SYS_ADMIN) the same code path serves reads and writes in userspace.
- `/tmp`, `/proc`, `/dev`, `/sys` inside the sandbox are real mounts (fresh
  tmpfs, fresh procfs, a minimal dev, read-only sysfs), not FUSE. They start
  empty or describe the machine, so there is nothing to trace, and the
  linker's temp files stay off the FUSE path.
- All files below the upper layer report one constant mtime
  (2000-01-01T00:00:00Z). Reason: mtimes are not content; a constant keeps
  actions deterministic. 1970 was avoided because zip-based tools reject it.

## D5. Observations

- An observation is `(path as walked through real directories, what was
  seen)`. FUSE lookups are per `(parent inode, name)`, so recorded paths never
  contain symlinked parents; symlinks are observed as `readlink` results and
  the kernel's follow-up lookups are observed in turn.
- Kinds: absent; directory exists; directory listing (hash of sorted
  name+type); symlink target; file metadata only (type, size, exec bit) when
  the file was looked up but never opened; file content (blake3 + exec bit)
  when it was opened.
- Directory observations implied by a deeper positive observation are dropped
  when the trace is finalised.
- Environment variables are observations too (`name -> value | unset`) for
  the variables an action declares after the fact (build-script
  `rerun-if-env-changed`, rustc `env-dep` in dep-info). If an action declares
  a variable that the client has set but the action was not given, it is
  rerun with it.

## D6. Process model for actions

- A small single-threaded zygote process forks sandbox children. Reason:
  forking from the multi-threaded daemon is slow and unsafe; the zygote also
  reaps children and reports exit status and rusage.
- Inside the PID namespace a minimal init (PID 1) runs the command as PID 2.
  Reason: PID 1 ignores default-action signals, which would change the exit
  status of anything that aborts; init also guarantees every stray process
  dies with the action.
- Actions run with the uid/gid of the requesting client.
- Each action gets its own cgroup when rargod has a delegated cgroup
  (`memory.peak` and `cpu.stat` give exact per-action memory and CPU);
  otherwise rusage from `wait4` is used.

## D7. Hashes and the store

- blake3 everywhere. Clean git-tracked files are identified through a
  persistent `git blob oid -> blake3` table filled the first time a blob is
  materialised; captured dirty files get both hashes computed, so the same
  bytes compare equal whether committed in one worktree or dirty in another.
- Host files (toolchain, system libraries, registry sources) are hashed on
  first read and remembered by `(dev, inode, size, mtime, ctime)`. They are
  passed through from where they are, not copied into the store.
- Store blobs are files named by hash, mode 0444. Artifacts are placed in a
  worktree by reflink when the store and worktree share a reflink-capable
  filesystem, by hard link when they share a filesystem without reflinks, by
  copy otherwise. Hard-linked blobs are checked against their recorded size
  and mtime before use, since a link can be written through by root.

## D8. What is cached

- Only successful actions are recorded. An earlier version also recorded
  rustc failures with exit code 1, on the argument that diagnostics are a
  deterministic function of the inputs. The first end-to-end run disproved
  that in practice: a bug in rargo itself made rustc fail, the failure was
  recorded, and it was replayed after the bug was fixed. A failure can also
  come from a full disk or memory pressure, none of which a trace captures.
  Replaying a stale failure is the kind of wrong answer that makes people
  delete caches, so failures are always recomputed. Concurrent identical
  failing compiles are still collapsed by in-flight deduplication.
- Test binary runs are cached only when they pass, for the same reason.

## D9. Zed is dropped from the benchmark set

- The user said mid-run to give up on Zed: it takes too long and pulls in
  too many unrelated system dependencies (it needed a private `cmake` just to
  get past one build script, and pins its own toolchain and some forty git
  dependencies). Acceptance is measured on rust-analyzer, Ruff and Bevy.
- What Zed did exercise before being dropped: the unsandboxed fetch step
  (toolchain install, git dependencies with submodules, crate downloads) and
  build-script failure reporting, both of which behaved.

## D10. Environment given to actions

- Actions get a clean environment: `PATH`, `HOME`, `USER`, `LOGNAME`,
  `SHELL`, locale and timezone variables, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`,
  certificate paths, and everything named `CARGO_*`, `RUST*`, `NIX_*`,
  minus the variables cargo sets per package. All of it is part of the key.
- Any other variable reaches an action only after the action has declared
  it (build-script `rerun-if-env-changed`, rustc `env-dep`). The declaration
  is remembered per command line; later requests get the client's value
  folded into the key up front. Reason: DESIGN, "environment variables
  cannot be traced".
- `RUSTC_WRAPPER` and `RUSTC_WORKSPACE_WRAPPER` are forced empty. Reason: a
  wrapper such as sccache would run inside the sandbox without its server,
  and rargo is the cache. `cargo clippy` still works because cargo-clippy
  sets the workspace wrapper itself, inside the planning action.
- `CARGO_HOME` is `/c` inside the sandbox and the worktree is `/w`.

## D11. The planner does not observe file sizes

- Files the planner looks up but never opens are recorded as "exists, with
  this executable bit", not with their size. Reason: cargo checks that
  `src/lib.rs` and friends exist; recording sizes re-planned after every
  edit to such a file. Cargo does not act on sizes of files it does not read.
  Every other action records sizes.

## D12. Writes into the worktree

- Whatever an action writes under `/w` outside the target directory is
  written to the real worktree after the action succeeds (snapshot files
  from tests, files generated into `src/` by build scripts, `Cargo.lock`
  from planning), as ordinary files owned by the client.
- Consequence, accepted: such a file is new to the next snapshot, so the
  action that wrote it (and whatever listed that directory) runs once more
  and then settles. Skipping that run would mean assuming an action behaves
  the same whether or not its own output already exists.

## D13. What appears in the worktree's target directory

- Only outputs of the units the command asked for: files cargo would link
  to a friendly name (`target/debug/foo`, `libfoo.rlib`) and executables
  (test binaries at their hashed path). Dependencies' rlibs, build-script
  output directories and incremental state stay in the store.
- Inside the sandbox every unit sees the uplifted names of the units it
  depends on, because tests refer to them (`CARGO_BIN_EXE_*`).

## D14. Fetching is done outside the sandbox

- Planning runs offline. If cargo's message says it needed the network or
  the toolchain is missing, rargod runs `rustc -V` (which makes rustup
  install the toolchain) and the forked cargo's `fetch` unsandboxed, as the
  client, in the real worktree, then snapshots again and re-plans once.
  Reason: downloads only add to cargo home and the lockfile; they need the
  network and write outside the sandbox; nothing about them is cacheable.

## D15. Tests

- A test binary run is an action with loopback-only network. Its stdout and
  stderr are streamed while it runs and stored separately; a replay prints
  stdout then stderr, so interleaving between the two streams is not
  preserved on replay.
- Test binaries see no terminal, so libtest does not colour its output.

## D16. Incremental compilation

- Units cargo compiles incrementally keep `-C incremental=<dir>`; the
  directory is neither traced nor part of the result. Its content after a
  successful run is stored as a manifest keyed by the unit's command line,
  tagged with worktree and HEAD commit.
- Seed choice: same worktree's newest; else the newest made at an ancestor
  of HEAD (checked with libgit2); else the newest of all. Shallow clones have
  no ancestry, so they fall to the last rule.
- rustc hard-links unchanged files from the old session directory into the
  new one; the sandbox filesystem turns a hard link of a view file into a
  reference to the same blob, so unchanged files cost nothing to store.
- A compiler that dies by signal or exits 101 while seeded is rerun without
  the seed and that state is dropped.
- Per command line: the newest state of each of up to 32 worktrees plus two
  more are kept.

## D17. Eviction

- The store has a byte budget (`--store-budget-gb`, default 100). Every 20
  seconds, if over budget, least-recently-used blobs are removed down to 90%
  of it. Blobs used in the last ten minutes are never removed, so a running
  build cannot lose an input. A trace whose outputs are gone is a miss and
  is dropped when next looked at.

## D18. Flake environments

- A repository with `flake.nix` (unless `.rargo.toml` says `flake.enabled =
  false`) is built in its default devShell: `nix print-dev-env --json` gives
  the variables, `nix-store -qR` over every store path they mention gives
  the closure. The result is remembered under a hash of `flake.nix`,
  `flake.lock` and the other `.nix` files at the top level and under `nix/`.
- Unit actions and tests then run in a root that contains only: the
  closure under `/nix/store`, `/w`, `/c`, a four-file `/etc`, and `/bin/sh`
  and `/usr/bin/env` as symlinks into the closure. `HOME` is `/tmp`.
- Store paths are not traced; the closure's identity is in every key.
  Reason: a store path's content is fixed by its name, and tracing it would
  put thousands of pointless observations into every trace.
- Planning still runs against the host root, with the flake's environment.
  Reason: the forked cargo is a host binary and needs the host's libraries.
  Its trace is therefore machine-specific; the plan it produces is not.
- Processes get the flake's `cargo` as `$CARGO` when there is one, because
  the forked cargo cannot run inside the restricted root.

## D19. Moving cache entries between machines

- `rargo --rargo-export DIR` / `--rargo-import DIR` (root only) copy all
  traces and the blobs they refer to. Imported blobs are re-hashed, so an
  export cannot place wrong bytes under a hash; the traces themselves are
  trusted, which is why import is restricted to root.
- This is the transport used to demonstrate phase 3's criterion. The worker
  protocol of phase 4 moves content by chunk and is the efficient path.

## D20. Verify mode and the drift check

- `RARGO_VERIFY=1` re-runs every unit that came from the cache from nothing
  (no cache, no incremental seed) and compares outputs. Differences are
  reported as warnings and counted in `--rargo-status`.
- rustc's incremental mode names the object files inside an rlib after the
  random compilation session, so two otherwise identical compilations give
  different rlib bytes (measured: only the member names and the table that
  lists them differ; object contents and `.rmeta` are identical). Verify
  therefore compares rlibs of incrementally compiled units by member content
  with the session component of the name removed. This also means a
  recompiled-but-unchanged library still relinks its binaries; making the
  names stable is a phase 5 item.
- The drift check (`rargo --rargo-drift-check`, and every six hours in the
  background for worktrees whose HEAD moved, when the machine is idle)
  clones HEAD into a scratch directory with `git clone --shared`, builds it
  with the toolchain's own cargo unsandboxed at low priority, builds it with
  rargo in verify mode, and reports if the two disagree on success or if any
  cached unit did not reproduce.

## D21. Workers

- Transport: our own reliable message layer over UDP (`net.rs`). Messages
  are independent (no head-of-line blocking between a bulk transfer and a
  file-system request), fragments are acknowledged individually, congestion
  control is slow start plus additive increase with halving on timeout.
  Reason: DESIGN asks for a stateful UDP session; "protocol conventions" are
  on the do-not-care list.
- The worker traces; the server predicts. With the request the server sends
  the answers to every lookup in the action's previous trace, evaluated
  against the current view, and pushes the content of every file that trace
  read which the worker is not known to hold. For an action that has never
  run it borrows the non-worktree, non-cargo-home part of the newest trace of
  the same program (every rustc reads the same sysroot) and adds the outputs
  of the action's dependencies. Anything not predicted is one request away.
- Content moves as chunks: `ar` archives are cut at members, ELF files at
  sections, the rest by a rolling hash (16/64/256 KiB). Blobs stay whole on
  disk at both ends; an index maps chunk hashes to places inside blobs.
  Whatever arrives is re-hashed as a whole before it is trusted.
- Dispatch rule: only when no local core is free, a worker has one, and
  (bytes the worker lacks / assumed bandwidth + round trips x measured RTT)
  is less than (queue length / cores x average action time).
- A worker failure is never the action's failure: the action is rerun
  locally. Output may then be shown twice.
- Remote compiles get no incremental seed and save no state. Planning,
  host-network actions and flake-rooted actions always run on the server.
- Test rig: server and workers in separate network namespaces joined by a
  bridge inside the server's namespace, `tc netem` on both ends of every
  link. Nothing is configured in the host's own namespace.

## D22. The rustc fork: what it changes and how rargo recognises it

The fork is optional (rargo works with any toolchain) and small: three
patches on rustc 1.100.0 (`forks/rust`).

- **Crate hash that ignores what dependents cannot see.** In 1.100 the crate
  hash is a digest of the encoded metadata plus the hash of the whole HIR.
  Measured on a three-function library: every edit, including one that
  changes a single literal inside a private function body, changed the
  `.rmeta`, so every dependent reran. The fork leaves three things out of the
  hash: span byte offsets (written fixed-width so nothing after them moves;
  the file, line and column of both ends are hashed instead), the
  source-file tables, and the HIR hash (replaced by the hash of global
  assembly items, the one kind of item that leaves no trace in metadata).
  After the patch the same edits leave the crate hash alone when they keep
  the line numbers of everything after them; adding a line inside a body
  still changes it, and has to, because dependents bake line numbers of
  inlined and generic code into their own output.
- **Measured on real history, it does not pay.** `dev/replay.py` moved a
  worktree through the last 30 first-parent commits of rust-analyzer and ran
  `rargo build -p rust-analyzer` at each, once with the fork and once with
  stock 1.100.0, each with a fresh store. Over the 29 commits after the
  first: 350 actions with the fork, 351 without (one commit saved one
  action). Real commits add or remove lines, and line numbers are in the
  hash on purpose. The fork is not recommended for speed; it stays
  available, optional, for the other patches and for `check` as a
  build (D23). Making line numbers leave the hash would require that no
  dependent's output contain a dependency's line numbers (inlined code's
  debug info and panic locations do), so it was not attempted.
  The swarm with body-only second edits (its favourable case, D23)
  agrees: stock 1.100.0 under rargo 109.1 s wall and 1,455 CPU-s, the fork
  139.3 s and 1,749 CPU-s; the fork's slower compiler outweighs what it
  saves.
- **rargo observes fork metadata by crate hash.** An `.rmeta` whose header
  carries the fork's marker byte is recorded in traces as its crate hash
  instead of its content hash (`Content::metadata_identity`). Bytes still
  differ between two such files (offsets, the source file's content hash),
  so without this the compiler change would buy nothing.
  Known cost: a dependent that was not rebuilt keeps debug info whose
  source-file checksum for the dependency's file is the old one, and cached
  warnings of a dependent never get refreshed line numbers from the
  dependency (they cannot have changed, by construction).
- **Reproducible rlibs in incremental mode.** Member names no longer contain
  the per-invocation random string. Verified: two incremental compilations
  from scratch give byte-identical rlibs with the fork and different ones
  with stock rustc.
- **`-Zno-codegen` accepted on stable.** Needed by D23; `RUSTC_BOOTSTRAP`
  cannot be used because it changes the crate hash.
- **Recognition.** The fork's sysroot carries a marker file,
  `lib/rustlib/rargo-fork`; the planner reports it in the plan. Nothing about
  `rustc -vV` changes, so cargo's own fingerprinting is unaffected. The
  toolchain is chosen the usual rustup way (`RUSTUP_TOOLCHAIN=rargo`,
  `rargo +rargo`); rargod never substitutes it silently, because building
  with a different compiler than the repository asks for is drift.
- **Build.** Distribution settings (thin LTO, one codegen unit, jemalloc)
  but no profile-guided optimisation, which needs the full `opt-dist`
  pipeline and a from-source LLVM. Measured cost: `cargo check` of
  rust-analyzer takes 91 CPU-seconds with the fork against 64 with the
  official 1.100.0 (1.4x). All fork measurements therefore compare the fork
  with itself (feature on and off) or say which compiler was used.

## D23. `check` as a build that stops at metadata (fork only)

With the fork, `rargo check` asks the planner for the *build* plan and takes
each unit only as far as the request needs: not at all, to metadata
(`-Zno-codegen` appended to the build's own command line, which yields
byte-identical metadata to the full build), or in full (proc macros, build
scripts and what they link). A metadata-only unit first looks for the full
build's cached result. Linked targets are type-checked the way `cargo check`
does (`--emit=metadata`, libraries passed as `.rmeta`).

Why: stock cargo's check and build are two disjoint sets of artifacts, so a
workspace that is both checked and built compiles every crate's front end
once per mode and stores both.

Measured on rust-analyzer with the fork compiler, cold store each time:

| | check as build | cargo-style check |
|---|---|---|
| cold `check` | 47.3 s, 128 CPU-s | 31.4 s, 103 CPU-s |
| `build` after it | 55.1 s | 58.3 s |
| `check` after a `build` | 0.03 s | 31 s (a full cold check) |

So it is a trade: the first check of a crate is slower (the build's metadata
includes optimised MIR for generic and inlinable functions, which
`cargo check` skips), a check of anything already built or tested is free,
and a build after a check gains almost nothing, because rustc cannot resume
code generation from a metadata-only compile of a crate. See STATUS for
which way the default was set and the swarm numbers behind it.

**Default: off** (`RARGO_CHECK_AS_BUILD=1` turns it on per request). The
20-agent rust-analyzer swarm with body-only second edits, fork compiler
both times (`dev/bench-fork.sh`): check as a build 166.8 s wall, 1,941
CPU-s, 54.2 GB; cargo-style check 139.3 s, 1,749 CPU-s, 39.4 GB. Every
agent checks before it builds, so the costlier first checks are paid many
times and the free check-after-build rarely comes up.

Not done, and why: making the metadata-only compile's incremental state a
seed for the full compile (so that a build after a check only generates
code). The compiler discards work products it did not reuse in a session,
so alternating check and build would throw away unchanged object files each
time; keeping the two lineages separate, as stock cargo also does, reuses
more.

## D24. Cold tier

Whole blobs not used for 30 minutes (`--cold-after-min`) and not linked into
any worktree are cut into content-defined chunks; each distinct chunk is
kept once, zstd-compressed, as a file under `store/chunks`; the blob file is
deleted. Anything that asks for a cold blob gets it reassembled and
re-verified against its hash first. `rargo --rargo-compact` does it now for
everything.

Why: `rargo --rargo-incr-stats` on the store of the 20-agent Ruff run showed
21.8 GiB of incremental state, 15.8 GiB of it `dep-graph.bin` files of which
no two are identical, but which shrink to 4.2 GiB as chunks; object files
were already shared at file level (2.6 GiB logical, 0.23 GiB stored).
Compacting that store took it from 28.8 GiB to 3.9 GiB in about a minute.

This is also the decision on the phase 5 item "an incremental state format
that deduplicates well between snapshots": the existing format already
deduplicates 3.8x under content-defined chunking plus compression, with no
compiler change, and "per-codegen-unit object caching" is what rustc's
incremental mode plus the file-level store already give (11x on object
files). A new on-disk format in the compiler was not written; nothing
measured asks for it.

Chunks are one file each rather than packs: simpler to collect (mark from
the cold blobs' recipes, delete the rest) and the count stays modest
(64 KiB average). Eviction treats a cold blob as costing what it added when
it went cold.

## D25. Documentation builds

rustdoc keeps files that cover every crate (the search index, `crates.js`,
the source-file list) in the output directory and extends them on each run.
Stock cargo lets the runs race under rustdoc's own lock on one real
directory. An action in rargo sees only what it is given, so two rustdoc
runs that do not see each other would each write a one-crate index.

- The rustdoc runs for one output directory form a chain: each sees the
  output of those before it and its files win over theirs. The order is
  dependencies first, then unit index, which is also an order cargo could
  have run them in.
- Cost: rustdoc runs for one directory do not run in parallel, and changing
  one crate's documentation reruns the runs after it in the chain. rustdoc's
  mergeable cross-crate info would remove both, but is unstable in 1.100.
- All files the runs wrote are linked into the real `target/doc` when the
  build finishes (before this decision nothing was; found by a test).
- `doc --open`: the planner cannot open anything, so it records the opener
  as a step; the client starts it outside any sandbox with real paths. The
  browser is cargo's `doc.browser`, else the client's `$BROWSER`, else
  `xdg-open`.

## D26. Worker link authentication

A worker runs whatever commands its server sends, and the server believes
the results, so the link is authenticated: `--worker-key FILE` on both
ends, and every datagram carries a 16-byte keyed BLAKE3 tag; datagrams
without a valid tag are dropped before parsing. rargod refuses to listen
for workers, or to become one, on a non-loopback address without a key.
Action ids start at a random value per server start, so traffic recorded
from an earlier session cannot answer for an action in this one. The link
is not encrypted: build inputs and outputs travel in the clear, which fits
a private build network and nothing else.

## D27. Per-action cgroups

When rargod runs in a cgroup systemd delegated to it (`Delegate=yes`), each
action gets its own cgroup: CPU time is read from it, and memory is the
peak, sampled every 100 ms, of the anonymous memory of all the action's
processes (never less than the largest process's peak RSS). The memory
figure feeds the scheduler's admission. Without delegation the old `rusage`
figures are used. Only the memory controller is enabled for the children:
with the CPU controller the kernel would share CPU per action instead of
per thread. The child moves itself into the cgroup before it forks anything
(the first version moved it from outside, after the fork, and lost most of
the CPU time of anything that started quickly).

## D28. Incremental state and workers (replaces the last point of D21)

A compile sent to a worker now gets its incremental seed like a local one
(the seed is part of the view the server answers lookups from) and its new
state comes back with the outputs and is stored. Before this, any crate
compiled remotely lost its state, and the next compile of it anywhere
started from an older one.

## D29. cargo's closing messages

rargo prints cargo's future-incompatibility warning with the affected
packages when the compiler reports any, including on a replayed build. It
does not write cargo's report file, and says so in the note.

`unused_dependencies` (on by default in cargo 1.100): rustc's per-unit
`unused_extern` reports are collected, replayed ones included. When cargo's
own grouping (package x dependency kind; dev-dependencies are never linted)
leaves an unused extern in a local package, the planner is run once more
with the reports in `__RARGO_UNUSED`; its stub executor feeds them through
cargo's normal stderr handling, and cargo's post-build lint prints exactly
what stock cargo prints (checked word for word in e2e). Why this way:
reimplementing the lint would mean reimplementing its manifest snippets and
lint-level rules; replaying into cargo reuses them. Cost: one planner run,
cached, only on builds where the warning is due.

`.d` files: written next to uplifted artifacts with cargo's content (the
rustc dep-info of the unit and its local dependencies, plus build scripts'
`rerun-if-changed`), checked equal to stock cargo's in e2e;
`build.dep-info-basedir` is honoured (plan format 5).

Not reproduced: `--timings` (the HTML report). rargo's own per-action
times are in `--rargo-status` and `RARGO_EXPLAIN`.

## D30. Workers after measuring them

The first worker benchmark (rust-analyzer cold `build`, server and workers
in namespaces, 1 ms each way, 0.1% loss, 1 Gbit) showed workers making
builds slower: 241.9 s with a fresh 8-core worker against 41.9 s for the
8-core server alone, and 51.8 s with a warm one. Four changes, each from a
measurement:

- **Transport.** `netbench` across the same link: 18.8 MiB/s. Losses were
  only detected by timeout, and each halved the window. Now a fragment is
  resent once three later ones are acknowledged, a loss shrinks the window
  to 0.7x at most once per round trip, and the window grows by 8 fragments
  per round trip: 72 MiB/s. A timeout still halves.
- **Hedging.** The fresh-worker trace: the first 8 actions went to the
  worker, which spent about a minute fetching the toolchain while every
  local core sat idle waiting for them. A dispatched action now also runs
  locally as soon as a local core falls idle, if the worker has produced
  no output for it yet; the first to finish wins and the other is
  withdrawn. The local copy asks for a core at `Priority::Idle`, which only
  gets a core no other request wants and is not counted as queued work.
  Hedging stops once the worker's output has started (showing it twice,
  or announcing metadata twice, is worse). Fresh worker: 241.9 s to 44.0 s.
- **Compression.** Fetching outputs was most of the remaining overhead
  (12.2 s over 59 remote actions). Messages over 4 KiB are zstd level 1
  when that is smaller; a 478 MB rlib is 81 MB. Fetch time 12.2 s to 5.6 s.
- **Cost model.** The guessed 300 round trips per never-run action are
  replaced, once a worker has run three actions, by its measured overhead
  (moving average of total minus run time), and the bandwidth by what
  output fetches achieve. `RARGO_DISPATCH=eager` (dispatch whenever no
  local core is free) was measured against it and is no better.

What it does not change: rust-analyzer's and Bevy's cold builds do not
scale with cores past about 8 even locally (rust-analyzer local-8 41.8 s,
local-12 41.1 s, local-16 40.6 s; Bevy libraries with stock cargo 96.8 s at
8 and 86.5 s at 16), because the crates on the critical path use the
machine's cores for their own code generation, and a worker can only take
whole actions. At 4 cores per machine, where cores still matter (local-4
53.1 s, local-8 41.8 s), one warm worker gives 50.2 s: it takes about 38 s
of compile time, but most of what extra local cores buy is parallelism
inside the critical crates. The phase 4 criterion is therefore measured on
the workload that does have parallelism across actions, the 20-agent swarm
(STATUS).

## D31. Remote compiles run without incremental state (replaces D28)

The 20-agent swarm with a worker (`dev/bench-worker-swarm.sh`, server 8
cores, worker 8, the D30 link) fetched 3.6 GB back from the worker: remote
compiles of workspace crates shipped their seed out and their new state
(hundreds of MB per crate with debug info) back. Swarm wall time: local-8
171.1 s, 8+8 181.7 s. Remote compiles now see an empty incremental
directory and their state is not brought back; the crate's next compile
seeds from the latest local state. `RARGO_REMOTE_INCR=1` restores D28.

Measured after the change, four runs of 8+8: 159.6, 173.8, 172.4, 175.5 s,
against local-8 171.0 s and local-16 115.4 s. One of the four had a failed
step, which did not recur in the next three; its logs were lost to the next
run's cleanup, so the swarm script now records failed steps with the end of
the agent's log. The worker does not reliably shorten the swarm: the
dispatcher keeps most actions local because their inputs (the metadata of
every crate an edit touched, tens to hundreds of MB per compile in
rust-analyzer) take as long to ship over 1 Gbit as waiting for a local
core. That is the dispatcher being right, not wrong.

## D32. What rargo leaves in `target/`, so that stock cargo keeps working

Found by the no-drift run (`dev/nodrift.sh`): after rargo had built and
tested rust-analyzer, Ruff and Bevy, stock `cargo build` worked but stock
`cargo test` failed in all three: "output file ... is not writeable". rargo
had placed test executables at their own paths inside the build directory
as hard links into the store; store files are read-only, and the mode
belongs to the shared inode. A writable link would have been worse: rustc
would then have written into a store file other worktrees share.

- Names cargo itself links (`target/debug/app`, `libfoo.rlib`, the
  `--artifact-dir` copies) stay hard links into the store: cargo removes the
  old file before it links a new one, so it never writes into them.
- An executable at the path rustc writes it to is placed only when the
  user asked for executables (`test --no-run`, `build --tests`), and then
  as a file of its own (a reflink where the filesystem has them, a copy
  otherwise), owned by the user. When rargo runs the tests itself they are
  not placed, and a store link left at that path by an earlier rargo is
  removed.
- Generated documentation is placed as files of their own too: rustdoc
  rewrites its shared files in place.
- e2e now runs stock `cargo test` and `cargo doc` after rargo in the same
  `target/`.

The drift check on Ruff also reported `tikv-jemalloc-sys`'s build script as
producing different output on a clean run: autoconf's `config.log` in
OUT_DIR, which records times and process ids and which nothing reads. A
verify difference now counts only if some unit of the build read one of
the differing files (looked up in the units' traces, under the keys with
declared variables folded in); otherwise it is reported as a note. What a
build script prints to cargo is now compared as well, which it was not
before.

## D33. Put the content store on a separate filesystem

Use `rargod --store DIR` to move blob, chunk and scratch I/O to a
reflink filesystem while keeping the database, socket and sandbox mount
under `--state`. The default store is `<state>/store`. Keep each store
paired with its state directory because the database describes the files
in that store.

Keep worktrees on the store filesystem when shared extents matter.
Placement into an ext4 worktree from the btrfs store makes a separate copy
because neither reflinks nor hard links can cross those filesystems.
`dev/e2e.sh` uses `RARGO_E2E_ROOT` for both its store and scratch worktrees
and keeps its daemon state under `RARGO_WORK`.

On this machine, btrfs was used because `mkfs.xfs` was unavailable.
`filefrag` confirmed shared extents for the app and integration test
executables on btrfs. After `build` and `test --no-run` in two worktrees,
the filesystem used 56.09 MiB more on btrfs and 102.95 MiB more on ext4
(`df` deltas). The corresponding aggregate `du` counts were 122.69 MiB
and 104.83 MiB. `du` counts shared extents once per inode and counts hard
links to an inode only once.

## D34. Remote first: the client syncs, the server does everything

The first implementation (D1 to D33) had `rargod` on the client's machine:
it opened worktrees on its own disk through libgit2 and inotify, was
reached over a Unix socket, ran actions as the calling user, and
hard-linked artifacts from its store into `target/`. The user had asked for
the opposite: clients that only sync files and environment, on any machine,
with all analysis, scheduling and building on a remote server ("remote
cargo"). `DESIGN.md` had misread "builds run on the same machine the rargo
clients talk to" as "the clients are on that machine".

What changed, and what did not:

- Unchanged: the planner (cargo fork), sandbox and tracing, trace cache,
  in-flight deduplication, scheduler, shared incremental compilation,
  store, cold tier, workers, verify mode.
- Replaced: how the server learns a worktree's content (D35, D36), the
  transport and who may connect (D37), where the environment comes from
  (D38), how results reach the worktree (D39), and everything that used to
  open the real worktree: fetching (D40), flake evaluation (D41), the drift
  check (D42), incremental seed selection (D43).
- Gone from the server: libgit2, inotify, the git-object-id table, the
  Unix socket, peer credentials.

Earlier decisions this replaces in part: D6 (actions ran as the client's
user; now as the server's build user), D7 (clean files were identified by
git object id; now every file by its content hash, computed on the client),
D10 (the client's `PATH`, `HOME` and `RUSTUP_HOME` were given to actions;
now the server's), D12 and D13 (writes into the worktree and `target/` were
done by the server on the shared disk; now sent to the client), D14, D19
(export and import were for root; now for users marked `admin`), D20, D32,
D33 (worktrees no longer share a file system with the store, so reflinks
only matter inside the server and inside the client's cache).

There is one code path. A server on the client's own machine is reached
over the loopback interface like any other; there is no local mode.
Reason: two paths would mean the remote one, the one that was asked for,
is the less exercised.

## D35. A worktree is a content-addressed tree; everything in it is synced

- A directory is a manifest: entries sorted by name, each a file (blake3 of
  its content, size, executable bit), a symlink (target) or a directory
  (hash of its manifest). Manifests are stored in the content store as
  blobs under the hash of their encoding, so they are deduplicated, evicted
  and collected like file contents, and a client is asked again for
  anything that has gone.
- The client sends the root hash. The server walks down level by level,
  asking for the manifests it lacks, then for the file contents it lacks.
  It remembers, per store generation, which subtrees were complete, so an
  unchanged tree is not walked again.
- The build starts when the whole tree is in the store. Rejected: starting
  at once and fetching contents from the client as actions open them. It
  would shorten the very first sync of a repository and nothing else, and
  it would make every snapshot depend on a client staying connected.
- Synced: everything except `.git` (D36), any directory holding a
  `CACHEDIR.TAG` (cargo writes one into every target directory, and the
  client writes it into the ones it creates), `target` at the root, special
  files, and what `.rargo.toml` lists under `[sync] exclude`. Git-ignored
  files are synced. Reason: Bevy ignores its `Cargo.lock`; without it the
  server would resolve dependencies afresh and build other versions than
  stock cargo does on the client, which is drift. Ignored generated sources
  are the same case. The cost is that a large ignored directory is hashed
  once on the client and uploaded once; `[sync] exclude` is the way out.
- The client has no daemon and no file watcher. Every command scans the
  worktree: one `lstat` per file, in parallel, against an index of what was
  hashed before (size, mtime, ctime, inode). A file whose timestamps are
  within 50 ms of when it was hashed is hashed once more on the next scan,
  because it may have changed again without its timestamps moving. Reason
  for no watcher: the user asked for a dumb client; measured, the scan of
  rust-analyzer is a few milliseconds (STATUS).
- Uploads: small files in batches, read and compressed in parallel; files
  over 1 MiB as compressed parts. The server re-hashes everything it
  receives. If a file no longer has the content the scan saw, the client
  says so and starts over (five attempts).
- Two clients missing the same content: one uploads, the other waits for
  that upload and is only asked itself if the first fails.
- The newest snapshot of each worktree is kept from garbage collection, so
  an idle worktree does not have to upload again after `--rargo-gc`.

## D36. `.git` is not synced; a per-commit stand-in is

- The first remote build of rust-analyzer printed `Could not find
  .git/HEAD`: its build script, like Ruff's, asks git for the commit to put
  in `--version`. With the daemon on the client's machine the real `.git`
  had been visible. Not showing one would make remote builds differ from
  stock cargo's for every project that stamps a version.
- The client builds, once per commit, a directory that is to git a
  depth-one clone without file contents: `HEAD`, the branch it names, a
  `shallow` file, a pack with the commit, its trees and the targets of
  tracked symlinks (git compares those by reading the object), and an index
  made with `git read-tree` from the commit. It is grafted into the synced
  tree as `.git`. rust-analyzer's is 420 KiB and takes 9 ms to make.
- Works in the sandbox: `rev-parse`, `log -1`, the branch name, `status`,
  `describe --always --dirty`. Does not: history (`describe --tags` fails
  as in any shallow clone, which build scripts already meet in CI) and file
  contents from git.
- It depends only on HEAD, never on the user's index, so it is shared by
  all worktrees at a commit and does not change when the user stages
  something.
- Its `core.abbrev` is set to the length the client's repository
  abbreviates to. Found by comparing version strings: git picks that length
  from the number of objects, so the stand-in said `37721d1` where the real
  repository says `37721d141`.
- Known inexactness: in a repository that has files committed with CRLF
  under an `eol=lf` attribute (rust-analyzer has two), `git status` in the
  sandbox reports them modified, as git does wherever it has to re-read
  such a file. `--dirty` is then set where it is not on the client.
- `[sync] git = false` in `.rargo.toml` leaves the sandbox without a
  repository.
- Rejected: syncing the real `.git` (size); syncing nothing (drift);
  a depth-one clone with contents (doubles the upload for one feature).

## D37. Transport and who may connect

- One TCP connection per command. Rejected: the workers' reliable-UDP
  layer (TCP is right for a stream to a client behind arbitrary networks);
  SSH as the transport (hundreds of milliseconds of connection setup per
  command against a no-op build that takes tens).
- Noise `NNpsk0` (X25519, ChaCha20-Poly1305, BLAKE2s) with a pre-shared key
  per user: mutual proof of the key, fresh session keys, no certificates to
  manage. The key file on the server has a line per user, `name secret
  [admin]`. Without a key file rargod only listens on loopback and accepts
  anyone on the machine; it refuses to listen elsewhere without one.
- The request rides in the first handshake message when it fits (it
  usually does), encrypted under the pre-shared key, so the server starts
  working after one round trip. That one message is not forward secret; it
  carries the command line and the allow-listed environment, and someone
  who recorded it could replay it: the server would start that build
  again and the replayer, lacking the session keys, would learn nothing.
- Users of one server share one cache and are trusted with each other's
  source: a team tool. `admin` gates garbage collection, compaction, and
  cache export and import (which name directories on the server).
- The client finds the server in `RARGO_SERVER`, `RARGO_USER`, `RARGO_KEY`
  or `RARGO_KEY_FILE`, else in `~/.config/rargo/config.toml` (`server`,
  `user`, `key`, `key-file`). `RARGO_FALLBACK=cargo` runs plain cargo when
  the server cannot be reached.
- Output to the client is one ordered queue; file contents in it are
  additionally held to a 32 MiB window, so a slow link slows the sender
  down instead of filling the server's memory.

## D38. The environment is the server's, steered by the client

- `PATH`, `HOME`, `RUSTUP_HOME`, certificate paths and `NIX_*` given to
  actions are rargod's own. The client's values name things on another
  machine.
- The client sends, unasked, variables on a fixed allow-list (cargo, rustc
  and rustdoc settings, `RUSTUP_TOOLCHAIN`, locale, colour, `RARGO_*`) and
  those `.rargo.toml` names under `[env] pass`. Never sent: `CARGO_HOME`,
  registry tokens, rustc wrappers, rargo's own key.
- Any other variable reaches an action only after an action declared it
  (as before, D10). The server then asks the connected client for its
  value, and the client remembers the name and sends it unasked from then
  on. Reason for asking instead of sending the whole environment: the
  client's environment holds secrets no build has asked for.
- The cargo home is the server's, one for all clients: one copy of each
  downloaded crate, shared by everyone. A client's own `config.toml` and
  `credentials.toml` are sent with each request and laid over it for that
  request only (file mounts in the sandbox; a private directory linking to
  the shared downloads for unsandboxed fetches). Not synced: cargo
  configuration in directories above the worktree other than the cargo
  home; paths in a configuration that name things on the client.
- Actions run as one user of the server's choosing (`--uid`, `--gid`;
  rargod's own by default). Who the client is does not enter any key.
- Messages and arguments are translated between `/w` and the client's
  worktree path, and `/c` and the client's cargo home path, so the user
  sees and types their own paths.

## D39. Results

- The server offers files by hash and size; the client answers with the
  hashes it lacks; contents follow. A file already in place is recognised
  by a `stat` against what the client recorded when it placed it.
- What cargo would hard-link into place (`target/debug/app`,
  `libfoo.rlib`) is kept in the client's cache (`~/.cache/rargo/blobs`) and
  placed by reflink where the file system has them, by read-only hard link
  otherwise, by copy across file systems. Twenty worktrees on one machine
  therefore hold one copy of a binary and download it once. A cache entry
  is re-hashed before it is used: someone may have written through a link.
- What other tools rewrite in place (an executable at the path rustc
  writes it to, documentation, anything written into the source tree) is
  always an ordinary writable file of its own (D32's finding, now enforced
  on the client).
- Test executables are not brought over unless asked for (`test --no-run`,
  `build --tests`): tests run on the server.
- `rargo run` and `doc --open`: the server sends the command; the client
  runs it, with its own environment plus what cargo adds. The mount
  namespace that makes `/w` resolve at run time is entered on the client,
  as before.
- A target directory outside the worktree works as long as the client
  names it (`CARGO_TARGET_DIR`); artifacts are written where the translated
  path says. The client writes wherever the server says: the server
  already decides what the binaries it hands over do.
- Cache entries unlinked for three days are removed from the client's
  cache, checked at most once an hour.

## D40. Fetching happens on the server

Planning runs offline (D14). When cargo says it needs the network, the
server writes the snapshot into a scratch directory, runs `rustc -V` there
(which makes rustup install the toolchain the repo names) and the forked
cargo's `fetch`, unsandboxed, as the build user, with the client's cargo
configuration and into the shared cargo home; one fetch at a time. A
`Cargo.lock` cargo wrote or changed is sent to the client's worktree, and
the build continues on the snapshot with that file applied, without
another round trip. A server without network access needs its cargo home
filled beforehand.

## D41. Flake evaluation

The flake's defining files (`flake.nix`, `flake.lock`, `.nix` files at the
top and under `nix/`, and now `rust-toolchain` and `rust-toolchain.toml`,
which devShells commonly read) are written into a scratch directory and
evaluated there; the result is remembered under their hash as before. A
flake that reads other files of the repository during evaluation fails to
evaluate; the error names the switch (`flake.enabled = false`).

A binary built in a flake sandbox is linked against the flake's libraries.
`rargo run` starts it on the client, which therefore needs those store
paths (entering the same devShell there provides them).

## D42. The drift check, both halves

- On the server (`--rargo-drift-check`, and in the background for each
  worktree's newest synced state): the snapshot is written to a scratch
  directory and built there with stock cargo, and built by rargo in verify
  mode. It checks exactly what a client synced, uncommitted changes
  included, where the old check cloned HEAD.
- On the client, `dev/nodrift.sh` and the e2e script run stock cargo in the
  worktree and `target/` rargo has used, and rargo again afterwards.

## D43. Incremental seeds without the repository

The server cannot ask git whether one commit is an ancestor of another. The
client sends HEAD and its last 64 first-parent ancestors (asked of git once
per HEAD and remembered), and a stable identifier of the worktree (host
name and path). Seed choice is as in D16 with "ancestor" meaning "in that
list". A hint only: any state is a valid seed.

## D44. The worker link is encrypted

Supersedes the last sentence of D26. On a keyed link (any link that leaves
the machine) each connection starts with a Noise `NNpsk0` exchange over
the worker key, carried as the first message each way on the existing
reliable transport, and every message after it is sealed with the
connection's keys (ChaCha20-Poly1305). Recorded traffic stays unreadable
if the key leaks later; a session recorded earlier cannot be replayed
because the keys are new each time.

- Messages, not datagrams, are encrypted: the transport delivers messages
  in arrival order, so each message carries the nonces it was sealed with
  and the sender never reuses one. The per-datagram keyed tag stays, so
  the transport's own headers (connection id, message id, fragment index)
  are still authenticated, though visible.
- What an observer still learns: message sizes and timing.
- A worker that gets no answer (server not up yet, or another key) keeps
  trying and says so, instead of exiting.
- Measured cost: none visible (rust-analyzer cold build through an 8-core
  server with one 8-core worker, 306 MiB sent to the worker: 46.5 s, in
  the range of the unencrypted runs).

## D45. Zed is back in the benchmark set (reverses D9)

The user asked for Zed in all acceptance work. What D9 gave up on is
handled this way:

- **System libraries.** This machine lacks several of Zed's (glib and
  others), and installing packages on a shared box is out. Zed ships a
  flake, and building in a flake's dev shell is what rargo does by default,
  so rargo evaluates it on the server; stock cargo, for the comparison,
  runs inside the same dev shell (`dev/zed-cargo.sh`, the shell's real
  cargo rather than Zed's timing wrapper around it).
- **`flake.files`.** Zed's flake reads repository files beyond the ones
  D41 writes out for evaluation (`Cargo.lock`, `script/cargo`, fonts,
  patches under `nix/`). A new policy key names them; they are written out
  with the rest and are part of the evaluation's cache key. Evaluating
  with the whole worktree visible was rejected: every edit would then
  re-evaluate the flake.
- **What a flake sandbox sees of the server.** Found by Zed's `webrtc-sys`
  build script, which downloads a library: a flake sandbox's synthetic
  `/etc` had no resolver configuration and no certificates, so an action
  the policy gave the network still could not use it. `/etc/resolv.conf`
  is now shown, name lookups use DNS, and the server's certificate bundle
  is shown at `/etc/ca-certificates.crt` with `SSL_CERT_FILE` and its
  relatives pointing at it unless the flake sets them.
- **Store paths in values.** `-fmacro-prefix-map=/nix/store/a=/nix/store/b`
  parsed as a store path ending in `=` (legal in names), and one of the two
  is a placeholder that does not exist. Paths that do not exist are tried
  without trailing punctuation and otherwise left out.
- **Lint data.** The unused-dependency data handed to the planner as one
  environment variable passed the kernel's 128 KiB limit per string on a
  workspace of Zed's size. It now travels in as many variables as needed
  (cargo fork patch 0007).
- **Benchmark-local pieces**, none of them in rargo: a commit in the Zed
  checkout adding `.rargo.toml` (`flake.files`, network for `webrtc-sys`,
  `HTTPS_PROXY` passed through); `GIT_LFS_SKIP_SMUDGE=1` for Nix, because
  one git dependency has LFS files that this machine's git-lfs cannot
  fetch and that no build needs; and `dev/rigproxy.py`, which gives the
  namespaced rig's server a way out to the internet over a unix socket
  without touching host network configuration.
- **Twenty stock target directories do not fit.** One is 37.5 GB after a
  check and a build; twenty with tests would need about 900 GB and the
  disk has 714 GB free, shared with live services. Stock cargo is measured
  with 10 agents, rargo with 10 and with 20, and the 20-agent stock
  figures are given as twice the 10-agent ones (the stock runs are bound
  by the 24-core cap, so work scales with agents); STATUS says which
  figures are measured and which extrapolated.

## D46. The server makes each cargo home overlay once

Found by the swarm reruns: three and eight failed steps in two runs,
"setting up the cargo configuration: File exists". Twenty clients with the
same cargo configuration arriving at a fresh server raced to create the
overlay directory's links. Creation is serialised. The earlier clean runs
were luck.

## D47. Dispatch to workers: first free core wins

Replaces the cost model of D30. Three things were wrong with deciding
once, on arrival, whether an action goes to a worker:

- "A local core is free" was judged by the number of running actions. A
  compiler's code-generation threads take cores from the same jobserver
  without being actions, so the server looked idle exactly when it was
  busiest. It is now judged by free tokens.
- An action that found the worker full queued for a local core only, and
  could never take a worker core that came free a moment later. It now
  waits for both and takes the first.
- The wait for a local core was estimated from the queue length, which is
  zero while helper threads hold every core; most actions were kept local
  as "transfer too costly". The estimate is gone. An action is kept from a
  worker only when its known inputs (from its last trace) would take
  longer to send than an average action takes to run.

Hedging (D30) stays: a dispatched action also runs locally when a core
nothing else wants falls idle before the worker has said anything. A file
a worker has produced and announced early (an `.rmeta`) is recorded as
held by that worker at once, so dependents sent there do not get it pushed
back.

## D48. A client that goes away takes its work with it

Before, a disconnect stopped the request's task but not the unit tasks it
had spawned: queued and running actions carried on and their results were
cached. The same was true of a hedged action's losing copy (D30): "dropping
the remote run withdraws the action" only forgot it on the server, while
the worker kept compiling on a core the server already counted as free.

Now cancellation follows ownership. A build's unit tasks are aborted when
the build is dropped. An action whose future is dropped kills its sandbox
(the zygote's `Kill`), waits for the process to die on a helper thread and
then tears down the action directory, its scratch files and its cgroup. A
remote run that is given up before the worker reports sends `Cancel`, and
the worker aborts the task (queued: it leaves the queue; running: its
sandbox is killed the same way).

Two consequences, both accepted:

- An action another build was also waiting for (in-flight deduplication)
  is killed with its owner; the waiting build notices, takes the slot and
  starts it again. Keeping it alive would need the action to be owned by
  nobody, which is a larger change than the case deserves.
- A compiler killed while its helper threads hold jobserver tokens does
  not return them. The scheduler already refills the pipe when nothing is
  running (it was needed for crashed compilers); until then the server
  runs a few tokens short.

Checked by e2e: a build script sleeping for ten minutes is gone within
five seconds of its client being killed, the scheduler shows nothing
running, and the next build works.

## D49. cargo's report files: future incompatibility and `--timings`

Both were gaps since D29.

- **Future incompatibility.** The compilers' `future_incompat_report`
  messages are collected per unit, rendered exactly as cargo renders them
  (`render_report`, compiler colours kept) and sent to the client, which
  owns `target/.future-incompat-report.json`: it merges the report into
  the file in cargo's on-disk format (same report again: same number; five
  kept), and answers with the number, which the server puts into cargo's
  closing warning. `--future-incompat-report` and
  `future-incompat-report.frequency` behave as in cargo (plan format 6
  carries both). Protocol 6 adds the two messages. Not reproduced: cargo's
  "update to a newer version" suggestion, which asks the registry.
  Checked by e2e: `cargo report future-incompatibilities --id 1` shows
  rargo's report, and its `per_package` text equals stock cargo's for the
  same crate, byte for byte.
- **`--timings`.** cargo's report describes cargo's job queue, which does
  not run here, so the report is rargo's own: one HTML file without
  scripts at `target/cargo-timings/cargo-timing.html` and a dated copy,
  with a bar per unit run for this build (metadata and code-generation
  phases), and a table of every unit: run or from the cache, when it was
  asked for, how long the answer took, the action's own run time, CPU time
  and peak memory. The planner no longer writes cargo's (empty) report
  while planning.

## D50. Temporary files of clients that did not finish

The client writes every file under a temporary name beside its
destination and renames it. A client stopped in between left the
temporary in `target/`. Two measures: SIGINT, SIGTERM and SIGHUP now
unlink the temporaries in use at that moment (a fixed table of paths, so
the handler only calls `unlink`) and then die of the signal as before; and
the first time a client writes into a directory it removes temporaries
whose name carries the process id of a process that no longer exists
(`kill -9`, power loss). The same sweep covers unfinished downloads in the
client's cache and half-made git stand-ins. A process id that has been
reused keeps a stale file until that process ends; nothing is ever removed
that a live process may be writing.

## D51. The drift check on a repository like Zed

The first drift check of Zed failed twice over, and neither was drift.

- **The stock half ran outside the flake.** The check built with plain
  `cargo` in the server's own environment, where Zed's system libraries do
  not exist (`glib-2.0` not found). A repository whose environment is a
  flake is now built by stock cargo in that environment too: the devShell's
  variables and its `cargo`, unsandboxed, with the variables the policy
  file passes. That is what its users run (`nix develop -c cargo build`).
- **Verify called 228 units different.** Zed's dev profile combines
  `incremental = true` with `split-debuginfo = "unpacked"`. rustc names
  each codegen unit's `.dwo` file after the compilation session, the
  object names its `.dwo`, and the identifier that ties the two together
  (`DW_AT_GNU_dwo_id`) is a hash that covers that name. Two clean builds
  by stock cargo itself differ in exactly these places and nowhere else
  (checked on a three-function crate: the session component of the names,
  and eight bytes per codegen unit in the object and in its `.dwo`).
  Verify now compares such outputs the way D43's finding already did for
  member names: files are paired by name without the session component,
  contents are compared with session names blanked, and a remaining
  difference is accepted only if it is at most eight bytes long and is one
  of the substitutions learnt from that unit's own `.dwo` pairs (packed
  split debug info: from the `.dwo` members of the archive). A difference
  in code, metadata or any other byte is still a difference. No DWARF is
  parsed; the rule is structural, and a `.dwo` pair that differs in more
  than one such place teaches nothing and is reported.

## D52. Dependencies named on the command line travel ahead of the action

A compile sent to a worker without a trace of its own (every compile of a
cold build) had only the toolchain files predicted; each `--extern` file
was fetched when the compiler opened it, one round trip per dependency
while a worker core waited. The files a command names with `--extern` are
now part of the prediction: their lookups are answered in advance and the
contents the worker lacks are pushed as the action is sent.

## D53. Hedged copies race; idle means idle; killed compilers give their cores back

Three faults in D30's hedging, found by reading it against D47 and D48:

- **The local copy did not race.** Once a local core fell idle the action
  ran here to the end and the worker's copy was abandoned, even when the
  worker was seconds from finishing; on a build's critical path that adds
  most of a compile. Both copies now run and the first to finish is the
  result. Only one may be heard: the first to produce output or announce
  metadata is kept and the other stopped at once (two announcements of
  metadata that differ would fail the build's consistency check).
- **"A core nothing else wants" was not that.** The scheduler fetched a
  token for an idle-priority request from the jobserver pipe like for any
  other, in competition with the helper threads of running compilers, and
  handed it a finished action's token directly. A hedge could therefore
  take a core from the code generation of the crate the build was waiting
  for. Idle-priority requests are now served only with a token that has
  lain in the pipe for 40 ms: a helper thread blocked on the pipe takes a
  token the moment it appears, so one still there is wanted by nobody.
- **A killed compiler kept its cores.** Stopping the losing copy (and
  D48's cancellation) kills rustc while its helper threads hold jobserver
  tokens, which then never return; the pipe was only topped up when the
  server was completely idle. Now the action's cgroup is frozen, its
  code-generation threads are counted (rustc names them `opt …`, `lto …`,
  `copy …`; each holds a token, except possibly one per compiler running
  on the compiler's own), the action is killed, and that many tokens less
  one per compiler are put back. That is a lower bound, never too many;
  whenever no action is running the pipe is settled to exactly its number
  in both directions. Workers get a delegated cgroup in the rig for this.

The timings report carries its figures as a data block, and
`dev/sim-workers.py` replays them on other machine shapes (see STATUS,
Workers).

## D54. What the worker series showed, and what was changed for it

Measured on the build of D53 (rust-analyzer cold build, remote client,
4 cores per machine): server alone 55.8 s, with one warm worker 51.0 s,
server with 8 cores 45.9 s; a one-file edit rebuilt in 20.9 s with the
worker against 10.7 s without. Four changes followed, each from a trace:

- **A crate with incremental state here is rebuilt here.** Remote compiles
  run without incremental state (D31), so after an edit the remote compile
  of a large crate was the slow one. An action with an incremental
  directory and a trace of its own is no longer offered to workers.
- **A crate's sources travel ahead.** A traced run showed about 66
  questions per remote compile, each a round trip: the crate's own source
  files, and names beside them that do not exist (`foo.rs` or
  `foo/mod.rs`?). The source tree below the crate root now goes with the
  action: its listings, declared complete so that a name not listed is
  absent without asking, and its `.rs` files. A crate root beside its
  `Cargo.toml` takes only its own directory. Questions per build went from
  about 6,100 to about 2,300.
- **Histories per kind of compile.** The "other run of the same program"
  used to predict a new compile's reads is now kept per crate type, and a
  linking compile without any history gets the toolchain's library
  directory.
- **A worker without the toolchain is not given work it cannot start.**
  The transfer guard now counts what is already queued on the link and,
  for an action that has never run, the toolchain files the worker lacks.
  The first time it trips for a worker, those files are sent; actions go
  there once the link has drained.

Where the build's time goes was read off `--timings` reports of the same
build on 4, 8 and 24 cores and with a worker: from the moment `hir-def`
starts, the build is one chain (`hir-def`, `hir-ty`, `hir`, `ide-*`,
`rust-analyzer`) that takes 30.5 to 32.2 s in every configuration. All of
what cores buy is in the wide phase before it. See STATUS for the figures.

## D55. After the second worker series; what the drift check does not call drift

**Workers.** The series at `a92e359` (STATUS) showed a warm worker taking
7.8 s off a 56.5 s build, and two regressions: a worker that had seen
nothing made the build 3.6 s slower, and the first rebuild after an edit
took 16.5 s instead of 10.9 s. Two rules:

- A crate that is compiled incrementally (a workspace member) is compiled
  on the server, first compile included. D54 kept only crates that already
  had state; but a first compile sent away leaves no state behind (D31),
  and the next rebuild of that crate was a compile from nothing.
  `RARGO_REMOTE_FIRST=1` restores D54's behaviour. The cost is that
  workers only ever get dependencies and build scripts, which is why the
  swarm (almost all workspace crates) cannot gain from them; the way out
  is to bring incremental state home, which D31 measured as too
  expensive when done in the foreground.
- An action about which nothing is known (no trace of its own, no other
  run of its program) runs on the server. A worker asked for everything
  such an action read one question at a time, toolchain included.

Measured after both: warm worker 49.2 s against 56.0 s alone and 45.2 s
with 8 local cores; a worker that has seen nothing 55.8 s; rebuild after
an edit 11.1 s against 11.3 s.

**Drift check.** Zed's check, after D51, still reported 24 units. All
three kinds were reproduced with stock cargo alone, two clean builds at
the same path:

- Anything linked while split debug info is on (build scripts, proc
  macros): about 10,000 places differ in a three-line build script's
  executable. The linker's tables are laid out by names that include the
  compiler's session.
- `wasmtime-wasi-io` (and with it `wasmtime-wasi`, `extension_host`): its
  dependency information lists the same files in a different order from
  run to run, and metadata and object code differ with it; a procedural
  macro that tracks files in hash order.
- Build scripts that compile C in parallel print the same `cargo:` lines
  in a different order.

Verify now recognises exactly these (a linking unit that has `.dwo`
outputs and differs only in linked files; a unit whose `.d` differs only
in order; a script whose sorted `cargo:` lines are equal) and reports
them as "not reproducible by the tools themselves", separately from
drift, with their number in the verdict. Linked files that differ only in
session names, debug-info identifiers and the linker's build id are
accepted as equal outright (the build id is found by reading the ELF
section table; nothing else is parsed). Anything else that differs is
still drift. The risk taken: a real difference that happens to look like
one of the three would be reported as not reproducible instead of as
drift; it would still be reported.

## D56. A remote compile's incremental state stays on the worker and comes home in the background

The first idea on the list of things not tried (STATUS, Workers). D31
found that shipping incremental state both ways in the foreground cost
more than it saved, and D55 therefore kept every workspace crate on the
server, which is why workers could not help the swarm.

- A compile on a worker sees the crate's state like a local one, and what
  it leaves is recorded as a state at once, although its files are only
  on the worker. `IncrStore` accepts a state whose files are in the store
  or held by a connected worker.
- Outputs come home in the foreground as before; state files are queued
  and fetched one at a time on an idle-priority core (putting a file
  together and hashing it costs CPU the builds are short of).
  `RARGO_STATE_HOME_AFTER_MS` delays that, for tests.
- A compile that starts from a state is offered only to workers that hold
  that state (all but 4 MiB of it). If none does, it runs on the server:
  a state is never shipped out. A first compile, with no state anywhere,
  may run on any worker.
- A compile that runs on the server while part of its state is on a
  worker fetches that part first (counted in the status text). If the
  worker has a core free and the server would have to fetch, the worker
  gets the compile even though a server core is free.
- The state's directory tree goes with the action, so the compiler's walk
  over its previous session costs no questions.
- A state whose files went away with a worker is dropped when it is next
  picked, or, if the worker dies between pick and run, the action is
  asked for again with the next best state (`SeedLost`).
- `RARGO_REMOTE_INCR=0` restores D55; `RARGO_REMOTE_FIRST` is gone.

Found on the way and fixed: requests to a peer were meant to be in flight
together and were not (the calls were lazy), so every megabyte of an
output fetched from a worker was its own round trip; a worker lease was
leaked when a compile with state at home found a free local core; and
concurrent dispatches each pushed the toolchain to a fresh worker
(1.8 GB instead of 460 MiB). `dev/e2e-worker.sh` (27 checks) covers
states left, used in place, fetched in the background and on demand, and
a worker killed with states on it.

Measured (STATUS has the tables): rust-analyzer cold build, 4 cores per
machine: 48.8 s with a warm worker and 51.9 s with one that has seen
nothing, against 56.5 s alone and 44.5 s with 8 local cores; rebuild
after an edit unchanged. The 20-agent swarm, 8 cores per machine:
139.9 s with a worker against 176.0 s alone and 122.4 s with 16 local
cores (before: 182.8, 179.8, 126.4). One agent step failed in that swarm
run with a compiler error on the worker ("required to be available in
rlib format"); see STATUS for what is known about it.

## D57. A client daemon and one long-lived session per machine (replaces D35's "no daemon" and D37's "one TCP connection per command")

The user asked for it: no command should pay for connection setup, a key
agreement or slow start, and a build should find the server already up to
date.

- **Daemon.** One per user, server, key and program version on a machine
  (the socket's name is a hash of those, so a changed configuration or a
  new binary gets its own daemon and the old one leaves when idle).
  `rargo` starts it if connecting to the socket fails: a detached process
  in its own session whose output goes to `daemon.log` in the cache
  directory. The socket is in a 0700 directory beside the cache, or in
  the runtime directory if that path is too long for a socket address.
  It exits after 30 minutes without a command
  (`RARGO_DAEMON_IDLE_SECS`), or when told (`rargo --rargo-daemon-stop`);
  `rargo --rargo-daemon-status` says what it is doing. If no daemon can
  be reached or started the command does the daemon's work itself, in
  process (`RARGO_NO_DAEMON=1` forces that).
- **`rargo` is a relay.** It decides what is cargo's business (unchanged),
  sends arguments, directory, terminal flags and its whole environment to
  the daemon, and prints what comes back. The daemon applies the
  allow-list and the policy to the environment per command; nothing of
  the daemon's own environment is used for a build. Closing the socket
  (Ctrl-C, a closed pipe) withdraws the request on the server.
- **Transport.** The reliable-UDP layer of the worker link, moved into
  its own crate (`rargo-net`). For clients a connection names its user in
  the key agreement (Noise `NNpsk0` under the user's key); from then on
  every datagram carries a 16-byte tag keyed from the handshake hash.
  Datagrams without it are accepted for the key agreement only, so an
  outsider who has seen a connection id can neither inject data nor move
  the connection. A tagged datagram from a new address moves the
  connection there. The server listens for UDP on the port it listens
  for TCP on.
- **TCP fallback.** If the UDP key agreement gets no answer in 2.5 s the
  daemon connects by TCP (the old handshake, without a request in it) and
  uses that one connection for everything. `RARGO_TRANSPORT=udp|tcp`
  forces one. Rejected: trying both at once and keeping the faster (two
  sessions to tear down on every start, for a case that is decided once
  per network).
- **Streams.** Frames carry a stream number and a sequence number,
  because the transport delivers messages in arrival order. Client to
  server there is one order over all streams, so that a request is never
  looked at before the change sent ahead of it; server to client each
  stream is ordered by itself, so one command's bulk does not hold up
  another's output. Stream 0 is the daemon's own.
- **Keepalive.** A ping every 10 s; no answer for 45 s and the daemon
  drops the link and makes a new one for the next command. A command that
  finds its link dead before it has shown anything is retried once on a
  new link. The server drops a link it has not heard from for 3 minutes.
- **Watching.** inotify on every directory the scan visits (not `.git`,
  not output directories). An event marks the worktree dirty and starts,
  20 ms later, a scan and a push. A command in a worktree that is not
  dirty and whose HEAD file is unchanged uses the last scan. Adding a
  watch after scanning a directory leaves a gap, so a scan that added
  watches is followed by one more. If a watch cannot be added (the
  per-user limit) the worktree is scanned for every command. Worktrees
  unused for four hours are dropped from the watcher.
- One command at a time per worktree places files, as cargo locks its
  build directory.

Protocol 7. `dev/e2e-link.sh` (33 checks): one daemon and one link over
many commands, an edit on the server before any command, an edit and a
build in the same instant, new directories, checkouts, the client's
address changing under the session, the daemon killed, the server
restarted, a command interrupted, no daemon, UDP dropped (TCP), and
stopping.

## D58. Sync and delivery cost no round trips

The user asked that every benchmark that crosses the client link be run
with 100 ms of round-trip time, and that sync not cost extra round trips.

- **Pushed deltas.** After a change the daemon sends an `Offer`: the
  manifests it has not sent before and, if the new file contents are
  under 1 MiB together, those contents, in one message, unasked. Above
  that (a worktree it sees for the first time, a branch switch) it sends
  the manifests with `ask`, the server answers with the contents it
  lacks, and the daemon uploads those: two round trips however deep the
  tree, where the old walk took one per level. Rejected: pushing a new
  worktree whole (twenty agents' checkouts of one commit would upload the
  repository twenty times).
- The server still verifies: a request names its root, and the server
  walks the tree it holds and asks for anything missing (D35's dialogue,
  now the fallback). The daemon's memory of what the server holds is
  dropped with the link.
- **Deliveries are not asked about.** The old `Deliver`/`Need` exchange
  cost a round trip per command, including the command with nothing to
  do. Now the server sends the listing, the contents it believes the
  worktree lacks, and `Delivered`, without waiting. It believes what it
  remembers: per worktree, the set of contents delivered so far, under a
  token the daemon stores once it has placed everything and names in its
  next request. Without a matching token (first build, server restarted)
  the server sends nothing blind and the daemon fetches what its cache
  lacks, on a stream of its own (one round trip, then only).
  Found by the tests: two commands fetching the same file at once got
  each other's pieces when fetches shared the control stream.
- `rargo --rargo-status` now counts files and manifests received, pushes,
  and files the server had to ask for.

## D59. The output directory is a file system served by the client daemon

The user's original ask, restated: the clearmake of cargo, audited builds
plus wink-in of derived objects into a dynamic view. The server side had
both; on the client, results were copied (or hard-linked) into `target/`.

- **Mount.** The daemon mounts a FUSE file system (`fuse.rargo`, the
  repo's own `fusewire`) over `<worktree>/target` at the first build in a
  worktree. The real directory stays underneath and is reached through a
  descriptor opened before mounting. Rejected: kernel overlayfs over a
  read-only FUSE lower layer (needs root or a user namespace, so other
  processes would not see it); a mount somewhere else with `target` a
  symlink to it (cargo and tools resolve and compare paths; stock cargo
  without the daemon would find a dangling link).
- **What is shown.** A table of delivered names (path, hash, size,
  executable bit, time), kept in the client's cache per worktree and
  reloaded when the directory is mounted again. A name is real if the
  directory underneath has it, else virtual if the table has it. No
  whiteouts: removing or renaming a virtual file edits the table;
  delivering a name removes a real file of that name.
- **Reading** a virtual file opens its content in the client cache,
  fetching it first if it is not there. Contents of 1 MiB and more are
  fetched by chunk: the client asks for the content's recipe, takes the
  chunks it knows from contents in its cache, asks for the rest, and
  checks the whole against its hash. The chunk index is in the daemon's
  memory and covers what it fetched by chunk; a persistent index is not
  kept.
- **Writing** (open for writing, truncation) makes the file real first:
  its content is fetched and copied into the real directory, unless the
  open truncates. Times and the executable bit of a virtual file change
  in the table.
- **Passthrough.** Real files are read and written by the kernel directly
  (FUSE passthrough) where the kernel has it, with one backing handle per
  file shared by all its opens (found by the tests: the kernel refuses a
  second, different one while the first is in use, and two processes
  reading one generated file got an I/O error). Virtual files and
  kernels without passthrough go through the daemon.
- **Lazy delivery.** A request says `lazy`; the server then sends each
  delivery as it is produced instead of at the end, with the contents of
  files up to 64 KiB that the daemon is not known to hold, and nothing
  else. Deliveries outside the mount (a worktree file a build wrote, such
  as `Cargo.lock`) are placed as files as before.
- **When not.** No mount if the command sets `CARGO_TARGET_DIR` or
  `--target-dir`, if `target` is a symlink, if the command runs in
  another mount namespace than the daemon (it would not see the mount;
  `rargo` sends its namespace with each request), without a daemon, or
  with `RARGO_OUTPUT=copy`. If mounting fails the daemon says so once in
  its log and copies. Mounting uses `mount(2)` directly, which needs root
  or `CAP_SYS_ADMIN`; going through `fusermount3` for unprivileged users
  is not implemented, so an unprivileged daemon copies.
- **Lifetime.** A daemon with mounted directories does not leave when
  idle. On `--rargo-daemon-stop`, SIGTERM or SIGINT it unmounts first. A
  mount whose daemon was killed, or that another daemon (another server
  or version) put there, is detached by the next daemon that needs the
  directory; a daemon checks that the directory still shows its own
  mount (by device number) before it delivers into it.
- **`cargo clean`.** cargo cannot remove a mount point. `rargo clean`
  (the shim) asks the daemon to let go of the worktree's output directory
  and forget what was delivered, then hands over to cargo.
- **Temporaries.** Copying clients' leftovers (`.rargo-tmp-<pid>-...`) of
  processes that no longer exist are removed from a directory when a
  delivery puts something into it.

`dev/e2e-link.sh` has the checks (27 of its 60): mounted, listed at once
without content crossing the link, fetched on run, nothing made real,
stock cargo build and test in the same directory, rargo after stock
cargo, copy mode and back, clean, a killed daemon's dead mount replaced,
stop leaves a plain directory in which stock cargo builds. `dev/e2e.sh`
passes in both modes (113 checks each).

Also in this round, for links with latency (D57's transport was only ever
run on a machine-room link):

- The transport sent a whole window at once and retransmitted in bursts;
  on the rig's client link at 20 ms each way it fell to under 3 MiB/s.
  It now paces what the window allows over a round trip (retransmissions
  included), ends slow start when round trips lengthen, grows the window
  by a twentieth per round trip and has a retransmission timeout with
  headroom over the round trip (it used to sit a hair above a steady
  round-trip time, so ordinary acknowledgements looked late and every
  round was taken for a loss). Measured with `linkperf` (an example in
  `rargo-net`): 111, 102 and 77 MiB/s at 2, 40 and 100 ms round trip on
  the 1 Gbit rig link, no retransmissions.
- The rig's netem queues were 1,000 packets, less than a 1 Gbit link has
  in flight at 20 ms: the emulated link dropped what a real one carries
  (TCP was spared because its segments are queued in batches). They are
  now sized for the link (`NETEM_LIMIT`, 100,000).
- Keepalives travel outside the daemon's send queue and outside the
  frame order, and a link counts as alive while anything arrives on it:
  a ping queued behind a large upload used to look like a dead server.

## D60. What the 100 ms campaign found

- **Twenty first syncs at once.** In the Ruff and Bevy swarms at 100 ms
  the twenty worktrees' first offers were all answered before any upload
  had arrived, so each uploaded what the server lacked: the repository,
  twenty times (447 MB pushed for rust-analyzer instead of 23 MB). An
  offer that has much to send now takes a turn in its daemon and looks
  again at what the server holds when its turn comes; the nineteen that
  follow have a few kilobytes to push. Two daemons on different machines
  can still both upload the same new content; the server does not make
  one wait for the other on this path as it does when it asks.
- **A busy server looked dead.** While the server took in those uploads
  (frame by frame, in order), keepalive answers waited behind them, the
  daemon heard nothing for 45 s and dropped the link: 2 failed steps in
  the Ruff swarm, 20 in Bevy's. On the UDP transport a link is now alive
  while any authenticated datagram arrives (acknowledgements included);
  over TCP, where only frames can be seen, the limit is six minutes.
- **A worker that cannot fetch an input.** See STATUS, Workers: the
  worker's failure to get a content from the server ended as a compiler
  error in the build's output. Such an action is now reported as failed
  by the worker and run on the server. The cause of the missing contents
  is not established.
- **Listings under churn.** Zed's no-drift run failed in stock cargo
  with "can't find crate" for a crate that had been built: the output
  file system continued a directory listing by position, and a
  temporary file removed by another compiler between two reads shifted
  everything after it by one, so a name that was there all along was
  skipped. A listing now continues after a number that belongs to the
  name (63 bits of a hash of it). `dev/e2e-link.sh` lists a directory
  150 times while files come and go; the build before the fix came up
  short 60 times. The no-drift runs of rust-analyzer, Ruff and Bevy had
  passed with the fault in place, by luck, and were repeated.
- **Benchmarks through the rig** enter only the client's network
  namespace (`nsenter --net`), not `ip netns exec`, which gives every
  command a mount namespace of its own in which the daemon's output
  directories would be invisible to the next command. A command that is
  in another mount namespace than its daemon gets its results copied.

## D61. The worker fault's cause; the output file system without one lock; fusermount3; the history benchmark

**The worker fault** (D60, STATUS) had two causes, both found by logging
on the server why a content could not be served and on the worker what
arrived.

- *Answers sent ahead named contents that could not be asked for.* When
  an action goes to a worker, the server sends ahead what it expects the
  action to look up. A file on the server's own disk (toolchain, registry
  sources) that the server had hashed before was sent with its hash; but
  only for files the action was predicted to read was it also remembered
  where that content could be read from. For the others (directories on
  the way, files predicted to be looked at but not read) the hash went
  out and the source did not. If the action then opened such a file, the
  worker asked for the content by hash and the server said it had none:
  "not in the store and never shown from a file", 161 to 190 times per
  swarm run. The compiler failed on the file. Now every answer that names
  a content remembers its source.
- *Pushes were put together too early.* A push of a content is several
  messages; the transport delivers messages independently, so they can
  overtake each other. The worker assembled the content when the message
  sent last arrived, with the recipe from whichever arrived first: an
  empty recipe if that was not the first sent. The content then failed
  its check ("arrived with the wrong content"), while the server went on
  believing the worker held it. Messages of a push are now numbered and
  counted, and the content is assembled when all are there.

After both: no content refused, no failed assembly, no worker failure in
the swarm (the fallback of D60, which reruns such an action on the
server, stays, and did not fire).

**The output file system** held one lock around every operation,
system calls included, and answered a lookup of a name that does not
exist without letting the kernel remember it. Zed's profile (incremental
compilation with unpacked split debug info) makes tens of thousands of
small files, and stock cargo in the mounted directory took 446 s for a
cold build against 168 s in a plain one.

- The table of delivered names is behind a reader-writer lock that
  ordinary operations on real files only read; the map from node
  numbers to paths has its own short lock; no lock is held across a
  system call.
- A lookup that finds nothing is answered as a negative entry the
  kernel may keep, like positive ones. A delivery, the only thing that
  makes a name exist behind the kernel's back, invalidates the entry at
  the deepest directory the kernel knows on the way to the name.
- As many FUSE worker threads as cores (8 to 32), where there were 8.

Zed, stock cargo through the mount, after: 198 s cold build, 55 s tests
(168 s and 29 s in a plain directory). Not done: avoiding the extra
`lstat`s and the second open per create, which is where I would look
for the rest.

**fusermount3.** If `mount(2)` is refused, `fusewire` runs `fusermount3`
with a socket in `_FUSE_COMMFD`, takes the connection's descriptor from
it, and unmounts through `fusermount3 -u -z`. Without privilege there is
no `allow_other` (the mount is the user's alone) and no passthrough
(file contents go through the daemon). `dev/e2e-link.sh` runs a build
as `nobody`; that part uses a directory in `/tmp`, because an
unprivileged user cannot reach anything under `/root`.

**The history benchmark** as first written here was wrong: I took (a) to
(d) for something of my own making over 30 commits. The brief defines
them; see D62.

## D62. The history benchmark as the brief defines it; where single-agent rargo loses to stock cargo

**The benchmark.** First-parent history of rust-analyzer over two years
(2,672 commits, 2024-11-24 to 2026-11-22), `build -p rust-analyzer` at
each commit, 100 ms of round-trip time on rargo's client link:

- (a) stock cargo, one worktree with a persistent target directory,
  every commit;
- (b) stock cargo, an empty target directory at every commit;
- (c) stock cargo, one worktree with a persistent target directory,
  only every 10th commit (the commits between are holes for it);
- (d) rargo, two agents (two worktrees of one client machine)
  interleaved in commit order: one builds every commit that is not a
  10th, the other only the 10th ones. The second agent's builds against
  (c) are the comparison that matters: both arrive at a commit nine
  commits after the last one they built; rargo's agent finds what the
  other agent built one commit earlier.

Each as median, 90th percentile, maximum and total (`dev/history.py`).
What had to be decided to run it:

- **A toolchain per commit.** Commits older than a few months do not
  compile with today's compiler (a lint that became an error). Each
  commit is built with the toolchain its workspace names as its minimum
  (`rust-version`: 1.82 to 1.98, nine toolchains), by stock cargo
  through rustup's `+toolchain` and by rargo through
  `RUSTUP_TOOLCHAIN`. For 72 commits whose dependencies already needed
  the next toolchain's cargo, the next one is used. Two commits are
  broken in the repository itself and fail for both.
- **Four cores per build, six builds at a time.** (b) alone is 2,672
  cold builds. Everything shares the machine's cap of 24 cores, so each
  build (stock cargo's `-j`, and rargo's server) gets 4 cores and six
  run side by side; cold builds are handed to whichever lane is free.
  The other tables in STATUS are at 24 cores; these are not comparable
  with them in absolute terms, only among themselves.
- **Crates fetched beforehand**, by each toolchain's cargo and by the
  server's planner (they keep the registry cache under different
  directory names), so that neither side downloads while timed and the
  rig's server, which has no route out, can build.
- A full clone was needed (`bench/ra-hist`); the benchmark's shallow
  clone could not be deepened.

**Where one agent alone loses to stock cargo.** Measured per action on
single history steps (24 cores, LAN): hashing and storing the binary
and the incremental states is not it. On a commit that changes one file
of `hir-ty`, every unit up to `ide` takes the same time under both to a
tenth of a second; then the `rust-analyzer` library takes 6.3 s under
rargo and 1.6 s under stock cargo, and the link 1.1 s against 0.9 s. On
a commit that changes no Rust file, stock cargo does nothing (0.14 s)
and rargo runs three actions (7.7 s): the crate's build script, the
library, the link.

The reason is the build script. It asks git for the commit and passes
it to the compiler, and tells cargo to rerun it when `.git/HEAD`
changes. In a linked worktree `.git` is a file, the script finds no
`.git/HEAD`, tells cargo nothing, and stock cargo never runs it again:
its binary goes on reporting the commit it was first built at. rargo
does not go by what a script declares; it saw the script read HEAD
through git, HEAD changed, so the script runs, the commit it passes on
is new, and the library that embeds it is compiled again. Of the 5.4 s
between the two on the one-file commit, 4.6 s are that recompilation;
with `[sync] git = false` (no repository in the sandbox, the script
finds no commit) the step takes 7.9 s against stock cargo's 7.1 s. The
remaining 0.8 s are rargo's own: 0.2 s before the first action starts,
0.2 s on the link step, 0.3 s after it (the 456 MiB binary is hashed
and stored, the delivery listed), and the round trip. Those were not
reduced.

What was changed: a linked worktree is now shown to the build as one.
`.git` is a file there, as on the client, naming a stand-in directory
beside it (`.rargo-git`, excluded from what git sees). Before, the
stand-in was a directory at `.git` whatever the client had, so the
script behaved as in a main checkout: it found `.git/HEAD`, and said so
to cargo. That was a difference from the client for no reason. It does
not change the timing: rargo reruns the script because of what it
read, not because of what it declared.

What was not changed: rargo still reruns the script and rebuilds the
library at every commit. That is the cost of the binary saying which
commit it is, which stock cargo in a linked worktree gets wrong. A
repository that prefers cargo's behaviour can set `[sync] git = false`.
Rejected: honouring `rerun-if-changed` over what was observed (it would
make rargo's cache as stale as cargo's in exactly this case).

## D63. The output file system: listings built once, fewer system calls per operation

STATUS named the extra `lstat`s and the second open per create as where
to look for what stock cargo still lost in a mounted `target/` on Zed
(198 s against 168 s cold, 55 s against 29 s for the tests).

- **Fewer system calls**, done first: a real file is opened once, for
  reading and writing, and that descriptor is both the handle and what
  the kernel reads and writes through (it was `lstat`, open, open); a
  create looks at what it opened instead of looking the name up again
  and makes parent directories only when the open says they are
  missing; rename, link and mkdir of names the table of delivered files
  has nothing to say about go straight to the real directory. Measured:
  203 s and 56 s. No difference. Kept, because it is less work per
  operation, but it was not the cause.
- **Listings**, the cause: the kernel reads a directory in calls of
  about a hundred names, and each call put the whole listing together
  again (read the real directory, merge the delivered names, hash every
  name for its cookie, sort). With Zed's unpacked debug information
  `deps` holds tens of thousands of files and every compiler lists it
  to find its dependencies: five hundred times the work per listing. A
  listing is now built when its reading starts and the calls that
  continue it use that one (kept for 30 s by directory). Names created
  after a reading began are not in it and names removed still are,
  which POSIX allows; a name that is there throughout is listed once,
  as before (the stress check: 150 listings against churn, none short).
  Measured: 175 s and 30 s.

Both measurements were made in windows during which the history
benchmark (D62) was stopped with SIGSTOP; the builds that were in
progress in those windows are left out of its statistics
(`history-tainted.json`).

## D64. The output file system keeps a listed directory's names

Profile of stock cargo's cold build of Zed through the mount after D63
(`bench/zed-prof.sh`; the daemon now counts and times its operations,
shown by `rargo --rargo-daemon-status`): 174.4 s against 168.8 s in a
plain directory; 1.3 million operations, 42.7 s of daemon CPU; and
listings still first, 45,652 `readdir` calls costing 13.1 s. D63 built
a listing once per reading, but `deps` is read by every compiler and
changes between any two readings, so every reading still read tens of
thousands of names from the disk, hashed and sorted them, and did so
holding the directory against the 98,000 creates the compilers were
making in it.

A directory that has been listed now keeps its names (by cookie, in a
map). Every operation that adds or removes a name (create, mknod,
mkdir, symlink, link, unlink, rmdir, rename) updates the map of the
directory it happens in; a delivery or a removal by the daemon, and a
rename that involves delivered names, drop the map, and it is read
again at the next listing. While a directory's names are being read
from the disk the changes made meanwhile are noted and applied when
the reading is done, so the map is complete however busy the directory
is. A listing is then a walk through the map from the cookie it is
continued at.

Measured: 170.9 s (plain 168.8 s); `readdir` 2.1 s, daemon CPU 31.4 s.
The 2 s that are left are the round trips of everything else and were
not pursued. Checked by the listing stress (150 listings against
churn, none short; names added by create, mkdir, link, symlink and
rename listed, removed and renamed-away ones not; the listing equal to
the real directory's) and `dev/e2e.sh`.

The risk taken: an operation that changes a directory's names and
tells the map nothing would leave a listing wrong until the directory
is forgotten. All that the file system offers are covered; the real
directory is reached by nothing but this file system and the daemon.

## D65. States go to workers ahead of need; contents travel behind questions; a second rig

Three changes made while the history benchmark held the machine's
cores, so each is tested and none is measured yet. The measurements
(`dev/bench-worker.sh`, `dev/bench-worker-swarm.sh`) follow the
benchmark, and a change that does not pay is taken out again.

**States ahead of need.** D56 gives a compile that starts from an
incremental state only to a worker that holds that state. A state made
on the server was held by no worker, so the crate's later compiles
stayed on the server: 871 of 1,476 compiles in the worker swarm. Now a
state saved on the server is queued for the workers (the newest of each
crate and worktree, newest first, at most 512 waiting), and a
background task sends each worker what it lacks of it, by chunk, when
that worker's link is reckoned free, up to 256 MiB per state. The
reverse direction (D56's bringing states home) is unchanged.
`RARGO_STATE_AHEAD=0` turns it off. Risk: bytes and link time spent on
states nobody compiles from again.

**Contents behind questions.** One queue per direction carried
everything in order, so a worker's question waited behind every pushed
megabyte queued before it, and so did the server's answer. There are
now two queues: `Push` and the replies that carry chunks in one, all
else in the other, which is emptied first. This replaces the idea of a
second connection: the transport delivers messages independently, so
one connection has no head-of-line blocking of its own beyond what is
already in its send buffer (8 MiB). Consequence handled: an action can
arrive before contents another action's push claimed earlier, so the
server remembers until when each pushed blob is in flight (the link's
reckoned free time plus 2 s) and names those in the action's `incoming`
list too; the worker waits for what is announced instead of asking.

**A second rig.** `dev/netns.sh` and the scripts that use it take `RIG`
(namespace prefix, default `rargo`) and `RIG_STATE` (directory, default
`nstest`), so that the suites can run beside a benchmark that holds the
first rig. The addresses are the same in both; they are in separate
namespaces. `bench/hist-freeze.py` would also stop a second rig's server
(it matches the state directory by prefix), so the suites are not run
during a pause.

## D66. A client is asked for its environment once per request, not once per action

Zed built from a fresh worktree on a second machine took 8.1 s at
100 ms of round-trip time when everything was in the server's cache
and the same build from the first machine took 0.38 s. With phases
recorded on both sides (`RARGO_SYNC_STATS=1` on a command prints the
daemon's; `RARGO_PHASES=1` in rargod's environment logs the server's):
7.25 s of 7.94 s were between "planned" and "units answered" on the
server, and on a 1 ms link the same phase took 0.05 s. So it was round
trips, and they were these: the engine, before it forms an action's
key, makes sure it knows the client's values of the variables that
action declared in earlier runs (`ClientEnv::ensure`), and asks the
client when it does not. A daemon that has never been asked sends
none, so it was asked at every action with a variable the server had
not yet heard of from it, and actions wait for their dependencies, so
the questions followed one another.

Now the trace cache keeps the set of every variable any action has
declared (read once from its records, extended as actions declare
more), and a request asks its client for all of them in one question,
at the start, while the worktree is prepared and the build planned.
What the engine asks later is only what an action declares for the
first time in this very build.

Measured: Zed's second machine 7.94 s to 0.86 s; rust-analyzer's 1.46 s
to 0.66 s. All three suites pass (113, 67, 28).

Considered: sending every such variable from the client unasked. The
client does not know the names (they come from build scripts that ran
on the server), and sending its whole environment would make every
variable of a developer's shell visible to the server, which the
allow-list exists to prevent. One question for names the server can
justify keeps that.

A cost: a client is asked for variables of builds it will never run
(another repository's). They are names and short values, once per
daemon.

**The offer, root first (same round of measurements).** With the
question gone, the offer stood out: 0.24 to 0.32 s where a round trip
is 0.10 s. A daemon that has not told this server about a tree sends
all its manifests and asks what is lacking; for a repository that is a
few hundred kilobytes, more than a link that has just been made lets
out in its first flight (64 datagrams), so it took two more round
trips. When the manifests are more than 32 KiB the daemon now sends
the root's hash alone first. A server that has the whole tree (the
usual case for a fresh worktree in a swarm: someone has built that
commit) answers with the files it lacks, usually none, and no manifest
is sent. A server that lacks any manifest is then sent them all, as
before, which costs that case one round trip more than it did; it is
the case that goes on to upload files. Measured on rust-analyzer: the
offer 0.24 s to 0.10 s, the command 0.66 s to 0.52 s. The daemon's
status counts the trees answered this way.

**D65 measured (2026-11-24).** Cold series at 4 cores per machine:
55.1 s alone, 47.7 s with a warm worker, 43.4 s with 8 local cores:
63%, unchanged. Swarm at 8 cores per machine: 178.9 s alone, 154.7 s
with a worker, 160.6 s with a worker and states sent ahead, 127.9 s
with 16 local cores. Sending states ahead moved 5.5 GiB and made the
scheduler keep more actions local for the busy link than it freed for
the worker, so it is off unless `RARGO_STATE_AHEAD=1`; the code and its
suite check stay, because the idea may pay with a cheaper way to send a
state (only what changed since the state the worker holds). Contents
behind questions stays on.

**D66, third step: the request does not wait for the offer's answer.**
The daemon sends the root (asking what is lacking) and the request one
after the other. The server handles a link's messages in order and the
offer in place, so the request finds the tree if the server has it. The
daemon reads the offer's answer while the conversation runs: nothing
lacking, and the tree is noted as known; anything lacking (or the
server asking for the tree piece by piece), and the request is
withdrawn (`Changed`), the tree offered as before, and the request
sent again. Nothing can have been built or printed for a tree the
server did not have. Measured on rust-analyzer at 100 ms: the offer
phase 0.10 s to 0. No protocol change.

## D67. The easy path: `rargo @host`, `rargo auth`, tokens (issues 5, 13, 14: the smallest complete version)

What a new user does, with nothing edited by hand: `cargo install rargo`
(or `curl -fsSL https://rargo.rs/install.sh | sh`), `rargo @rargo.rs` in a
project, `rargo auth`, `rargo build`. This entry is the design of the
smallest version of each piece that makes that path whole; what each
initiative still lacks is listed at the end.

**The server is a property of the project (14).** `rargo @host` writes
`host` under `[workspace.metadata.rargo]` of the workspace's `Cargo.toml`
(`[package.metadata.rargo]` for a package outside a workspace) with
`toml_edit`, so the rest of the file stays as it was; cargo ignores
`metadata` tables, so stock cargo sees no difference. A build takes the
server from `RARGO_SERVER`, else from the project, else from the old
`~/.config/rargo/config.toml`, which keeps working.

**From a name to an address: an HTTPS lookup.** A host is written as
`name:port`, which is the server's address, or as a bare name. A bare
name is asked `https://name/.well-known/rargo.json`, which answers with
the server's address (`{"server": "build.rargo.rs:7459", ...}`); the
answer is kept for a day in the client's cache and an old answer is
used if the name cannot be asked. Chosen over a DNS SRV record because
`rargo auth` needs HTTPS to that name anyway (one mechanism, one thing
for the owner of a name to set up), because the client would otherwise
need a resolver library to read SRV records, and because a web server
is easier to stand up and to check than a DNS record of an unusual
type. Cost: the client carries an HTTPS client (`ureq` with `rustls`;
the static binary grew from 4.4 to about 6 MB stripped).

**rargod answers HTTP on its TCP port.** A client's TCP link starts
with the protocol's magic; a connection that starts with `GET `, `POST`
or `HEAD` is answered as HTTP instead: the lookup above and the pages
and calls of `rargo auth`. No second port to open, and a server on a
private network is asked directly (`rargo @build.lan:7459` uses
`http://build.lan:7459`). A public name puts something that speaks
HTTPS in front and forwards `/.well-known/rargo.json` and `/auth*` to
that port; rargo.rs does. `--public HOST:PORT` says which address the
lookup hands out when it is not the name the request came to.

**`rargo auth` (13)** is a device flow. The client asks `/auth/start`
and gets a device secret, which it keeps, and an eight-character code,
which it prints with the address of the page (and opens the page if
there is a display). On the page a person sees the code, checks it
against the terminal, and enters an invitation. The client, polling
`/auth/poll` with its device secret, then receives a token and stores
it in `~/.config/rargo/credentials.toml` (mode 600), under the host as
the project writes it. It works without a browser on that machine: the
page can be opened anywhere. `rargo auth status` says which server and
who; `rargo auth remove` forgets the token; `rargo auth --token T`
stores one an administrator made. `login` and `logout` are cargo's and
are passed to cargo as before.

**How the page knows who you are: an invitation, for now.** An
administrator makes one (`rargod token invite --state DIR --user ann`):
a code that is good for a number of uses and days and may carry the
user's name and standing. The eight-character code of the flow grants
nothing by itself, a wrong invitation is counted against it (five, then
the flow has to be started again), and the token goes only to the
holder of the device secret. Signing in with GitHub is the better
answer for a public server and is not built: it needs an OAuth
application registered by the owner of the name, with its secret given
to the server. Until then the first step for a new person is being
handed an invitation.

**Tokens (5).** A token is an id and a secret; the client presents the
id as the user name of the handshake and the secret as its key, so the
wire protocol is unchanged (protocol 7). They live in `tokens.json` in
the server's state directory, rewritten whole under a lock by whoever
changes it and read again by the server whenever the file has changed:
`rargod token create|list|revoke|invite` on the server's machine take
effect at once, with no restart. A token may expire. A session whose
token was revoked or has expired is closed within seconds (looked at
when anything arrives on the link, at most every other second). The
key file keeps working beside it; a server may also run with tokens
only.

*Secrets are not kept in plain text*, with a limit that has to be said:
the file holds the key derived from the secret, not the secret, and an
invitation's code only as a hash. But the handshake (Noise NNpsk0) is
symmetric, so the derived key is all a thief of that file would need
to connect as the user. The file is therefore as sensitive as the key
file (mode 600), and "not in plain text" protects only the secret
itself. Removing that limit means a handshake with a key pair per
client, the server holding public keys: a protocol change, not done
here.

**Not in this version, by initiative.**
- 14: the contents of `.rargo.toml` (`[env]`, `[network]`, `[flake]`,
  `[sync]`) have not moved into `Cargo.toml`; no migration command for
  the old files.
- 13: GitHub sign-in; revoking a token from the client (`rargo auth
  remove` forgets it locally and says how it is revoked on the server).
- 5: token commands for an administrator from a client (they run on
  the server's machine); key pairs.

Tested by `dev/e2e-auth.sh` (36 checks: the whole path against a
server with tokens only, the browser played by curl; a second project
needs no second `rargo auth`; create, list, revoke while the server
runs; a revoked token's open session is closed) and unit tests of the
token store, the flow and the manifest edit.

## D68. `rargo auth` signs in with GitHub; the server keeps an allow list (issue 13, the owner's decision)

The browser step of `rargo auth` (D67) identifies a person by their
GitHub account. The page shows the terminal's code and one button; the
button sends the browser to GitHub's authorization page with the
server's OAuth application and a random value that names the waiting
request; GitHub sends the browser back to `/auth/github/callback`; the
server exchanges what came back for an access token, asks GitHub who it
belongs to (`GET /user`: the login, nothing else is used and no scope is
asked for), and drops that access token. If the login is on the
server's allow list, a rargo token is made for that name with the
standing the list gives it and handed to the terminal that holds the
device secret.

- **The allow list** is in `tokens.json` beside the tokens:
  `rargod token allow LOGIN [--admin]`, `rargod token disallow LOGIN`
  (which also revokes the tokens that account signed in for, so its
  open sessions end within seconds), shown by `rargod token list`.
  Logins are compared without regard to case, as GitHub does.
- **The application** (client id, client secret, the address the pages
  are reached at) is in `github.json` in the state directory, mode 600,
  written by `rargod token github --client-id ID --client-secret-file
  FILE --url https://NAME` and read when needed: setting it up needs no
  restart. A server without it asks for an invitation as before, so
  private servers need no GitHub.
- **Why an OAuth application and not something that needs none.** Only
  GitHub can say who is at the browser, and it says so only to a
  registered application. Registering one is a form on github.com that
  belongs to the owner of the name (the callback address is theirs);
  it cannot be done through the API or by a collaborator.
- rargod now carries an HTTPS client (`ureq`, as the client does) for
  the two questions it asks GitHub.
- The exchange is bound to the request by the random `state` value,
  which is known only to the server and the browser it redirected; a
  callback with a value that belongs to no waiting request is refused.

Tested in `dev/e2e-auth.sh` against a stand-in for GitHub's three
addresses (`dev/fake-github.py`; `RARGO_GITHUB_URL` and
`RARGO_GITHUB_API` point rargod at it): the redirect carries the
application's id and the way back; an account that is not allowed gets
nothing and is told what its administrator has to run, and the terminal
keeps waiting; a made-up callback is refused; an allowed account is
signed in and its build works; taking it off the list stops its builds.
**Not tested against github.com itself**: that needs the application,
which is the owner's to register.

## D69. `cargo nextest`: real nextest drives, every test process is a cached action (initiative 15; design, not built)

**The choice.** The issue offers two ways: run real nextest in the
sandbox and intercept the test processes it starts, or take nextest's
test list and schedule each test as an action of rargo's own. This takes
the first, in a form that also gives what the second promises.

- rargo builds the test binaries the way it builds for `cargo test
  --no-run`: on the server, from the shared cache, nothing on the
  client. nextest is then run on the server against that build without
  building anything itself (its reuse-build mode: `--binaries-metadata`
  and `--cargo-metadata`, produced by rargo's planner for the same
  command line).
- nextest starts every test in a process of its own. Those processes are
  started through a runner (cargo's `target.<triple>.runner`, which
  nextest honours): a small program that hands the test's command line
  to rargod as an action. The action runs in rargo's sandbox, is traced
  like any other, and its result (exit status, output, files it wrote)
  is cached under the test binary, the test's name and what it read.
  The runner prints the recorded output and exits with the recorded
  status, so nextest sees a test process that behaved as the real one
  did. A test whose binary and inputs did not change is replayed.
- So nextest itself decides everything a user sees: progress and failure
  output, `--no-fail-fast`, retries and flaky handling, `-E` filters,
  partitioning, profiles from `.config/nextest.toml`, JUnit files. It is
  exact by construction, and caching is per test because nextest's unit
  of work is the process the runner stands in for. Listing (`nextest
  list`) runs each binary with `--list` through the same runner and is
  cached per binary.
- Parallelism is nextest's `--test-threads`, set by default to the
  cores rargo may use for this client; each test action is scheduled
  by rargo's scheduler and may go to a worker.

**What it costs and risks.** A replayed test still costs a process (the
runner) and a round trip inside the server, a few milliseconds each; a
suite of ten thousand replayed tests is seconds, not zero. Timing in
nextest's output is the replay's, not the original run's: a replayed
test is shown by nextest as passed in a few milliseconds, and the
summary line rargo adds says how many were replayed (`--no-cache` runs
them for real). Retries of a failing test must not be served from the
cache: a failed result is not cached (as for `cargo test` today), so a
retry runs again, which is what makes flaky detection work.

**The nextest version** is the one the repository pins (`.config/
nextest.toml`'s `nextest-version`, or `[workspace.metadata.rargo]`), else
the client's, sent with the request; the server installs it on first
use beside the toolchains.

**Other third-party subcommands that compile** (`cargo fix`, `cargo
install`, `cargo llvm-cov`, `cargo miri`, `cargo hack`, ...) are passed to
the real cargo and build locally. That stays so for now; rargo will say
so in one line when it passes on a subcommand it knows to compile.

**Order of work** (the issues filed from the initiative): route nextest
and run it on the server from rargo's build; the runner and per-test
caching; the pinned version; output checked against stock on a failing
and a flaky test; the swarm variant measured; the notice for other
subcommands.


## D70. A killed daemon's `target/` is unmounted by a process that outlives it (issue 6)

A FUSE mount outlives the process that serves it: a daemon killed with
SIGKILL (or crashed, or taken by the out-of-memory killer) left
`target/` answering "Transport endpoint is not connected" to everything
until the next rargo command replaced the mount. Nothing inside the
daemon can run at that moment, so something outside it has to.

- `fusewire::MountOptions::auto_unmount`: at mount, the session forks a
  guard. The guard holds nothing of the daemon's but the read end of a
  pipe (every other descriptor is closed, the FUSE connection included,
  so it keeps nothing alive), is in a session of its own and ignores
  the signals a terminal or a service manager sends to a group, and
  blocks in `read`. A byte means the daemon unmounted itself: the guard
  exits. End of file means the daemon is gone: the guard lazily unmounts
  the directory (`umount2`, or `fusermount3 -u -z` for a mount this user
  made through it) and exits. It costs one sleeping process per mounted
  worktree.
- The guard is a fork, and a fork has its parent's name and command
  line: the first end-to-end run killed the daemon with `pkill -KILL -f`
  and killed the guard with it. It renames itself and overwrites its
  argument strings, and shows as `unmount-guard: DIRECTORY`, which
  neither `pkill rargo` nor `pkill -f 'rargo --rargo-daemon'` matches.
  Mounting returns only when the guard has said it is set up.
- It unmounts only its own mount: the kernel's mount id (statx, Linux
  5.8; asked without a question to the file system, which cannot answer)
  is recorded at mount and compared before unmounting, in the guard and
  in `Session::unmount`. Before 5.8 the comparison is skipped.
- `Session::unmount` stands the guard down and waits for it before it
  unmounts, so no guard of an old mount is alive when the same directory
  is mounted again.
- **Not `fusermount3 -o auto_unmount`.** It was tried first. The helper
  (3.14) unmounts only if opening the directory fails with ENOTCONN at
  the moment it looks; after a kill it exited without unmounting in
  every trial here.
- **What is left is a plain directory**, as after a stopped daemon:
  what was written locally is there, what the server delivered is not
  (it is in the cache and on the server, and listed again by the next
  rargo command). Stock cargo builds in it and builds again what it does
  not find. Copying delivered files into the worktree at that moment is
  not possible for a killed daemon and was not added for a stopped one.
- Not covered: the guard itself being killed too (`kill -9` of both, a
  machine going down). After a reboot there is no mount to clean up.

Tested: `crates/fusewire/tests/auto_unmount.rs` (a process killed while
mounted, with and without the option; the guard gone after an ordinary
unmount; a second mount of the same directory untouched by the first
session), as root in CI and through `fusermount3` as a user;
`dev/e2e-link.sh` kills the daemon with SIGKILL and runs stock
`cargo build` and `cargo test` in the worktree, and kills an
unprivileged user's daemon.
