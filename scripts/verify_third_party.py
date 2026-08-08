#!/usr/bin/env python3
"""Verify the exact third-party notices and license payload."""

from hashlib import sha256
from pathlib import Path
import shutil
import sys
import tempfile


EXPECTED = {
    "THIRD_PARTY_NOTICES.md": "916cd966eddb97e27a0d62dab4ed606902925e57df7de3215146daa56fd105a4",
    "third_party/licenses/CODEX-APACHE-2.0.txt": "5a2c67d1a8994f276f10581ef0d16b70e1b48d2a5c1bab0d92de64ea680125b3",
    "third_party/licenses/OPENCODE-MIT.txt": "625f0f619133f89bbbb2abe37369613dfa1885eba1e50d02170deb62bb42cb6b",
}


def validate(root):
    problems = []
    licenses = root / "third_party/licenses"
    actual = (
        {str(path.relative_to(root)) for path in licenses.iterdir() if path.is_file()}
        if licenses.is_dir()
        else set()
    )
    expected = {path for path in EXPECTED if path.startswith("third_party/licenses/")}
    if actual != expected:
        problems.append("third-party license file set differs from the pinned set")
    for relative, digest in EXPECTED.items():
        path = root / relative
        try:
            actual_digest = sha256(path.read_bytes()).hexdigest()
        except OSError:
            problems.append(f"missing third-party notice or license: {relative}")
            continue
        if actual_digest != digest:
            problems.append(f"third-party notice or license digest changed: {relative}")
    return problems


def self_test(root):
    problems = validate(root)
    with tempfile.TemporaryDirectory(prefix="third-party-license-") as temporary:
        fixture = Path(temporary)
        (fixture / "third_party").mkdir()
        shutil.copytree(root / "third_party/licenses", fixture / "third_party/licenses")
        shutil.copy2(root / "THIRD_PARTY_NOTICES.md", fixture)
        target = fixture / "third_party/licenses/OPENCODE-MIT.txt"
        target.write_bytes(target.read_bytes() + b"mutation")
        if not validate(fixture):
            problems.append("negative test failed: mutated license was accepted")
        target.unlink()
        if not validate(fixture):
            problems.append("negative test failed: deleted license was accepted")
    return problems


def main(argv):
    root = Path(__file__).resolve().parents[1]
    if argv == []:
        problems = validate(root)
    elif argv == ["--self-test"]:
        problems = self_test(root)
    else:
        print("usage: verify_third_party.py [--self-test]", file=sys.stderr)
        return 2
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    print("third-party notices and license digests verified")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
