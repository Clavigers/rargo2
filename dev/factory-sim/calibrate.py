#!/usr/bin/env python3
"""Measures the real factory and writes calibration.json, which sim.py loads.

  python3 calibrate.py [--repo OWNER/NAME] [--ssh HOST] [--cache DIR]

Reads pull requests, issues and CI runs through gh, and the watchdog and
foreman logs over ssh. With --cache DIR it reuses files already in DIR.
Each value is written with the evidence behind it, and the page shows that.
"""
import argparse
import collections
import json
import math
import pathlib
import re
import statistics as st
import subprocess
import tempfile
from datetime import datetime

ap = argparse.ArgumentParser()
ap.add_argument("--repo", default="Clavigers/rargo2")
ap.add_argument("--ssh", default="hetzner")
ap.add_argument("--cache")
args = ap.parse_args()
cache = pathlib.Path(args.cache or tempfile.mkdtemp())
cache.mkdir(parents=True, exist_ok=True)


def fetch(name, cmd):
    f = cache / name
    if not f.exists():
        f.write_text(subprocess.run(cmd, shell=True, capture_output=True, text=True, check=True).stdout)
    return f.read_text()


R = args.repo
prs = json.loads(fetch("prs.json", f"gh pr list -R {R} --state all --limit 2000 --json number,createdAt,mergedAt,headRefName,closingIssuesReferences"))
issues = json.loads(fetch("issues.json", f"gh issue list -R {R} --state all --limit 2000 --json number,createdAt,closedAt,labels,author,body"))
runs = json.loads(fetch("runs.json", f"gh run list -R {R} --workflow ci.yml --limit 1000 --json conclusion,startedAt,updatedAt"))
rels = json.loads(fetch("rel.json", f"gh release list -R {R} --limit 200 --json publishedAt"))
watch = fetch("watch.log", f"ssh -o BatchMode=yes {args.ssh} cat /root/watchdog/watch.log").splitlines()
foreman = fetch("foreman.log", f"ssh -o BatchMode=yes {args.ssh} cat /root/rargo-work/foreman/log.txt").splitlines()

ts = lambda s: datetime.fromisoformat(s.replace("Z", "+00:00"))
mins = lambda a, b: (ts(b) - ts(a)).total_seconds() / 60
labels = lambda i: {l["name"] for l in i["labels"]}
out, today = {}, {}


def put(key, value, source, into=out):
    into[key] = dict(value=round(value, 3), source=source)


merged = [p for p in prs if p["mergedAt"]]
span_days = mins(min(p["mergedAt"] for p in merged), max(p["mergedAt"] for p in merged)) / 1440
active_hours = len({p["mergedAt"][:13] for p in merged})
open_to_merge = st.median(mins(p["createdAt"], p["mergedAt"]) for p in merged)

# CI.
done = [r for r in runs if r["conclusion"] in ("success", "failure")]
fails = sum(r["conclusion"] == "failure" for r in done)
put("ci_time", st.median(mins(r["startedAt"], r["updatedAt"]) for r in done), f"median of {len(done)} CI runs")
put("ci_flake", fails / len(done), f"{fails} of {len(done)} CI runs failed")

# The independent review, from the watchdog log.
started, durations, verdicts = {}, [], collections.defaultdict(list)
stamp = lambda l: datetime.strptime(l[:19], "%Y-%m-%d %H:%M:%S")
for l in watch:
    if m := re.search(r"review #(\d+): started", l):
        started[m[1]] = stamp(l)
    if m := re.search(r"review #(\d+): verdict: (verified|not_done)", l):
        verdicts[m[1]].append(m[2])
        if m[1] in started:
            durations.append((stamp(l) - started[m[1]]).total_seconds() / 60)
first_bad = sum(v[0] == "not_done" for v in verdicts.values())
again = [v for v in verdicts.values() if v[0] == "not_done" and len(v) > 1]
again_bad = sum(v[1] == "not_done" for v in again)
p_first = first_bad / len(verdicts)
put("gate_time", st.mean(durations), f"mean of {len(durations)} reviews in watch.log")
# The review sends work back when it catches missing work or a bug. Split the
# measured rate using assumed catch rates (0.85 and 0.4) and bug rate (0.2).
p_inc = (1 - (1 - p_first) / (1 - 0.2 * 0.4)) / 0.85
put("p_incomplete", p_inc, f"{first_bad} of {len(verdicts)} first reviews reopened the issue; "
    "solved for the PR rate assuming the review catches 85% of gaps and 40% of bugs")
put("rework_defect", (again_bad / len(again)) / p_first,
    f"{again_bad} of {len(again)} second reviews failed, against {first_bad} of {len(verdicts)} first reviews")

# Workers.
locks = {a: sum(f"wip-{a}" in labels(i) for i in issues) for a in ("claude", "codex", "grok")}
prs_per_issue = len(merged) / max(1, sum(bool(i["closedAt"]) and "initiative" not in labels(i) for i in issues))
claude_prs = len(merged) * locks["claude"] / sum(locks.values())
cycle = active_hours * 60 / claude_prs
put("work_time", cycle - open_to_merge,
    f"claude held {locks['claude']} of {sum(locks.values())} locked issues, so about {claude_prs:.0f} of {len(merged)} "
    f"merged PRs in {active_hours} active hours: {cycle:.1f} min per PR, less the {open_to_merge:.1f} min median from PR open to merge")
put("claude_quota", 168, "ran the whole window without a usage stop")
put("codex_quota", locks["codex"] * prs_per_issue * cycle / 60,
    f"locked {locks['codex']} issues before its weekly limit stopped it; at {prs_per_issue:.2f} PRs per issue and {cycle:.1f} min per PR")
put("grok_quota", locks["grok"] * prs_per_issue * cycle / 60 * 7 / span_days,
    f"locked {locks['grok']} issues in {span_days:.1f} days between usage stops, scaled to a week")
agent_bugs = sum("bug" in labels(i) and i["author"]["login"] != R.split("/")[0] for i in issues)
put("self_find", agent_bugs / len(merged), f"agents filed {agent_bugs} bug issues over {len(merged)} merged PRs")

# Demand: what the owner asks for, and how initiatives fan out.
inits = [i for i in issues if "initiative" in labels(i)]
sizes = []
children = set()
for init in inits:
    kids = {i["number"] for i in issues if i is not init and re.search(rf"#{init['number']}\b", i["body"] or "")}
    children |= kids
    sizes.append(max(1, len(kids)))
singles = [i for i in issues if i["author"]["login"] == R.split("/")[0]
           and i["number"] not in children and "initiative" not in labels(i)]
put("requests_per_day", (len(inits) + len(singles)) / span_days,
    f"{len(inits)} initiatives and {len(singles)} single issues from the owner in {span_days:.1f} days")
put("init_share", len(inits) / (len(inits) + len(singles)), f"{len(inits)} of {len(inits) + len(singles)} owner requests were initiatives")
put("init_median", st.median(sizes), f"initiative sizes, by issues that mention them: {sorted(sizes)}")
put("init_sigma", st.pstdev(math.log(s) for s in sizes), "spread of the log of those sizes")

# How the issue graph grows after planning: issues agents file that no
# initiative planned, and initiative children filed well after the rest.
owner = R.split("/")[0]
unplanned = sum(i["author"]["login"] != owner and "bug" not in labels(i) and "initiative" not in labels(i)
                and i["number"] not in children for i in issues)
late = 0
for init in inits:
    born = sorted(mins(init["createdAt"], i["createdAt"]) for i in issues
                  if i is not init and re.search(rf"#{init['number']}\b", i["body"] or ""))
    late += sum(b > st.median(born) + 60 for b in born) if born else 0
put("p_followup", (unplanned + late) / len(merged),
    f"{unplanned} issues filed by agents outside any initiative, and {late} initiative children filed over an hour "
    f"after the rest, over {len(merged)} merged PRs")

# Foreman and the way things run today.
ticks = [datetime.strptime(l[:16], "%Y-%m-%d %H:%M") for l in foreman if " tick" in l[:28]]
put("foreman_tick", st.median((b - a).total_seconds() / 60 for a, b in zip(ticks, ticks[1:])),
    f"median gap between {len(ticks)} ticks in the foreman log")
put("freeze_days", span_days / len(rels), f"{len(rels)} releases in {span_days:.1f} days", today)
put("restart_delay", 7.5, "watch.sh polls every 5 min and nudges on the second idle check", today)

result = dict(repo=R, window_days=round(span_days, 2), merged_prs=len(merged),
              reference=dict(prs_per_day=round(len(merged) / span_days, 1), prs_per_issue=round(prs_per_issue, 2),
                             first_review_reopen_rate=round(p_first, 3),
                             peak_prs_in_a_day=max(collections.Counter(p["mergedAt"][:10] for p in merged).values())),
              params=out, presets=dict(Today=today))
pathlib.Path(__file__).with_name("calibration.json").write_text(json.dumps(result, indent=1) + "\n")
print(json.dumps(result, indent=1))
