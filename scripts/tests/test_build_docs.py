#!/usr/bin/env python3
"""Exercise build-script validation as a process, without building dependencies."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class BuildDocsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build_directory = tempfile.TemporaryDirectory(prefix="kit-build-docs-")
        cls.binary = Path(cls.build_directory.name) / "build-docs"
        subprocess.run(["rustc", "--edition=2024", str(ROOT / "build.rs"), "-o", str(cls.binary)], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.build_directory.cleanup()

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="kit-build-input-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.docs = self.root / "docs" / "user"
        self.docs.mkdir(parents=True)
        self.output = self.root / "out"
        self.output.mkdir()

    def run_build(self, expected_error=None, out_dir=True):
        environment = {key: value for key, value in os.environ.items() if key != "OUT_DIR"}
        if out_dir:
            environment["OUT_DIR"] = str(self.output)
        result = subprocess.run([str(self.binary)], cwd=self.root, env=environment, capture_output=True, text=True)
        self.assertNotIn("panicked", result.stderr)
        if expected_error is None:
            self.assertEqual(result.returncode, 0, result.stderr)
            return (self.output / "bundled_docs.rs").read_text()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(expected_error, result.stderr)
        self.assertFalse((self.output / "bundled_docs.rs").is_file())

    def test_valid_sorted_docs(self):
        (self.docs / "z.md").write_text("last")
        (self.docs / "a.md").write_text("first")
        (self.docs / "ignored.txt").write_text("not bundled")
        self.assertEqual(self.run_build(), '&[\n    ("docs/user/a.md", "first"),\n    ("docs/user/z.md", "last"),\n]')

    def test_missing_root(self):
        self.docs.rmdir()
        self.run_build("could not collect docs/user")

    def test_empty_docs(self):
        self.run_build("must contain bundled Markdown documentation")

    def test_invalid_utf8(self):
        (self.docs / "bad.md").write_bytes(b"\xff")
        self.run_build("could not read docs/user/bad.md")

    def test_missing_out_dir(self):
        (self.docs / "a.md").write_text("first")
        self.run_build("OUT_DIR is not set", out_dir=False)

    def test_unwritable_output(self):
        (self.docs / "a.md").write_text("first")
        self.output.rmdir()
        self.output.write_text("not a directory")
        self.run_build("could not write")

    @unittest.skipIf(os.name == "nt", "symlink creation may require elevation")
    def test_symlink_rejected(self):
        (self.docs / "a.md").symlink_to("outside.md")
        self.run_build("documentation path must not be a symlink")


if __name__ == "__main__":
    unittest.main()
