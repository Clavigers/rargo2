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
