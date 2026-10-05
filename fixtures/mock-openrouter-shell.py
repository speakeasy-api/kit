#!/usr/bin/env python3
"""Local provider boundary for the desktop's real ACP/compose/shell regression.

Prints one ephemeral loopback port, then serves OpenRouter-compatible SSE.
No model service, credentials, or shell execution occurs in this fixture.
"""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.respond(json.dumps({"data": []}).encode(), "application/json")

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        results = [m for m in body["messages"] if m.get("role") == "tool"]
        if results:
            # Do not claim success merely because a tool returned an error.
            result = json.loads(results[-1]["content"])
            success = result.get("success") is True and result.get("exit_code") == 0
            delta = {"role": "assistant", "content": "REAL_SHELL_COMPLETE" if success else "REAL_SHELL_FAILED"}
            reason = "stop"
        else:
            name = next(t["function"]["name"] for t in body["tools"]
                        if t["function"]["name"].split(".")[-1] == "compose")
            delta = {"role": "assistant", "tool_calls": [{
                "index": 0, "id": "real-shell-pwd", "type": "function",
                "function": {"name": name, "arguments": json.dumps({
                    "script": 'return shell({command: "pwd", timeout_seconds: 3})'
                })}
            }]}
            reason = "tool_calls"
        chunk = {"id": "local-shell", "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]}
        self.respond(("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode(), "text/event-stream")

    def respond(self, body, content_type):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    with HTTPServer(("127.0.0.1", 0), Provider) as server:
        print(server.server_port, flush=True)
        server.serve_forever()
