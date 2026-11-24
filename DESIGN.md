# rargo design

rargo is remote cargo. Status: built; `STATUS.md` says what was verified
and measured, `DECISIONS.md` how the open ends were closed.

An earlier version of this document put the server on the client's machine,
reading worktrees from its own disk. That was a misreading of what was
asked for, and the first implementation followed it. This version is the
intended design; `DECISIONS.md` D34 onward records the change.

## What this is for

A small team of humans each directs dozens of coding agents. The agents work
in separate git worktrees of the same repo, on the humans' own machines and
elsewhere, and run overlapping `build`, `check` and `test` commands. Today
that means N copies of the same compile, N full `target/` directories, cores
thrashing, and disks filling up, on machines that were bought to be typed on.

rargo moves all of it to a build server and makes N worktrees cost roughly
one build plus their differences.

### What must be true

1. **Shared work is done once.** Across worktrees, across agents, across
   people and machines, across time, and across builds that are running
   right now.
2. **A worktree holds source and the artifacts it asked for.** Nothing else.
3. **The client machine does no build work.** It syncs files and
   environment and waits. Analysis, scheduling and every compile happen on
   the server.
4. **Fast in every sense a person or agent perceives**: latency of one
   command and throughput of the whole swarm.
5. **Drop-in.** Point it at a large cargo project and it works. Setup is no
   harder than building that repo for the first time.
6. **No drift.** Using rargo for years never makes a repo depend on rargo.
   Stock cargo keeps working.
7. **Nobody ever nukes the cache.** It is never wrong, so there is never a
   reason to.

### What we do not care about

Coupling (to git, Linux, cargo, rustc, Nix), implementation complexity,
general-purpose scalability, protocol conventions. Where any of these
conflicts with the list above, the list wins.

## The model

One server process, `rargod`, owns all build work. Every `rargo` command,
in every worktree on every machine, is a client of it: it says what its
worktree contains, sends what the server does not have yet, and gets back
output and the artifacts it asked for.

```
machine 1: worktree A ─ rargo ─┐                             ┌─ worktree states (synced trees)
machine 1: worktree B ─ rargo ─┼─ client daemon ─┐ encrypted ├─ analysis (cargo, memoized)
machine 2: worktree C ─ rargo ─── client daemon ─┼── UDP ─── rargod ─ scheduler (all cores, all memory)
   ...                                           │ sessions  ├─ sandboxed, traced actions ── workers
machine n: worktree Z ─ rargo ─── client daemon ─┘           └─ content store + trace cache
```

The client side is deliberately dumb. It does not parse manifests, resolve
dependencies or know what a crate is. It does three things: describe a
directory by content, pass on an allow-listed environment, and put files
where it is told. Those three things are done by a **client daemon**, one
per user and server on a machine, which the first `rargo` command starts
and every later one talks to over a Unix socket. The daemon exists for the
link, not for the build: it holds one long-lived session to the server, so
that no command pays for a connection, a key agreement or a slow start,
and it watches the worktrees it has seen, so that a change is on the
server before anyone asks for a build (see "The client daemon and its
link").

An **action** is one process the build needs: a rustc invocation, a build
script run, a link, a test binary run. Every action runs on the server (or
a worker of it) in a sandbox whose filesystem rargod serves, so rargod sees
exactly which files it read, which lookups failed, and which files it wrote.
That record is the action's **trace**. Nothing about an action's inputs is
ever declared.

### Decisions on the open questions

- **Topology.** One `rargod` for the team, reached over the network.
  Builds run there; `rargod` can also dispatch actions to a fleet of
  workers. The server is the single owner of analysis, the cache and
  scheduling; workers only execute; clients only sync. A server on the same
  machine as its clients is the same thing over the loopback interface.
- **Nix.** Optional. By default the environment (toolchain, linker, system
  libraries) is the server's, captured by tracing it. If the repo has a
  flake, the sandbox is restricted to that closure instead, which is
  stricter and stable across servers.
- **Platform.** The server is Linux, x86_64 first. The client is plain
  code that reads files and speaks to one socket; it watches worktrees
  with inotify and is only tested on Linux, and what it brings back are
  Linux binaries.

## Mechanisms

### Syncing a worktree

A worktree is described as a tree of directory manifests. A manifest lists
a directory's entries by name: a file by the hash of its content, a symlink
by its target, a subdirectory by the hash of its manifest. The hash of the
root manifest therefore names the whole worktree state; two worktrees share
every subtree they have in common; after an edit only the manifests on the
path to the edited file are new.

The daemon scans a worktree when it first sees it and whenever its
watcher (inotify on every synced directory) says something changed. It
keeps an index of what it hashed last time (size, times, inode), so a scan
is one `lstat` per file plus reading what changed; a worktree without an
event since its last scan is not scanned at all. The watcher only says
*that* something changed. If it cannot vouch for a worktree (events were
dropped, a directory could not be watched), the worktree is scanned for
every command, as a client without a watcher would.

Changes are pushed, not pulled. The daemon remembers what the server holds
(what it sent, and what the server said it had) and after an edit sends,
in one message and without being asked, the changed files and the
manifests on their paths. It does so as the change happens, so a build
usually finds the server up to date; a build started in the same instant
sends the change ahead of the request on the same ordered channel. Sync
therefore costs no round trip of its own. A worktree the daemon has not
seen before is different: most of it is probably on the server already
(another agent's checkout of the same commit), so the daemon sends its
manifests and asks, once, which file contents are missing. What the
daemon believes is only ever a hint: the request names the root hash, the
server checks that it holds the whole tree and asks for anything it lacks
(after a garbage collection, say), level by level as before.

Everything in the worktree is synced except `.git`, cargo's own output
directories, and what the repo's policy file excludes. Whether git ignores
a file does not matter: an ignored `Cargo.lock` or generated source file is
as much an input as a tracked one, and leaving it out would make the server
build something other than what stock cargo builds on the client.

The server's snapshot of a worktree is that tree: immutable, complete in
the store before the build starts, and identical for identical source no
matter which machine, user or directory it came from. An agent can keep
editing while its build runs and the build stays consistent.

### Git

`.git` is not synced; it can be gigabytes and almost none of it is an input
to a build. But build scripts do ask git which commit they are built from.
So the client makes a stand-in per commit: `HEAD`, its branch, the commit
object, its trees, and an index read from the commit, with no history and
no file contents (hundreds of kilobytes for a large repo). In the sandbox
that is a shallow clone: questions about HEAD, the branch and whether the
tree is dirty get the same answers as on the client.

HEAD and its recent ancestors are also sent as a hint for choosing
incremental state. Git is never part of correctness.

### Syncing the environment

The environment of a build is the server's: its toolchains (installed on
demand from the repo's `rust-toolchain` file), its linker and libraries,
its cargo home with one shared copy of every downloaded crate. The client
contributes only what steers a build without naming anything on the
client's disk:

- variables on a fixed allow-list (`CARGO_*`, `RUSTFLAGS`, `RUSTUP_TOOLCHAIN`
  and the like), and those the repo's policy file names;
- any other variable only after an action has declared that it reads it
  (`rerun-if-env-changed`, `env!`): the server then asks for it, and the
  client sends it unasked from then on;
- the cargo configuration files in the client's cargo home, laid over the
  server's cargo home for that client's requests only.

Paths the user types and paths in messages are translated, so a diagnostic
names the file where it is on the client.

### Canonical paths

Inside the sandbox every worktree appears at the same path (`/w`), cargo
home at another fixed path, and so on. Identical source therefore produces
identical command lines, identical cargo metadata hashes and identical
outputs no matter where, or on whose machine, it is checked out. Without
this, worktrees share nothing.

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
its own. The same holds one step earlier: when two clients are missing the
same file content, one uploads it and the other waits for that upload.

### Sandbox and tracing

Each action gets its own mount, PID and network namespace on the server.
Its root filesystem is a FUSE mount served by rargod, with reads passed
through to backing files in the content store so file data never crosses
userspace. Each action sees a distinct subtree of the mount, so kernel
caching cannot hide one action's lookups behind another's.

Writes go to a private upper layer. Whatever an action writes is its output,
including files written into the worktree by tests (snapshot files, for
example). Those are sent to the client's worktree; on a cache hit they are
sent again.

Network is loopback-only by default; the repo's policy file can open it for
named tests and build scripts.

### Analysis

rargod runs cargo as a library, inside the same traced view, to get the unit
graph. That is itself a cached action keyed on the manifests, lockfile and
config it read, so worktrees with the same manifests get their graph
instantly. rargo replaces cargo's fingerprinting and job queue; cargo's
resolution and command-line construction are kept as is, which is what keeps
behaviour identical.

Anything that needs the outside world (installing a toolchain, downloading
crates, writing a lockfile) is done by the server, into its cargo home. A
lockfile cargo wrote goes back to the client's worktree, as stock cargo
would have left it there.

### Scheduling

One scheduler owns every core and all memory of the server. Human requests
outrank agent requests. Actions are admitted by measured memory use from
their previous runs, because a swarm of concurrent links is how a machine
runs out of memory. rustc's internal parallelism draws from the same pool
through a jobserver rargod owns.

### Results

The client gets what cargo would have left for it and nothing more: the
artifacts the command asked for at the names cargo gives them, cargo's
`.d` files, generated documentation, files a build wrote into the worktree.
The server offers them by hash; the client asks only for content it does
not have. An artifact cargo would hard-link into place is kept once in the
client's cache and shared between that machine's worktrees.

`rargo test` runs the tests on the server and shows their output. `rargo
run` brings the binary over and runs it on the client, where the user's
terminal, files and ports are.

### Storage

One content-addressed store on the server holds everything: worktree
content, manifests, outputs, incremental state. A size budget with
least-recently-used eviction replaces `cargo clean`; a client is simply
asked again for whatever source has gone.

Chunk-level storage (content-defined chunking with format-aware boundaries:
archive members in `.rlib`, sections in objects) is used for the cold tier
and for transfer to workers. Hot files stay whole so they can be passed
through to the sandbox.

### Shared incremental compilation

The trace cache answers "has exactly this been built before". Incremental
compilation covers the miss: the crate changed, but mostly it did not.

rustc's incremental state for a crate is only a starting point. rustc checks
every piece of it against the current source before reusing anything, so any
state is a safe seed for any source; a closer one is just faster. That makes
it shareable, across worktrees and across machines:

- After an action, the crate's incremental state is stored as an immutable
  entry keyed by crate and tagged with the worktree and commit it came from.
- Before an action, rargod picks a seed: the same worktree's last state for
  that crate, otherwise the newest from a commit the client named as an
  ancestor, otherwise the most recent of any. An agent that branches off
  main starts warm from main's state.
- The action gets the seed as a private copy-on-write layer, so concurrent
  builds of the same crate never contend for rustc's session lock.

Canonical paths are what make a state from one worktree usable in another.
The seed is excluded from the trace, because it must not affect the result.

States are large and are only an optimisation, so they are evicted freely.
If rustc crashes with a seed, the action is retried without one and that
state is discarded.

A compile that ran on a worker leaves its state there. The state is
recorded at once, with its files still on the worker; they come home in
the background on idle cores, or at once when a compile on the server
starts from them. A later compile of the crate is offered to the worker
that holds the state it would start from and uses it in place. A state
whose files were lost with a worker is forgotten; the next best one is
used.

### Workers

An action is already self-contained: a command, a snapshot, and a filesystem
that rargod serves on demand. Running it on another machine means serving
that filesystem over the network instead of locally. A worker is a sandbox
host that faults content in from the server, keeps a local content cache, and
streams back the trace and outputs.

A stateful session protocol lives between `rargod` and each worker: the
server knows what every worker already holds and sends only what is
missing, predicted from the action's previous trace. The scheduler sends an
action to a worker when the server's cores are the bottleneck and the
transfer is cheaper than the wait.

### The client daemon and its link

One daemon per user and server on a client machine, started by the first
`rargo` command and gone after half an hour without one. `rargo` itself
only passes its arguments, directory and environment to the daemon over a
Unix socket and relays output and the exit code; an interrupted command
closes the socket, and the daemon withdraws the request from the server.

The daemon holds one session to the server on the reliable-UDP transport
that also carries the worker links: messages cut into datagrams,
acknowledged and retransmitted individually, so a lost datagram delays
its own message and nothing else. A session is known by its id, not by
the client's address: a datagram that proves to be the session's moves
the session to wherever it came from, so a laptop that changes networks
keeps its session, and a keepalive every ten seconds holds a NAT's
mapping open. The congestion window and the round-trip estimate live as
long as the session. Every command is a stream of the session; one more
stream carries what the daemon does on its own (pushing changes).
Where UDP does not get through, the daemon uses one long-lived TCP
connection with the same messages on it.

Round trips are what a slow link charges for, so the protocol asks as
little as it can. A request arrives with its worktree already there.
Results are delivered without a question: the server remembers, per
worktree, what it has delivered, the daemon confirms with a token that it
placed all of it, and from then on the server sends the contents the
worktree lacks along with the listing. Whatever the daemon still lacks
(its cache was pruned, the server was restarted and remembers nothing)
it fetches, which costs a round trip only then. A build with nothing to
do is one message each way.

### The output directory

rargo is meant to be for cargo what clearmake was for make: builds whose
inputs are audited rather than declared, and derived objects that are
winked in rather than rebuilt or copied. On the server that is the trace
cache and the shared store. On the client it is the output directory:
`target/` in a worktree is a file system the daemon serves (FUSE), not a
directory that results are copied into.

- What the server built appears there as soon as the server has it: a
  name, a size, an executable bit and a content hash, sent while the
  build is still running. Nothing is copied into the worktree.
- Content is fetched when something opens the file, into the client's
  cache, which all worktrees of the machine share. Large contents are
  fetched by chunk (content-defined chunks, the same as between server and
  workers), so the next version of a binary costs the chunks that
  changed. Small files (dependency information, fingerprints, generated
  sources) are sent along with their names, because fetching each on open
  would cost a round trip apiece.
- Underneath the mount is the real `target/` directory. Everything that
  is written locally lands there: what stock cargo builds, what a test
  writes. A name is real if the real directory has it, otherwise virtual
  if the server delivered it. Writing to a virtual file makes it real
  first; deleting or renaming one edits the table of delivered names.
  So stock cargo works in the same directory, mounted or not: unmounted,
  `target/` is an ordinary cargo output directory that lacks what was
  never materialised, and stock cargo rebuilds that.
- Where a file system cannot be mounted (no FUSE, no permission, an
  output directory of the command's own choosing, a command in another
  mount namespace than the daemon) results are copied into place as
  before, shared between worktrees through the cache by hard link or
  reflink. `RARGO_OUTPUT=copy` asks for that.

### Tests

A test binary run is an action like any other. If nothing it read has
changed, it does not rerun and its recorded output is replayed. Agents rerun
whole suites after small edits, so this is expected to save more time than
compile caching. `--no-cache` forces a real run.

### Who may connect

A client daemon proves who it is with a key shared with the server, one
per user; its session is encrypted with fresh keys per session, and after
the key agreement every datagram carries a tag only the two ends can make,
so nobody else can move or disturb a session. The server
trusts its users with each other's work: they share one cache. It is a team
tool, not a public service.

### Keeping cargo honest

The sandbox is stricter than a plain cargo build, so most things that pass
under rargo pass under cargo. The exceptions are canonical paths, the
server's environment and cached tests. Two checks watch for drift:

- On the server, a background check builds a recently synced worktree
  state with stock cargo in a scratch copy and with rargo re-running every
  cached unit from nothing, and reports any disagreement.
- On the client, what rargo leaves in a worktree must never get in stock
  cargo's way: only ordinary files at the paths cargo uses, nothing rargo
  needs to find again.

Agents type `cargo`. An optional `cargo` shim routes `build`, `check`,
`clippy`, `test`, `run` and `doc` to rargo and passes everything else to real
cargo.

## Risks

- **The link.** Every command pays for a round trip; a first sync pays
  for the repo, and a big binary has to come back. Measured in
  `STATUS.md`, on a LAN and with 100 ms of round-trip time.
- **A mounted output directory** disappears with its daemon: after a
  reboot, or when the daemon is stopped, `target/` shows only what was
  written locally until the next rargo command mounts it again (the
  listing is kept; contents are still in the cache or on the server). A
  daemon that is killed outright cannot unmount; a small process it
  left watching does, at once (D70), so the directory is never a dead
  mount. Stock `cargo clean` cannot remove a mount point;
  through the shim it can, because the daemon lets go first.
- **A daemon on every client machine** is one more thing that can be
  stale or stuck. It is keyed by server, user, key and program version,
  so a new configuration or version gets a new one; a command that cannot
  reach or start one runs without it (`RARGO_NO_DAEMON=1` forces that),
  slower and correct.
- **FUSE overhead** on rustc's many small reads. Measured; the fallback is a
  forked rustc that reports its own inputs.
- **Cargo's library API is unstable** and does not route build-script
  execution through its executor hook: a small fork.
- **Untraceable inputs**: environment variables, network, time. Handled by
  policy, not observation.
- **The server's environment is not the client's.** A build that depends on
  something installed only on the client's machine fails on the server
  until the repo says how to get it (a flake) or the server has it.
- **Incremental compilation has a history of bugs** in rustc, and sharing
  states widely exposes more of them. Mitigations: retry without a seed on
  any crash, and the drift check compares incremental and clean outputs.
- **Incremental states are large** and deduplicate poorly as files; the
  cold tier stores them as chunks.
- **Proc macros and build scripts with nondeterministic output** reduce
  sharing but do not break correctness.
