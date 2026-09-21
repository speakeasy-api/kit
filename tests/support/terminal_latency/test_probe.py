"""Small protocol/measurement checks; the actual latency probe still requires a PTY."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("probe.py")
spec = importlib.util.spec_from_file_location("probe", SCRIPT)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class ProbeTests(unittest.TestCase):
    def test_escape_stripping_preserves_glyph_after_closed_hyperlink(self):
        self.assertEqual(probe.ESCAPES.sub(b"", b"\x1b]8;;\x1b\\Q\x1b[?2026l"), b"Q")

    def test_v2_prompt_is_acknowledged_before_streaming(self):
        with tempfile.TemporaryDirectory() as directory:
            env = dict(os.environ, PROBE_REQUEST_LOG=str(Path(directory) / "requests.jsonl"),
                       PROBE_HISTORY="1")
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
