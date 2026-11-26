# Factory2

The software factory that builds rargo2. `DATA-MODEL.md` is how it is
modelled; this page is how to run it and steer it.

## What runs

On the factory machine, under `/root/factory`, three systemd services:

| Service | Does |
|---|---|
| `factory-observer` | Every minute: records what happened on GitHub into the log, and merges pull requests whose latest push passed CI and every reviewer. |
| `factory-dictator` | Every 20 minutes: takes back work from agents that stopped or ran out of quota, preempts for bugs that waited too long, closes finished initiatives, and plans unparked initiatives into issues. |
| `factory-watchdog` | Keeps one headless session (Claude Code, Codex, Grok) running per agent that has work, each in its own worktree under `state/worktrees`. A session does one issue (`AGENT.md`) and ends. |

### Review

Each push is reviewed by the reviewers in the config. `placeholder` approves
everything. `review-mux` (`factory/review/`) asks AI review bots, defined in
`deploy/bots.json`, and runs inside the observer: for each pushed commit it
picks the bots worth asking (free ones and expiring credits first, the best
paid one for 300+ line changes or the label `review:deep`), triggers each as
the account that installed it, waits for their answers, and posts one PR
comment with their findings merged. No findings approves; any finding asks for
changes, and the next agent reads that comment. If no bot could review, the
push waits and the owner is told. Bot approvals never count. What it asked and
spent is in its own record beside the log.

To turn it on, add to the observer's service

    Environment=FACTORY_BOTS=/root/factory/src/deploy/bots.json
    Environment=FACTORY_REVIEW_RECORD=/root/factory/review.jsonl
    Environment=FACTORY_REPORT_AS=sadosystems
    Environment=GH_TOKEN_SADOSYSTEMS=...    # each bot's trigger_as account

and apply a config with `"reviewers": ["review-mux"]`. The bots must be
installed on the repository (see review_mux's docs/setup.md).

Everything they do goes through one log, `/root/factory/log.jsonl`. Nothing
else is the factory's state: not labels, not the services' memory.

## Seeing it

    ssh hetzner
    cd /root/factory
    PYTHONPATH=src FACTORY_LOG=log.jsonl venv/bin/python -m factory status
    PYTHONPATH=src venv/bin/python -m factory check log.jsonl
    journalctl -u factory-observer -u factory-dictator -u factory-watchdog -f
    ls -t state/logs | head          # each agent session's full output

Alarms (GitHub showing something the model says cannot happen) also go to
the ntfy topic in the service files.

## Steering it from GitHub

- **File work** as an issue with exactly one label: `enhancement`, `bug`,
  `friction` or `initiative`. Without one the factory asks and waits.
- **Place it** with lines in the body: `Part of #N` for its initiative,
  `Depends on #N` for each issue it waits for.
- **Hold it**: label `guard`, or assign it to the owner. A held issue is not
  taken and a held initiative is not planned until that is undone.
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
