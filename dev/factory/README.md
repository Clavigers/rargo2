# Factory2

The software factory that builds rargo2. `DATA-MODEL.md` is how it is
modelled; this page is how to run it and steer it.

## What runs

On the factory machine, under `/root/factory`, three systemd services:

| Service | Does |
|---|---|
| `factory-observer` | Every minute: records what happened on GitHub into the log, and merges pull requests whose latest push passed CI and every reviewer. |
| `factory-foreman` | Every 20 minutes: takes back work from agents that stopped or ran out of quota, preempts for bugs that waited too long, closes finished initiatives, and plans unparked initiatives into issues. |
| `factory-watchdog` | Keeps one headless session (Claude Code, Codex, Grok) running per agent that has work, each in its own worktree under `state/worktrees`. A session does one issue (`AGENT.md`) and ends. |

Everything they do goes through one log, `/root/factory/log.jsonl`. Nothing
else is the factory's state: not labels, not the services' memory.

## Seeing it

    ssh hetzner
    cd /root/factory
    PYTHONPATH=src FACTORY_LOG=log.jsonl venv/bin/python -m factory status
    PYTHONPATH=src venv/bin/python -m factory check log.jsonl
    journalctl -u factory-observer -u factory-foreman -u factory-watchdog -f
    ls -t state/logs | head          # each agent session's full output

Alarms (GitHub showing something the model says cannot happen) also go to
the ntfy topic in the service files.

## Steering it from GitHub

- **File work** as an issue with exactly one label: `enhancement`, `bug`,
  `friction` or `initiative`. Without one the factory asks and waits.
- **Place it** with lines in the body: `Part of #N` for its initiative,
  `Depends on #N` for each issue it waits for.
- **Hold it**: label `guard`. A guarded issue is not taken and a guarded
  initiative is not planned until the label goes.
- **Lock it** for yourself or another agent outside the factory: a label
  `wip-NAME`. The factory leaves it alone while any `wip-` label is on it.
- **Close or merge** anything on GitHub whenever you like; the factory
  follows, and an agent working on it loses it.
- Issues from accounts other than the owner and the factory's own are not the
  factory's.

## Changing it

The policy is `deploy/config.json`. A new version goes into the log, never
over it:

    FACTORY_LOG=log.jsonl FACTORY_ACTOR=human venv/bin/python -m factory submit \
      "{\"event\":\"config_applied\",\"config\":$(cat src/deploy/config.json)}"

with `version` one higher than the one in force. The code deploys by copying
`dev/factory` to `/root/factory/src`, running `src/deploy/install.sh`, and
restarting the services. A model change must keep `tests/golden.jsonl`, the
log as first written, legal.
