"""Small protocol/measurement checks; the actual latency probe still requires a PTY."""
import io
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("probe.py")
spec = importlib.util.spec_from_file_location("probe", SCRIPT)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class ProbeTests(unittest.TestCase):
    def test_escape_stripping_preserves_glyph_after_closed_hyperlink(self):
        self.assertEqual(probe.ESCAPES.sub(b"", b"\x1b]8;;\x1b\\Q\x1b[?2026l"), b"Q")

    def test_stream_outlasts_old_chunk_limit_until_runner_stops_it(self):
        with tempfile.TemporaryDirectory() as directory:
            stop = Path(directory) / "stop-stream"
            output = io.StringIO()
            request = {"jsonrpc": "2.0", "id": "p", "method": "session/prompt",
                       "params": {"sessionId": "test", "prompt": []}}
            paced_chunks = 0

            def pace(seconds):
                nonlocal paced_chunks
                self.assertEqual(seconds, 0.005)
                paced_chunks += 1
                # Cross the former 6000-chunk/30-second boundary without a slow test.
                if paced_chunks > 6000:
                    stop.touch()

            env = {"PROBE_REQUEST_LOG": str(Path(directory) / "requests.jsonl"),
                   "PROBE_HISTORY": "0", "PROBE_STOP_STREAM": str(stop)}
            with (
                patch.dict(os.environ, env),
                patch.object(sys, "stdin", io.StringIO(json.dumps(request) + "\n")),
                patch.object(sys, "stdout", output),
                patch.object(probe.time, "sleep", pace),
            ):
                probe.agent()
            messages = [json.loads(line) for line in output.getvalue().splitlines()]
            updates = [message["params"]["update"] for message in messages[1:]]
            chunks = [update for update in updates if update["sessionUpdate"] == "agent_message_chunk"]
            self.assertGreater(len(chunks), 6000)
            self.assertTrue(stop.exists(), "stream must not complete before the runner stops it")
            self.assertEqual(updates[-1], {"sessionUpdate": "state_update", "state": "idle",
                                          "stopReason": "end_turn"})
            self.assertFalse(any(update.get("state") == "idle" for update in updates[:-1]))

    def test_v2_prompt_is_acknowledged_before_streaming(self):
        with tempfile.TemporaryDirectory() as directory:
            env = dict(os.environ, PROBE_REQUEST_LOG=str(Path(directory) / "requests.jsonl"),
                       PROBE_HISTORY="1", PROBE_STOP_STREAM=str(Path(directory) / "stop-stream"))
            child = subprocess.Popen([sys.executable, str(SCRIPT), "--agent", "--session-id", "test"],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     text=True, env=env)
            try:
                child.stdin.write(json.dumps({"jsonrpc": "2.0", "id": "p", "method": "session/prompt",
                                              "params": {"sessionId": "test", "prompt": []}}) + "\n")
                child.stdin.flush()
                self.assertEqual(json.loads(child.stdout.readline()),
                                 {"jsonrpc": "2.0", "id": "p", "result": {}})
                state = json.loads(child.stdout.readline())
                self.assertEqual(state["params"]["update"],
                                 {"sessionUpdate": "state_update", "state": "running"})
                chunk = json.loads(child.stdout.readline())
                self.assertEqual(chunk["params"]["update"]["sessionUpdate"], "agent_message_chunk")
            finally:
                child.kill()
                child.communicate(timeout=5)


if __name__ == "__main__":
    unittest.main()
