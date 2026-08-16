#!/usr/bin/env python3
import json
import sys
import threading
import time

write_lock = threading.Lock()
next_session = 1
supports_fork = "--no-fork" not in sys.argv


def send(message):
    with write_lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def respond(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def prompt(request):
    params = request["params"]
    session_id = params["sessionId"]
    text = params["prompt"][0]["text"]
    time.sleep(0.40)
    if "MOCK_STRUCTURED_OUTPUT" in text:
        text = json.dumps({"approved": True, "reason": "mock approved"})
    if "MOCK_RICH_OUTPUT" in text:
        updates = [
            {
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "internal"},
            },
            {
                "sessionUpdate": "agent_message_chunk",
                "content": {
                    "type": "image",
                    "data": "aGVsbG8=",
                    "mimeType": "image/png",
                },
            },
            {
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "Inspect files",
            },
            {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
            },
            {
                "sessionUpdate": "plan",
                "entries": [
                    {"content": "Inspect", "priority": "high", "status": "completed"}
                ],
            },
            {"sessionUpdate": "usage_update", "used": 10, "size": 100},
        ]
        for update in updates:
            send({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": session_id, "update": update},
            })
        text = "rich done"
    send({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            },
        },
    })
    respond(request["id"], {"stopReason": "end_turn"})


for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        respond(request["id"], {
            "protocolVersion": 1,
            "agentCapabilities": {
                "sessionCapabilities": {"fork": {}} if supports_fork else {}
            },
        })
    elif method == "session/new":
        respond(request["id"], {"sessionId": "base"})
    elif method == "session/fork":
        if not supports_fork:
            send({
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": {"code": -32601, "message": "Method not found"},
            })
            continue
        session_id = f"branch-{next_session}"
        next_session += 1
        respond(request["id"], {"sessionId": session_id})
    elif method == "session/prompt":
        threading.Thread(target=prompt, args=(request,), daemon=True).start()
    elif method == "session/cancel":
        pass
    elif method == "session/close":
        respond(request["id"], {})
