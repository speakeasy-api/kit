#!/usr/bin/env python3
"""Generate or validate source-controlled, local-only G00 attestations."""

import argparse
import hashlib
import json
import os
import platform
import re
import shlex
import stat
import subprocess
import sys
from collections import Counter
from datetime import date
from pathlib import Path

import yaml

from req_lint_lib.loader import discover_shards, load_registry_dir
from yaml_utils import load_yaml_file


ROOT = Path(__file__).resolve().parents[1]
ATTESTATION_DIR = ROOT / "requirements/attestations/phase0"
REPORT = ROOT / "requirements/reports/phase0-exit.md"
HEX64 = set("0123456789abcdef")
EXCLUDED_DIR_NAMES = {".git", ".superworkflow", ".evidence-tmp", ".tools", "__pycache__", "target"}
EXCLUDED_FILES = {
    REPORT.relative_to(ROOT).as_posix(),
    "requirements/reports/m001-exit.md",
    "requirements/reports/m002-exit.md",
    "requirements/reports/m003-exit.md",
    "requirements/reports/m004-exit.md",
    "requirements/registry.yaml",
    "requirements/evidence.yaml",
    "requirements/tombstones.yaml",
    "requirements/id-ledger.yaml",
    "requirements/report.md",
}
COMMANDS = {
    "EV-1.04-C-001": "python3 scripts/req_lint.py --coverage 8-1597",
    "EV-1.04-C-002": "python3 scripts/req_lint.py --aggregate",
    "EV-1.04-C-003": "python3 scripts/check_architecture.py binary",
    "EV-1.04-C-004": "cargo test --locked --no-run --all-targets",
    "EV-1.04-C-800": "python3 scripts/generate_registry.py --check",
    "EV-1.04-C-801": "cargo test --locked --test conformance req_lint::req_lint_real_conformance_corpus -- --exact",
    "EV-1.04-C-802": "cargo test --locked --test conformance req_lint::req_lint_real_conformance_corpus -- --exact",
    "EV-1.04-C-803": "python3 scripts/req_lint.py --aggregate",
    "EV-1.04-C-804": "python3 scripts/req_lint.py --aggregate",
    "EV-1.09-C-001": "sh scripts/verify_pins.sh",
    "EV-1.09-C-002": "sh scripts/verify_pins.sh",
    "EV-1.09-C-003": "sh scripts/verify_pins.sh",
    "EV-1.09-C-004": "sh scripts/verify_pins.sh",
}
FIELDS = {
    "artifact", "artifact_digest", "base_commit_sha", "candidate_identity", "environment",
    "environment_digest", "evidence_id", "evidence_job", "record_id", "run_id",
    "schema_version", "source_tree_digest", "trust_scope", "trusted_for_release", "versions",
    "workflow_ref",
}


class Dumper(yaml.SafeDumper):
    def ignore_aliases(self, data):
        return True


def sha256_bytes(value):
    return hashlib.sha256(value).hexdigest()


def file_digest(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def source_file_digest(path, relative):
    if relative.startswith("requirements/registry.d/") and relative.endswith(".yaml"):
        document = load_yaml_file(path)
        if not isinstance(document, list) or any(not isinstance(record, dict) for record in document):
            raise ValueError(f"invalid registry shard in source manifest: {relative}")
        document = [dict(record) for record in document]
        for record in document:
            for field in ("artifact_digest", "environment_digest", "versions"):
                record[field] = None
        encoded = json.dumps(document, sort_keys=True, separators=(",", ":")).encode("utf-8")
        return sha256_bytes(encoded)
    return file_digest(path)


def source_manifest(root=ROOT):
    records = []
    for directory, dirnames, filenames in os.walk(root, followlinks=False):
        directory = Path(directory)
        dirnames[:] = sorted(name for name in dirnames if name not in EXCLUDED_DIR_NAMES)
        for name in sorted(filenames):
            path = directory / name
            relative = path.relative_to(root).as_posix()
            if (
                relative in EXCLUDED_FILES
                or relative.startswith("requirements/attestations/")
                or relative.startswith("requirements/reports/m004/")
                or name == ".DS_Store"
                or name.endswith(".pyc")
            ):
                continue
            if path.is_symlink():
                mode = "120000"
                digest = sha256_bytes(os.readlink(path).encode("utf-8"))
            elif path.is_file():
                mode = "100755" if stat.S_IMODE(path.stat().st_mode) & 0o111 else "100644"
                digest = source_file_digest(path, relative)
            else:
                raise ValueError(f"unsupported source-tree entry: {relative}")
            records.append(f"{digest}  {mode}  {relative}")
    return "\n".join(sorted(records)) + "\n"


def source_tree_digest(root=ROOT):
    return sha256_bytes(source_manifest(root).encode("utf-8"))


def attestation_set_digest(paths):
    records = [
        f"{file_digest(path)}  {path.relative_to(ROOT).as_posix()}" for path in sorted(paths)
    ]
    return sha256_bytes(("\n".join(records) + "\n").encode("utf-8"))


def records():
    loaded, _ = load_registry_dir(ROOT / "requirements/registry.d", discover_shards(ROOT / "requirements/registry.d"))
    return loaded


def phase0_records():
    selected = {
        record["evidence_id"]: record
        for record in records()
        if record.get("evidence_id") in COMMANDS and record.get("latest_result") == "pass"
    }
    if set(selected) != set(COMMANDS):
        raise ValueError(
            f"passing evidence set differs from G00 commands: missing={sorted(set(COMMANDS) - set(selected))} "
            f"extra={sorted(set(selected) - set(COMMANDS))}"
        )
    return selected


def current_base_commit():
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True, capture_output=True, check=False
    )
    if result.returncode or len(result.stdout.strip()) != 40:
        raise ValueError("current Git base commit cannot be resolved")
    return result.stdout.strip()


def environment():
    commands = (["rustc", "--version"], ["cargo", "--version"], ["python3", "--version"])
    lines = []
    for command in commands:
        result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True, check=True)
        lines.append((result.stdout or result.stderr).strip())
    lines.extend(
        [
            f"{platform.system()} {platform.release()} {platform.machine()}",
            f"Cargo.lock sha256={file_digest(ROOT / 'Cargo.lock')}",
        ]
    )
    return "\n".join(lines) + "\n"


def versions(environment_text):
    lines = environment_text.splitlines()
    return {
        "cargo": lines[1].split()[1],
        "cargo_lock_sha256": lines[4].split("=", 1)[1],
        "os": lines[3],
        "python": lines[2].split()[1],
        "rustc": lines[0].split()[1],
    }


def artifact_digest(artifact):
    encoded = json.dumps(artifact, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return sha256_bytes(encoded)


FORBIDDEN_SHELL = re.compile(r"[;#|\n\r<>`$()]")
ENVIRONMENT_ASSIGNMENT = re.compile(r"[A-Z_][A-Z0-9_]*=.*", re.DOTALL)
ALLOWED_STEP_ENVIRONMENT = {"KIT_M004_REPORT_DIR"}
ALLOWED_STEP_ENVIRONMENT_VALUES = {
    "KIT_M004_REPORT_DIR": "requirements/reports/m004/source-semantics",
}
STEP_FIELDS = {
    "argv", "environment", "exit_code", "output", "output_digest", "proof_kind", "test_count"
}
JSON_SCHEMA_PAIRS = {
    (
        "eval/preregistration/schema/v1/preregistration.schema.json",
        "requirements/reports/m004/source-semantics/preregistration.json",
    ),
    (
        "eval/preregistration/schema/v1/registration.schema.json",
        "requirements/reports/m004/source-semantics/registered-preregistration.json",
    ),
    (
        "eval/reports/schema/v1/statistical-report.schema.json",
        "requirements/reports/m004/source-semantics/statistical-report.json",
    ),
}
PROOF_COMMANDS = {
    ("python3", "scripts/req_lint.py", "--coverage", "8-1597"): "requirement_lint",
    ("python3", "scripts/req_lint.py", "--aggregate"): "requirement_lint",
    ("python3", "scripts/check_architecture.py", "binary"): "architecture_check",
    ("python3", "scripts/generate_registry.py", "--check"): "projection_check",
    ("sh", "scripts/verify_pins.sh"): "pin_check",
    ("python3", "-m", "openapi_spec_validator", "docs/api/openapi.yaml"): "openapi_schema",
    ("python3", "scripts/check_dogfood_harness.py"): "dogfood_boundary",
    **{
        ("check-jsonschema", "--schemafile", schema, target): "json_schema"
        for schema, target in JSON_SCHEMA_PAIRS
    },
    ("cargo", "test", "--locked", "--no-run", "--all-targets"): "cargo_compile",
}


def proof_kind(argv):
    try:
        return PROOF_COMMANDS[tuple(argv)]
    except KeyError as error:
        raise ValueError(f"unrecognized proof-only command: {argv!r}") from error


def command_steps(command):
    if not isinstance(command, str) or not command or FORBIDDEN_SHELL.search(command):
        raise ValueError("command contains forbidden shell composition")
    try:
        tokens = shlex.split(command, posix=True)
    except ValueError as error:
        raise ValueError(f"command is not valid narrow shell grammar: {error}") from error
    groups = []
    current = []
    for token in tokens:
        if token == "&&":
            if not current:
                raise ValueError("command contains an empty step")
            groups.append(current)
            current = []
        elif "&&" in token:
            raise ValueError("command uses unrecognized shell composition")
        else:
            current.append(token)
    if not current:
        raise ValueError("command contains an empty step")
    groups.append(current)

    steps = []
    for tokens in groups:
        environment = {}
        while tokens and ENVIRONMENT_ASSIGNMENT.fullmatch(tokens[0]):
            name, value = tokens.pop(0).split("=", 1)
            if name not in ALLOWED_STEP_ENVIRONMENT:
                raise ValueError(f"command uses unrecognized environment override: {name}")
            if value != ALLOWED_STEP_ENVIRONMENT_VALUES[name]:
                raise ValueError(f"command uses unrecognized {name} target: {value}")
            environment[name] = value
        if not tokens:
            raise ValueError("environment assignment has no command")
        if tokens[:2] == ["cargo", "test"] and "--no-run" not in tokens:
            kind = "cargo_test"
        else:
            kind = proof_kind(tokens)
        steps.append({"argv": tokens, "environment": environment, "kind": kind})
    return steps


def validate_proof(kind, argv, output):
    if proof_kind(argv) != kind:
        raise ValueError(f"{kind!r} proof command is not allowlisted")

    match = None
    if kind == "requirement_lint" and "--coverage" in argv:
        match = re.fullmatch(r"(?P<unmapped>[0-9]+) unmapped\n?", output)
        valid = bool(match and int(match["unmapped"]) == 0)
    elif kind == "requirement_lint":
        match = re.fullmatch(
            r"(?P<records>[0-9]+) record\(s\) across (?P<shards>[0-9]+) shard\(s\), "
            r"(?P<findings>[0-9]+) finding\(s\)\n?",
            output,
        )
        valid = bool(
            match
            and int(match["records"]) == len(records())
            and int(match["shards"]) == len(discover_shards(ROOT / "requirements/registry.d"))
            and int(match["findings"]) == 0
        )
    elif kind == "architecture_check":
        match = re.fullmatch(r"cargo metadata: exactly (?P<targets>[0-9]+) binary target: kit\n?", output)
        valid = bool(match and int(match["targets"]) == 1)
    elif kind == "projection_check":
        match = re.fullmatch(
            r"generated (?P<projections>[0-9]+) projections from (?P<records>[0-9]+) records\n?",
            output,
        )
        valid = bool(
            match
            and int(match["projections"]) == 5
            and int(match["records"]) == len(records())
        )
    elif kind == "pin_check":
        match = re.fullmatch(
            rf"pin manifest verified \(normal mode\): {re.escape(str(ROOT / 'docs/compatibility/build-manifest.yaml'))}\n"
            r"unpinned\.mutable_protocols=(?P<protocols>[0-9]+)\n"
            r"unpinned\.mutable_datasets=(?P<datasets>[0-9]+)\n"
            r"unpinned\.mutable_models=(?P<models>[0-9]+)\n"
            r"unpinned\.mutable_harnesses=(?P<harnesses>[0-9]+)\n?",
            output,
        )
        valid = bool(match and all(int(value) == 0 for value in match.groupdict().values()))
    elif kind == "openapi_schema":
        valid = output in {"docs/api/openapi.yaml: OK", "docs/api/openapi.yaml: OK\n"}
    elif kind == "dogfood_boundary":
        valid = output in {
            "dogfood harness: separate black-box package using only the Kit executable and public surfaces",
            "dogfood harness: separate black-box package using only the Kit executable and public surfaces\n",
        }
    elif kind == "json_schema":
        valid = (
            tuple(argv[2:]) in JSON_SCHEMA_PAIRS
            and all((ROOT / value).is_file() for value in argv[2:])
            and output in {"ok -- validation done", "ok -- validation done\n"}
        )
    elif kind == "cargo_compile":
        valid = bool(re.search(r"^  Executable .+ \([^\n]+\)$", output, re.MULTILINE))
    else:
        valid = False
    if not valid:
        raise ValueError(f"{kind!r} proof validator rejected command output")


def validate_artifact(command, artifact):
    if not isinstance(artifact, dict) or set(artifact) != {"command", "evidence_id", "exit_code", "steps"}:
        raise ValueError("artifact fields differ from structured command schema")
    if artifact.get("command") != command or artifact.get("exit_code") != 0:
        raise ValueError("artifact is not a passing exact command")
    manifests = command_steps(command)
    steps = artifact.get("steps")
    if not isinstance(steps, list) or len(steps) != len(manifests):
        raise ValueError("artifact step set differs from command manifest")
    total = 0
    saw_test = False
    for index, (step, manifest) in enumerate(zip(steps, manifests), 1):
        if not isinstance(step, dict) or set(step) != STEP_FIELDS:
            raise ValueError(f"step {index} fields differ from structured command schema")
        if step.get("argv") != manifest["argv"] or step.get("environment") != manifest["environment"]:
            raise ValueError(f"step {index} argv/environment differs from command manifest")
        if step.get("exit_code") != 0 or not isinstance(step.get("output"), str):
            raise ValueError(f"step {index} did not retain a successful command output")
        if step.get("output_digest") != sha256_bytes(step["output"].encode("utf-8")):
            raise ValueError(f"step {index} output digest is invalid")
        if manifest["kind"] == "cargo_test":
            counts = [int(value) for value in re.findall(r"test result: ok\. ([0-9]+) passed;", step["output"])]
            if not counts or any(count == 0 for count in counts):
                raise ValueError(f"cargo test step {index} must independently report >0 passed: {counts}")
            count = sum(counts)
            if step.get("test_count") != count or step.get("proof_kind") is not None:
                raise ValueError(f"cargo test step {index} retained invalid test metadata")
            total += count
            saw_test = True
        else:
            if step.get("test_count") is not None or step.get("proof_kind") != manifest["kind"]:
                raise ValueError(f"proof step {index} retained invalid proof metadata")
            validate_proof(manifest["kind"], manifest["argv"], step["output"])
    return total if saw_test else None


def run_command(evidence_id, command):
    retained = []
    for index, manifest in enumerate(command_steps(command), 1):
        result = subprocess.run(
            manifest["argv"],
            cwd=ROOT,
            env={**os.environ, **manifest["environment"]},
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        if result.returncode:
            raise ValueError(
                f"{evidence_id} step {index} failed ({manifest['argv']!r}):\n{result.stdout}"
            )
        output = result.stdout
        if manifest["kind"] == "cargo_test":
            counts = [int(value) for value in re.findall(r"test result: ok\. ([0-9]+) passed;", output)]
            test_count = sum(counts) if counts and all(count > 0 for count in counts) else 0
            proof = None
        else:
            test_count = None
            proof = manifest["kind"]
        retained.append({
            "argv": manifest["argv"],
            "environment": manifest["environment"],
            "exit_code": result.returncode,
            "output": output,
            "output_digest": sha256_bytes(output.encode("utf-8")),
            "proof_kind": proof,
            "test_count": test_count,
        })
    artifact = {"command": command, "evidence_id": evidence_id, "exit_code": 0, "steps": retained}
    validate_artifact(command, artifact)
    return artifact


def require_rebind_source(document, current_digest, label):
    if document.get("source_tree_digest") != current_digest:
        raise ValueError(f"{label}: retained output is from a different source digest; actual rerun required")


def dump_yaml(data):
    return yaml.dump(
        data, Dumper=Dumper, allow_unicode=True, default_flow_style=False, sort_keys=False, width=1000
    )


def update_records(results, environment_digest, version_data):
    by_id = {record["evidence_id"]: record for record in phase0_records().values()}
    paths = sorted((ROOT / "requirements/registry.d").glob("*.yaml"))
    for path in paths:
        document = load_yaml_file(path)
        changed = False
        for record in document:
            evidence_id = record.get("evidence_id")
            if evidence_id not in results:
                continue
            record["artifact_digest"] = results[evidence_id]["artifact_digest"]
            record["environment_digest"] = environment_digest
            record["versions"] = version_data
            changed = True
        if changed:
            path.write_text(dump_yaml(document), encoding="utf-8")
    if set(by_id) != set(results):
        raise ValueError("evidence result set does not match passing records")


def run_jobs():
    results = {}
    for evidence_id, command in COMMANDS.items():
        artifact = run_command(evidence_id, command)
        results[evidence_id] = {"artifact": artifact, "artifact_digest": artifact_digest(artifact)}
        print(f"{evidence_id}: pass", flush=True)
    return results


def write_report(documents, tree_digest, set_digest):
    all_records = records()
    classes = Counter(record["record_class"] for record in all_records)
    statuses = Counter(record["status"] for record in all_records)
    inventory = load_yaml_file(ROOT / "requirements/source-inventory.yaml")
    optional = load_yaml_file(ROOT / "requirements/policy/optional.yaml")
    manifest = load_yaml_file(ROOT / "docs/compatibility/build-manifest.yaml")
    pins = {record["id"]: record["value"] for record in manifest["pins"]}
    test_jobs = [
        document
        for document in documents
        if validate_artifact(document["artifact"]["command"], document["artifact"]) is not None
    ]
    projection_paths = [
        ROOT / "requirements/registry.yaml",
        ROOT / "requirements/evidence.yaml",
        ROOT / "requirements/tombstones.yaml",
        ROOT / "requirements/id-ledger.yaml",
        ROOT / "requirements/report.md",
    ]
    lines = [
        "# Phase 0 Exit Report",
        "",
        "- Gate: `G00`",
        f"- Run date: `{date.today().isoformat()}`",
        f"- Result: **PASS (local G00, {len(documents)}/{len(documents)} implemented records)**",
        f"- Candidate identity: `worktree:{tree_digest}`",
        f"- Source-tree SHA-256: `{tree_digest}`",
        f"- Local attestation-set SHA-256: `{set_digest}`",
        f"- Test jobs: **{len(test_jobs)}/{len(test_jobs)} selected at least one passing test**",
        f"- Non-test proof jobs: **{len(documents) - len(test_jobs)}/{len(documents) - len(test_jobs)} produced authoritative output**",
        "- Release result: **EXPECTED FAIL**",
        "",
        "The candidate identity is intentionally the current worktree digest, not the docs-only",
        f"base commit `{current_base_commit()}`. The digest input is the sorted manifest",
        "`<sha256><two spaces><mode><two spaces><path>` over every source-tree file. It",
        "excludes `.git/`, `.superworkflow/`, `.evidence-tmp/`, `.tools/`, every `target/`",
            "and `__pycache__/`, `*.pyc`, `.DS_Store`, `requirements/attestations/**`, generated",
            "projections/milestone exit reports, and retained M004 report artifacts. Registry shards are",
            "hashed canonically with only run-derived artifact/environment/version fields nulled, avoiding",
            "an evidence self-reference while retaining all requirement and command semantics. Source-controlled attestations",
            "are local-only and rejected for release.",
        "The legitimate non-test jobs are compile-only `EV-1.04-C-004`; governance validators",
        "`EV-1.04-C-001`, `EV-1.04-C-002`, `EV-1.04-C-003`, `EV-1.04-C-800`,",
        "`EV-1.04-C-803`, `EV-1.04-C-804`; and pin validators `EV-1.09-C-001` through",
        "`EV-1.09-C-004`. The compile-only job must report a built test executable; every other",
        "non-test job must exit zero and satisfy its allowlisted proof validator.",
        "",
        "## Registry",
        "",
        f"- Records: {len(all_records)}",
        f"- Shards: {len(discover_shards(ROOT / 'requirements/registry.d'))}",
        f"- Requirements: {classes['requirement']}",
        f"- Promises: {classes['promise']}",
        f"- Decisions: {classes['decision']}",
        f"- Risks: {classes['risk']}",
        f"- Implemented records: {statuses['implemented']}",
        f"- Proposed records: {statuses['proposed']}",
        f"- Active records: {statuses['active']}",
        f"- Optional mechanisms: {len(optional['mechanisms'])}",
        f"- Inventory atoms: {len(inventory['atoms'])}",
        f"- RFC coverage: {inventory['coverage']['covered_nonblank_lines']}/{inventory['nonblank_line_count']} nonblank lines, {inventory['coverage']['uncovered_nonblank_lines']} unmapped",
        "",
        "## Generated Projections",
        "",
        "| Projection | SHA-256 |",
        "| --- | --- |",
    ]
    lines.extend(
        f"| `{path.relative_to(ROOT).as_posix()}` | `{file_digest(path)}` |" for path in projection_paths
    )
    lines.extend(
        [
            "",
            "## Evidence Jobs",
            "",
            "| Record | Evidence | Job | Command | Artifact SHA-256 |",
            "| --- | --- | --- | --- | --- |",
        ]
    )
    for document in sorted(documents, key=lambda item: item["evidence_id"]):
        command = document["artifact"]["command"].replace("|", "\\|")
        lines.append(
            f"| `{document['record_id']}` | `{document['evidence_id']}` | `{document['evidence_job']}` | "
            f"`{command}` | `{document['artifact_digest']}` |"
        )
    lines.extend(
        [
            "",
            "Each row binds the exact command, exit code, captured output, artifact digest,",
            "environment text/digest, record ID, evidence ID/job, and worktree identity.",
            "",
            "## Reproducibility",
            "",
            f"- Builder: `{pins['build.supported_dev_platforms']}`",
            "- Two independent source copies and target directories built with `SOURCE_DATE_EPOCH=0` and `CARGO_INCREMENTAL=0`.",
            f"- Both binary SHA-256 values: `{pins['build.reproducible_artifact_sha256']}`; `cmp` exited 0.",
            f"- Reproducible environment SHA-256: `{pins['build.reproducible_environment_sha256']}`.",
            f"- Cargo.lock SHA-256: `{pins['build.cargo_lock_sha256']}`.",
            f"- Build-input closure SHA-256: `{pins['build.input_closure_sha256']}`.",
            "- `closure_manifest_recorded_post_run=true`: the retained artifact predates embedded closure evidence.",
            "  Its current closure is bound by pre-artifact mtimes and byte-identical retained source copies; future runs record the closure before building and embed its digest.",
            "- Vendored Runlet and Agentkit tests ran with external target directories before snapshot verification.",
            "",
            "## Release Gate",
            "",
            "Strict release remains closed by unresolved product milestones, pending optional",
            "decisions, non-green dashboards, blocked release pins, the absence of a distinct",
            "ancestor baseline containing a registry, and the absence of external trusted",
            "commit-bound attestations. Release mode rejects this `worktree:` identity and all",
            "source-controlled attestations.",
            "",
        ]
    )
    REPORT.write_text("\n".join(lines), encoding="utf-8")


def generate():
    initial = subprocess.run(
        ["python3", "scripts/generate_registry.py"], cwd=ROOT, text=True, capture_output=True, check=False
    )
    if initial.returncode:
        raise ValueError(initial.stdout + initial.stderr)
    results = run_jobs()
    environment_text = environment()
    environment_digest = sha256_bytes(environment_text.encode("utf-8"))
    version_data = versions(environment_text)
    update_records(results, environment_digest, version_data)
    generated = subprocess.run(
        ["python3", "scripts/generate_registry.py"], cwd=ROOT, text=True, capture_output=True, check=False
    )
    if generated.returncode:
        raise ValueError(generated.stdout + generated.stderr)
    tree_digest = source_tree_digest()
    identity = f"worktree:{tree_digest}"
    base_commit = current_base_commit()
    phase_records = phase0_records()
    ATTESTATION_DIR.mkdir(parents=True, exist_ok=True)
    for path in ATTESTATION_DIR.glob("*.json"):
        path.unlink()
    documents = []
    for evidence_id, result in results.items():
        record = phase_records[evidence_id]
        document = {
            "artifact": result["artifact"],
            "artifact_digest": result["artifact_digest"],
            "base_commit_sha": base_commit,
            "candidate_identity": identity,
            "environment": environment_text,
            "environment_digest": environment_digest,
            "evidence_id": evidence_id,
            "evidence_job": record["evidence_job"],
            "record_id": record["id"],
            "run_id": f"local-{date.today().strftime('%Y%m%d')}-g00",
            "schema_version": 3,
            "source_tree_digest": tree_digest,
            "trust_scope": "local_g00_only",
            "trusted_for_release": False,
            "versions": version_data,
            "workflow_ref": "local://g00-worktree-attestation",
        }
        path = ATTESTATION_DIR / f"{evidence_id}.json"
        path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        documents.append(document)
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    write_report(documents, tree_digest, attestation_set_digest(paths))
    return validate()


def rebind():
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    if len(paths) != len(COMMANDS):
        raise ValueError(f"expected {len(COMMANDS)} retained attestations, found {len(paths)}")
    retained = {}
    environment_text = None
    tree_digest = source_tree_digest()
    for path in paths:
        document = json.loads(path.read_text(encoding="utf-8"))
        require_rebind_source(document, tree_digest, path)
        evidence_id = document.get("evidence_id")
        artifact = document.get("artifact")
        if isinstance(artifact, dict) and artifact.get("evidence_id") != evidence_id:
            raise ValueError(f"{path}: artifact evidence_id does not match attestation")
        if evidence_id not in COMMANDS or not isinstance(artifact, dict):
            raise ValueError(f"{path}: retained artifact is invalid")
        validate_artifact(COMMANDS[evidence_id], artifact)
        retained[evidence_id] = {"artifact": artifact, "artifact_digest": artifact_digest(artifact)}
        if environment_text is None:
            environment_text = document.get("environment")
        elif environment_text != document.get("environment"):
            raise ValueError("retained attestation environments differ")
    if set(retained) != set(COMMANDS):
        raise ValueError("retained evidence set differs from G00 commands")

    identity = f"worktree:{tree_digest}"
    base_commit = current_base_commit()
    phase_records = phase0_records()
    version_data = versions(environment_text)
    environment_digest = sha256_bytes(environment_text.encode("utf-8"))
    for path in paths:
        path.unlink()
    documents = []
    for evidence_id, result in retained.items():
        record = phase_records[evidence_id]
        document = {
            "artifact": result["artifact"],
            "artifact_digest": result["artifact_digest"],
            "base_commit_sha": base_commit,
            "candidate_identity": identity,
            "environment": environment_text,
            "environment_digest": environment_digest,
            "evidence_id": evidence_id,
            "evidence_job": record["evidence_job"],
            "record_id": record["id"],
            "run_id": f"local-{date.today().strftime('%Y%m%d')}-g00",
            "schema_version": 3,
            "source_tree_digest": tree_digest,
            "trust_scope": "local_g00_only",
            "trusted_for_release": False,
            "versions": version_data,
            "workflow_ref": "local://g00-worktree-attestation",
        }
        (ATTESTATION_DIR / f"{evidence_id}.json").write_text(
            json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        documents.append(document)
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    write_report(documents, tree_digest, attestation_set_digest(paths))
    return validate()


def validate(historical=False):
    errors = []
    tree_digest = source_tree_digest()
    identity = f"worktree:{tree_digest}"
    phase_records = phase0_records()
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    if len(paths) != len(COMMANDS):
        errors.append(f"expected {len(COMMANDS)} attestations, found {len(paths)}")
    seen = set()
    documents = []
    stored_bindings = None
    for path in paths:
        try:
            def unique_object(pairs):
                keys = [key for key, _ in pairs]
                if len(keys) != len(set(keys)):
                    raise ValueError("duplicate JSON key")
                return dict(pairs)

            document = json.loads(
                path.read_text(encoding="utf-8"), object_pairs_hook=unique_object
            )
            if not isinstance(document, dict):
                raise ValueError("top level must be an object")
        except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
            errors.append(f"{path}: invalid JSON: {error}")
            continue
        evidence_id = document.get("evidence_id")
        if set(document) != FIELDS:
            errors.append(f"{path}: fields differ from schema v3")
            continue
        if evidence_id in seen or evidence_id not in phase_records:
            errors.append(f"{path}: duplicate or unknown evidence_id {evidence_id!r}")
            continue
        seen.add(evidence_id)
        if path.stem != evidence_id:
            errors.append(f"{path}: filename does not match evidence_id {evidence_id!r}")
        record = phase_records[evidence_id]
        artifact = document.get("artifact")
        if isinstance(artifact, dict) and artifact.get("evidence_id") != evidence_id:
            errors.append(f"{path}: artifact evidence_id does not match attestation")
        source_digest = document.get("source_tree_digest")
        stored_identity = f"worktree:{source_digest}"
        expected = artifact
        try:
            validate_artifact(
                artifact.get("command") if historical and isinstance(artifact, dict) else COMMANDS[evidence_id],
                artifact,
            )
        except (TypeError, ValueError) as error:
            errors.append(f"{path}: invalid structured local evidence: {error}")
        checks = {
            "schema_version": 3,
            "candidate_identity": stored_identity if historical else identity,
            "record_id": record["id"],
            "evidence_job": record["evidence_job"],
            "artifact": expected,
            "artifact_digest": artifact_digest(expected),
            "environment_digest": sha256_bytes(str(document.get("environment", "")).encode("utf-8")),
            "versions": record.get("versions"),
            "trust_scope": "local_g00_only",
            "trusted_for_release": False,
            "workflow_ref": "local://g00-worktree-attestation",
        }
        if not historical:
            checks["source_tree_digest"] = tree_digest
            checks["base_commit_sha"] = current_base_commit()
        for field, value in checks.items():
            if document.get(field) != value:
                scope = "stored set/record/command" if historical else "current source/record/command"
                errors.append(f"{path}: {field} is not bound to the {scope}")
        if not isinstance(source_digest, str) or len(source_digest) != 64 or set(source_digest) - HEX64:
            errors.append(f"{path}: source_tree_digest is not a SHA-256 digest")
        base_commit = document.get("base_commit_sha")
        if not isinstance(base_commit, str) or len(base_commit) != 40 or set(base_commit) - HEX64:
            errors.append(f"{path}: base_commit_sha is not a Git SHA")
        try:
            stored_versions = versions(document.get("environment"))
        except (AttributeError, IndexError):
            stored_versions = None
        if document.get("versions") != stored_versions:
            errors.append(f"{path}: versions are not bound to the stored environment")
        if document.get("artifact_digest") != record.get("artifact_digest"):
            errors.append(f"{path}: artifact digest differs from {record['id']}")
        if document.get("environment_digest") != record.get("environment_digest"):
            errors.append(f"{path}: environment digest differs from {record['id']}")
        if not document.get("run_id") or not document.get("versions") or not document.get("base_commit_sha"):
            errors.append(f"{path}: provenance metadata is incomplete")
        bindings = {
            field: document.get(field)
            for field in (
                "base_commit_sha", "candidate_identity", "environment", "environment_digest",
                "run_id", "source_tree_digest", "versions",
            )
        }
        if stored_bindings is None:
            stored_bindings = bindings
        elif bindings != stored_bindings:
            errors.append(f"{path}: provenance differs within the stored attestation set")
        documents.append(document)
    if seen != set(COMMANDS):
        errors.append(f"attestation evidence set mismatch: {sorted(set(COMMANDS) - seen)}")
    if paths:
        digest = attestation_set_digest(paths)
        report = REPORT.read_text(encoding="utf-8") if REPORT.is_file() else ""
        if f"Local attestation-set SHA-256: `{digest}`" not in report:
            errors.append("phase0 report does not bind the stored attestation-set digest")
        report_identity = stored_bindings["candidate_identity"] if historical and stored_bindings else identity
        report_source = stored_bindings["source_tree_digest"] if historical and stored_bindings else tree_digest
        if f"Candidate identity: `{report_identity}`" not in report:
            errors.append("phase0 report does not bind the attestation candidate identity")
        if f"Source-tree SHA-256: `{report_source}`" not in report:
            errors.append("phase0 report does not bind the attestation source-tree digest")
        for document in documents:
            command = document["artifact"]["command"].replace("|", "\\|")
            row = (
                f"| `{document['record_id']}` | `{document['evidence_id']}` | `{document['evidence_job']}` | "
                f"`{command}` | `{document['artifact_digest']}` |"
            )
            if row not in report:
                errors.append(f"phase0 report does not bind {document['evidence_id']}")
    if errors:
        for error in errors:
            print(f"G00 attestation error: {error}", file=sys.stderr)
        return 1
    if historical:
        stored_identity = stored_bindings["candidate_identity"]
        print(
            f"G00 historical attestations valid: {len(paths)} records, status=stale_for_current_gate, "
            f"candidate={stored_identity}, set_sha256={attestation_set_digest(paths)}; "
            "historical mode does not validate the current tree"
        )
    else:
        print(
            f"G00 local attestations valid: {len(paths)} records, status=current, candidate={identity}, "
            f"set_sha256={attestation_set_digest(paths)}"
        )
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true", help="run all G00 jobs and replace local attestations/report")
    mode.add_argument("--rebind", action="store_true", help="bind retained passing output to the current tree")
    mode.add_argument(
        "--historical", action="store_true",
        help="audit the stored attestation set without asserting current-tree validity",
    )
    args = parser.parse_args(argv)
    try:
        return generate() if args.write else rebind() if args.rebind else validate(historical=args.historical)
    except (KeyError, OSError, TypeError, ValueError, yaml.YAMLError) as error:
        print(f"G00 attestation error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
