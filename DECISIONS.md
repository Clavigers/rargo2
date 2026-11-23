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
