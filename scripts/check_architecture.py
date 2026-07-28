#!/usr/bin/env python3
"""Assert the Phase 0 Cargo target and module topology."""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MODULES = {
    "agent", "api", "capabilities", "cli", "domain", "evaluation", "executor",
    "protocols", "runtime", "store", "telemetry", "verify", "web", "workspace",
}


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("assertion", choices=("binary", "modules", "path-auth"))
    args = parser.parse_args(argv)
    if args.assertion == "binary":
        result = subprocess.run(
            ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        if result.returncode:
            print(result.stderr, file=sys.stderr, end="")
            return result.returncode
        metadata = json.loads(result.stdout)
        binaries = [
            target["name"]
            for package in metadata["packages"]
            for target in package["targets"]
            if "bin" in target["kind"]
        ]
        if binaries != ["kit"]:
            raise SystemExit(f"expected exactly one Kit binary target, found {binaries}")
        print("cargo metadata: exactly 1 binary target: kit")
        return 0

    if args.assertion == "path-auth":
        path_auth = ROOT / "src/workspace/path_auth"
        syscall_module = path_auth / "unix/sys.rs"
        forbidden = re.compile(r"libc::(?:openat|syscall)\b|libc::SYS_openat2\b")
        violations = []
        for path in path_auth.rglob("*.rs"):
            if path != syscall_module and forbidden.search(path.read_text(encoding="utf-8")):
                violations.append(str(path.relative_to(ROOT)))
        if violations:
            raise SystemExit("raw path-auth opens outside unix/sys.rs: " + ", ".join(violations))

        source = syscall_module.read_text(encoding="utf-8")
        compact = re.sub(r"\s+", "", source)
        required = {
            "component flags": "letflags=requested_flags|libc::O_NOFOLLOW|libc::O_CLOEXEC;",
            "openat flags": "libc::openat(directory.as_raw_fd(),name.as_ptr(),flags)",
            "openat2 flags": "how.flags=flagsasu64;",
            "openat2 beneath": "how.resolve=libc::RESOLVE_BENEATH|libc::RESOLVE_NO_SYMLINKS;",
            "openat2 syscall": "libc::SYS_openat2",
        }
        missing = [name for name, fragment in required.items() if fragment not in compact]
        if missing:
            raise SystemExit("path-auth syscall invariants missing: " + ", ".join(missing))
        print("path_auth raw opens isolated with NOFOLLOW/CLOEXEC/BENEATH invariants")
        return 0

    source = (ROOT / "src/lib.rs").read_text(encoding="utf-8")
    source = source.replace("#[cfg(any(test, debug_assertions))]\npub mod test_support;", "")
    declarations = set(re.findall(r"^pub mod ([a-z_]+);$", source, re.M))
    directories = {path.name for path in (ROOT / "src").iterdir() if path.is_dir()}
    if declarations != MODULES or directories != MODULES:
        raise SystemExit(
            f"expected modules {sorted(MODULES)}, declarations={sorted(declarations)}, "
            f"directories={sorted(directories)}"
        )
    print("src/lib.rs and src/: exact 14 module set: " + ",".join(sorted(MODULES)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
