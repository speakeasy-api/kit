#!/usr/bin/env python3
"""Trim the shutdown tail off a recorded asciicast and hold the final frame.

usage: trim-cast.py <file.cast>...   (rewrites in place)
"""
import json
import sys

HOLD_SECONDS = 6.0
LEAVE_ALT_SCREEN = "\x1b[?1049l"
PROMPT = "\x1b[34m> \x1b[39m"
CLEAR = "\x1b[2J"
HEAD_WINDOW_SECONDS = 6.0


def trim(path: str) -> None:
    with open(path) as f:
        lines = [l for l in f.read().split("\n") if l.strip()]
    header = json.loads(lines[0])
    events = [json.loads(l) for l in lines[1:]]

    if "zsh" in header.get("command", ""):
        elapsed = 0.0
        for i, ev in enumerate(events):
            elapsed += ev[0]
            if elapsed > HEAD_WINDOW_SECONDS:
                break
            if ev[1] == "o" and CLEAR in ev[2]:
                events = events[i:]
                events[0] = [0.0, events[0][1], events[0][2][events[0][2].index(CLEAR):]]
                break

    cut = None
    for i, ev in enumerate(events):
        if ev[1] == "o" and LEAVE_ALT_SCREEN in ev[2]:
            cut = i
            break
    if cut is None:
        for i in range(len(events) - 1, -1, -1):
            if events[i][1] == "o" and PROMPT in events[i][2]:
                cut = i + 1
                break
    if cut is not None:
        while cut > 0 and events[cut - 1][1] == "o" and events[cut - 1][2].startswith("\x1b[?") and "l" in events[cut - 1][2][-3:]:
            cut -= 1
        events = events[:cut]
    events = [ev for ev in events if ev[1] != "x"]
    if not (events and events[-1][2] == "" and events[-1][0] == HOLD_SECONDS):
        events.append([HOLD_SECONDS, "o", ""])

    header.pop("idle_time_limit", None)
    with open(path, "w") as f:
        f.write(json.dumps(header, separators=(",", ":")) + "\n")
        for ev in events:
            f.write(json.dumps(ev, separators=(",", ":"), ensure_ascii=False) + "\n")
    print(f"{path}: {len(lines) - 1} -> {len(events)} events")


for p in sys.argv[1:]:
    trim(p)
