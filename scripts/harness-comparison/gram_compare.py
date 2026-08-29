#!/usr/bin/env python3
"""Like-for-like 'ship a Linear issue in speakeasy-api/gram' comparison across Kit / Codex / Claude.

Reads sessions.json (from parse_all.py), fetches PR stats with gh (cached in prs.json),
prints markdown to stdout. Session->PR mapping is hand-verified from raw transcripts
(gh pr create output / Claude gitOperation.pr.created / Kit shell() results); see PR_MAP.
"""
import json
import os
import re
import statistics as st
import subprocess
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
S = json.load(open(os.path.join(HERE, "sessions.json")))
by_id = {s["session_id"]: s for s in S}

# ---------------------------------------------------------------- subagent linkage + rollup (same rules as analyze.py)
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
        continue
    children[s["parent"]].append(s["session_id"])

ROLL = ("api_requests", "tool_calls", "inner_calls", "input_fresh", "input_cache_read", "input_cache_write",
        "input_total", "output_tokens", "reasoning_tokens", "compactions")


def rollup(sid, seen=None):
    seen = seen if seen is not None else set()
    s = by_id[sid]
    r = {k: s[k] for k in ROLL}
    r["peak_context"] = s["peak_context"]
    r["subagents"] = 0
    r["sub_inferred"] = 0
    for c in children.get(sid, []):
        if c in seen:
            continue
        seen.add(c)
        cr = rollup(c, seen)
        for k in ROLL:
            r[k] += cr[k]
        r["peak_context"] = max(r["peak_context"], cr["peak_context"])
        r["subagents"] += 1 + cr["subagents"]
        r["sub_inferred"] += cr["sub_inferred"] + (1 if by_id[c].get("parent_inferred") else 0)
    return r


# ---------------------------------------------------------------- selection
# (harness, issue, [session ids], [PR numbers], note)
PR_MAP = [
    ("kit", "DNO-881", ["s-1787088137876-76514-1"], [5460], "prompt: 'raise a pr for this issue linear: DNO-881'"),
    ("kit", "DNO-927", ["s-1787160298888-11355-1"], [5515], "prompt: 'ship this linear issue: DNO-927'"),
    ("kit", "DNO-939", ["s-1787332399269-74710-1"], [5601], "prompt: 'ship this issue: DNO-939'; PR later shepherded in a separate Claude session (9a1fecf1, 2026-08-24)"),
    ("kit", "DNO-941", ["s-1787566769555-37837-1"], [5627], "prompt: 'ship this issue: DNO-941 ignore existing PR raised by a bot' (bot PR #5589 closed)"),
    ("kit", "DNO-761", ["s-1787586134474-91752-1"], [5644], "prompt: 'in a worktree ship this issue: DNO-761 ... slack thread'"),
    ("codex", "DNO-396", ["019f37c2-fa82-7ec2-b9c2-f0954d041b7c"], [3901], "prompt: 'use ship-issue skill with linear ticket: DNO-396'"),
    ("codex", "AGE-2563", ["019e7467-9768-71d2-bdf9-8babdc9f8bb6", "019e74a7-3e19-7441-adde-24751aad2664"], [3116],
     "TWO sessions summed: 'check this linear issue: AGE-2563 ...' (investigate+implement, 17 user msgs) then 'check the linear issue on this branch, sum up the changes, raise a PR'"),
    ("claude", "DNO-675", ["dad31c12-55ce-4654-9deb-2fa7264a9cbb"], [4694], "/ship-issue DNO-675"),
    ("claude", "DNO-117", ["77f3dbbd-7052-47e5-82ac-13d30670521e"], [4855], "/ship-issue DNO-117 (old issue, ground yourself in codebase)"),
    ("claude", "AGE-3104", ["879962ef-41c6-430e-b2bf-03f9708be2d1"], [4987, 4997], "/ship-issue AGE-3104; same session also opened follow-up #4997 next morning (both counted)"),
    ("claude", "(none)", ["f636fac8-1994-4274-870d-5f815aafcf08"], [5128], "'[Image] raise a PR to fix the styling ...' (no Linear issue)"),
    ("claude", "(none)", ["c72d171f-1b3f-42e9-bc0b-a16cd81a3ffe"], [5230, 5229], "'[Image] raise a PR to make everything user-configurable on this UI' (no Linear issue); #5229 is the split-off migration PR; #5229 later shepherded by a Codex session"),
    ("claude", "DNO-883->DNO-884", ["09b89485-ca39-4089-aae0-76c06752bf18"], [5445], "'check this linear issue: DNO-883 verify it against docs' -> DNO-883 canceled, session shipped DNO-884 instead"),
    ("claude", "GRW-48", ["ed889062-f2fb-4a2e-bcf9-811a6ba374eb"], [5827], "'ship this linear issue: GRW-48 ...'; PR shepherded next day in a separate Claude session (b5258fa4, 42 req, 3.5h)"),
]

EXCLUDED = [
    ("claude", "DNO-715", "649658b2", "docs issue; PR is speakeasy-api/marketing-site#1981 (+246/-3, 9 files, merged) — different repo"),
    ("claude", "AGE-3091", "770547bf", "docs issue (opus-5, 603 req); PRs are marketing-site#1949 (+188/-8, 19 files) and #1948 mermaid rendering (+858/-4) — different repo"),
    ("codex", "DNO-480", "019f5899", "'on the branch for DNO-480 ... find, fix and push' — bugfix pushed to an existing branch, no PR created"),
    ("codex", "AGE-2563 (investigation)", "019eb622/019eb6df/019f0313/019f03cd", "'check/investigate/plan' Linear issue sessions (DNO-202/203/349/277) — analysis only, no PR"),
    ("codex", "self-serve billing epic", "019fff9e", "$ship-epic (42 spawn_agent, 9674 req, 84h wall) producing the #5302..#5372 stack; multi-issue, delegated implementation to `claude -p` sessions that appear on disk as ~60 top-level Claude sessions in gram-dno-8xx worktrees — not per-issue comparable"),
    ("claude", "litellm stack", "c0172b29", "'I started working on <project> there's a stack of PRs open' — multi-PR stack continuation, 860 req"),
    ("?", "killswitch stack DNO-974..980", "n/a", "PRs #5798..#5839 on 2026-08-27; implementation delegated to Claude worktree sessions (bbb2229e, e9c6a93b, ...) but the orchestrator session is not in any of the three transcript roots"),
]

GEN = re.compile(r'(^|/)(gen|generated|__generated__)/|\.gen\.|_gen\.go$|\.sql\.go$|/goa/|openapi.*\.(json|yaml)$|pnpm-lock|package-lock|\.snap$|/sdk/|client/sdk|/dist/|\.pb\.go$|gen\.lock$|migrations/.*\.sql$', re.I)
PR_CACHE = os.path.join(HERE, "prs.json")
prs = json.load(open(PR_CACHE)) if os.path.exists(PR_CACHE) else {}


def pr_info(n):
    k = str(n)
    if k not in prs:
        out = subprocess.check_output(["gh", "pr", "view", k, "--repo", "speakeasy-api/gram", "--json",
                                       "number,title,additions,deletions,changedFiles,state,mergedAt,createdAt,closedAt,headRefName,author,commits,files"])
        d = json.loads(out)
        d["gen_lines"] = sum(f["additions"] + f["deletions"] for f in d["files"] if GEN.search(f["path"]))
        d["files"] = len(d["files"])
        d["commits"] = len(d["commits"])
        d["author"] = d["author"]["login"]
        prs[k] = d
        json.dump(prs, open(PR_CACHE, "w"), indent=1)
    return prs[k]


LINEAR = {  # from `linear issue view <id> --json` (estimate is null on every issue; priority 0=none,1=urgent..4=low)
    "DNO-881": ("Surface billing information in admin dashboard", 0), "DNO-927": ("align PAYG invoices with billable OpenRouter key policy", 0),
    "DNO-939": ("Configure inference limits in billing admin", 0), "DNO-941": ("Track customer inference spend going forward", 3),
    "DNO-761": ("security: Excessive Session Timeout", 4), "DNO-396": ("clear current token and re-auth on auth failures", 2),
    "AGE-2563": ("investigate async hook support in Codex", 3), "DNO-675": ("malformed URNs in telemetry_logs from assistant triggers", 2),
    "DNO-117": ("MS Teams trigger", 3), "AGE-3104": ("speed up assistant CI build with Rust caching", 0),
    "DNO-883->DNO-884": ("send PAYG activation confirmation (DNO-883 canceled)", 0), "GRW-48": ("improve billing page", 4),
}
PRIO = {0: "none", 1: "urgent", 2: "high", 3: "medium", 4: "low"}


def fmt_k(n):
    return f"{n/1000:.0f}k" if n < 1e6 else f"{n/1e6:.2f}M"


def fmt_dur(sec):
    m = sec / 60
    return f"{m:.0f}m" if m < 120 else f"{m/60:.1f}h"


rows = []
for harness, issue, sids, prns, note in PR_MAP:
    ss = [by_id[s] for s in sids]
    r = {k: 0 for k in ROLL}
    r.update(peak_context=0, subagents=0, sub_inferred=0)
    for s in ss:
        rr = rollup(s["session_id"])
        for k in ROLL + ("subagents", "sub_inferred"):
            r[k] += rr[k]
        r["peak_context"] = max(r["peak_context"], rr["peak_context"])
    pi = [pr_info(n) for n in prns]
    add = sum(p["additions"] for p in pi)
    dele = sum(p["deletions"] for p in pi)
    files = sum(p["changedFiles"] for p in pi)
    gen = sum(p["gen_lines"] for p in pi)
    rows.append(dict(
        harness=harness, issue=issue, sids=sids, prs=prns, note=note, pr=pi,
        add=add, dele=dele, files=files, lines=add + dele, hand=add + dele - gen, gen=gen,
        merged=all(p["state"] == "MERGED" for p in pi),
        state="/".join(p["state"] for p in pi),
        model="+".join(sorted({s["model"] or "?" for s in ss})),
        wall=sum(s["duration_s"] for s in ss), active=sum(s["active_s"] for s in ss),
        turns=sum(s["assistant_turns"] for s in ss), user_msgs=sum(s["user_msgs"] for s in ss),
        start=min(s["start"] for s in ss)[:10], **r,
    ))
    rows[-1]["uncached"] = rows[-1]["input_fresh"] + rows[-1]["input_cache_write"]
rows.sort(key=lambda r: r["lines"])


def bucket(lines):
    return "<300" if lines < 300 else ("300-1000" if lines <= 1000 else ">1000")


print("# Ship-a-Linear-issue in speakeasy-api/gram: Kit vs Codex vs Claude, matched by PR size\n")
print(f"Data: `sessions.json` ({len(S)} sessions parsed from ~/.kit, ~/.codex, ~/.claude). Selection: top-level (non-subagent) sessions with cwd under a gram checkout/worktree whose first prompt asks to ship/implement a Linear issue or raise a PR, and which demonstrably created a PR in speakeasy-api/gram. PR stats via `gh pr view`; Linear via `linear issue view --json` (read-only). Generated by `gram_compare.py`.\n")

print("""## 0. Headline (read the caveats before quoting)

- 14 rows / 15 sessions / 16 PRs: Kit 5 (gpt-5.6-sol, Aug 18-24), Codex 2 (gpt-5.5, May+Jul), Claude 7 (fable-5 x6, opus-5 x1, Jul 29-Aug 27). Every PR merged.
- Matched on hand-written lines (3.b, 300-1000 bucket: Kit n=3, Codex n=2, Claude n=2) Kit used the least total input per line (median ~50k vs Claude ~100k vs Codex ~113k) and the least active time per line (0.13 vs 0.31 vs 0.31 min), with similar output per line (274 / 321 / 325). Claude had the lowest *uncached* input per line (1.8k vs Kit 3.7k vs Codex 5.5k) because almost all of its input is cache-read; Kit's fresh input is higher per line because the OpenAI-side cache misses more and because of compactions.
- Kit sessions issue the most API requests (median 497 vs 312 / 247) and compact the most (median 3; DNO-881 compacted 11 times over 17h wall). Claude never compacted in any row and runs at the highest peak context (355k median).
- Kit PR sizes are inflated by generated Goa/OpenAPI output: 3 of 5 Kit PRs are 72-90% generated lines, versus 1 of 7 Claude PRs (#5230, 61%). Total-line buckets (3.a) therefore flatter Kit; use 3.b.
- Two PRs needed a separate shepherd session to reach merge: Kit DNO-939 #5601 (Claude shepherd, 3 days later) and Claude GRW-48 #5827 (Claude shepherd next day). Codex/Claude sessions that shepherded their own PR carry that polling cost inside their row (Codex DNO-396 3.8h wall, Claude AGE-3104 11.8h wall).
""")
print("## 1. Selected sessions (n=%d rows, %d sessions)\n" % (len(rows), sum(len(r["sids"]) for r in rows)))
print("| # | harness | date | issue (Linear prio) | session(s) | PR(s) | how the PR was identified / note |\n|---|---|---|---|---|---|---|")
for i, r in enumerate(rows, 1):
    li = LINEAR.get(r["issue"])
    prio = f" ({PRIO[li[1]]})" if li else ""
    print(f"| {i} | {r['harness']} | {r['start']} | {r['issue']}{prio} | {', '.join(s[:20] for s in r['sids'])} | {', '.join('#%d' % n for n in r['prs'])} | {r['note']} |")
print()
print("Sessions considered but excluded from the like-for-like set:\n")
print("| harness | issue/topic | session | reason |\n|---|---|---|---|")
for h, i, sid, why in EXCLUDED:
    print(f"| {h} | {i} | {sid} | {why} |")
print()

print("## 2. Per-session table, sorted by PR size (additions+deletions)\n")
print("| harness | issue | PR | files / +/- | hand-written lines | merged | model | wall | active | turns (user msgs) | API req | total input | uncached input (fresh+cache-write) | cache read | output | peak ctx | compactions | subagents | tool calls |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
for r in rows:
    prs_s = ", ".join(f"#{p['number']}" for p in r["pr"])
    merged = "yes" if r["merged"] else r["state"]
    tc = f"{r['tool_calls']}" + (f" (+{r['inner_calls']} inner)" if r["harness"] == "kit" else "")
    sub = str(r["subagents"]) + ("*" if r["sub_inferred"] else "")
    print(f"| {r['harness']} | {r['issue']} | {prs_s} | {r['files']} / +{r['add']}/-{r['dele']} | {r['hand']} ({100*r['hand']/r['lines']:.0f}%) | {merged} | {r['model']} | {fmt_dur(r['wall'])} | {fmt_dur(r['active'])} | {r['turns']} ({r['user_msgs']}) | {r['api_requests']} | {fmt_k(r['input_total'])} | {fmt_k(r['uncached'])} | {fmt_k(r['input_cache_read'])} | {fmt_k(r['output_tokens'])} | {fmt_k(r['peak_context'])} | {r['compactions']} | {sub} | {tc} |")
print("\n`*` = at least one Kit subagent link was inferred (same daemon pid + time window) rather than recorded. Kit `tool calls` are `compose` scripts; `inner` = shell()/edit()/subagent()/... calls parsed from those scripts. Wall/active for the Codex AGE-2563 row is the sum of two sessions. `hand-written lines` excludes files matching generated-code paths (server/gen/**, *.sql.go, client/sdk/**, openapi3.yaml, migrations/*.sql, lockfiles) using per-file stats from `gh pr view --json files`.\n")

print("## 3. Per-line efficiency, bucketed by PR size (medians within bucket; n in parentheses)\n")


def med(xs):
    return st.median(xs) if xs else None


def f(v, kind):
    if v is None:
        return "-"
    if kind == "tok":
        return f"{v:,.0f}"
    if kind == "min":
        return f"{v:.2f}"
    if kind == "k":
        return fmt_k(v)
    if kind == "pct":
        return f"{100*v:.0f}%"
    return f"{v:.0f}"


def metrics(L):
    return [
        ("total input tok / line", lambda r: r["input_total"] / r[L], "tok"),
        ("uncached input tok / line", lambda r: r["uncached"] / r[L], "tok"),
        ("output tok / line", lambda r: r["output_tokens"] / r[L], "tok"),
        ("active min / line", lambda r: r["active"] / 60 / r[L], "min"),
        ("wall min / line", lambda r: r["wall"] / 60 / r[L], "min"),
        ("API requests", lambda r: r["api_requests"], "n"),
        ("API requests / line", lambda r: r["api_requests"] / r[L], "min"),
        ("peak context", lambda r: r["peak_context"], "k"),
        ("compactions", lambda r: r["compactions"], "n"),
        ("merged rate", lambda r: 1.0 if r["merged"] else 0.0, "pct"),
    ]


for L, label in (("lines", "TOTAL changed lines (additions+deletions, incl. generated)"), ("hand", "HAND-WRITTEN changed lines (generated files excluded)")):
  print(f"### 3.{'a' if L=='lines' else 'b'} Bucketed by {label}\n")
  for b in ("<300", "300-1000", ">1000", "all"):
    sub = [r for r in rows if b == "all" or bucket(r[L]) == b]
    print(f"#### {label.split(' ')[0].lower()} lines {b}\n")
    hs = ("kit", "codex", "claude")
    print("| metric | " + " | ".join(f"{h} (n={sum(1 for r in sub if r['harness']==h)})" for h in hs) + " |\n|---|---|---|---|")
    for name, fn, kind in metrics(L):
        vals = {h: [fn(r) for r in sub if r["harness"] == h] for h in hs}
        if name == "merged rate":
            print(f"| {name} | " + " | ".join(f(st.mean(vals[h]) if vals[h] else None, kind) for h in hs) + " |")
        else:
            print(f"| {name} | " + " | ".join(f(med(vals[h]), kind) for h in hs) + " |")
    print("| PRs in bucket (lines) | " + " | ".join(", ".join(f"#{r['prs'][0]} ({r[L]})" for r in sub if r["harness"] == h) or "-" for h in hs) + " |")
    print()

print("### Same metrics, per row (for eyeballing the spread)\n")
print("| harness | issue | total lines | hand lines | input/hand line | uncached/hand line | output/hand line | active min/hand line | req/100 hand lines |\n|---|---|---|---|---|---|---|---|---|")
for r in sorted(rows, key=lambda r: r["hand"]):
    print(f"| {r['harness']} | {r['issue']} | {r['lines']} | {r['hand']} | {r['input_total']/r['hand']:,.0f} | {r['uncached']/r['hand']:,.0f} | {r['output_tokens']/r['hand']:,.0f} | {r['active']/60/r['hand']:.2f} | {100*r['api_requests']/r['hand']:.1f} |")
print()

print("## 4. Outcome notes (not merged / closed / redone)\n")
for r in rows:
    for p in r["pr"]:
        if p["state"] != "MERGED":
            print(f"- {r['harness']} {r['issue']}: #{p['number']} is {p['state']}")
print("- All %d PRs in the like-for-like set are MERGED (%s)." % (sum(len(r["pr"]) for r in rows), ", ".join(f"#{p['number']}" for r in rows for p in r["pr"])))
print("- Follow-up sessions on the same issue/PR found by scanning every top-level first prompt (any harness):")
print("  - Kit DNO-939 (#5601, opened 2026-08-21): merged only on 2026-08-24 after a separate Claude session `shepherd this PR: 5601` (9a1fecf1, 39 req). Kit did not drive the PR to merge on its own.")
print("  - Claude GRW-48 (#5827, opened 2026-08-27 20:03): merged 2026-08-28 after a separate Claude session `shepherd #5827` (b5258fa4, 42 req, 17 turns, 3.5h wall).")
print("  - Claude (no issue) #5229/#5230: same-day Codex session `shepherd 5229` (019ffb6a, 86 req) drove the migration PR to merge.")
print("  - Kit DNO-941: a bot PR (#5589, speakeasyforgebot, +3857/-88) had already implemented the issue in the wrong dashboard; Daniel told Kit to ignore it; Kit's #5627 merged, #5589 closed. Not a redo of Kit's work.")
print("  - Claude DNO-883: session was a 'verify' request; DNO-883 was canceled in Linear and the session shipped sibling DNO-884 (#5445) instead. Counted as a ship, but the prompt was not a ship-issue prompt.")
print("  - Codex AGE-2563: implementation and PR creation were two Codex sessions ~70 min apart on the same branch; summed. The first session had 17 user messages (heavily steered).")
print("  - No session in the set was followed by a second implementation attempt of the same issue in another harness.")
print()

print("## 5. Caveats\n")
print("""- Models differ per row and per harness era: Kit rows are all gpt-5.6-sol (Aug 18-24); Codex rows are gpt-5.5 (May/Jul); Claude rows are claude-fable-5 except DNO-883/884 (claude-opus-5). Token counts are therefore not price-comparable and tokenizers differ (OpenAI vs Anthropic), so cross-harness token ratios are indicative only.
- Cache accounting differs. Claude reports input / cache_read / cache_creation separately (total = sum). Codex reports input_tokens with cached_input_tokens as a subset (fresh = input - cached; no cache-write figure). Kit reports input + cached + cache_write as separate counters, and Kit's `peak_context` comes from `usage.metadata.context_used` when present, else input tokens. Claude's raw `input_tokens` is ~0 in every row (everything is cache-read or cache-creation), so the comparable "uncached input" column is fresh + cache-write (Claude: 190k-350k per session; Kit/Codex: fresh only, cache-write counter is 0 on OpenAI models). Anthropic bills cache writes at 1.25x and cache reads at 0.1x; OpenAI bills cached input at 0.1-0.25x — so cost is not derivable from these counts without per-model pricing.
- Kit subagents are separate session files; parent links are recovered from transcript notifications and, when missing, inferred from daemon pid + time overlap (rows marked `*`). Kit `tool calls` count `compose` invocations; the real tool activity is the `inner` count parsed from compose scripts with a regex, which undercounts loops and dynamic calls.
- Codex `spawn_agent` children are linked via parent_thread_id. Claude subagents are the `/subagents/*.jsonl` files under the parent session. Claude `claude -p` invocations spawned by other harnesses (e.g. the Codex ship-epic) are NOT linked and appear as top-level Claude sessions; none of those are in this set.
- `active` = sum of gaps between consecutive transcript events capped at 5 min; it removes overnight idle but still includes CI-wait polling loops (which inflate wall/active and API requests for sessions that shepherd their own PR). Kit rows include the shepherd loop inside the session; Kit DNO-939 and Claude GRW-48 had their shepherding done in a separate session that is NOT included in their row.
- Assistant turns: Kit counts completed assistant items; Codex counts task_complete events; Claude counts end_turn stop reasons (falls back to user message count). `user msgs` is shown alongside because a high count means Daniel steered the session heavily (Codex AGE-2563: 17; Kit DNO-881: 15 turns).
- PR size by total lines includes generated code (Kit DNO-881 #5460: 16,046 of 17,837 lines are server/gen/**; Kit #5601 72% generated, #5627 84%; Claude #5230 61%). Section 3.a follows the requested total-line buckets; 3.b re-buckets by hand-written lines using a path regex for generated files (server/gen/**, *.sql.go, client/sdk/**, openapi3.yaml, migrations/*.sql, lockfiles) — the regex is heuristic (e.g. hand-edited migrations count as generated).
- Linear `estimate` is null on every issue in the set (the teams do not use estimates); only priority is available and is shown in section 1.
- n is small (5 Kit, 2 Codex, 7 Claude); by total lines Codex only appears in the 300-1000 bucket and Kit only once below 1000, so bucket medians are often single observations. The hand-written-lines bucketing (3.b) spreads rows more evenly and is the better like-for-like view. Treat every cross-harness number as anecdotal.
- Two Claude rows have no Linear issue at all (prompt was an image + 'raise a PR'); they are included because the task shape (implement from a spec, open PR, iterate on CI/review) matches, but the spec came from screenshots rather than a ticket.
""")
