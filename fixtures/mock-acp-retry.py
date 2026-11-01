#!/usr/bin/env python3
"""Synthetic ACP notices exercise transport, not provider accounting."""
import json
import pathlib
import sys
import time

version = int(sys.argv[1])
release = pathlib.Path(sys.argv[2])
terminal = sys.argv[3] if len(sys.argv) > 3 else "succeeded"


def send(value):
    print(json.dumps(value), flush=True)


def update(value):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": "retry-child", "update": value}})


def notice(status):
    update({"sessionUpdate": "notice", "severity": "info",
            "title": "Provider retry " + status,
            "_meta": {"kit/providerRetry": {
                "status": status, "route": "open_ai_responses", "attempts": 2,
                "category": "transport" if status == "scheduled" else None,
                "next_delay_ms": 25 if status == "scheduled" else 0,
                "completed_backoff_ms": 0 if status == "scheduled" else 25}}})


for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    result = {}
    if method == "initialize":
        notices_enabled = (version == 2 or isinstance(
            request["params"].get("clientCapabilities", {}).get("session", {}).get("notices"), dict))
        result = ({"protocolVersion": 1, "agentCapabilities": {}}
                  if version == 1 else
                  {"protocolVersion": 2, "info": {"name": "retry-fixture", "version": "1"},
                   "capabilities": {"session": {}}})
    elif method == "session/new":
        result = {"sessionId": "retry-child"}
    elif method == "session/prompt":
        if version == 2:
            send({"jsonrpc": "2.0", "id": request["id"], "result": {"messageId": "accepted"}})
            update({"sessionUpdate": "state_update", "state": "running"})
        if notices_enabled:
            update({"sessionUpdate": "notice", "severity": "info", "title": "Ordinary notice"})
        notice("scheduled")
        deadline = time.monotonic() + 20
        while not release.exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("parent did not observe live retry progress")
            time.sleep(0.01)
        notice(terminal)
        answer = {"sessionUpdate": "agent_message_chunk",
                  "content": {"type": "text", "text": "CHILD_DONE"}}
        if version == 2:
            answer["messageId"] = "answer"
        update(answer)
        if version == 2:
            update({"sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn"})
            continue
        result = {"stopReason": "end_turn"}
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})
