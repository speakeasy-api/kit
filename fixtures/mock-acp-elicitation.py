#!/usr/bin/env python3
"""Sequential ACP peer that requires a proper elicitation cancellation response."""
import json
import sys


def send(message):
    print(json.dumps({"jsonrpc": "2.0", **message}), flush=True)


def respond(request_id, result):
    send({"id": request_id, "result": result})


while line := sys.stdin.readline():
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        respond(request["id"], {
            "protocolVersion": 1,
            "agentCapabilities": {"sessionCapabilities": {"close": {}}},
        })
    elif method == "session/new":
        respond(request["id"], {"sessionId": "elicitation-session"})
    elif method == "session/prompt":
        session_id = request["params"]["sessionId"]
        mode = request["params"]["prompt"][0]["text"]
        params = {"mode": mode, "sessionId": session_id, "message": "Input needed"}
        if mode == "form":
            params["requestedSchema"] = {"type": "object", "properties": {}}
        else:
            params.update(url="https://example.invalid/elicitation", elicitationId="elicit-1")
        send({"id": "child-elicitation", "method": "elicitation/create", "params": params})
        response = json.loads(sys.stdin.readline())
        assert response["id"] == "child-elicitation", response
        assert "error" not in response, response
        assert response["result"] == {"action": "cancel"}, response
        send({"method": "session/update", "params": {
            "sessionId": session_id,
            "update": {"sessionUpdate": "agent_message_chunk", "content": {
                "type": "text", "text": "elicitation cancelled",
            }},
        }})
        respond(request["id"], {"stopReason": "end_turn"})
    elif method == "session/close":
        respond(request["id"], {})
