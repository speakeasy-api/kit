"""Exercise the release job's shell with a fake Kit binary; no network or secrets."""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/release.yml").read_text()
JOB = WORKFLOW.split("  announce-release:\n", 1)[1].split("  container-build:\n", 1)[0]
SCRIPT = textwrap.dedent(JOB.split("        run: |\n", 1)[1])
FAKE_KEY = 'fake-gram-"-\\-token'
PERMALINK = "https://example.slack.com/archives/C123456/p1234567890123456"
MOCK_KIT = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import stat
import sys

if sys.argv[1:] == ["--version"]:
    print("kit mock")
    sys.exit(0)
args = sys.argv[1:]
assert args[0] == "prompt"
assert args[args.index("--provider") + 1] == "openrouter"
assert args[args.index("--model") + 1] == "openai/gpt-5.6-terra"
assert args[args.index("--root") + 1] == os.environ["GITHUB_WORKSPACE"]
config = Path(args[args.index("--mcp-config") + 1])
assert config.parent.parent == Path(os.environ["RUNNER_TEMP"])
assert stat.S_IMODE(config.stat().st_mode) == 0o600
assert "GRAM_API_KEY" not in os.environ
server = json.loads(config.read_text())["mcpServers"]["slack"]
assert server["type"] == "http"
assert server["url"] == "https://mcp.example.test/slack"
assert server["headers"] == {
    "Authorization": "Bearer " + FAKE_KEY_LITERAL,
    "Gram-Environment": "release-bot",
}
prompt = args[-1]
assert "announce-kit-release skill" in prompt
assert "v1.2.2..v1.2.3" in prompt
assert "channel named test-releases" in prompt
assert "tool_search" in prompt
assert "pagination" in prompt
assert "no channel-history access" in prompt
assert "actual message returned" in prompt
assert "Do not call history tools or message search" in prompt
mode = os.environ["MOCK_MODE"]
if mode == "failure":
    sys.exit(7)
if mode == "success":
    print(PERMALINK_LITERAL)
elif mode == "extra-text":
    print("Failed verification; do not treat this link as success")
    print(PERMALINK_LITERAL)
elif mode == "no-link":
    print("Unable to post announcement")
print("session_id: test-session")
'''.replace("FAKE_KEY_LITERAL", repr(FAKE_KEY)).replace("PERMALINK_LITERAL", repr(PERMALINK))


class ReleaseAnnouncementTests(unittest.TestCase):
    def run_job(self, mode="success", overrides=None):
        with tempfile.TemporaryDirectory(prefix="kit-slack-test-") as temporary:
            root = Path(temporary)
            (root / "tmp").mkdir()
            (root / "dist").mkdir()
            package = root / "dist/kit-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
            with tarfile.open(package, "w:gz") as archive:
                data = MOCK_KIT.encode()
                info = tarfile.TarInfo("kit")
                info.size = len(data)
                info.mode = 0o755
                archive.addfile(info, io.BytesIO(data))
            env = {
                "PATH": os.environ["PATH"],
                "HOME": str(root),
                "GITHUB_WORKSPACE": str(root),
                "GITHUB_STEP_SUMMARY": str(root / "summary"),
                "RUNNER_TEMP": str(root / "tmp"),
                "RELEASE_VERSION": "1.2.3",
                "RELEASE_TAG": "v1.2.3",
                "PREVIOUS_TAG": "v1.2.2",
                "RELEASE_NOTES_MODEL": "openai/gpt-5.6-terra",
                "OPENROUTER_API_KEY": "fake-openrouter",
                "SLACK_CHANNEL": "test-releases",
                "SLACK_MCP_URL": "https://mcp.example.test/slack",
                "GRAM_ENVIRONMENT": "release-bot",
                "GRAM_API_KEY": FAKE_KEY,
                "MOCK_MODE": mode,
            }
            env.update(overrides or {})
            result = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", SCRIPT],
                cwd=root, env=env, text=True, capture_output=True, timeout=15,
            )
            self.assertEqual(list((root / "tmp").iterdir()), [], "MCP config leaked")
            self.assertNotIn(FAKE_KEY, result.stdout + result.stderr)
            summary = root / "summary"
            return result, summary.read_text() if summary.exists() else ""

    def test_workflow_contract(self):
        self.assertIn("needs: [verify, publish, containers]", JOB)
        self.assertIn("ref: ${{ needs.verify.outputs.release_sha }}", JOB)
        self.assertIn("GRAM_API_KEY: ${{ secrets.GRAM_API_KEY }}", JOB)
        self.assertNotIn("SLACK_BOT_TOKEN", JOB)
        self.assertEqual(WORKFLOW.count('--model "$RELEASE_NOTES_MODEL"'), 2)
        self.assertEqual(
            WORKFLOW.count("OPENROUTER_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}"), 2,
        )
        self.assertIn("RELEASE_NOTES_MODEL: openai/gpt-5.6-terra", WORKFLOW)
        subprocess.run(["bash", "-n"], input=SCRIPT, text=True, check=True)

    def test_success_private_config_and_summary(self):
        result, summary = self.run_job()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(summary, f"Slack release announcement: {PERMALINK}\n")

    def test_missing_settings_fail_closed(self):
        for name in ["OPENROUTER_API_KEY", "SLACK_CHANNEL", "SLACK_MCP_URL",
                     "GRAM_ENVIRONMENT", "GRAM_API_KEY"]:
            with self.subTest(name=name):
                result, summary = self.run_job(overrides={name: ""})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"Missing required {name}", result.stdout)
                self.assertEqual(summary, "")

    def test_untrusted_url_fails_and_cleans_config(self):
        for url in ["http://mcp.example.test/slack",
                    "https://user:password@mcp.example.test/slack"]:
            with self.subTest(url=url):
                result, summary = self.run_job(overrides={"SLACK_MCP_URL": url})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must be an HTTPS MCP endpoint", result.stderr)
                self.assertEqual(summary, "")

    def test_cli_failure_propagates(self):
        result, summary = self.run_job(mode="failure")
        self.assertEqual(result.returncode, 7)
        self.assertEqual(summary, "")

    def test_only_a_single_permalink_confirms_success(self):
        for mode in ["no-link", "empty", "extra-text"]:
            with self.subTest(mode=mode):
                result, summary = self.run_job(mode=mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("did not confirm", result.stdout)
                self.assertEqual(summary, "")


if __name__ == "__main__":
    unittest.main()
