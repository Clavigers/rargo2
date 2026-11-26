# Rules for a well instrumented software factory

A factory is well instrumented when its own records are enough to rebuild what
happened, measure it, and compare it with a simulation of the same factory,
without reading chat transcripts or guessing from GitHub history.

## 1. One event log

1. Every actor writes to one append-only event log. There is no second log
   with a different shape.
2. One line is one event, as JSON, with at least `t` (UTC, ISO 8601, with
   seconds), `event`, and `actor`.
3. Events name things by stable ids: issue number, pull request number, push
   number within the pull request, release version, agent name.
4. The simulation writes the same log in the same format. Any analysis runs
   unchanged on a real log or a simulated one.

## 2. A fixed vocabulary

5. Event names come from a short, fixed list. Adding a name is a versioned
   change to this list, never an ad hoc string.

| Event | Meaning | Extra fields |
|---|---|---|
| `filed` | An issue or initiative exists | `issue`, `kind`, `by`, `parent` |
| `split` | The foreman turned an initiative into sub-issues | `issue`, `children`, `deps` |
| `blocked`, `unblocked` | An issue started or stopped waiting on another | `issue`, `on` |
| `taken` | An agent took an issue into its slot | `issue`, `agent`, `model` |
| `released` | An issue left an agent's slot without a push | `issue`, `agent`, `reason`: `quota`, `preempted`, `stale`, `found_work` |
| `push` | An agent pushed to a pull request | `issue`, `pr`, `push`, `agent`, `model` |
| `ci_done` | CI finished on a push | `pr`, `push`, `result`: `pass`, `fail`, `cancelled` |
| `review_done` | The review agent finished on a push | `pr`, `push`, `result`: `approved`, `changes`, `cancelled`, `reason` |
| `merged` | A pull request merged into main | `pr`, `issue`, `push` |
| `quota_out`, `quota_back` | An agent's plan stopped or resumed it | `agent`, `model` |
| `foreman_tick` | The foreman ran | `planned`, `unlocked`, `preempted` |
| `freeze`, `rc`, `release` | Release branch cut, candidate built, release published | `version`, `build` |
| `test_session` | The power user or the human used a build | `by`, `version`, `minutes` |
| `bug_found` | A bug was found | `issue`, `by`, `where`: `review`, `work`, `rc`, `release`; `version` |

## 3. Record decisions, not only outcomes

6. Every transition that a person or agent chose is logged with its reason:
   why an issue was released, why review asked for changes, why the foreman
   pulled an agent off work, why a release was held.
7. Every push records which model wrote it. Defect rates per model are measured
   from this field, not inferred.
8. Every bug records who found it and in which build. A bug found in a release
   is linked to the pull request that introduced it as soon as that is known.

## 4. Time is the primary measurement

9. Every state an item can be in has a start event and an end event, so the
   time spent in each state is computable for every item.
10. Waiting is logged as explicitly as working. An agent that is idle, out of
    quota, or holding a lock it cannot use is a state with events, not the
    absence of events.

## 5. The configuration is part of the record

11. The factory's policies live in one versioned file: agents and their models,
    plans and quotas, review timing, foreman interval, freeze and soak rules,
    power user schedule.
12. Every change to that file is logged as an event with the new version. Every
    measurement window names the configuration version it ran under.
13. The simulation reads the same configuration file. A preset and a factory
    version are the same object.

## 6. Validation is routine

14. Each factory version is followed by a validation round: the real log of a
    window is compared with the distribution of the same statistics over many
    simulated runs of the same configuration and the same filing history.
15. Statistics used to set a parameter are never used to validate it. A window
    used for calibration is not the window used for validation.
16. A real statistic outside the 5th to 95th percentile of the simulated
    distribution is a finding. A pattern of findings is a modelling problem; a
    single finding is noted and watched.
17. Parameters with no measurement behind them are marked as guesses in the
    configuration until a window measures them.

## 7. Cheap to keep

18. Logging is part of each actor's normal work, not a separate reporting step.
    An actor that cannot write its event does not take the action.
19. Nothing in the log needs a human to interpret it. Free text is allowed only
    in `reason` fields.
20. The log is never edited. Corrections are new events that reference the
    event they correct.
