#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import threading
import time

write_lock = threading.Lock()
log_lock = threading.Lock()
state_lock = threading.Lock()
next_session = 1
next_injection = 1
supports_fork = "--no-fork" not in sys.argv
supports_steer = "--steer" in sys.argv
supports_models = "--models" in sys.argv
fail_first_close = "--fail-first-close" in sys.argv
failed_close = False
selected_models = {}


def option(name):
    prefix = name + "="
    return next((arg[len(prefix):] for arg in sys.argv if arg.startswith(prefix)), None)


def argument_value(name):
    try:
        return sys.argv[sys.argv.index(name) + 1]
    except (ValueError, IndexError):
        return None


request_log = option("--request-log")
new_release = option("--new-release")
fork_release = option("--fork-release")
prompt_release = option("--prompt-release")
prompt_release_text = option("--prompt-release-text")
inject_release = option("--inject-release")
close_release = option("--close-release")
close_release_session = option("--close-release-session")
fail_close_session = option("--fail-close-session")
prompt_capability_names = set((option("--prompt-capabilities") or "image,audio").split(","))
model_ids = ["mock/default", "mock/requested"]

if "--fail-start" in sys.argv:
    sys.exit(2)

stale_pid_file = os.environ.get("MOCK_STALE_LOCK_PID_FILE")
if stale_pid_file:
    previous = []
    if os.path.exists(stale_pid_file):
        with open(stale_pid_file, encoding="utf-8") as log:
            previous = [line.strip() for line in log if line.strip()]
    effective_force = "--force" in sys.argv
    if effective_force and previous:
        try:
            os.kill(int(previous[-1].split(":", 1)[0]), 0)
        except ProcessLookupError:
            pass
        else:
            sys.exit(91)
    with open(stale_pid_file, "a", encoding="utf-8") as log:
        log.write(f"{os.getpid()}:{'force' if effective_force else 'normal'}\n")

if os.environ.get("MOCK_CHILD_PID_FILE"):
    child = subprocess.Popen([sys.executable, "-c", "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"] )
    with open(os.environ["MOCK_CHILD_PID_FILE"], "w") as file:
        file.write(str(child.pid))


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
    if request.get("method") in ("session/new", "session/fork"):
        entry["cwd"] = params["cwd"]
    if request.get("method") == "session/prompt":
        entry["text"] = params["prompt"][0]["text"]
    if request.get("method") == "session/inject":
        entry["text"] = params["content"][0]["text"]
        entry["mode"] = params["mode"]
    if request.get("method") is None and "result" in request:
        entry["id"] = request.get("id")
        entry["result"] = request["result"]
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
    text = next((block.get("text", "") for block in params["prompt"] if block.get("type") == "text"), "")
    if "MOCK_REJECT" in text:
        send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "prompt rejected"}})
        return
    respond(request["id"], {})
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "state_update", "state": "running"}}})
    if "MOCK_PERMISSION" in text:
        send({
            "jsonrpc": "2.0", "id": "permission-1", "method": "session/request_permission",
            "params": {"sessionId": session_id, "title": "Approve?", "options": []},
        })
    if "MOCK_HANG" in text:
        return
    should_gate = prompt_release is not None and (
        prompt_release_text is None or prompt_release_text == text
    )
    while should_gate and not os.path.exists(prompt_release):
        time.sleep(0.01)
    if prompt_release is None:
        time.sleep(0.40)
    if "MOCK_CWD" in text:
        text = os.getcwd()
    if "MOCK_SELECTED_MODEL" in text:
        text = selected_models.get(session_id, model_ids[0])
    if "MOCK_STRUCTURED_OUTPUT" in text:
        text = json.dumps({"approved": True, "reason": "mock approved"})
    if "MOCK_REFUSAL" in text:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "state_update", "state": "idle", "stopReason": "refusal"}}})
        return
    if "MOCK_MEDIA" in text:
        text = ",".join(block.get("type", "unknown") for block in params["prompt"])
    if not text and any(block.get("type") in ("image", "audio") for block in params["prompt"]):
        send({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": {
                "sessionUpdate": "user_message", "messageId": "attachment-user",
                "content": params["prompt"],
            }},
        })
    if "MOCK_ECHO" in text:
        send({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "user-1",
                "content": {"type": "text", "text": text},
            }},
        })
    if "MOCK_EMPTY_REPLACEMENT" in text:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "user_message", "messageId": "empty-user", "content": []}}})
    if "MOCK_NULL_REPLACEMENT" in text:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "user_message", "messageId": "null-user", "content": None}}})
    if "MOCK_OMITTED_REPLACEMENT" in text:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "user_message", "messageId": "omitted-user"}}})
    if "MOCK_INTERRUPTION" in text:
        for update in [
            {"sessionUpdate": "agent_message_chunk", "messageId": "agent-1", "content": {"type": "text", "text": "stale response"}},
            {"sessionUpdate": "notice", "severity": "warning", "title": "Response interrupted; replacement follows"},
            {"sessionUpdate": "agent_message", "messageId": "agent-1", "content": [{"type": "text", "text": "fresh response"}]},
        ]:
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}})
        text = "replacement done"
    if "MOCK_UNKNOWN" in text:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "vendor_future_update", "opaque": {"value": 1}}}})
    if "MOCK_SETTLEMENT" in text:
        for active in [True, True, False, False]:
            send({"jsonrpc": "2.0", "method": "kit/turn/state", "params": {"turn_id": 41, "active": active}})
    if "MOCK_BACKGROUND" in text:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {
            "sessionUpdate": "tool_call_update", "toolCallId": "background-1", "title": "Background task", "rawInput": {"background": True},
        }}})
    if "MOCK_BACKGROUND_RECOMPUTE" in text:
        for update in [
            {"sessionUpdate": "tool_call_update", "toolCallId": "late-background", "title": "Late background"},
            {"sessionUpdate": "tool_call_update", "toolCallId": "late-background", "rawInput": {"background": True}},
            {"sessionUpdate": "tool_call_update", "toolCallId": "cleared-background", "title": "Cleared background", "rawInput": {"background": True}},
            {"sessionUpdate": "tool_call_update", "toolCallId": "cleared-background", "rawInput": None},
        ]:
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}})
    if "MOCK_RICH_OUTPUT" in text:
        updates = [
            {
                "sessionUpdate": "agent_thought_chunk",
                "messageId": "thought-1",
                "content": {"type": "text", "text": "internal"},
            },
            {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "agent-1",
                "content": {
                    "type": "image",
                    "data": "aGVsbG8=",
                    "mimeType": "image/png",
                },
            },
            {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "title": "Inspect files",
            },
            {"sessionUpdate": "tool_call_update", "toolCallId": "call-1"},
            {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
            },
            {
                "sessionUpdate": "plan_update",
                "plan": {"type": "items", "planId": "main", "entries": [
                    {"content": "Inspect", "priority": "high", "status": "completed"}
                ]},
            },
            {"sessionUpdate": "usage_update", "used": 10, "size": 100},
            {"sessionUpdate": "fixture_edge", "nested": {"value": 1}},
        ]
        for update in updates:
            send({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": session_id, "update": update},
            })
            if update["sessionUpdate"] == "tool_call_update" and "title" in update:
                sys.stderr.write("\x01kit-runtime\x01" + json.dumps({"event": "child_started", "call": "call-1:compose:shell", "tool": "shell", "summary": "echo mock", "at": 1}) + "\n")
                sys.stderr.flush()
            elif update["sessionUpdate"] == "tool_call_update" and "status" in update:
                sys.stderr.write("\x01kit-runtime\x01" + json.dumps({"event": "child_finished", "call": "call-1:compose:shell", "tool": "shell", "ok": True, "summary": "done", "millis": 2}) + "\nmock diagnostic\n")
                sys.stderr.flush()
        text = "rich done"
    send({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "agent-1",
                "content": {"type": "text", "text": text},
            },
        },
    })
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn", "usage": {"totalTokens": 12, "inputTokens": 7, "outputTokens": 5}}}})
    if "--exit-after-prompt" in sys.argv:
        time.sleep(0.05)
        os._exit(0)


def inject(request):
    global next_injection
    params = request["params"]
    text = next((block.get("text", "") for block in params["content"] if block.get("type") == "text"), "")
    if "MOCK_REJECT_INJECT" in text:
        send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "injection rejected"}})
        return
    with state_lock:
        message_id = f"injected-{next_injection}"
        next_injection += 1
    respond(request["id"], {"messageId": message_id})
    while inject_release is not None and not os.path.exists(inject_release):
        time.sleep(0.01)
    send({
        "jsonrpc": "2.0", "method": "session/update",
        "params": {"sessionId": params["sessionId"], "update": {
            "sessionUpdate": "user_message", "messageId": message_id, "content": params["content"],
        }},
    })


def close(request):
    global failed_close
    if "--ignore-close" in sys.argv:
        return
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
            "protocolVersion": int(option("--protocol-version") or "2"),
            "info": {"name": "mock-acp", "version": "2.0.0"},
            "capabilities": {"session": {
                "prompt": {name: {} for name in prompt_capability_names if name},
                **({"inject": {"modes": ["steer"], "steerInStream": ["finish"]}} if supports_steer else {}),
            }},
        })
    elif method == "session/new":
        while new_release is not None and not os.path.exists(new_release):
            time.sleep(0.01)
        session_id = argument_value("--session-id") or "base"
        selected_models[session_id] = model_ids[0]
        result = {"sessionId": session_id}
        if os.environ.get("KIT_RUNTIME_EVENTS"):
            sys.stderr.write("\x01kit-runtime\x01" + json.dumps({"event": "session_started", "session_id": result["sessionId"]}) + "\n")
            sys.stderr.flush()
        if os.environ.get("MOCK_EXIT_TAIL"):
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            sys.stdout.flush()
            os._exit(0)
        if supports_models:
            result["configOptions"] = [{
                "configId": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": model_ids[0],
                "options": [
                    {"value": value, "name": value} for value in model_ids
                ],
            }]
        respond(request["id"], result)
        if "--crash-after-new" in sys.argv:
            threading.Thread(target=lambda: (time.sleep(0.05), os._exit(86)), daemon=True).start()
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": result["sessionId"],
            "update": {"sessionUpdate": "available_commands_update", "availableCommands": [
                {"name": "compact", "description": "Compact the session context"}
            ]},
        }})
    elif method == "session/list":
        cursor = request.get("params", {}).get("cursor")
        if cursor is None:
            respond(request["id"], {
                "sessions": [{
                    "sessionId": "catalog-new", "cwd": request["params"]["cwd"],
                    "title": "Newest session", "updatedAt": "2026-08-30T12:34:56.123Z",
                }],
                "nextCursor": "page:2",
            })
        elif cursor == "page:2":
            respond(request["id"], {
                "sessions": [{
                    "sessionId": "catalog-old", "cwd": request["params"]["cwd"],
                    "title": "Older session", "updatedAt": "2026-08-29T10:00:00Z",
                }]
            })
        else:
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "invalid cursor"}})
    elif method == "session/resume":
        if os.environ.get("MOCK_STALE_LOCK") and not globals().get("effective_force", False):
            send({
                "jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32000, "message": "session is locked; use --force to override a stale lock"},
            })
            continue
        fail_once = os.environ.get("MOCK_FAIL_RESUME_ONCE_FILE") or os.environ.get("MOCK_FAIL_LOAD_ONCE_FILE")
        if fail_once and not os.path.exists(fail_once):
            with open(fail_once, "w", encoding="utf-8") as marker:
                marker.write("failed")
            send({
                "jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32000, "message": "injected load failure"},
            })
            continue
        session_id = request["params"]["sessionId"]
        if os.environ.get("KIT_RUNTIME_EVENTS"):
            sys.stderr.write("\x01kit-runtime\x01" + json.dumps({"event": "session_started", "session_id": session_id}) + "\n")
            sys.stderr.flush()
        selected_models[session_id] = model_ids[0]
        for update in [
            {"sessionUpdate": "user_message_chunk", "messageId": "user-1", "content": {"type": "text", "text": "replayed user"}},
            {"sessionUpdate": "agent_message_chunk", "messageId": "agent-1", "content": {"type": "text", "text": "replayed assistant"}},
        ]:
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}})
        result = {}
        if supports_models:
            result["configOptions"] = [{
                "configId": "model", "name": "Model", "category": "model", "type": "select",
                "currentValue": model_ids[0], "options": [{"value": value, "name": value} for value in model_ids],
            }]
        respond(request["id"], result)
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": session_id,
            "update": {"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "compact", "description": "Compact the session context"}]},
        }})
    elif method == "session/fork":
        threading.Thread(target=fork, args=(request,), daemon=True).start()
    elif method == "session/prompt":
        threading.Thread(target=prompt, args=(request,), daemon=True).start()
    elif method == "session/inject":
        threading.Thread(target=inject, args=(request,), daemon=True).start()
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
