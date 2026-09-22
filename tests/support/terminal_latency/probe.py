#!/usr/bin/env python3
"""Real TUI PTY probe; stdlib only. See README.md for scope and reproduction."""
import argparse
import errno
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time


def agent():
    def send(message):
        print(json.dumps(message), flush=True)

    def update(session, index, text):
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": session, "update": {"sessionUpdate": "agent_message_chunk",
            "messageId": f"message-{index}", "content": {"type": "text", "text": text}}}})

    for line in sys.stdin:
        request = json.loads(line)
        method = request.get("method")
        with open(os.environ["PROBE_REQUEST_LOG"], "a") as log:
            log.write(json.dumps(request) + "\n")
        result = {}
        if method == "initialize":
            result = {"protocolVersion": 2, "info": {"name": "latency-fixture", "version": "1"},
                      "capabilities": {"session": {}}, "authMethods": []}
        elif method == "session/new":
            session = sys.argv[sys.argv.index("--session-id") + 1]
            result = {"sessionId": session}
        elif method == "session/prompt":
            session = request["params"]["sessionId"]
            send({"jsonrpc": "2.0", "id": request["id"], "result": {}})
            send({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": session, "update": {"sessionUpdate": "state_update", "state": "running"}}})
            # The runner stops streaming after hot measurement, including on failure.
            stop_stream = Path(os.environ["PROBE_STOP_STREAM"])
            for index in range(int(os.environ.get("PROBE_HISTORY", "1000"))):
                update(session, index, f"replay {index}: **bold** `code` and a small paragraph.\n\n")
            index = 0
            while not stop_stream.exists():
                update(session, 100000, f"stream {index}: some text with **markdown**.\n")
                index += 1
                time.sleep(0.005)
            send({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": session, "update": {"sessionUpdate": "state_update", "state": "idle",
                "stopReason": "end_turn"}}})
            continue
        elif method is None:
            continue
        if "id" in request:
            send({"jsonrpc": "2.0", "id": request["id"], "result": result})


QUERIES = [
    (b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\", b"\x1b_Gi=31;ENOTSUP\x1b\\"),
    (b"\x1b[16t", b"\x1b[6;16;8t"), (b"\x1b[5n", b"\x1b[0n"),
    (b"\x1b[?u", b"\x1b[?0u"), (b"\x1b[c", b"\x1b[?1;2c"),
    (b"\x1b[6n", b"\x1b[1;1R"),
]
# Strip terminal commands before looking for the deliberately unique uppercase glyph.
ESCAPES = re.compile(rb"\x1b\].*?(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[()][A-Z0-9]|\x1b.")


def run(args):
    args.binary = str(Path(args.binary).resolve())
    if args.samples < 1 or args.history < 0:
        raise ValueError("samples must be positive; history must be nonnegative")
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="kit-latency-") as directory:
        home = Path(directory)
        root = home / "workspace"
        root.mkdir()
        stop_stream = home / "stop-stream"
        pid, master = pty.fork()
        if pid == 0:
            env = {"PATH": os.environ["PATH"], "HOME": str(home), "TERM": "xterm-256color",
                   "LANG": "en_US.UTF-8", "PROBE_ROOT": str(root), "PROBE_SCRIPT": str(Path(__file__).resolve()),
                   "PROBE_STOP_STREAM": str(stop_stream),
                   "PROBE_REQUEST_LOG": str(output / "agent-requests.jsonl"), "PROBE_PYTHON": sys.executable, "PROBE_HISTORY": str(args.history)}
            os.chdir(root)
            os.execve(str(Path(args.binary).resolve()), [args.binary], env)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        pending = b""
        frame_end = b"\x1b[?2026l"
        frame_tail = b""
        output_bytes = 0
        completed_frames = 0
        raw = open(output / "terminal.bin", "wb")
        events = open(output / "events.jsonl", "w")
        def pump(timeout):
            nonlocal pending, frame_tail, output_bytes, completed_frames
            if not select.select([master], [], [], timeout)[0]:
                return b""
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    raise RuntimeError("TUI exited; inspect terminal.bin") from error
                raise
            if not data:
                raise RuntimeError("TUI exited")
            raw.write(data)
            output_bytes += len(data)
            framed = frame_tail + data
            completed_frames += framed.count(frame_end)
            frame_tail = framed[-(len(frame_end) - 1):]
            events.write(json.dumps({"read_ns": time.perf_counter_ns(), "bytes": len(data)}) + "\n")
            pending += data
            for query, reply in QUERIES:
                while query in pending:
                    os.write(master, reply)
                    pending = pending.replace(query, b"", 1)
            pending = pending[-256:]
            return data
        def drain(seconds):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                pump(min(0.01, max(0, deadline-time.monotonic())))
        def samples(mode):
            mode_started = time.monotonic()
            initial_bytes, initial_frames = output_bytes, completed_frames
            values = []
            for index in range(args.samples):
                drain(0.03)
                glyph = b"Q" if index % 2 == 0 else b"Z"
                started = time.perf_counter_ns()
                os.write(master, glyph)
                received = b""
                deadline = time.monotonic() + 10
                while (glyph not in ESCAPES.sub(b"", received) or
                       b"\x1b[?2026l" not in received[received.rfind(glyph) + 1:]):
                    if time.monotonic() > deadline:
                        raise TimeoutError(f"no rendered glyph in {mode} sample {index}")
                    received += pump(0.1)
                ended = time.perf_counter_ns()
                latency = (ended-started)/1e6
                values.append(latency)
                events.write(json.dumps({"mode": mode, "sample": index, "key_ns": started,
                                        "frame_end_ns": ended, "latency_ms": latency}) + "\n")
                os.write(master, b"\x7f")
            ordered = sorted(values)
            return {"n": len(values), **{f"p{p}_ms": ordered[math.ceil(p/100*len(values))-1]
                                       for p in (50, 95, 99)}, "max_ms": max(values),
                    "elapsed_seconds": time.monotonic() - mode_started,
                    "output_bytes": output_bytes - initial_bytes,
                    "completed_frames": completed_frames - initial_frames}
        try:
            drain(3)
            if termios.tcgetattr(master)[3] & (termios.ECHO | termios.ICANON):
                raise RuntimeError("TUI did not enter raw mode; refusing to measure PTY echo")
            idle = samples("idle")
            drain(0.2)
            os.write(master, b"replay")
            drain(0.2)
            os.write(master, b"\r")
            drain(0.1)
            hot = samples("hot")
            stop_stream.touch()
            raw.flush()
            if b"stream" not in (output / "terminal.bin").read_bytes():
                raise RuntimeError("no replay stream was rendered during hot measurements")
            digest = hashlib.sha256(Path(args.binary).read_bytes()).hexdigest()
            result = {"binary": args.binary, "binary_sha256": digest, "history_messages": args.history,
                      "terminal": "120x40", "idle": idle, "hot": hot}
            (output / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result))
        finally:
            stop_stream.touch()
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.close(master)
            os.waitpid(pid, 0)
            raw.close()
            events.close()


if __name__ == "__main__":
    if "--agent" in sys.argv:
        agent()
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("binary")
        parser.add_argument("--output", required=True)
        parser.add_argument("--samples", type=int, default=100)
        parser.add_argument("--history", type=int, default=1000)
        run(parser.parse_args())
