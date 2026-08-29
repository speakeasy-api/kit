#!/usr/bin/env python3
"""Parse Kit, Codex CLI and Claude Code transcripts into per-session summaries.

Output: sessions.json (list of dicts) in the same directory as this script.
"""
import glob
import json
import os
import re
import sys
from datetime import datetime, timezone

HOME = os.path.expanduser("~")
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "sessions.json")

CHILD_TOOLS = ("shell", "edit", "subagent", "prompt", "fork", "a2a", "tool_search", "auth", "tool", "read")
CHILD_RE = re.compile(r"\b(" + "|".join(CHILD_TOOLS) + r")\s*\(")


def iso(ms):
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).isoformat()


def parse_ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


def active_seconds(times, cap=300.0):
    ts = sorted(times)
    return sum(min(b - a, cap) for a, b in zip(ts, ts[1:]))


def base_summary(harness, sid):
    return dict(
        harness=harness, session_id=sid, project=None, cwd=None, start=None, end=None,
        duration_s=None, assistant_turns=0, api_requests=0, tool_calls=0, inner_calls=0,
        input_fresh=0, input_cache_read=0, input_cache_write=0, input_total=0, output_tokens=0,
        reasoning_tokens=0, peak_context=0, compactions=0, model=None, models={}, first_prompt=None,
        title=None, user_msgs=0, depth=0, parent=None, children=[], max_fanout=0, kit_version=None,
        file=None, is_subagent=False, active_s=0.0,
    )


# ---------------------------------------------------------------- Kit
def parse_kit(path):
    sid = os.path.basename(path)[:-6]
    s = base_summary("kit", sid)
    s["file"] = path
    seen_resp = set()
    completed_ts = set()
    times = []
    tool_names = {}
    for line in open(path, encoding="utf-8", errors="replace"):
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "replacement" in d:
            s["compactions"] += 1
            continue
        it = d.get("item")
        if not it:
            continue
        kind = it.get("kind")
        parts = it.get("parts") or []
        ca = it.get("created_at")
        if ca:
            times.append(ca)
        if kind == "System":
            for p in parts:
                t = (p.get("Text") or {}).get("text", "")
                m = re.search(r"rooted at (\S+?)\.", t)
                if m and not s["cwd"]:
                    s["cwd"] = m.group(1)
                m = re.search(r"subagent depth: (\d)/", t)
                if m:
                    s["depth"] = int(m.group(1))
                m = re.search(r"Kit version (\S+?) ", t)
                if m:
                    s["kit_version"] = m.group(1)
        elif kind == "User":
            s["user_msgs"] += 1
            if s["first_prompt"] is None:
                for p in parts:
                    t = (p.get("Text") or {}).get("text")
                    if t:
                        s["first_prompt"] = t[:400]
                        break
        elif kind == "Assistant":
            u = it.get("usage")
            resp_id = None
            for p in parts:
                for k, v in p.items():
                    md = (v or {}).get("metadata") or {}
                    for mk, mv in md.items():
                        if isinstance(mv, dict):
                            resp_id = resp_id or mv.get("response_id")
                            mdl = mv.get("model")
                            if mdl:
                                s["models"][mdl] = s["models"].get(mdl, 0) + 1
                    if k == "ToolCall":
                        s["tool_calls"] += 1
                        tool_names[v.get("name")] = tool_names.get(v.get("name"), 0) + 1
                        script = (v.get("input") or {}).get("script") or ""
                        inner = CHILD_RE.findall(script)
                        s["inner_calls"] += len(inner)
                        fan = sum(1 for x in inner if x == "subagent")
                        s["max_fanout"] = max(s["max_fanout"], fan)
                    if k == "Text" and s["first_prompt"] is not None:
                        pass
            if it.get("finish_reason") in ("Completed", "Stop", "EndTurn") and ca not in completed_ts:
                completed_ts.add(ca)
                s["assistant_turns"] += 1
            if u and u.get("tokens"):
                key = resp_id or (ca, json.dumps(u["tokens"], sort_keys=True))
                if key in seen_resp:
                    continue
                seen_resp.add(key)
                t = u["tokens"]
                inp = t.get("input_tokens") or 0
                cached = t.get("cached_input_tokens") or 0
                cw = t.get("cache_write_input_tokens") or 0
                s["api_requests"] += 1
                s["input_fresh"] += max(inp - cached, 0)
                s["input_cache_read"] += cached
                s["input_cache_write"] += cw
                s["input_total"] += inp + cw
                s["output_tokens"] += t.get("output_tokens") or 0
                s["reasoning_tokens"] += t.get("reasoning_tokens") or 0
                ctx = ((u.get("metadata") or {}).get("context_used")) or inp
                s["peak_context"] = max(s["peak_context"], ctx, inp)
        elif kind in ("Tool", "Notification"):
            for m in re.finditer(r'(?:"generation":\s*\d+,\s*"id":\s*"(s-\d+-\d+-\d+)"|\{"id":\s*"(s-\d+-\d+-\d+)",\s*"status")', line):
                m = type("M", (), {"group": staticmethod(lambda i, g=m.group(1) or m.group(2): g)})
                if m.group(1) != sid:
                    s["children"].append(m.group(1))
    s["children"] = sorted(set(s["children"]))
    if times:
        s["start"], s["end"] = iso(min(times)), iso(max(times))
        s["duration_s"] = (max(times) - min(times)) / 1000
        s["active_s"] = active_seconds([t / 1000 for t in times])
    if s["models"]:
        s["model"] = max(s["models"], key=s["models"].get)
    s["is_subagent"] = s["depth"] > 0
    s["project"] = s["cwd"]
    s["tool_names"] = tool_names
    return s


# ---------------------------------------------------------------- Codex
def parse_codex(path):
    s = base_summary("codex", os.path.basename(path)[:-6])
    s["file"] = path
    times = []
    last_total = None
    tool_names = {}
    prev_cum = None
    ri_user = ri_assistant = 0
    alt_compactions = 0
    for line in open(path, encoding="utf-8", errors="replace"):
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        ts = d.get("timestamp")
        if ts:
            try:
                times.append(parse_ts(ts))
            except ValueError:
                pass
        t = d.get("type")
        p = d.get("payload") or {}
        if t == "session_meta":
            s["cwd"] = s["cwd"] or p.get("cwd")
            s["session_id"] = p.get("id") or s["session_id"]
            s["originator"] = p.get("originator")
            s["parent"] = p.get("parent_thread_id")
            if p.get("source") == "subagent" or p.get("thread_source") not in (None, "user"):
                s["is_subagent"] = True
        elif t == "turn_context":
            mdl = p.get("model")
            if mdl:
                s["models"][mdl] = s["models"].get(mdl, 0) + 1
            s["cwd"] = s["cwd"] or p.get("cwd")
        elif t == "event_msg":
            pt = p.get("type")
            if pt == "token_count":
                info = p.get("info") or {}
                last = info.get("last_token_usage") or {}
                tot = info.get("total_token_usage") or {}
                if last and last.get("input_tokens") is not None:
                    cum = tot.get("total_tokens")
                    if cum is not None and cum == prev_cum:
                        continue
                    prev_cum = cum
                    inp = last.get("input_tokens") or 0
                    cached = last.get("cached_input_tokens") or 0
                    s["api_requests"] += 1
                    s["input_fresh"] += max(inp - cached, 0)
                    s["input_cache_read"] += cached
                    s["input_total"] += inp
                    s["output_tokens"] += last.get("output_tokens") or 0
                    s["reasoning_tokens"] += last.get("reasoning_output_tokens") or 0
                    s["peak_context"] = max(s["peak_context"], inp)
                    last_total = tot
            elif pt == "user_message":
                s["user_msgs"] += 1
                msg = p.get("message") or ""
                if s["first_prompt"] is None and not msg.startswith(("# AGENTS.md", "<environment_context", "## Code review guidelines", "<permissions")):
                    s["first_prompt"] = msg[:400]
            elif pt == "task_complete":
                s["assistant_turns"] += 1
            elif pt in ("context_compacted", "compacted"):
                alt_compactions += 1
        elif t == "compacted":
            s["compactions"] += 1
        elif t == "response_item":
            pt = p.get("type")
            if pt in ("function_call", "custom_tool_call"):
                s["tool_calls"] += 1
                n = p.get("name")
                tool_names[n] = tool_names.get(n, 0) + 1
                if n == "spawn_agent":
                    s["max_fanout"] = max(s["max_fanout"], 1)
            elif pt == "compaction":
                alt_compactions += 1
            elif pt == "message" and p.get("role") == "user":
                txt = " ".join(c.get("text", "") for c in (p.get("content") or []) if isinstance(c, dict))
                if not txt.startswith(("# AGENTS.md", "<environment_context", "## Code review guidelines", "<permissions", "<user_shell", "<turn_aborted")):
                    ri_user += 1
                    if s["first_prompt"] is None:
                        s["first_prompt"] = txt[:400]
            elif pt == "message" and p.get("role") == "assistant":
                ri_assistant += 1
    s["user_msgs"] = max(s["user_msgs"], ri_user)
    if s["compactions"] == 0 and alt_compactions:
        s["compactions"] = alt_compactions
    if times:
        s["active_s"] = active_seconds(times)
    if s["assistant_turns"] == 0:
        s["assistant_turns"] = min(ri_assistant, s["user_msgs"]) or ri_assistant
    if times:
        s["start"], s["end"] = datetime.fromtimestamp(min(times), tz=timezone.utc).isoformat(), datetime.fromtimestamp(max(times), tz=timezone.utc).isoformat()
        s["duration_s"] = max(times) - min(times)
    if s["models"]:
        s["model"] = max(s["models"], key=s["models"].get)
    s["project"] = s["cwd"]
    s["tool_names"] = tool_names
    s["codex_total_usage"] = last_total
    return s


# ---------------------------------------------------------------- Claude Code
def parse_claude(path):
    s = base_summary("claude", os.path.basename(path)[:-6])
    s["file"] = path
    s["is_subagent"] = "/subagents/" in path
    if s["is_subagent"]:
        s["parent"] = path.split("/subagents/")[0].split("/")[-1]
    times = []
    seen_req = set()
    tool_names = {}
    compact_summaries = 0
    for line in open(path, encoding="utf-8", errors="replace"):
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = d.get("type")
        ts = d.get("timestamp")
        if ts and t in ("user", "assistant", "system"):
            try:
                times.append(parse_ts(ts))
            except ValueError:
                pass
        if d.get("cwd") and not s["cwd"]:
            s["cwd"] = d["cwd"]
        if t == "ai-title" and not s["title"]:
            s["title"] = d.get("aiTitle")
        elif t == "system" and d.get("subtype") == "compact_boundary":
            s["compactions"] += 1
        elif t == "user":
            if d.get("isCompactSummary"):
                compact_summaries += 1
            msg = d.get("message") or {}
            c = msg.get("content")
            if isinstance(c, str):
                text = c
            elif isinstance(c, list):
                text = " ".join(x.get("text", "") for x in c if isinstance(x, dict) and x.get("type") == "text")
            else:
                text = ""
            is_tool_result = isinstance(c, list) and any(isinstance(x, dict) and x.get("type") == "tool_result" for x in c)
            if not is_tool_result and not d.get("isMeta") and text and not text.startswith(("<local-command", "<command-name>", "<system-reminder")):
                s["user_msgs"] += 1
                if s["first_prompt"] is None:
                    s["first_prompt"] = text[:400]
        elif t == "assistant":
            msg = d.get("message") or {}
            mdl = msg.get("model")
            if mdl and mdl != "<synthetic>":
                s["models"][mdl] = s["models"].get(mdl, 0) + 1
            content = msg.get("content") or []
            for blk in content:
                if isinstance(blk, dict) and blk.get("type") == "tool_use":
                    s["tool_calls"] += 1
                    n = blk.get("name")
                    tool_names[n] = tool_names.get(n, 0) + 1
                    if n in ("Agent", "Task"):
                        pass
            if msg.get("stop_reason") == "end_turn" or (content and isinstance(content[-1], dict) and content[-1].get("type") == "text" and msg.get("stop_reason") != "tool_use"):
                pass
            u = msg.get("usage")
            rid = d.get("requestId") or msg.get("id")
            if u and rid and rid not in seen_req:
                seen_req.add(rid)
                inp = u.get("input_tokens") or 0
                cr = u.get("cache_read_input_tokens") or 0
                cw = u.get("cache_creation_input_tokens") or 0
                s["api_requests"] += 1
                s["input_fresh"] += inp
                s["input_cache_read"] += cr
                s["input_cache_write"] += cw
                s["input_total"] += inp + cr + cw
                s["output_tokens"] += u.get("output_tokens") or 0
                s["reasoning_tokens"] += ((u.get("output_tokens_details") or {}).get("thinking_tokens") or 0)
                s["peak_context"] = max(s["peak_context"], inp + cr + cw)
            if msg.get("stop_reason") in ("end_turn", "stop_sequence"):
                s["assistant_turns"] += 1
    if s["compactions"] == 0:
        s["compactions"] = compact_summaries
    # Claude Code often omits stop_reason; fall back to counting user prompts as turns.
    if s["assistant_turns"] == 0:
        s["assistant_turns"] = s["user_msgs"]
    # subagent fan-out: count parallel Agent tool_use in one assistant message
    for line in open(path, encoding="utf-8", errors="replace"):
        if '"Agent"' not in line and '"Task"' not in line:
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if d.get("type") != "assistant":
            continue
        content = (d.get("message") or {}).get("content") or []
        fan = sum(1 for b in content if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("name") in ("Agent", "Task"))
        s["max_fanout"] = max(s["max_fanout"], fan)
    if times:
        s["start"], s["end"] = datetime.fromtimestamp(min(times), tz=timezone.utc).isoformat(), datetime.fromtimestamp(max(times), tz=timezone.utc).isoformat()
        s["duration_s"] = max(times) - min(times)
        s["active_s"] = active_seconds(times)
    if s["models"]:
        s["model"] = max(s["models"], key=s["models"].get)
    s["project"] = s["cwd"]
    s["tool_names"] = tool_names
    return s


def main():
    out = []
    kit_files = sorted(glob.glob(os.path.join(HOME, ".kit/sessions/*.jsonl")))
    codex_files = sorted(glob.glob(os.path.join(HOME, ".codex/sessions/*/*/*/*.jsonl")))
    claude_files = sorted(glob.glob(os.path.join(HOME, ".claude/projects/**/*.jsonl"), recursive=True))
    print(f"kit={len(kit_files)} codex={len(codex_files)} claude={len(claude_files)}", file=sys.stderr)
    for fn, files in (("kit", kit_files), ("codex", codex_files), ("claude", claude_files)):
        parser = {"kit": parse_kit, "codex": parse_codex, "claude": parse_claude}[fn]
        for i, f in enumerate(files):
            try:
                out.append(parser(f))
            except Exception as e:  # noqa
                print(f"ERR {f}: {e}", file=sys.stderr)
            if i % 200 == 0:
                print(f"{fn} {i}/{len(files)}", file=sys.stderr)
    # link kit parents
    by_id = {s["session_id"]: s for s in out if s["harness"] == "kit"}
    for s in out:
        if s["harness"] == "kit":
            for c in s["children"]:
                if c in by_id:
                    by_id[c]["parent"] = s["session_id"]
    json.dump(out, open(OUT, "w"), indent=0)
    print(f"wrote {OUT} ({len(out)} sessions)", file=sys.stderr)


if __name__ == "__main__":
    main()
