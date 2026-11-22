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

- rustc failures with exit code 1 (ordinary diagnostics) are cached; signals
  and ICEs are not. Test binary runs are cached only when they pass. Reason:
  a diagnostic failure is a deterministic function of the inputs; a crash or
  a failing test may not be, and replaying a stale failure would be the kind
  of wrong answer that makes people distrust the cache.
