#!/usr/bin/env python3
"""Aggregate + matched-task tables from sessions.json. Prints markdown to stdout."""
import json
import os
import re
import statistics as st
from collections import defaultdict, Counter

HERE = os.path.dirname(os.path.abspath(__file__))
S = json.load(open(os.path.join(HERE, "sessions.json")))
by_id = {s["session_id"]: s for s in S}
# Kit fallback linkage: same daemon pid (session id = s-<ms>-<pid>-<n>), depth-1 parent whose time window covers the child's start.
kit = [s for s in S if s["harness"] == "kit" and s["start"]]
for c in kit:
    if not c["is_subagent"] or c.get("parent"):
        continue
    pid = c["session_id"].split("-")[2]
    cands = [p for p in kit if p["depth"] == c["depth"] - 1 and p["end"] and p["start"] <= c["start"] <= p["end"]
             and p["session_id"].split("-")[2] == pid]
    if cands:
        c["parent"] = max(cands, key=lambda p: (p["cwd"] == c["cwd"], p["start"]))["session_id"]
        c["parent_inferred"] = True
children = defaultdict(list)
for s in S:
    par = by_id.get(s.get("parent"))
    if not par:
        continue
    if s["harness"] == "kit" and par["depth"] != s["depth"] - 1:
        s["parent"] = None
        continue
    children[s["parent"]].append(s["session_id"])


def max_concurrent(sids):
    ev = []
    for sid in sids:
        c = by_id[sid]
        if c["start"] and c["end"]:
            ev.append((c["start"], 1))
            ev.append((c["end"], -1))
    ev.sort()
    cur = best = 0
    for _, d in ev:
        cur += d
        best = max(best, cur)
    return best

ROLL = ("api_requests", "tool_calls", "inner_calls", "input_fresh", "input_cache_read", "input_cache_write",
        "input_total", "output_tokens", "reasoning_tokens", "compactions")


def rollup(sid, seen=None):
    seen = seen or set()
    s = by_id[sid]
    r = {k: s[k] for k in ROLL}
    r["peak_context"] = s["peak_context"]
    r["subagents"] = 0
    r["max_fanout"] = max(s["max_fanout"], max_concurrent(children.get(sid, [])))
    r["subagent_depth"] = 0
    for c in children.get(sid, []):
        if c in seen:
            continue
        seen.add(c)
        cr = rollup(c, seen)
        for k in ROLL:
            r[k] += cr[k]
        r["peak_context"] = max(r["peak_context"], cr["peak_context"])
        r["subagents"] += 1 + cr["subagents"]
        r["max_fanout"] = max(r["max_fanout"], cr["max_fanout"])
        r["subagent_depth"] = max(r["subagent_depth"], 1 + cr["subagent_depth"])
    return r


def norm(p):
    return re.sub(r"\s+", " ", (p or "").strip())


def fmt_k(n):
    return f"{n/1000:.0f}k" if n < 1e6 else f"{n/1e6:.2f}M"


def fmt_dur(sec):
    if sec is None:
        return "-"
    m = sec / 60
    return f"{m:.0f}m" if m < 120 else f"{m/60:.1f}h"


def row(s, task):
    r = rollup(s["session_id"])
    return (f"| {s['harness']} | {task} | {s['model'] or '?'} | {fmt_dur(s['duration_s'])} / {fmt_dur(s['active_s'])} | "
            f"{s['assistant_turns']} | {r['tool_calls']}{' (+' + str(r['inner_calls']) + ' inner)' if s['harness']=='kit' else ''} | "
            f"{fmt_k(r['input_total'])} (fresh {fmt_k(r['input_fresh'])}, cache-read {fmt_k(r['input_cache_read'])}, cache-write {fmt_k(r['input_cache_write'])}) | "
            f"{fmt_k(r['output_tokens'])} | {fmt_k(r['peak_context'])} | {r['compactions']} | {r['subagents']} |")


HDR = ("| harness | task | model | wall / active | turns | tool calls | total input tokens | output | peak ctx | compactions | subagents |\n"
       "|---|---|---|---|---|---|---|---|---|---|---|")


def find(harness, pattern, cwd_sub=None, start=None):
    out = []
    for s in S:
        if s["harness"] != harness or s["is_subagent"] or not s["first_prompt"] or s["api_requests"] == 0:
            continue
        if cwd_sub and cwd_sub not in (s["cwd"] or ""):
            continue
        if start and not s["start"].startswith(start):
            continue
        if re.search(pattern, norm(s["first_prompt"]), flags=re.I):
            out.append(s)
    return sorted(out, key=lambda s: s["start"])


print("## Matched tasks\n")
groups = [
    ("A. Ship a Linear issue end-to-end in speakeasy-api/gram (worktree, implement, PR, CI loop)", [
        ("kit", r"^(ship this (linear )?issue|in a worktree ship this issue|raise a pr for this issue linear)", "gram", None),
        ("codex", r"use ship-issue skill with linear ticket", "gram", None),
        ("claude", r"ship-issue</command-name>|^ship this linear issue", "gram", None),
    ]),
    ("B. Shepherd an existing PR to merge in speakeasy-api/gram (watch CI + review comments, fix, loop)", [
        ("kit", r"^shepherd( this pr)?:? #?\d+$", "gram", None),
        ("codex", r"^\$?shepherd \d+$", "gram", None),
        ("claude", r"shepherd</command-name>\s*<command-args>\d+</command-args>|^shepherd (this pr: )?#?\d+$", "gram", None),
    ]),
    ("C. Review / summarize uncommitted changes in projects/kit", [
        ("kit", r"^summarize uncommitted changes$", "kit", None),
        ("codex", r"^review uncommitted changes$", "kit", None),
        ("claude", r"^review the (current )?uncommitted (changes|diff) in [^ ]*/kit", "kit", None),
    ]),
    ("D. Rebase a PR on main in a worktree and push", [
        ("kit", r"^in a worktree rebase #1 on main", "kit", None),
        ("codex", r"^in a worktree #5293 rebase this on main", "gram", None),
    ]),
    ("E. Dependency update check in projects/kit", [
        ("kit", r"^(check (if we can update these deps|which of these dependencies)|some deps are out of date)", "kit", None),
    ]),
    ("F. percy-bench: 'prompt.md implement in reference/<model>' (Daniel's own model bench; no Kit run)", [
        ("codex", r"prompt\.md implement in", "percy-bench", None),
        ("claude", r"prompt\.md implement in", "percy-bench", None),
    ]),
    ("G. 'generate a file that's 10mb+ then read it in full via a tool call' (identical prompt; no Kit run)", [
        ("codex", r"generate a file that's 10mb", None, None),
        ("claude", r"generate a file that's 10mb", None, None),
    ]),
]
matched_rows = []
for title, specs in groups:
    print(f"### {title}\n")
    print(HDR)
    for harness, pat, cwd, start in specs:
        for s in find(harness, pat, cwd, start):
            task = norm(s["first_prompt"])
            task = re.sub(r"<command-message>.*?</command-message>\s*", "", task)
            task = re.sub(r"<command-name>(.*?)</command-name>\s*<command-args>(.*?)</command-args>.*", r"\1 \2", task)
            task = task[:70]
            print(row(s, task))
            matched_rows.append((title, s))
    print()

# ------------------------------------------------------------------ aggregates
print("## Aggregate stats (top-level sessions only, subagent sessions rolled into their parent; sessions with >=1 API request)\n")


def med(xs):
    return st.median(xs) if xs else float("nan")


print("| metric | kit | codex | claude |\n|---|---|---|---|")
tops = {h: [s for s in S if s["harness"] == h and not s["is_subagent"] and s["api_requests"] > 0] for h in ("kit", "codex", "claude")}
rolls = {h: [rollup(s["session_id"]) for s in tops[h]] for h in tops}
rows = []
rows.append(("sessions (top-level)", {h: len(tops[h]) for h in tops}))
rows.append(("subagent sessions on disk", {h: sum(1 for s in S if s["harness"] == h and s["is_subagent"]) for h in tops}))
rows.append(("median assistant turns", {h: med([s["assistant_turns"] for s in tops[h]]) for h in tops}))
rows.append(("median API requests (rolled up)", {h: med([r["api_requests"] for r in rolls[h]]) for h in tops}))
rows.append(("median tool calls (rolled up)", {h: med([r["tool_calls"] for r in rolls[h]]) for h in tops}))
rows.append(("median tool calls / turn", {h: med([r["tool_calls"] / max(s["assistant_turns"], 1) for s, r in zip(tops[h], rolls[h])]) for h in tops}))
rows.append(("median total input tokens / session (rolled up)", {h: med([r["input_total"] for r in rolls[h]]) for h in tops}))
rows.append(("median fresh (uncached) input tokens / session", {h: med([r["input_fresh"] for r in rolls[h]]) for h in tops}))
rows.append(("median cache-read share of input", {h: med([r["input_cache_read"] / r["input_total"] for r in rolls[h] if r["input_total"]]) for h in tops}))
rows.append(("median output tokens / session", {h: med([r["output_tokens"] for r in rolls[h]]) for h in tops}))
rows.append(("median input tokens / assistant turn", {h: med([r["input_total"] / max(s["assistant_turns"], 1) for s, r in zip(tops[h], rolls[h])]) for h in tops}))
rows.append(("median input tokens / API request", {h: med([r["input_total"] / r["api_requests"] for r in rolls[h] if r["api_requests"]]) for h in tops}))
rows.append(("median output tokens / assistant turn", {h: med([r["output_tokens"] / max(s["assistant_turns"], 1) for s, r in zip(tops[h], rolls[h])]) for h in tops}))
rows.append(("median peak context (tokens)", {h: med([r["peak_context"] for r in rolls[h]]) for h in tops}))
rows.append(("median wall-clock duration", {h: med([s["duration_s"] for s in tops[h]]) for h in tops}))
rows.append(("median active duration (gaps capped at 5m)", {h: med([s["active_s"] for s in tops[h]]) for h in tops}))
rows.append(("sessions with >=1 compaction", {h: f"{sum(1 for r in rolls[h] if r['compactions'])} ({100*sum(1 for r in rolls[h] if r['compactions'])/len(rolls[h]):.0f}%)" for h in tops}))
rows.append(("total compactions / total top-level sessions", {h: f"{sum(r['compactions'] for r in rolls[h])}/{len(rolls[h])}" for h in tops}))
rows.append(("total input tokens, all top-level trees", {h: fmt_k(sum(r["input_total"] for r in rolls[h])) for h in tops}))
rows.append(("compactions per 100M input tokens", {h: f"{100e6*sum(r['compactions'] for r in rolls[h])/sum(r['input_total'] for r in rolls[h]):.1f}" for h in tops}))
rows.append(("compactions per 1000 API requests", {h: f"{1000*sum(r['compactions'] for r in rolls[h])/sum(r['api_requests'] for r in rolls[h]):.1f}" for h in tops}))
rows.append(("sessions using subagents", {h: f"{sum(1 for r in rolls[h] if r['subagents'])} ({100*sum(1 for r in rolls[h] if r['subagents'])/len(rolls[h]):.0f}%)" for h in tops}))
rows.append(("max subagents in one session tree", {h: max(r["subagents"] for r in rolls[h]) for h in tops}))
rows.append(("max concurrent subagents under one parent", {h: max(r["max_fanout"] for r in rolls[h]) for h in tops}))
rows.append(("kit subagent files linked to a parent", {h: (f"{sum(1 for s in S if s['harness']=='kit' and s['is_subagent'] and s.get('parent'))}/{sum(1 for s in S if s['harness']=='kit' and s['is_subagent'])}" if h == "kit" else "-") for h in tops}))
rows.append(("max subagent nesting depth", {h: max(r["subagent_depth"] for r in rolls[h]) for h in tops}))
for name, d in rows:
    def f(v):
        if isinstance(v, str):
            return v
        if "duration" in name:
            return fmt_dur(v)
        if "tokens" in name and "share" not in name and "/ turn" not in name:
            return fmt_k(v)
        if "share" in name:
            return f"{100*v:.0f}%"
        return f"{v:.1f}" if isinstance(v, float) else str(v)
    print(f"| {name} | " + " | ".join(f(d[h]) for h in ("kit", "codex", "claude")) + " |")
print()

print("## Longest sessions\n")
for h in tops:
    ss = tops[h]
    rr = dict(zip((s["session_id"] for s in ss), rolls[h]))
    print(f"### {h}\n")
    print("| criterion | session | model | wall / active | turns | API reqs | tool calls | input | output | peak ctx | compactions | subagents | prompt |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for crit, key in (("wall-clock", lambda s: s["duration_s"]), ("active time", lambda s: s["active_s"]),
                      ("assistant turns", lambda s: s["assistant_turns"]), ("API requests (rolled up)", lambda s: rr[s["session_id"]]["api_requests"]),
                      ("total input tokens (rolled up)", lambda s: rr[s["session_id"]]["input_total"]), ("compactions", lambda s: rr[s["session_id"]]["compactions"]),
                      ("subagents", lambda s: rr[s["session_id"]]["subagents"])):
        s = max(ss, key=key)
        r = rr[s["session_id"]]
        print(f"| {crit} | {s['session_id'][:24]} | {s['model']} | {fmt_dur(s['duration_s'])} / {fmt_dur(s['active_s'])} | {s['assistant_turns']} | {r['api_requests']} | {r['tool_calls']} | {fmt_k(r['input_total'])} | {fmt_k(r['output_tokens'])} | {fmt_k(r['peak_context'])} | {r['compactions']} | {r['subagents']} | {norm(s['first_prompt'])[:80]} |")
    print()

print("## Per-group medians for matched families A and B\n")
print("| family | harness | n | median active | median turns | median tool calls | median input | median output | median peak ctx | median compactions |\n|---|---|---|---|---|---|---|---|---|---|")
fam = defaultdict(lambda: defaultdict(list))
for title, s in matched_rows:
    fam[title[:2]][s["harness"]].append(s)
for f in ("A.", "B."):
    for h in ("kit", "codex", "claude"):
        ss = fam[f][h]
        if not ss:
            continue
        rr = [rollup(s["session_id"]) for s in ss]
        print(f"| {f} | {h} | {len(ss)} | {fmt_dur(med([s['active_s'] for s in ss]))} | {med([s['assistant_turns'] for s in ss]):.0f} | {med([r['tool_calls'] for r in rr]):.0f} | {fmt_k(med([r['input_total'] for r in rr]))} | {fmt_k(med([r['output_tokens'] for r in rr]))} | {fmt_k(med([r['peak_context'] for r in rr]))} | {med([r['compactions'] for r in rr]):.0f} |")
print()
print("## Kit subagent sessions (depth>=1) profile\n")
subs = [s for s in S if s["harness"] == "kit" and s["is_subagent"] and s["api_requests"] > 0]
print(f"- n={len(subs)}; median API requests {med([s['api_requests'] for s in subs]):.0f}; median input {fmt_k(med([s['input_total'] for s in subs]))}; median output {fmt_k(med([s['output_tokens'] for s in subs]))}; median active {fmt_dur(med([s['active_s'] for s in subs]))}; median peak ctx {fmt_k(med([s['peak_context'] for s in subs]))}")
print(f"- depth distribution: {dict(Counter(s['depth'] for s in subs))}")
print(f"- inferred parent links: {sum(1 for s in S if s.get('parent_inferred'))}")
top_fan = sorted(((rollup(s['session_id'])['max_fanout'], rollup(s['session_id'])['subagents'], s['session_id'], norm(s['first_prompt'])[:60]) for s in tops['kit']), reverse=True)[:5]
print(f"- top kit sessions by concurrent subagents (concurrent, total, id, prompt): {top_fan}")
print()
print("## Model mix (top-level sessions)\n")
for h in tops:
    print(f"- {h}: {dict(Counter(s['model'] for s in tops[h]).most_common(6))}")
print()
print("## Date ranges\n")
for h in tops:
    print(f"- {h}: {min(s['start'] for s in tops[h])[:10]} .. {max(s['start'] for s in tops[h])[:10]}")
print()
print("## Kit tool-call composition (all kit sessions incl. subagents)\n")
inner = Counter()
for s in S:
    if s["harness"] == "kit":
        pass
print(f"- compose calls: {sum(s['tool_calls'] for s in S if s['harness']=='kit')}, inner child-tool calls parsed from scripts: {sum(s['inner_calls'] for s in S if s['harness']=='kit')}")
print(f"- codex tool names: {dict(Counter(sum((list(s['tool_names'].items()) for s in S if s['harness']=='codex'), [])).most_common(0))}")
cn = Counter()
for s in S:
    if s["harness"] == "codex":
        for k, v in s["tool_names"].items():
            cn[k] += v
print(f"- codex tool names: {dict(cn.most_common(8))}")
cn = Counter()
for s in S:
    if s["harness"] == "claude":
        for k, v in s["tool_names"].items():
            cn[k] += v
print(f"- claude tool names: {dict(cn.most_common(12))}")
