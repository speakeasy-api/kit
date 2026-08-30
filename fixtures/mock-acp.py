#!/usr/bin/env python3
import json
import os
import sys
import threading
import time

write_lock = threading.Lock()
log_lock = threading.Lock()
state_lock = threading.Lock()
next_session = 1
supports_fork = "--no-fork" not in sys.argv
supports_models = "--models" in sys.argv
fail_first_close = "--fail-first-close" in sys.argv
failed_close = False
selected_models = {}


def option(name):
    prefix = name + "="
    return next((arg[len(prefix):] for arg in sys.argv if arg.startswith(prefix)), None)


request_log = option("--request-log")
new_release = option("--new-release")
fork_release = option("--fork-release")
prompt_release = option("--prompt-release")
prompt_release_text = option("--prompt-release-text")
close_release = option("--close-release")
close_release_session = option("--close-release-session")
fail_close_session = option("--fail-close-session")
model_ids = ["mock/default", "mock/requested"]

if "--fail-start" in sys.argv:
    sys.exit(2)


def send(message):
    with write_lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def respond(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def log_request(request):
    if request_log is None:
        return
    params = request.get("params", {})
    entry = {"method": request.get("method")}
    if "sessionId" in params:
        entry["sessionId"] = params["sessionId"]
    if request.get("method") == "session/prompt":
        entry["text"] = params["prompt"][0]["text"]
    with log_lock:
        with open(request_log, "a", encoding="utf-8") as log:
            log.write(json.dumps(entry, separators=(",", ":")) + "\n")
            log.flush()


def fork(request):
    global next_session
    if "--fail-fork" in sys.argv:
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32000, "message": "fork failed"},
        })
        return
    if not supports_fork:
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32601, "message": "Method not found"},
        })
        return
    source_id = request["params"]["sessionId"]
    with state_lock:
        session_id = f"branch-{next_session}"
        next_session += 1
        selected_models[session_id] = selected_models.get(source_id, model_ids[0])
    while fork_release is not None and not os.path.exists(fork_release):
        time.sleep(0.01)
    respond(request["id"], {"sessionId": session_id})


def prompt(request):
    params = request["params"]
    session_id = params["sessionId"]
    text = params["prompt"][0]["text"]
    should_gate = prompt_release is not None and (
        prompt_release_text is None or prompt_release_text == text
    )
    while should_gate and not os.path.exists(prompt_release):
        time.sleep(0.01)
    if prompt_release is None:
        time.sleep(0.40)
    if "MOCK_SELECTED_MODEL" in text:
        text = selected_models.get(session_id, model_ids[0])
    if "MOCK_STRUCTURED_OUTPUT" in text:
        text = json.dumps({"approved": True, "reason": "mock approved"})
    if "MOCK_REFUSAL" in text:
        respond(request["id"], {"stopReason": "refusal"})
        return
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
    if "--exit-after-prompt" in sys.argv:
        time.sleep(0.05)
        os._exit(0)


def close(request):
    global failed_close
    if "--slow-close" in sys.argv:
        time.sleep(0.40)
    session_id = request["params"]["sessionId"]
    should_gate = close_release is not None and (
        close_release_session is None or close_release_session == session_id
    )
    while should_gate and not os.path.exists(close_release):
        time.sleep(0.01)
    with state_lock:
        should_fail = (fail_close_session == session_id and not failed_close) or (
            fail_first_close and not failed_close
        )
        if should_fail:
            failed_close = True
    if should_fail:
        send({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32000, "message": "close failed"},
        })
    else:
        respond(request["id"], {})


for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    log_request(request)
    if method == "initialize":
        respond(request["id"], {
            "protocolVersion": 1,
            "agentCapabilities": {
                "sessionCapabilities": (
                    {"fork": {}, "close": {}} if supports_fork else {"close": {}}
                )
            },
        })
    elif method == "session/new":
        while new_release is not None and not os.path.exists(new_release):
            time.sleep(0.01)
        selected_models["base"] = model_ids[0]
        result = {"sessionId": "base"}
        if supports_models:
            result["configOptions"] = [{
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": model_ids[0],
                "options": [
                    {"value": value, "name": value} for value in model_ids
                ],
            }]
        respond(request["id"], result)
    elif method == "session/fork":
        threading.Thread(target=fork, args=(request,), daemon=True).start()
    elif method == "session/prompt":
        threading.Thread(target=prompt, args=(request,), daemon=True).start()
    elif method == "session/set_config_option":
        params = request["params"]
        value = params["value"]
        if params["configId"] != "model" or value not in model_ids:
            send({
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": {"code": -32602, "message": "unknown model"},
            })
        else:
            selected_models[params["sessionId"]] = value
            respond(request["id"], {"configOptions": []})
    elif method == "session/cancel":
        pass
    elif method == "session/close":
        threading.Thread(target=close, args=(request,), daemon=True).start()
