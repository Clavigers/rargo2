"""factory: the rargo2 factory's log, from the command line.

  check LOG                 every line well formed and legal: exit 0
  submit [--dry] BODY       append one event; BODY is its JSON without the
                            envelope. Prints the line. --dry: check only.
                            exit 0 written (or would be), 1 malformed,
                            2 refused (a decision that did not happen),
                            3 alarm (an observation the model says cannot be)
  status                    the factory at a glance
  config                    the config in force, as JSON
  mine                      what this agent holds, as JSON
  ready                     issues ready to take, best first, as JSON
  init CONFIG               start the log with this config (a JSON file)
  usage STATE [--every S]   read each plan's quota use and reset times into
                            STATE/usage.json (once, or every S seconds)
  watchdog REPO STATE       run the watchdog: keep a session per agent, each
                            in a worktree of the clone REPO, under STATE
  observe SLUG OWNER [LOGIN...]
                            run the observer and merger on GitHub repository
                            SLUG (org/name). OWNER is the human's login: their
                            acts on GitHub are the human's. Issues from OWNER
                            and the LOGINs given are the factory's
  pm SLUG CHECKOUT           run the project manager; it cuts release branches in the clone CHECKOUT
  power-user SLUG CHECKOUT      run the power user; it uses each candidate build in the clone CHECKOUT
  dictator SLUG CHECKOUT     run the dictator; it plans in the clone CHECKOUT
  doc                       rewrite the generated half of DATA-MODEL.md
  schema                    JSON Schema of an event body

submit, mine and ready read the log from FACTORY_LOG and act as FACTORY_ACTOR;
the watchdog sets both for every session it starts. status also says when
this code differs from dev/factory on origin/main in the clone FACTORY_REPO
(default: the directory repo beside the log).
"""
import json
import os
import pathlib
import sys

from pydantic import ValidationError

from . import doc, model
from .ledger import Alarm, Ledger, Refused
from .world import Illegal, World

HERE = pathlib.Path(__file__).parent.parent
DOC = HERE / "DATA-MODEL.md"
PROMPT = HERE / "AGENT.md"
DICTATOR_PROMPT = HERE / "DICTATOR.md"


def env(name: str) -> str:
    v = os.environ.get(name)
    if not v:
        sys.exit(f"factory: {name} is not set")
    return v


def check(path: str) -> int:
    w = World()
    with open(path) as f:
        for n, text in enumerate(f, 1):
            try:
                w.apply(model.parse(text))
            except (ValidationError, ValueError) as err:
                print(f"{path}:{n}: malformed: {err}", file=sys.stderr)
                return 1
            except Illegal as err:
                print(f"{path}:{n}: illegal: {err.msg}", file=sys.stderr)
                return 1
    print(f"{path}: {w.seq} lines, legal")
    return 0


def submit(body: str, dry: bool) -> int:
    try:
        line = Ledger(env("FACTORY_LOG")).submit(env("FACTORY_ACTOR"), body, dry=dry)
    except (ValidationError, ValueError) as err:
        print(f"malformed: {err}", file=sys.stderr)
        return 1
    except Refused as err:
        print(err, file=sys.stderr)
        return 2
    except Alarm as err:
        print(f"ALARM: {err}", file=sys.stderr)
        return 3
    print(line.dump())
    return 0


def main(argv: list[str]) -> int:
    match argv:
        case ["check", path]:
            return check(path)
        case ["submit", body]:
            return submit(body, dry=False)
        case ["submit", "--dry", body]:
            return submit(body, dry=True)
        case ["status"]:
            print(Ledger(env("FACTORY_LOG")).world.status())
            from .drift import line
            repo = os.environ.get("FACTORY_REPO") or pathlib.Path(env("FACTORY_LOG")).parent / "repo"
            said = line(HERE, pathlib.Path(repo))
            if said:
                print(said)
            return 0
        case ["config"]:
            print(Ledger(env("FACTORY_LOG")).world.config.model_dump_json(indent=1))
            return 0
        case ["mine"]:
            w = Ledger(env("FACTORY_LOG")).world
            print(json.dumps(w.agent_view(env("FACTORY_ACTOR"))))
            return 0
        case ["ready"]:
            w = Ledger(env("FACTORY_LOG")).world
            print(json.dumps([{"issue": n, "kind": str(w.issues[n].kind), "pr": w.issues[n].pr}
                              for n in w.ready_queue(os.environ.get("FACTORY_ACTOR"))]))
            return 0
        case ["init", config]:
            body = json.dumps({"event": "config_applied",
                               "config": json.loads(pathlib.Path(config).read_text())})
            led = Ledger(env("FACTORY_LOG"))
            if led.world.seq:
                sys.exit("factory: the log is not empty; apply a new config with submit")
            print(led.submit("human", body).dump())
            return 0
        case ["observe", slug, owner, *trusted]:
            import time
            from .github import GitHub
            from .observer import Observer
            gh = GitHub(slug)
            reviewers = {}
            if os.environ.get("FACTORY_BOTS"):  # the review bots, through review-mux
                from .review.bots import load
                from .review.mux import Mux
                from .review.record import Record
                mux = Mux(gh, Record(env("FACTORY_REVIEW_RECORD")), load(env("FACTORY_BOTS")),
                          report_as=os.environ.get("FACTORY_REPORT_AS") or None)
                reviewers["review-mux"] = mux.verdict
            obs = Observer(Ledger(env("FACTORY_LOG")), gh, owner,
                           trusted=frozenset(trusted), reviewers=reviewers)
            while True:
                obs.step()
                time.sleep(60)
        case ["dictator", slug, checkout]:
            from .dictator import Dictator
            from .github import GitHub
            Dictator(Ledger(env("FACTORY_LOG")), GitHub(slug), pathlib.Path(checkout),
                    DICTATOR_PROMPT.read_text(),
                    orchestrate_prompt=(HERE / "ORCHESTRATE.md").read_text(),
                    answer_prompt=(HERE / "ANSWER.md").read_text(),
                    state=pathlib.Path(env("FACTORY_LOG")).parent / "state").run()
            return 0
        case ["pm", slug, checkout]:
            import shlex
            from .github import GitHub
            from .pm import Pm
            cmd = os.environ.get("FACTORY_RELEASE")  # run with the version; unset: publish nothing
            Pm(Ledger(env("FACTORY_LOG")), GitHub(slug), pathlib.Path(checkout),
               publish=shlex.split(cmd) if cmd else None,
               notes_prompt=(HERE / "RELEASE-NOTES.md").read_text(),
               state=pathlib.Path(env("FACTORY_LOG")).parent / "state").run()
            return 0
        case ["power-user", slug, checkout]:
            from .github import GitHub
            from .poweruser import PowerUser
            PowerUser(Ledger(env("FACTORY_LOG")), GitHub(slug), pathlib.Path(checkout),
                      (HERE / "POWER-USER.md").read_text()).run()
            return 0
        case ["usage", state, *rest]:
            from . import usage
            return usage.main([state, *rest])
        case ["watchdog", repo, state]:
            from .watchdog import Watchdog
            Watchdog(Ledger(env("FACTORY_LOG")), pathlib.Path(repo), pathlib.Path(state),
                     PROMPT.read_text()).run()
            return 0
        case ["doc"]:
            DOC.write_text(doc.splice(DOC.read_text()))
            return 0
        case ["schema"]:
            print(json.dumps(model.BODY.json_schema(), indent=2))
            return 0
    print(__doc__, file=sys.stderr)
    return 2


sys.exit(main(sys.argv[1:]))
