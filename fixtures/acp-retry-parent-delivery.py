#!/usr/bin/env python3
"""Real Kit ACP parent + synthetic child notices, with local-only inference.

This verifies delivery, not provider retry accounting or retry rate limiting.
The child cannot finish until its scheduled notice reaches the parent's client.
"""
import collections
import http.server
import json
import os
import pathlib
import queue
import subprocess
import sys
import tempfile
import threading

KIT = sys.argv[1]
CHILD = str(pathlib.Path(__file__).with_name("mock-acp-retry.py").resolve())


def run(version, background):
    with tempfile.TemporaryDirectory() as directory:
        root = pathlib.Path(directory)
        (root / ".kit").mkdir()
        release = root / "release"
        config = ('provider = "openrouter"\nmodel = "test/model"\n'
                  'credential_store = "memory"\n[acp.retry]\ncommand = "python3"\n'
                  f'args = {json.dumps([CHILD, str(version), str(release)])}\n')
        (root / ".kit/config.toml").write_text(config)
        requests = []
        errors = queue.Queue()

        class Provider(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    requests.append(body)
                    if len(requests) == 1:
                        args = {"script": 'return subagent({prompt:"transport", harness:"acp.retry"})'}
                        if background:
                            args["background"] = True
                        delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "retry-delivery",
                                 "type": "function", "function": {"name": body["tools"][0]["function"]["name"],
                                 "arguments": json.dumps(args)}}]}
                        reason = "tool_calls"
                    else:
                        done = "CHILD_DONE" in json.dumps(body["messages"])
                        delta = {"role": "assistant", "content": "PARENT_DONE" if done else ""}
                        reason = "stop"
                    chunk = {"id": "local", "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]}
                    response = ("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(response)))
                    self.end_headers()
                    self.wfile.write(response)
                except Exception as exc:
                    errors.put(exc)
                    raise

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        environment = {"HOME": directory, "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                       "OPENROUTER_API_KEY": "local-only",
                       "OPENROUTER_BASE_URL": f"http://127.0.0.1:{server.server_port}"}
        # A file, rather than an unread stderr pipe, cannot block the child.
        with (root / "stderr").open("w+") as stderr:
            process = subprocess.Popen([KIT, "acp", "--protocol-version", str(version),
                                        "--root", directory], env=environment,
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                       stderr=stderr, text=True)
            received = queue.Queue()

            def reader():
                for line in process.stdout:
                    received.put(json.loads(line))
                received.put(None)

            threading.Thread(target=reader, daemon=True).start()
            messages = []

            def send(ident, method, params):
                process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": ident,
                                                "method": method, "params": params}) + "\n")
                process.stdin.flush()

            def receive():
                message = received.get(timeout=25)
                assert message is not None, "ACP parent closed unexpectedly"
                messages.append(message)
                # Never silently allow an unexpected interactive permission request.
                assert not ("method" in message and "id" in message), message
                assert "error" not in message, message
                return message

            def response(ident):
                while True:
                    message = receive()
                    if message.get("id") == ident:
                        return message["result"]

            try:
                send(1, "initialize", {"protocolVersion": version,
                     "clientInfo": {"name": "retry-test", "version": "1"}, "clientCapabilities": {"subagents": {}},
                     "info": {"name": "retry-test", "version": "1"}, "capabilities": {}})
                assert response(1)["protocolVersion"] == version
                send(2, "session/new", {"cwd": directory, "mcpServers": []})
                session = response(2)["sessionId"]
                send(3, "session/prompt", {"sessionId": session,
                     "prompt": [{"type": "text", "text": "Run the transport test"}]})
                notices = []
                child_ids = set()
                parent_done = False
                while not parent_done:
                    message = receive()
                    params = message.get("params", {})
                    update = params.get("update", {})
                    progress = update.get("_meta", {}).get("kit/providerRetry")
                    if progress:
                        assert params["sessionId"] != session, "child notice lost its owner"
                        child_ids.add(params["sessionId"])
                        notices.append(progress)
                        if progress["status"] == "scheduled":
                            assert not release.exists(), "duplicate scheduled notice"
                            release.write_text("observed")
                    if params.get("sessionId") == session and "PARENT_DONE" in json.dumps(update):
                        parent_done = True
                # A protocol round trip is a deterministic barrier, not a sleep
                # hoping that no second outcome appears.
                send(4, "session/new", {"cwd": directory, "mcpServers": []})
                response(4)
                all_updates = [m.get("params", {}).get("update", {}) for m in messages]
                all_notices = [u for u in all_updates if u.get("sessionUpdate") == "notice"]
                retries = [u["_meta"]["kit/providerRetry"] for u in all_notices
                           if "kit/providerRetry" in u.get("_meta", {})]
                assert [n["status"] for n in retries] == ["scheduled", "succeeded"], retries
                assert len(child_ids) == 1
                assert any(n["title"] == "Ordinary notice" for n in all_notices)
                assert all(set(n) == {"status", "route", "attempts", "category", "next_delay_ms",
                                      "completed_backoff_ms"} for n in retries)
                assert retries[0]["attempts"] == 2 and retries[1]["completed_backoff_ms"] == 25
                terminals = collections.Counter(u.get("toolCallId") for u in all_updates
                            if u.get("status") in ("completed", "failed"))
                assert terminals and all(n == 1 for n in terminals.values()), terminals
                assert errors.empty(), list(errors.queue)
            except Exception:
                stderr.flush()
                stderr.seek(0)
                print(f"v{version} background={background}; messages={json.dumps(messages)[-12000:]}", file=sys.stderr)
                print(stderr.read()[-4000:], file=sys.stderr)
                raise
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                server.shutdown()


for protocol in (1, 2):
    for detached in (False, True):
        run(protocol, detached)
