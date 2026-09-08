"""Test-only ACP boundary for real image attachment/output contracts."""
import base64
import json
import struct
import sys
import zlib

image, log = sys.argv[1:]
sequence = 0

def chunk(kind, payload):
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))

def alternate_png(width=2, height=3):
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return base64.b64encode(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress((b"\x00" + b"\xff" * (width * 3)) * height)) + chunk(b"IEND", b"")).decode()


def send(value):
    print(json.dumps(dict(jsonrpc="2.0", **value)), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    params = request.get("params", {})
    result = {}
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {
            "promptCapabilities": {"image": True},
            "sessionCapabilities": {"fork": {}, "close": {}}}}
    elif method in ("session/new", "session/fork"):
        sequence += 1
        result = {"sessionId": "native-" + str(sequence)}
    elif method == "session/prompt":
        with open(log, "a") as output:
            output.write(json.dumps(params) + "\n")
        text = params["prompt"][0]["text"].split("\n")[0]
        count = 0 if text == "zero" else 9 if text == "overcount" else 4 if text == "overpixels" else 3 if text == "duplicate-distinct" else 2 if text in ("multiple", "multiple-root", "duplicate", "duplicate-root", "invalid-mime", "invalid-png", "same-pixels") else 1
        for index in range(count):
            payload = "invalid!" if text == "corrupt" else image
            mime = "image/png"
            if index == 1:
                if text in ("multiple", "multiple-root"):
                    payload = alternate_png()
                elif text == "invalid-mime":
                    mime = "image/jpeg"
                elif text == "invalid-png":
                    payload = base64.b64encode(b"not a PNG").decode()
                elif text == "same-pixels":
                    original = base64.b64decode(image)
                    payload = base64.b64encode(original[:-12] + chunk(b"tEXt", b"Comment\x00alternate encoding") + original[-12:]).decode()
            if text == "duplicate-distinct" and index == 2:
                payload = alternate_png()
            if text == "overpixels" and index > 0:
                payload = alternate_png(4096, 4096)
            with open(log + ".images", "a") as emitted:
                emitted.write(json.dumps({"data": payload, "mime": mime}) + "\n")
            send({"method": "session/update", "params": {
                "sessionId": params["sessionId"], "update": {
                    "sessionUpdate": "agent_message_chunk", "content": {
                        "type": "image", "data": payload,
                        "mimeType": mime}}}})
        surrounding = '{"result": null}' if text == "attempt" else "" if text in ("root", "zero", "duplicate-root", "multiple-root") else '{"caption":"native"}'
        if surrounding:
            send({"method": "session/update", "params": {
                "sessionId": params["sessionId"], "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": surrounding}}}})
        result = {"stopReason": "end_turn"}
    if "id" in request:
        send({"id": request["id"], "result": result})
