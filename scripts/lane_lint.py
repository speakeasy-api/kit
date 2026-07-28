#!/usr/bin/env python3
"""Validate the fixed CI lane manifest set without external YAML packages."""

from pathlib import Path
import re
import shutil
import sys
import tempfile


REQUIRED_COMMANDS = {
    "fmt": "cargo fmt --package kit -- --check",
    "lint": "cargo clippy --locked --all-targets -- -D warnings",
    "unit": "cargo test --locked --lib --bins",
    "integration": "cargo test --locked --test conformance --test integration",
    "req-lint": "python3 scripts/req_lint.py --aggregate",
    "schema-compat": "cargo test --locked --test conformance eval_manifest",
    "fault": "cargo test --locked --test fault",
    "adversarial": "cargo test --locked --test adversarial",
    "reproducible-build": "python3 scripts/reproducible_build.py",
    "licenses": "cargo deny check licenses",
    "vuln-scan": "cargo audit --deny warnings",
    "evidence-report": "python3 scripts/req_lint.py --aggregate",
}
REQUIRED_KEYS = {"schema_version", "name", "commands"}
KEY_RE = re.compile(r"^([a-z_]+):(.*)$")


def parse_manifest(path):
    values = {}
    commands = []
    active_list = None
    problems = []

    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        return {}, [], [f"{path}: cannot read manifest: {error}"]

    for number, line in enumerate(lines, 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line.startswith("  - "):
            command = line[4:].strip()
            if active_list != "commands":
                problems.append(f"{path}:{number}: list item outside commands")
            elif not command:
                problems.append(f"{path}:{number}: empty command")
            elif command in commands:
                problems.append(f"{path}:{number}: duplicate command")
            else:
                commands.append(command)
            continue
        if line[0].isspace():
            problems.append(f"{path}:{number}: unsupported indentation")
            continue

        match = KEY_RE.fullmatch(line)
        if not match:
            problems.append(f"{path}:{number}: expected a top-level key")
            active_list = None
            continue
        key, raw_value = match.groups()
        value = raw_value.strip()
        if key not in REQUIRED_KEYS:
            problems.append(f"{path}:{number}: unknown key {key!r}")
        elif key in values:
            problems.append(f"{path}:{number}: duplicate key {key!r}")
        elif key == "commands":
            if value:
                problems.append(f"{path}:{number}: commands must be a block list")
            values[key] = None
        elif not value:
            problems.append(f"{path}:{number}: empty value for {key!r}")
            values[key] = value
        else:
            values[key] = value
        active_list = key if key == "commands" else None

    missing = REQUIRED_KEYS - values.keys()
    if missing:
        problems.append(f"{path}: missing keys: {', '.join(sorted(missing))}")
    if not commands:
        problems.append(f"{path}: commands must contain at least one command")
    return values, commands, problems


def validate(directory):
    problems = []
    if not directory.is_dir():
        return [f"{directory}: lane directory not found"]

    paths = sorted(directory.glob("*.yaml"))
    actual = {path.stem for path in paths}
    expected = set(REQUIRED_COMMANDS)
    if actual != expected:
        missing = expected - actual
        extra = actual - expected
        if missing:
            problems.append(f"missing lanes: {', '.join(sorted(missing))}")
        if extra:
            problems.append(f"unexpected lanes: {', '.join(sorted(extra))}")
    if len(paths) != len(REQUIRED_COMMANDS):
        problems.append(
            f"{directory}: expected {len(REQUIRED_COMMANDS)} manifests, found {len(paths)}"
        )

    for path in paths:
        values, commands, manifest_problems = parse_manifest(path)
        problems.extend(manifest_problems)
        lane = path.stem
        if values.get("schema_version") != "1":
            problems.append(f"{path}: schema_version must be 1")
        if values.get("name") != lane:
            problems.append(f"{path}: name must match filename stem {lane!r}")
        required = REQUIRED_COMMANDS.get(lane)
        if required and required not in commands:
            problems.append(f"{path}: missing required command: {required}")
        if lane == "fmt" and any(
            re.search(r"(?:^|\s)--all(?:\s|$)", command) for command in commands
        ):
            problems.append(f"{path}: fmt lane must not use --all")
    return problems


def self_test(directory):
    problems = validate(directory)
    if problems:
        return problems

    with tempfile.TemporaryDirectory(prefix="lane-lint-") as temporary:
        root = Path(temporary)
        missing_dir = root / "missing"
        shutil.copytree(directory, missing_dir)
        (missing_dir / "fmt.yaml").unlink()
        if not validate(missing_dir):
            problems.append("negative test failed: missing lane was accepted")

        content_dir = root / "content"
        shutil.copytree(directory, content_dir)
        path = content_dir / "unit.yaml"
        text = path.read_text(encoding="utf-8")
        path.write_text(
            text.replace(REQUIRED_COMMANDS["unit"], "true", 1), encoding="utf-8"
        )
        if not validate(content_dir):
            problems.append("negative test failed: invalid lane content was accepted")

        path = content_dir / "fmt.yaml"
        text = path.read_text(encoding="utf-8")
        path.write_text(
            text.replace(REQUIRED_COMMANDS["fmt"], "cargo fmt --all -- --check", 1),
            encoding="utf-8",
        )
        if not any("fmt lane must not use --all" in problem for problem in validate(content_dir)):
            problems.append("negative test failed: fmt --all was accepted")
    return problems


def main(argv):
    default_directory = Path(__file__).resolve().parents[1] / "ci" / "lanes"
    if argv == ["--self-test"]:
        problems = self_test(default_directory)
        success = "12 lane manifests valid; missing lane and content rejected"
    elif len(argv) <= 1 and (not argv or argv[0] != "--self-test"):
        directory = Path(argv[0]) if argv else default_directory
        problems = validate(directory)
        success = "12 lane manifests valid"
    else:
        print(f"usage: {Path(sys.argv[0]).name} [lane-directory|--self-test]", file=sys.stderr)
        return 2

    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        return 1
    print(success)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
