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
supports_config_options = "--config-options" in sys.argv
selected_options = {}
fail_first_close = "--fail-first-close" in sys.argv
failed_close = False
selected_models = {}
v2_cancellations = {}


def option(name):
    prefix = name + "="
    return next((arg[len(prefix):] for arg in sys.argv if arg.startswith(prefix)), None)


request_log = option("--request-log")
idle_config_release = option("--idle-config-release")
idle_config_sent = option("--idle-config-sent")
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
    if "id" in request:
        entry["id"] = request["id"]
    if "requestId" in params:
        entry["requestId"] = params["requestId"]
    if "sessionId" in params:
        entry["sessionId"] = params["sessionId"]
    if request.get("method") in ("session/new", "session/fork"):
        entry["cwd"] = params["cwd"]
        entry["additionalDirectories"] = params.get("additionalDirectories", [])
    if request.get("method") == "session/prompt":
        entry["text"] = " ".join(block["text"] for block in params["prompt"] if block["type"] == "text")
    with log_lock:
        with open(request_log, "a", encoding="utf-8") as log:
            log.write(json.dumps(entry, separators=(",", ":")) + "\n")
            log.flush()



def config_options(session_id):
    # Copy state under its lock; construct snapshots and write outside it.
    with state_lock:
        model = selected_models.get(session_id, model_ids[0])
        values = selected_options.get(session_id, {}).copy()
    result = []
    if supports_models:
        result.append({
            "id": "model", "name": "Model", "category": "model",
            "type": "select", "currentValue": model,
            "options": [{"value": value, "name": value} for value in model_ids],
        })
    if supports_config_options:
        for option_id, category, choices, default in [
            ("reasoning_effort", "thought_level", ["low", "high"], "low"),
            ("mode", "mode", ["code", "plan"], "code"),
        ]:
            result.append({
                "id": option_id, "name": option_id, "category": category,
                "type": "select", "currentValue": values.get(option_id, default),
                "options": [{"value": value, "name": value} for value in choices],
            })
        result.append({
            "id": "custom_enabled", "name": "Custom enabled", "type": "boolean",
            "currentValue": values.get("custom_enabled", False),
        })
    if "--v2" in sys.argv:
        for option in result:
            option["configId"] = option.pop("id")
    return result


def config_update(session_id):
    send({
        "jsonrpc": "2.0", "method": "session/update",
        "params": {"sessionId": session_id, "update": {
            "sessionUpdate": "config_option_update",
            "configOptions": config_options(session_id),
        }},
    })


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
        selected_options[session_id] = selected_options.get(source_id, {}).copy()
    while fork_release is not None and not os.path.exists(fork_release):
        time.sleep(0.01)
    respond(request["id"], {"sessionId": session_id,
                             "configOptions": config_options(session_id)})


def prompt(request):
    if "--v2" in sys.argv:
        # A startup idle can race the new prompt; it must not settle this turn.
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": request["params"]["sessionId"],
            "update": {"sessionUpdate": "state_update", "state": "idle", "stopReason": "refusal"}
        }})
        respond(request["id"], {})
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": request["params"]["sessionId"],
            "update": {"sessionUpdate": "state_update", "state": "running"}
        }})
    params = request["params"]
    session_id = params["sessionId"]
    text = " ".join(block["text"] for block in params["prompt"] if block["type"] == "text")
    if "--v2" in sys.argv and text == "MOCK_WAIT_CANCEL":
        event = threading.Event()
        with state_lock:
            v2_cancellations[session_id] = event
        with open(option("--accepted-marker"), "w", encoding="utf-8") as marker:
            marker.write("accepted")
        event.wait()

    should_gate = prompt_release is not None and (
        prompt_release_text is None or prompt_release_text == text
    )
    while should_gate and not os.path.exists(prompt_release):
        time.sleep(0.01)
    if prompt_release is None:
        time.sleep(0.40)
    if "--echo-prompt-content" in sys.argv:
        text = json.dumps(params["prompt"])
    if "MOCK_CWD" in text:
        text = os.getcwd()
    if "MOCK_SELECTED_MODEL" in text:
        with state_lock:
            text = selected_models.get(session_id, model_ids[0])
    idle_config_update = "MOCK_IDLE_CONFIG_UPDATE" in text
    if supports_config_options and ("MOCK_CONFIG_UPDATE" in text or idle_config_update):
        with state_lock:
            selected_options[session_id] = {
                "reasoning_effort": "low", "mode": "plan", "custom_enabled": False,
            }
        if not idle_config_update:
            config_update(session_id)
    if "MOCK_CONFIG_OPTIONS" in text:
        text = json.dumps(config_options(session_id))
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
            {"sessionUpdate": "usage_update", "used": 10, "size": 100,
             "cost": {"amount": 0.25, "currency": "USD"}},
            {"sessionUpdate": "session_info_update", "title": "Child title"},
            {"sessionUpdate": "available_commands_update", "availableCommands": []},
            {"sessionUpdate": "notice", "severity": "info", "title": "Working"},
            {"sessionUpdate": "compaction_update", "compactionId": "compact-1",
             "status": "in_progress"},
            {"sessionUpdate": "compaction_summary_chunk", "compactionId": "compact-1",
             "content": {"type": "text", "text": "Retained context"}},
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
    if "--v2" in sys.argv:
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": session_id, "update": {
                "sessionUpdate": "state_update", "state": "idle", "stopReason": (text.removeprefix("MOCK_STOP:") if text.startswith("MOCK_STOP:") else "end_turn")
            }
        }})
    else:
        respond(request["id"], {"stopReason": "end_turn"})
    if idle_config_update:
        while idle_config_release is not None and not os.path.exists(idle_config_release):
            time.sleep(0.01)
        config_update(session_id)
        if idle_config_sent is not None:
            with open(idle_config_sent, "w", encoding="utf-8") as marker:
                marker.write("sent")
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
        if "--require-compaction" in sys.argv:
            assert request["params"]["clientCapabilities"]["session"]["compaction"] == {}
        params = request["params"]
        assert params["protocolVersion"] == 2
        assert params["clientInfo"]["name"] == "kit"
        assert params["clientInfo"]["version"]
        if "--v2" in sys.argv:
            assert params["info"] == params["clientInfo"]
            assert params["capabilities"] == {}
            respond(request["id"], {
                "protocolVersion": 2, "info": {"name": "mock", "version": "1"},
                "capabilities": {"session": {"fork": {}} if supports_fork else {}}
            })
            continue
        respond(request["id"], {
            "protocolVersion": 1,
            "agentCapabilities": {
                "promptCapabilities": {
                    "image": "--prompt-content" in sys.argv,
                    "embeddedContext": "--prompt-content" in sys.argv,
                },
                "sessionCapabilities": (
                    ({"fork": {}, "close": {}} if supports_fork else {"close": {}})
                    | ({"delete": {}} if "--delete" in sys.argv else {})
                    | ({} if "--no-additional-directories" in sys.argv else {"additionalDirectories": {}})
                )
            },
        })
    elif method == "session/new":
        while new_release is not None and not os.path.exists(new_release):
            time.sleep(0.01)
        with state_lock:
            selected_models["base"] = model_ids[0]
            selected_options["base"] = {}
        result = {"sessionId": "base"}
        if supports_models or supports_config_options:
            result["configOptions"] = config_options("base")
        respond(request["id"], result)
    elif method == "session/resume":
        params = request["params"]
        if "--v2" not in sys.argv or params.get("replayFrom") != {"type": "start"}:
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "replay required"}})
            continue
        session_id = params["sessionId"]
        for text in ["replayed ", "history"]:
            send({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": session_id, "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}
                }
            }})
            send({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": session_id, "update": {
                    "sessionUpdate": "state_update", "state": "idle", "stopReason": "refusal"
                }
            }})
        if session_id == "replay-stall":
            with open(os.path.join(params["cwd"], "replay-stalled"), "w") as marker:
                marker.write(str(os.getpid()))
            while True:
                time.sleep(0.01)
        if session_id == "replay-eof":
            sys.exit(0)
        if session_id == "replay-failure":
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603, "message": "replay failed"}})
            continue
        with state_lock:
            selected_models[session_id] = model_ids[0]
            selected_options[session_id] = {}
        respond(request["id"], {"configOptions": config_options(session_id)})
    elif method == "session/fork":
        threading.Thread(target=fork, args=(request,), daemon=True).start()
    elif method == "session/prompt":
        threading.Thread(target=prompt, args=(request,), daemon=True).start()
    elif method == "session/set_config_option":
        params = request["params"]
        value = params["value"]
        option_id = params["configId"]
        advertised = next((item for item in config_options(params["sessionId"])
                           if item.get("configId", item.get("id")) == option_id), None)
        valid = advertised is not None and (
            (advertised["type"] == "boolean" and type(value) is bool) or
            (advertised["type"] == "select" and type(value) is str and
             value in [item["value"] for item in advertised["options"]])
        )
        if not valid:
            send({
                "jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32602, "message": "invalid config option value"},
            })
        else:
            with state_lock:
                if option_id == "model":
                    selected_models[params["sessionId"]] = value
                else:
                    selected_options[params["sessionId"]][option_id] = value
            respond(request["id"], {"configOptions": config_options(params["sessionId"])})
    elif method == "session/cancel":
        with state_lock:
            event = v2_cancellations.pop(request["params"]["sessionId"], None)
        if event is not None:
            event.set()
    elif method == "session/close":
        threading.Thread(target=close, args=(request,), daemon=True).start()

    elif method == "session/delete":
        if "--slow-delete" in sys.argv:
            continue
        if "--fail-delete" in sys.argv:
            send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "delete failed"}})
        else:
            respond(request["id"], {})
