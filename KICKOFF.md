Implement rargo as specified in `DESIGN.md`, completely, in one continuous
run. Work for as many hours as it takes.

- Build every mechanism and every phase in the design, in the order that
  makes engineering sense. There is no spikes phase: build the real thing.
  If FUSE tracing overhead turns out too high, switch to the fallback named
  in the Risks section without asking.
- Forking cargo and rustc is expected. Clone them under `/root/rargo-work`.
- Workers: there is no second machine. Run workers in separate network
  namespaces on this box with injected latency and loss (`tc netem`).
- Acceptance is measured on large public workspaces (rust-analyzer, Bevy,
  Zed, Ruff), including a simulated swarm of 20 agents in 20 git worktrees
  running overlapping build, check and test commands, compared against stock
  cargo with one target directory per worktree: wall time, CPU time, disk.
- Also verify the no-drift property: every benchmark repo still builds and
  tests with stock cargo after being used with rargo.
- Keep `DECISIONS.md` (every decision and why) and `STATUS.md` (per
  mechanism: implemented, how it was verified, measured numbers, known
  weaknesses) current throughout. `STATUS.md` is the final report.
- Obey the machine rules in `CLAUDE.md`.

The user has said: own all decisions, leave no gaps you can articulate, do
not stop until it is done.
