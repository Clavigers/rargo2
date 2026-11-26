# rargo2 factory2 design

Factory2 builds rargo2, a deliberately small codebase, until every planned
feature is in and no bugs remain. One human sits outside the factory. Inside
it, a foreman, a crew of coding agents, a review agent, a product manager and a
power user work through GitHub. Every actor writes to one event log, as set out
in *Rules for a well instrumented software factory*.

## Goal

The factory is measured by one number: the time until all filed scope is merged
and released and no known or hidden bugs remain. A bug's cost is the time it
adds to that number: the time until it is found, the work to fix it, and any
bugs the fix introduces. Scope is finite. When the human freezes scope, the
factory works only on bugs and friction until the codebase is bug free.

## The two artifacts

- **The repository.** Issues, pull requests and `main`. Everything the factory
  does is an issue or a pull request.
- **The releases.** A release candidate on a release branch, and the latest
  release, which is what the human and the power user use.

## Issues

Every unit of work is a GitHub issue with exactly one type label:

| Label | Meaning |
|---|---|
| `initiative` | A larger goal. It is never worked on directly. |
| `enhancement` | One change that adds or improves behaviour. |
| `bug` | Behaviour that is wrong. Bugs always outrank other work. |
| `friction` | Behaviour that is correct but hard to use or understand. |

A sub-issue names its initiative and the sub-issues it waits for. An issue
whose dependencies are open is blocked; all others are ready.

## Actors

### The human

The human is the only actor outside the factory. While awake, the human either
uses the latest release or writes issues and initiatives into the repository.
Bugs the human finds while using a release are filed as `bug` issues with the
release version. The human decides scope and when to freeze it.

### The foreman

The foreman acts on a fixed tick, every 20 minutes. On each tick it, in order:

1. Takes back locks held by agents that have stopped, and returns those issues
   to the queue.
2. If a bug has waited longer than the preemption limit and no agent is free,
   pulls the agent that is least far into lower-priority work off it, and
   returns that issue to the queue.
3. Splits new initiatives into sub-issues with explicit dependencies, keeping
   independent work parallel and dependent work serial, until the ready queue
   holds enough work for every agent. Each sub-issue gets a t-shirt size label
   (`size:XS`, `size:S`, `size:M`, `size:L`, `size:XL`, `size:XXL`) on the same
   scale the size check puts on pull requests by lines changed, so every
   estimate can be compared with the pull request that closed it.

The foreman does not write code.

### Coding agents

Each agent is a short-lived session of one model: Claude Opus 5.5, GPT-6.2 Sol
or Grok 4.7. An agent holds at most one issue at a time.

1. It takes the highest-priority ready issue: pull requests that need work
   first, then bugs, then everything else, oldest first. Taking an issue is a
   lock, recorded as a label and an event.
2. It opens a pull request for the issue, or continues the existing one.
3. It works until the change is complete, then pushes, and releases the issue.
   It does not wait for checks.
4. Work it discovers that the issue does not cover becomes new issues, linked
   to the same initiative, with dependencies where needed.
5. When its plan's quota runs out, it stops where it is. Its lock stays until
   the quota returns or the foreman takes the issue back.

Bug fixes go to the agent whose measured defect rate is lowest, so that fixes
introduce as few new bugs as possible.

### The watchdog

The watchdog keeps agent sessions alive. It starts a fresh session for an
agent slot that is free and has quota, and it records every start and stop.
It makes no decisions about work.

### Pull requests, CI and review

One issue has one pull request. Every push to it starts CI and the review
agent together, on that commit.

- If CI fails, any review in progress is cancelled. The pull request moves to
  **Needs work**.
- If CI passes and the review agent requests changes, the pull request moves to
  **Needs work**, with the reason.
- If CI passes and the review agent approves the same push, the pull request
  merges into `main` and the issue closes.

A pull request in Needs work goes back to the front of the queue. The next push
restarts both checks from scratch. Nothing merges without a green CI run and an
approval on the same commit.

The review agent is independent of the coding agents. It checks that the change
does what the issue asks, that it is complete, and that it does not introduce a
bug.

### The product manager

The product manager owns releases.

1. On a fixed cadence it freezes features: it cuts a release branch from `main`
   and builds a release candidate.
2. Only bug fixes enter the release branch after the freeze. Each is
   backported from `main` and produces a new candidate build.
3. The candidate is released when it has no known bugs, has passed the required
   number of power user test sessions, and has gone a set soak time without a
   new bug being found.
4. It writes the release notes from the merged pull requests and closed issues
   in the release.

### The power user

The power user is an agent in a VM with access only to the website, the
documentation and the release notes. It uses each release candidate as a real
user would, in sessions. It files what it finds as `bug` or `friction` issues
against the candidate version. It never reads the source code.

## Flow

```
human ──files──▶ issues ──foreman splits──▶ ready ──▶ agent slot ──push──▶ checks (CI + review)
                                              ▲                               │
                                              └──────── needs work ◀── red ───┤
                                                                     green + approved
                                                                              ▼
human ◀── latest release ◀── release candidate ◀── freeze ◀──────────────── main
   │                               │
   └── bugs and friction ◀─────────┴── power user
```

## Instrumentation and the simulation

Every actor writes its events to the shared log as it acts. The factory's
policies (agents, models, plans, quotas, foreman tick, preemption limit, review
timing, freeze cadence, soak time, test sessions) live in one versioned
configuration file. The simulation in `dev/factory-sim` reads the same
configuration and writes the same log.

Each factory version is followed by a validation round: its real log is
compared with many simulated runs of the same configuration and the same filing
history. Parameters that the real log measures replace the simulation's
guesses, and the next version is designed with the calibrated simulation.
