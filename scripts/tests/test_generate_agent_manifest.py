"""Exercise the manifest generator through its release CLI."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "generate-agent-manifest.py"
TARGETS = {
    "darwin-aarch64": "aarch64-apple-darwin",
    "linux-x86_64": "x86_64-unknown-linux-gnu",
}


class AgentManifestTests(unittest.TestCase):
    def run_generator(self, version, directory):
        return subprocess.run(
            [sys.executable, str(SCRIPT), version, str(directory)],
            capture_output=True, text=True,
        )

    def test_release_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            for target in TARGETS.values():
                (directory / f"kit-v1.2.3-{target}.tar.gz").write_bytes(target.encode())
            result = self.run_generator("1.2.3", directory)
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads((directory / "agent.json").read_text())
            self.assertEqual(manifest["id"], "kit")
            self.assertEqual(manifest["version"], "1.2.3")
            self.assertEqual(manifest["license"], "MIT")
            self.assertEqual(manifest["license_url"],
                             "https://github.com/speakeasy-api/kit/blob/v1.2.3/LICENSE")
            binary = manifest["distribution"]["binary"]
            self.assertEqual(set(binary), set(TARGETS))
            for platform, target in TARGETS.items():
                self.assertEqual(binary[platform], {
                    "archive": "https://github.com/speakeasy-api/kit/releases/download/"
                               f"v1.2.3/kit-v1.2.3-{target}.tar.gz",
                    "sha256": hashlib.sha256(target.encode()).hexdigest(),
                    "cmd": "./kit", "args": ["acp"],
                })
            # Re-running publication produces identical bytes.
            before = (directory / "agent.json").read_bytes()
            self.assertEqual(self.run_generator("1.2.3", directory).returncode, 0)
            self.assertEqual((directory / "agent.json").read_bytes(), before)

    def test_missing_archive_fails_without_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            (directory / "kit-v1.2.3-aarch64-apple-darwin.tar.gz").write_bytes(b"macos")
            self.assertNotEqual(self.run_generator("1.2.3", directory).returncode, 0)
            self.assertFalse((directory / "agent.json").exists())

    def test_invalid_or_preview_version_fails_without_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            for version in ["v1.2.3", "1.2.3-rc.1", "1.2", "../1.2.3", "01.2.3"]:
                with self.subTest(version=version):
                    result = self.run_generator(version, Path(tmp))
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("X.Y.Z release version", result.stderr)
                    self.assertFalse((Path(tmp) / "agent.json").exists())


if __name__ == "__main__":
    unittest.main()
