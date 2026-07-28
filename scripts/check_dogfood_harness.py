#!/usr/bin/env python3
import json
import subprocess
from pathlib import Path


root = Path(__file__).resolve().parent.parent
manifest = root / "dogfood-harness/Cargo.toml"
source = (root / "dogfood-harness/tests/dogfood.rs").read_text(encoding="utf-8")
integration = (root / "tests/integration/main.rs").read_text(encoding="utf-8")
metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1", "--manifest-path", manifest],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
)

for forbidden in ("use kit::", "kit::", "daemon.json", "state.sqlite", "kit.db"):
    if forbidden in source:
        raise SystemExit(f"dogfood harness uses private Kit surface: {forbidden}")
if "mod dogfood" in integration:
    raise SystemExit("dogfood must be a separate Cargo test target")
package = metadata["packages"][0]
if package["name"] != "kit-dogfood-harness" or any(
    dependency["name"] == "kit" for dependency in package["dependencies"]
):
    raise SystemExit("dogfood harness metadata must describe a separate package with no Kit dependency")
for required in ('["repo", "status"]', '["project", "create"'):
    if required not in source:
        raise SystemExit(f"dogfood harness is missing public discovery surface: {required}")
print("dogfood harness: separate black-box package using only the Kit executable and public surfaces")
