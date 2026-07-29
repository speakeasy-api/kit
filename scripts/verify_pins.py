#!/usr/bin/env python3
"""Strict validation for the build pin manifest and its repository evidence."""

import argparse
import ast
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
from urllib.parse import urlparse

try:
    from jsonschema import Draft202012Validator
except ModuleNotFoundError as error:
    raise SystemExit(
        f"missing CI dependency {error.name!r}; install requirements/ci-requirements.lock"
    ) from error

from yaml_utils import YamlLoadError, load_yaml_text as load_governance_yaml


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "docs/compatibility/build-manifest.yaml"
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
SEMVER = re.compile(r"^[0-9]+\.[0-9]+(?:\.[0-9]+)?$")
PIN_ID = re.compile(r"^[a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+$")
BLOCKER = re.compile(
    r"^(?P<id>(?:BLK|EXT)-[0-9]{2})@(?P<path>docs/[A-Za-z0-9._/-]+\.md)#(?P<anchor>[a-z0-9._-]+)$"
)
FLOATING = re.compile(r"(?:^|[=;:/ ])(?:latest|main|master|stable|nightly|head)(?:$|[=;:/ ])", re.I)

REQUIRED_IDS = {
    "rust.toolchain", "rust.agentkit_minimum", "agentkit.release", "agentkit.commit",
    "agentkit.tree", "agentkit.snapshot_sha256", "agentkit.checkout_clean",
    "agentkit.lock_sha256", "agentkit.features", "runlet.registry_version",
    "runlet.registry_checksum", "runlet.snapshot_sha256", "runlet.application_source",
    "protocol.acp.wire", "protocol.acp.direct_crate", "protocol.acp.tokio_helper",
    "protocol.acp.single_linked_version", "protocol.a2a.wire", "protocol.mcp.revision",
    "protocol.mcp.rmcp_crate", "protocol.mcp.application", "schema.json.dialect",
    "toon.spec", "toon.spec_commit", "toon.spec_tarball_sha256",
    "toon.fixture_manifest_sha256", "toon.serde_toon2", "toon.conformance",
    "grammar.runtime", "grammar.languages", "grammar.queries",
    "structural.ast_grep_core", "structural.ast_grep_language", "lsp.protocol",
    "lsp.position_encoding", "lsp.servers", "scip.schema", "scip.index",
    "harness.swe_bench_verified", "harness.swe_bench_multilingual",
    "harness.swe_bench_live", "harness.swe_bench_multimodal", "harness.terminal_bench_2_1",
    "image.swe_bench_verified", "image.swe_bench_multilingual", "image.swe_bench_live",
    "image.swe_bench_multimodal", "image.terminal_bench_2_1", "image.capacity_bytes",
    "tool.check_jsonschema", "tool.openapi_spec_validator", "tool.cargo_public_api",
    "tool.act", "tool.cargo_metadata", "tool.cargo_tree", "build.supported_dev_platforms",
    "build.runner_image", "build.action_checkout", "build.cargo_lock_sha256",
    "build.reproducible_artifact_sha256", "build.reproducible_environment_sha256",
    "build.input_closure_sha256",
}
REQUIRED_LANES = {
    "fmt", "lint", "unit", "integration", "req-lint", "schema-compat", "fault",
    "adversarial", "reproducible-build", "licenses", "vuln-scan", "evidence-report",
}
GOVERNANCE_COMMANDS = {
    "python3 scripts/lane_lint.py --self-test",
    "sh scripts/verify_pins.sh --self-test",
    "sh scripts/lint_threat_model.sh",
    "python3 scripts/dashboard_lint.py",
    "python3 scripts/generate_registry.py --check",
    "python3 scripts/validate_g00_attestations.py",
    "python3 scripts/validate_milestone.py --self-test",
    "python3 scripts/check_architecture.py binary",
    "python3 scripts/check_architecture.py modules",
    "python3 scripts/req_lint.py --aggregate",
    "python3 scripts/req_lint.py --coverage 8-1597",
    "cargo test --locked --test conformance req_lint",
}
WORKFLOW_JOBS = {
    "m003-external", "m003-platform-source", "m004-dogfood", "m004-stats",
    "m004-attestations", "lane", "release-validate", "publish",
}
M003_EXTERNAL_COMMANDS = (
    "cargo test --locked --test adversarial trial_grader_access::production_helper_denies_agent_grader_input_reads_and_writes -- --ignored --exact",
    "cargo test --locked --test adversarial trial_grader_access::one_thousand_production_helper_trials_have_fresh_attested_writable_leases -- --ignored --exact",
    "cargo test --locked --test conformance container_limits::explicitly_requested_actual_bounds_require_all_external_evidence -- --ignored --exact",
    "cargo test --locked --test conformance container_limits::formatter_measurements_are_all_or_nothing_helper_evidence -- --exact",
    "cargo test --locked --test conformance edit_format::formatter_rejects_requested_provenance_that_differs_from_measured_bytes -- --exact",
    "cargo test --locked --test conformance edit_format::formatter_rejects_absent_authoritative_measurements -- --exact",
)
PROTECTED_WORKFLOW_IF = (
    "github.event_name == 'workflow_dispatch' && github.ref == "
    "format('refs/heads/{0}', github.event.repository.default_branch) && github.workflow_ref == "
    "format('{0}/.github/workflows/ci.yaml@refs/heads/{1}', github.repository, "
    "github.event.repository.default_branch)"
)
PUBLISH_IF = PROTECTED_WORKFLOW_IF + " && startsWith(inputs.candidate_ref, 'refs/tags/v')"
PUBLISH_RUN = "\n".join(
    (
        "CANDIDATE_TAG=${CANDIDATE_REF#refs/tags/}",
        'test "$CANDIDATE_TAG" != "$CANDIDATE_REF"',
        'gh release create "$CANDIDATE_TAG" --repo "$GITHUB_REPOSITORY" '
        '--verify-tag --target "$CANDIDATE_SHA" --generate-notes',
    )
)
RELEASE_COMMANDS = tuple(
    re.compile(pattern, re.I)
    for pattern in (
        r"\bgh\s+release\s+(?:create|edit|delete|upload)\b",
        r"\bgh\s+api\b[^\n]*(?:/releases|/git/refs|/contents)",
        r"\bgit\s+push\b",
        r"\b(?:cargo|npm|yarn|pnpm)\s+publish\b",
        r"\b(?:docker|podman)\s+push\b",
        r"\btwine\s+upload\b",
    )
)

MANIFEST_SCHEMA = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "additionalProperties": False,
    "required": ["schema_version", "source_rfc_sha256", "recorded_at_utc", "pins"],
    "properties": {
        "schema_version": {"const": 1},
        "source_rfc_sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
        "recorded_at_utc": {"type": "string", "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}$"},
        "pins": {
            "type": "array",
            "minItems": 1,
            "items": {
                "type": "object",
                "additionalProperties": False,
                "required": ["id", "status", "value", "blocked_by", "evidence"],
                "properties": {
                    "id": {"type": "string"},
                    "status": {"enum": ["pinned", "blocked"]},
                    "value": {"type": "string"},
                    "blocked_by": {"type": "string"},
                    "evidence": {"type": "string", "minLength": 1},
                },
            },
        },
    },
}

EVIDENCE_DIGESTS = {
    "runlet.registry_checksum": "docs/decisions/DR-0004-runlet-pin.md",
    "protocol.mcp.rmcp_crate": "docs/decisions/PRE-0005-protocol-revisions.md",
    "toon.spec_tarball_sha256": "docs/decisions/PRE-0006-toon.md",
    "toon.fixture_manifest_sha256": "docs/decisions/PRE-0006-toon.md",
    "toon.serde_toon2": "docs/decisions/PRE-0006-toon.md",
}

EVIDENCE_PATH = re.compile(
    r"^(?P<path>(?:RFC\.md|IMPLEMENTATION_PLAN\.md|Cargo\.(?:toml|lock)|rust-toolchain\.toml|"
    r"(?:vendor|docs|scripts|ci|requirements|\.github)/[A-Za-z0-9._/-]+))"
    r"(?::(?P<start>[0-9]+)(?:-(?P<end>[0-9]+))?)?(?:\s|$)"
)
AUTHORITATIVE_URL_HOSTS = {
    "api.github.com", "crates.io", "github.com", "json-schema.org", "pypi.org",
}
EVIDENCE_COMMANDS = {
    "tool.check_jsonschema": "check-jsonschema --version",
    "tool.openapi_spec_validator": "python3 -m openapi_spec_validator docs/api/openapi.yaml",
    "tool.cargo_metadata": "cargo --version",
    "tool.cargo_tree": "cargo tree --version",
    "build.cargo_lock_sha256": "shasum -a 256 Cargo.lock",
}


def load_yaml_text(text, source):
    try:
        return load_governance_yaml(text, str(source))
    except YamlLoadError as error:
        raise ValueError(str(error)) from error


def load_manifest(path):
    try:
        return load_yaml_text(path.read_text(encoding="utf-8"), path)
    except (OSError, UnicodeError) as error:
        raise ValueError(f"{path}: cannot read manifest: {error}") from error


def pin_map(document):
    return {
        record.get("id"): record
        for record in document.get("pins", [])
        if isinstance(record, dict) and isinstance(record.get("id"), str)
    }


def markdown_anchors(text):
    anchors = set()
    for heading in re.findall(r"^#{1,6}\s+(.+?)\s*#*\s*$", text, re.M):
        slug = re.sub(r"[^a-z0-9 _-]", "", heading.lower()).replace(" ", "-")
        anchors.add(slug)
    return anchors


def validate_blocker(value, errors):
    match = BLOCKER.fullmatch(value)
    if not match:
        errors.append(f"blocker must be an exact BLK-NN or EXT-NN repository link: {value!r}")
        return
    target = (ROOT / match["path"]).resolve()
    try:
        target.relative_to(ROOT)
    except ValueError:
        errors.append(f"blocker escapes repository: {value!r}")
        return
    if not target.is_file():
        errors.append(f"blocker document does not exist: {match['path']}")
        return
    text = target.read_text(encoding="utf-8")
    if match["id"] not in text:
        errors.append(f"blocker ID {match['id']} is absent from {match['path']}")
    if match["anchor"] not in markdown_anchors(text):
        errors.append(f"blocker anchor #{match['anchor']} is absent from {match['path']}")


def semver_tuple(value):
    return tuple(int(part) for part in value.split(".")) + (0,) * (3 - value.count(".") - 1)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def snapshot_entries(root, generated_target_roots=()):
    entries = {}
    for directory, dirnames, filenames in os.walk(root, followlinks=False):
        directory = Path(directory)
        for name in list(dirnames):
            path = directory / name
            if path.relative_to(root).as_posix() in generated_target_roots and not path.is_symlink():
                dirnames.remove(name)
                continue
            if path.is_symlink():
                dirnames.remove(name)
                filenames.append(name)
        for name in filenames:
            path = directory / name
            relative = path.relative_to(root).as_posix()
            try:
                if path.is_symlink():
                    entries[relative] = (
                        "120000",
                        hashlib.sha256(os.readlink(path).encode()).hexdigest(),
                    )
                elif path.is_file():
                    mode = "100755" if path.stat().st_mode & 0o111 else "100644"
                    entries[relative] = (mode, sha256(path))
                else:
                    entries[relative] = ("unsupported", "")
            except OSError as error:
                entries[relative] = ("unreadable", str(error))
    return entries


def validate_url(value, label, errors, allowed_hosts=AUTHORITATIVE_URL_HOSTS):
    parsed = urlparse(value)
    if (
        parsed.scheme != "https"
        or parsed.hostname not in allowed_hosts
        or parsed.username is not None
        or parsed.password is not None
    ):
        errors.append(f"{label} must use HTTPS on an authoritative host: {value!r}")


def validate_pin_evidence(record, errors):
    pin_id = record["id"]
    evidence = record["evidence"]
    previous_path = None
    for token in evidence.split(";"):
        token = token.strip()
        for url in re.findall(r"(?:https?|file)://[^\s;]+", token):
            validate_url(url, f"{pin_id} evidence URL", errors)
        match = EVIDENCE_PATH.match(token)
        if match:
            relative = match["path"]
            previous_path = relative
            target = ROOT / relative
            if not target.is_file():
                errors.append(f"{pin_id} evidence path does not exist: {relative}")
                continue
            if match["start"]:
                line_count = sum(1 for _ in target.open(encoding="utf-8"))
                start = int(match["start"])
                end = int(match["end"] or start)
                if start < 1 or end < start or end > line_count:
                    errors.append(f"{pin_id} evidence lines are outside {relative}: {start}-{end}")
        elif previous_path and (lines := re.fullmatch(r"([0-9]+)(?:-([0-9]+))?", token)):
            target = ROOT / previous_path
            line_count = sum(1 for _ in target.open(encoding="utf-8"))
            start, end = int(lines[1]), int(lines[2] or lines[1])
            if start < 1 or end < start or end > line_count:
                errors.append(f"{pin_id} evidence lines are outside {previous_path}: {start}-{end}")
    command = EVIDENCE_COMMANDS.get(pin_id)
    if command and command not in evidence:
        errors.append(f"{pin_id} evidence must name the verified command {command!r}")


def run_metadata(errors):
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=ROOT, text=True, capture_output=True, check=False,
    )
    if result.returncode:
        errors.append(f"cargo metadata --locked failed: {result.stderr.strip()}")
        return None
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        errors.append(f"cargo metadata returned invalid JSON: {error}")
        return None


def validate_snapshot(name, pins, errors):
    root = ROOT / "vendor" / name
    manifest_path = root / "SNAPSHOT-MANIFEST.sha256"
    metadata_path = root / "SNAPSHOT-METADATA.yaml"
    evidence_path = ROOT / "docs" / "compatibility" / "pins" / f"{name}-snapshot.yaml"
    pin = pins[f"{name}.snapshot_sha256"]["value"]
    manifest_digest = sha256(manifest_path)
    if pin != manifest_digest:
        errors.append(f"{name}.snapshot_sha256 does not match {manifest_path.relative_to(ROOT)}")

    metadata = load_manifest(metadata_path)
    evidence = load_manifest(evidence_path)
    for source, document in ((metadata_path, metadata), (evidence_path, evidence)):
        recorded = document.get("final_snapshot", {}).get("aggregate_sha256")
        if recorded != pin:
            errors.append(f"{source.relative_to(ROOT)} final snapshot digest does not match {name}.snapshot_sha256")

    exclusions = metadata.get("final_snapshot", {}).get("attestation_files_excluded_from_payload_digest")
    expected_exclusions = ["SNAPSHOT-MANIFEST.sha256", "SNAPSHOT-METADATA.yaml"]
    if exclusions != expected_exclusions:
        errors.append(f"{metadata_path.relative_to(ROOT)} must exclude only the snapshot manifest and metadata")
    for excluded in expected_exclusions:
        path = root / excluded
        if not path.is_file() or path.is_symlink() or path.stat().st_mode & 0o111:
            errors.append(f"{path.relative_to(ROOT)} must be a regular non-executable attestation file")

    generated_target_roots = metadata.get("final_snapshot", {}).get("generated_target_roots_excluded", [])
    if not isinstance(generated_target_roots, list) or any(
        not isinstance(relative, str)
        or not relative
        or relative.startswith("/")
        or "//" in relative
        or any(part in ("", ".", "..") for part in relative.split("/"))
        or relative.split("/")[-1] != "target"
        for relative in generated_target_roots
    ) or len(generated_target_roots) != len(set(generated_target_roots)):
        errors.append(
            f"{metadata_path.relative_to(ROOT)} generated target roots must be unique relative target directories"
        )
        generated_target_roots = []
    if evidence.get("final_snapshot", {}).get("generated_target_roots_excluded") != generated_target_roots:
        errors.append(
            f"{evidence_path.relative_to(ROOT)} generated target roots do not match snapshot metadata"
        )

    records = manifest_path.read_text(encoding="utf-8").splitlines()
    if records != sorted(records, key=lambda record: record.split("  ", 2)[-1]):
        errors.append(f"{manifest_path.relative_to(ROOT)} is not sorted")
    manifest_entries = {}
    for line in records:
        match = re.fullmatch(r"([0-9a-f]{64})  (100644|100755|120000)  (.+)", line)
        if not match:
            errors.append(f"{manifest_path.relative_to(ROOT)} has an invalid record: {line!r}")
            continue
        expected, mode, relative = match.groups()
        path = root / relative
        try:
            path.resolve(strict=False).relative_to(root.resolve())
        except ValueError:
            errors.append(f"{manifest_path.relative_to(ROOT)} path escapes snapshot: {relative!r}")
            continue
        if relative in manifest_entries:
            errors.append(f"{manifest_path.relative_to(ROOT)} repeats {relative!r}")
            continue
        manifest_entries[relative] = (mode, expected)
        try:
            if mode == "120000":
                actual = hashlib.sha256(os.readlink(path).encode()).hexdigest()
            else:
                actual = sha256(path)
            actual_mode = "120000" if path.is_symlink() else (
                "100755" if path.stat().st_mode & 0o111 else "100644"
            )
        except OSError as error:
            errors.append(f"{path.relative_to(ROOT)} cannot be verified: {error}")
            continue
        if actual_mode != mode:
            errors.append(f"{path.relative_to(ROOT)} mode {actual_mode} does not match {mode}")
        if actual != expected:
            errors.append(f"{path.relative_to(ROOT)} does not match its snapshot manifest digest")
    actual_entries = snapshot_entries(root, generated_target_roots)
    for excluded in expected_exclusions:
        actual_entries.pop(excluded, None)
    manifest_paths = set(manifest_entries)
    actual_paths = set(actual_entries)
    for relative in sorted(actual_paths - manifest_paths):
        errors.append(f"{root.relative_to(ROOT) / relative} is unlisted snapshot payload")
    for relative in sorted(manifest_paths - actual_paths):
        errors.append(f"{root.relative_to(ROOT) / relative} is listed but absent from snapshot payload")
    for relative in sorted(manifest_paths & actual_paths):
        if actual_entries[relative] != manifest_entries[relative]:
            errors.append(f"{root.relative_to(ROOT) / relative} mode or digest differs from snapshot manifest")

    expected_count = metadata.get("final_snapshot", {}).get("payload_file_count")
    if len(manifest_entries) != expected_count:
        errors.append(f"{manifest_path.relative_to(ROOT)} has {len(manifest_entries)} records, expected {expected_count}")
    evidence_records = evidence.get("files")
    if evidence_records is not None:
        recorded = {}
        for item in evidence_records:
            if not isinstance(item, dict) or set(item) != {"path", "mode", "sha256"}:
                errors.append(f"{evidence_path.relative_to(ROOT)} has an invalid files record")
                continue
            relative = item["path"]
            if relative in recorded:
                errors.append(f"{evidence_path.relative_to(ROOT)} repeats {relative!r}")
            recorded[relative] = (item["mode"], item["sha256"])
        if recorded != manifest_entries:
            errors.append(f"{evidence_path.relative_to(ROOT)} files do not exactly match the snapshot manifest")
    evidence_count = evidence.get("final_snapshot", {}).get(
        "payload_file_count", evidence.get("final_snapshot", {}).get("file_count")
    )
    if evidence_count != len(manifest_entries):
        errors.append(f"{evidence_path.relative_to(ROOT)} snapshot file count does not match the manifest")
    if metadata.get("final_snapshot", {}).get("manifest") != manifest_path.name:
        errors.append(f"{metadata_path.relative_to(ROOT)} does not name its snapshot manifest")
    if evidence.get("final_snapshot", {}).get("manifest") != manifest_path.relative_to(ROOT).as_posix():
        errors.append(f"{evidence_path.relative_to(ROOT)} does not name the repository snapshot manifest")


def validate_act(pins, errors, document=None, execute_installed=True):
    path = ROOT / "docs" / "compatibility" / "pins" / "act.yaml"
    document = load_manifest(path) if document is None else document
    version = pins["tool.act"]["value"]
    if document.get("tool") != "act" or document.get("version") != version:
        errors.append("docs/compatibility/pins/act.yaml does not match tool.act")
    release_prefix = "https://github.com/nektos/act/releases/"
    if document.get("release_url") != f"{release_prefix}tag/v{version}":
        errors.append("act release URL must be the pinned GitHub release")
    if document.get("checksums_url") != f"{release_prefix}download/v{version}/checksums.txt":
        errors.append("act checksums URL must be the pinned GitHub release asset")
    platforms = document.get("platforms", [])
    expected_platforms = {"Darwin/arm64", "Darwin/x86_64", "Linux/arm64", "Linux/x86_64"}
    if {item.get("platform") for item in platforms if isinstance(item, dict)} != expected_platforms:
        errors.append("act pin must cover Darwin and Linux on arm64 and x86_64")
    for item in platforms:
        source_url = str(item.get("source_url", "")) if isinstance(item, dict) else ""
        validate_url(source_url, "act platform source_url", errors, {"github.com"})
        if not isinstance(item, dict) or not HEX64.fullmatch(str(item.get("sha256", ""))):
            errors.append("act platform checksum must be SHA-256")
        elif not source_url.startswith(f"{release_prefix}download/v{version}/"):
            errors.append("act platform URL does not match tool.act")
    verification = document.get("verification", {})
    if verification.get("command") != ".tools/bin/act -l" or verification.get("exit_code") != 0:
        errors.append("act evidence must record a successful .tools/bin/act -l run")
    binary = ROOT / ".tools" / "bin" / "act"
    if execute_installed and binary.is_file():
        result = subprocess.run([str(binary), "--version"], text=True, capture_output=True, check=False)
        if result.returncode or result.stdout.strip() != f"act version {version}":
            errors.append("installed act binary does not match tool.act")


def validate_workflow(workflow, pins, errors):
    jobs = workflow.get("jobs", {})
    if not isinstance(jobs, dict) or set(jobs) != WORKFLOW_JOBS:
        actual = sorted(jobs) if isinstance(jobs, dict) else []
        errors.append(f"workflow job set must be exactly {sorted(WORKFLOW_JOBS)}; found {actual}")
        jobs = jobs if isinstance(jobs, dict) else {}

    read_permissions = {"contents": "read"}
    specifications = {
        "m003-external": {
            "permissions": read_permissions,
            "environment": "m003-external",
            "needs": None,
            "if": None,
        },
        "m003-platform-source": {
            "permissions": read_permissions,
            "environment": None,
            "needs": None,
            "if": None,
        },
        "m004-dogfood": {
            "permissions": read_permissions,
            "environment": "m004-external",
            "needs": None,
            "if": None,
        },
        "m004-stats": {
            "permissions": read_permissions,
            "environment": "m004-external",
            "needs": None,
            "if": None,
        },
        "m004-attestations": {
            "permissions": read_permissions,
            "environment": None,
            "needs": ["m003-external", "m004-dogfood", "m004-stats"],
            "if": None,
        },
        "lane": {
            "permissions": read_permissions,
            "environment": None,
            "needs": None,
            "if": None,
        },
        "release-validate": {
            "permissions": {"contents": "read", "actions": "read"},
            "environment": "release-validation",
            "needs": ["lane", "m003-external", "m004-dogfood", "m004-stats", "m004-attestations"],
            "if": PROTECTED_WORKFLOW_IF,
        },
        "publish": {
            "permissions": {"contents": "write"},
            "environment": "release-publish",
            "needs": ["lane", "release-validate"],
            "if": PUBLISH_IF,
        },
    }
    for job_name, expected in specifications.items():
        job = jobs.get(job_name, {})
        if not isinstance(job, dict):
            errors.append(f"workflow job {job_name} must be a mapping")
            continue
        for field, value in expected.items():
            if job.get(field) != value:
                errors.append(f"workflow job {job_name} must set {field} to {value!r}")

    if workflow.get("permissions") != read_permissions:
        errors.append("workflow permissions must be contents: read only")
    dispatch = workflow.get("on", {}).get("workflow_dispatch", {})
    inputs = dispatch.get("inputs", {}) if isinstance(dispatch, dict) else {}
    required_inputs = {
        "candidate_ref", "baseline_ref", "attestation_run_id", "attestation_artifact",
        "attestation_workflow_ref",
    }
    if set(inputs) != required_inputs or any(
        not isinstance(inputs[name], dict) or inputs[name].get("required") is not True
        for name in required_inputs & set(inputs)
    ):
        errors.append("workflow_dispatch must require the exact release validation inputs")

    action_pin = pins["build.action_checkout"]["value"]
    expected_checkouts = {
        "m003-external": [{
            "ref": "${{ inputs.candidate_ref || github.ref }}",
            "fetch-depth": 0,
            "persist-credentials": False,
        }],
        "m003-platform-source": [{
            "ref": "${{ inputs.candidate_ref || github.ref }}",
            "fetch-depth": 0,
            "persist-credentials": False,
        }],
        "m004-dogfood": [{
            "ref": "${{ inputs.candidate_ref || github.ref }}",
            "fetch-depth": 0,
            "persist-credentials": False,
        }],
        "m004-stats": [{
            "ref": "${{ inputs.candidate_ref || github.ref }}",
            "fetch-depth": 0,
            "persist-credentials": False,
        }],
        "m004-attestations": [],
        "lane": [{
            "ref": "${{ inputs.candidate_ref || github.ref }}",
            "fetch-depth": 0,
            "persist-credentials": False,
        }],
        "release-validate": [
            {
                "ref": "${{ github.workflow_sha }}",
                "path": "validator",
                "fetch-depth": 0,
                "persist-credentials": False,
            },
            {
                "ref": "${{ inputs.candidate_ref }}",
                "path": "candidate",
                "fetch-depth": 0,
                "persist-credentials": False,
            },
        ],
        "publish": [],
    }
    for job_name, job in jobs.items():
        if not isinstance(job, dict):
            continue
        permissions = job.get("permissions")
        if job_name != "publish" and (
            permissions == "write-all"
            or isinstance(permissions, dict) and permissions.get("contents") == "write"
        ):
            errors.append(f"workflow job {job_name} may not have contents: write")
        checkouts = []
        steps = job.get("steps", [])
        if not isinstance(steps, list):
            errors.append(f"workflow job {job_name} steps must be a list")
            continue
        for step in steps:
            if not isinstance(step, dict):
                errors.append(f"workflow job {job_name} contains a non-mapping step")
                continue
            uses = step.get("uses")
            if uses is not None:
                uses = str(uses)
                if uses.startswith("actions/checkout@"):
                    options = step.get("with", {})
                    if not isinstance(options, dict):
                        options = {}
                    checkouts.append(options)
                    if uses != f"actions/checkout@{action_pin}":
                        errors.append(f"workflow job {job_name} checkout does not match build.action_checkout")
                    if options.get("fetch-depth") != 0 or options.get("persist-credentials") is not False:
                        errors.append(f"workflow job {job_name} checkout must fetch history without credentials")
                elif "release" in uses.lower() or "publish" in uses.lower():
                    errors.append(f"workflow job {job_name} uses forbidden release action {uses!r}")
            run = step.get("run")
            if run is not None and any(pattern.search(str(run)) for pattern in RELEASE_COMMANDS):
                if job_name != "publish" or str(run).strip() != PUBLISH_RUN:
                    errors.append(f"workflow job {job_name} contains a forbidden release-affecting command")
        if job_name in expected_checkouts and checkouts != expected_checkouts[job_name]:
            errors.append(f"workflow job {job_name} checkout refs must be {expected_checkouts[job_name]!r}")

    release_job = jobs.get("release-validate", {})
    if isinstance(release_job, dict) and release_job.get("outputs") != {
        "candidate_sha": "${{ steps.resolve-refs.outputs.candidate_sha }}"
    }:
        errors.append("release-validate must export the separately resolved candidate SHA")
    publish_job = jobs.get("publish", {})
    expected_publish_steps = [{
        "name": "Publish validated GitHub release",
        "env": {
            "GH_TOKEN": "${{ github.token }}",
            "CANDIDATE_REF": "${{ inputs.candidate_ref }}",
            "CANDIDATE_SHA": "${{ needs.release-validate.outputs.candidate_sha }}",
        },
        "run": PUBLISH_RUN + "\n",
    }]
    if isinstance(publish_job, dict) and publish_job.get("steps") != expected_publish_steps:
        errors.append("publish must contain only the exact validated GitHub release creation step")

    runner = jobs.get("lane", {}).get("runs-on") if isinstance(jobs.get("lane"), dict) else None
    runner_record = pins["build.runner_image"]
    if not isinstance(runner, str) or not runner_record["value"].startswith(f"{runner};"):
        errors.append("workflow runner label does not match build.runner_image")
    if runner_record["status"] != "blocked" or not runner_record["blocked_by"].startswith("EXT-"):
        errors.append("mutable hosted runner must carry an explicit EXT blocker")

    external = jobs.get("m003-external", {})
    if isinstance(external, dict):
        expected_matrix = {
            "include": [
                {"arch": "x86_64", "runner": ["self-hosted", "linux", "x64", "kit-m003"]},
                {"arch": "aarch64", "runner": ["self-hosted", "linux", "arm64", "kit-m003"]},
            ]
        }
        strategy = external.get("strategy", {})
        if (
            external.get("runs-on") != "${{ matrix.runner }}"
            or not isinstance(strategy, dict)
            or strategy.get("matrix") != expected_matrix
            or strategy.get("fail-fast") is not False
        ):
            errors.append("m003-external must remain pending on the externally provisioned runner")
        if external.get("timeout-minutes") != 720:
            errors.append("m003-external must retain its explicit timeout")
        external_text = "\n".join(
            str(step.get("run", ""))
            for step in external.get("steps", [])
            if isinstance(step, dict)
        )
        for command in M003_EXTERNAL_COMMANDS:
            if command not in external_text:
                errors.append(f"m003-external is missing exact helper test command: {command}")
        for pin in (
            "KIT_TRIAL_AGENT_IMAGE",
            "KIT_TRIAL_GRADER_IMAGE",
            "KIT_CONTAINER_LIMIT_PROBE_IMAGE",
            "KIT_CONTAINER_HELPER_SHA256",
        ):
            if pin not in external_text:
                errors.append(f"m003-external does not verify immutable pin {pin}")

    req_lane = load_yaml_text((ROOT / "ci/lanes/req-lint.yaml").read_text(encoding="utf-8"), "ci/lanes/req-lint.yaml")
    missing_governance = GOVERNANCE_COMMANDS - set(req_lane.get("commands", []))
    if missing_governance:
        errors.append(f"req-lint lane omits governance checks: {', '.join(sorted(missing_governance))}")


def _ast_same(node, expression):
    expected = ast.parse(expression, mode="eval").body
    return ast.dump(node, include_attributes=False) == ast.dump(expected, include_attributes=False)


def _ast_dict(node):
    if not isinstance(node, ast.Dict):
        return None
    result = {}
    for key, value in zip(node.keys, node.values):
        if not isinstance(key, ast.Constant) or not isinstance(key.value, str) or key.value in result:
            return None
        result[key.value] = value
    return result


def _assignments(node):
    result = {}
    for item in ast.walk(node):
        if isinstance(item, ast.Assign) and len(item.targets) == 1 and isinstance(item.targets[0], ast.Name):
            result[item.targets[0].id] = item.value
    return result


def _call_name(call):
    parts = []
    node = call.func
    while isinstance(node, ast.Attribute):
        parts.append(node.attr)
        node = node.value
    if isinstance(node, ast.Name):
        parts.append(node.id)
    return ".".join(reversed(parts))


def _calls(node, name):
    return [item for item in ast.walk(node) if isinstance(item, ast.Call) and _call_name(item) == name]


def _keyword(call, name):
    return next((item.value for item in call.keywords if item.arg == name), None)


def _call_matches(call, argument, **keywords):
    return (
        bool(call.args)
        and _ast_same(call.args[0], argument)
        and all(
            (value := _keyword(call, name)) is not None and _ast_same(value, expression)
            for name, expression in keywords.items()
        )
    )


def validate_reproducible_build(source, lane, errors):
    prefix = "reproducible-build contract: "

    def require(condition, message):
        if not condition:
            errors.append(prefix + message)

    require(
        lane == {
            "schema_version": 1,
            "name": "reproducible-build",
            "commands": ["python3 scripts/reproducible_build.py"],
        },
        "lane must exactly invoke python3 scripts/reproducible_build.py",
    )
    try:
        tree = ast.parse(source, filename="scripts/reproducible_build.py")
    except SyntaxError as error:
        errors.append(prefix + f"script is not valid Python: {error}")
        return

    functions = {
        item.name: item
        for item in tree.body
        if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef))
    }
    main = functions.get("main")
    runner = functions.get("run")
    cleaner = functions.get("clean_work")
    require(main is not None, "script must define main")
    require(runner is not None, "script must define the process runner")
    require(cleaner is not None, "script must define transient-work cleanup")
    if main is None or runner is None or cleaner is None:
        return

    module_assignments = {
        item.targets[0].id: item.value
        for item in tree.body
        if isinstance(item, ast.Assign)
        and len(item.targets) == 1
        and isinstance(item.targets[0], ast.Name)
    }
    expected_paths = {
        "WORK": 'EVIDENCE / "reproducible-build-work"',
        "SOURCE_A": 'WORK / "repro-src-a" / "src"',
        "SOURCE_B": 'WORK / "repro-src-b" / "src"',
        "TARGET_A": 'WORK / "repro-src-a" / "target"',
        "TARGET_B": 'WORK / "repro-src-b" / "target"',
        "CARGO_HOME": 'WORK / "cargo-home"',
        "HOME": 'WORK / "home"',
        "ENVIRONMENT_FILE": 'EVIDENCE / "repro-environment.json"',
        "ARTIFACT_FILE": 'EVIDENCE / "reproducible-build.json"',
    }
    require(
        all(name in module_assignments and _ast_same(module_assignments[name], expression)
            for name, expression in expected_paths.items()),
        "sources and targets must be distinct isolated paths with JSON evidence paths",
    )
    require(
        all(
            isinstance(module_assignments.get(name), ast.Constant)
            and isinstance(module_assignments[name].value, int)
            and module_assignments[name].value > 0
            for name in ("FETCH_TIMEOUT", "BUILD_TIMEOUT", "PROBE_TIMEOUT")
        ),
        "fetch, build, and probe timeouts must be positive constants",
    )

    assignments = _assignments(main)
    ignored = assignments.get("ignored")
    ignored_values = {
        item.value
        for call in [ignored]
        if isinstance(call, ast.Call) and _call_name(call) == "shutil.ignore_patterns"
        for item in call.args
        if isinstance(item, ast.Constant) and isinstance(item.value, str)
    }
    require(
        {".git", ".evidence-tmp", "target", "__pycache__", "*.pyc", ".DS_Store"}
        <= ignored_values,
        "source copies must exclude repository and generated state",
    )
    copy_calls = _calls(main, "shutil.copytree")
    expected_copies = {("ROOT", "SOURCE_A"), ("ROOT", "SOURCE_B")}
    actual_copies = {
        (call.args[0].id, call.args[1].id)
        for call in copy_calls
        if len(call.args) >= 2
        and isinstance(call.args[0], ast.Name)
        and isinstance(call.args[1], ast.Name)
        and (symlinks := _keyword(call, "symlinks")) is not None
        and _ast_same(symlinks, "True")
        and (ignore := _keyword(call, "ignore")) is not None
        and _ast_same(ignore, "ignored")
    }
    require(actual_copies == expected_copies, "must make two distinct clean source copies")
    mkdir_names = {
        call.func.value.id
        for call in ast.walk(main)
        if isinstance(call, ast.Call)
        and isinstance(call.func, ast.Attribute)
        and call.func.attr == "mkdir"
        and isinstance(call.func.value, ast.Name)
    }
    require(
        {"TARGET_A", "TARGET_B", "CARGO_HOME", "HOME"} <= mkdir_names,
        "must create two distinct targets and the shared isolated homes",
    )

    remap_flags = assignments.get("remap_flags")
    remaps = remap_flags.elts if isinstance(remap_flags, ast.List) else []
    require(
        all(any(_ast_same(item, expected) for item in remaps) for expected in (
            'f"--remap-path-prefix={SOURCE_A}=/workspace"',
            'f"--remap-path-prefix={SOURCE_B}=/workspace"',
            'f"--remap-path-prefix={ROOT}=/workspace"',
        )),
        "all source paths must be remapped",
    )
    require(
        all(any(_ast_same(item, expected) for item in remaps) for expected in (
            'f"--remap-path-prefix={TARGET_A}=/target"',
            'f"--remap-path-prefix={TARGET_B}=/target"',
        )),
        "both target paths must be remapped",
    )
    require(
        any(_ast_same(item, 'f"--remap-path-prefix={CARGO_HOME}=/cargo-home"') for item in remaps),
        "the shared Cargo cache path must be remapped",
    )

    common_env = _ast_dict(assignments.get("common_env"))
    expected_environment = {
        "CARGO_ENCODED_RUSTFLAGS": '"\\x1f".join(remap_flags)',
        "CARGO_HOME": "str(CARGO_HOME)",
        "CARGO_INCREMENTAL": '"0"',
        "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": '"1"',
        "HOME": "str(HOME)",
        "LANG": '"C"',
        "LC_ALL": '"C"',
        "PATH": 'os.environ["PATH"]',
        "RUSTUP_HOME": "rustup_home",
        "SOURCE_DATE_EPOCH": '"0"',
        "TZ": '"UTC"',
    }
    require(
        common_env is not None
        and set(common_env) == set(expected_environment)
        and all(_ast_same(common_env[name], expression) for name, expression in expected_environment.items()),
        "every process must receive only the fixed environment contract",
    )
    require(
        any(_ast_same(item, '"-C"') for item in remaps)
        and any(_ast_same(item, '"codegen-units=1"') for item in remaps)
        and common_env is not None
        and "CARGO_PROFILE_RELEASE_CODEGEN_UNITS" in common_env
        and _ast_same(common_env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"], '"1"'),
        "code generation must be fixed to one unit",
    )

    build_envs = {}
    for name in ("build_env_a", "build_env_b"):
        value = assignments.get(name)
        if isinstance(value, ast.BinOp) and isinstance(value.op, ast.BitOr) and _ast_same(value.left, "common_env"):
            build_envs[name] = _ast_dict(value.right)
    require(
        all(
            env is not None
            and set(env) == {"CARGO_NET_OFFLINE", "CARGO_TARGET_DIR"}
            and _ast_same(env["CARGO_NET_OFFLINE"], '"true"')
            and _ast_same(env["CARGO_TARGET_DIR"], f"str(TARGET_{suffix})")
            for name, suffix in (("build_env_a", "A"), ("build_env_b", "B"))
            for env in [build_envs.get(name)]
        ),
        "offline builds must use distinct target directories inherited from the shared environment",
    )
    require(
        common_env is not None
        and "CARGO_HOME" in common_env
        and _ast_same(common_env["CARGO_HOME"], "str(CARGO_HOME)"),
        "fetch and both builds must share one Cargo home",
    )

    run_calls = _calls(main, "run")
    require(
        any(_call_matches(call, '["cargo", "fetch", "--locked"]', cwd="SOURCE_A",
                          env="common_env", timeout="FETCH_TIMEOUT") for call in run_calls),
        "dependency fetch must be locked into the shared Cargo home",
    )
    command = assignments.get("command")
    build_command_ok = command is not None and _ast_same(
        command, '["cargo", "build", "--locked", "--offline", "--release"]'
    )
    require(
        build_command_ok
        and any(_call_matches(call, "command", cwd="SOURCE_A", env="build_env_a",
                              timeout="BUILD_TIMEOUT") for call in run_calls)
        and any(_call_matches(call, "command", cwd="SOURCE_B", env="build_env_b",
                              timeout="BUILD_TIMEOUT") for call in run_calls),
        "both builds must be locked, offline, and release mode",
    )

    popen_calls = _calls(tree, "subprocess.Popen")
    forbidden_process_calls = sum(
        (_calls(tree, name) for name in (
            "subprocess.run", "subprocess.call", "subprocess.check_call", "subprocess.check_output"
        )),
        [],
    )
    require(
        len(popen_calls) == 1
        and not forbidden_process_calls
        and _keyword(popen_calls[0], "start_new_session") is not None
        and _ast_same(_keyword(popen_calls[0], "start_new_session"), "True")
        and _keyword(popen_calls[0], "env") is not None
        and _ast_same(_keyword(popen_calls[0], "env"), "env")
        and all(_keyword(call, "timeout") is not None for call in run_calls)
        and any(
            _keyword(call, "timeout") is not None
            and _ast_same(_keyword(call, "timeout"), "timeout")
            for call in _calls(runner, "process.communicate")
        ),
        "every process must use the timeout runner in a new session",
    )
    timeout_handlers = [
        handler
        for item in runner.body
        if isinstance(item, ast.Try)
        for handler in item.handlers
        if handler.type is not None and _ast_same(handler.type, "subprocess.TimeoutExpired")
    ]
    timeout_handler = timeout_handlers[0] if len(timeout_handlers) == 1 else None
    nested_timeout_handlers = [
        handler
        for item in (timeout_handler.body if timeout_handler else [])
        if isinstance(item, ast.Try)
        for handler in item.handlers
        if handler.type is not None and _ast_same(handler.type, "subprocess.TimeoutExpired")
    ]
    require(
        timeout_handler is not None
        and len(nested_timeout_handlers) == 1
        and any(len(call.args) == 2 and _ast_same(call.args[0], "process.pid")
            and _ast_same(call.args[1], "signal.SIGTERM")
            for call in _calls(timeout_handler, "os.killpg"))
        and any(len(call.args) == 2 and _ast_same(call.args[0], "process.pid")
                and _ast_same(call.args[1], "signal.SIGKILL")
                for call in _calls(nested_timeout_handlers[0], "os.killpg")),
        "timeout handling must terminate and then kill the process group",
    )

    require(
        any(_ast_same(item.test, "not filecmp.cmp(binary_a, binary_b, shallow=False)")
            for item in ast.walk(main) if isinstance(item, ast.If)),
        "final artifacts must be compared byte-for-byte",
    )

    canonical = functions.get("canonical_digest")
    writer = functions.get("write_json")
    canonical_assignments = _assignments(canonical) if canonical else {}
    require(
        canonical is not None
        and (encoded := canonical_assignments.get("encoded")) is not None
        and _ast_same(
            encoded,
            'json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")',
        )
        and any(_ast_same(item.value, "hashlib.sha256(encoded).hexdigest()")
                for item in canonical.body if isinstance(item, ast.Return)),
        "environment digest must use canonical JSON and SHA-256",
    )
    require(
        writer is not None
        and any(
            _ast_same(
                item.value,
                'path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\\n", encoding="utf-8")',
            )
            for item in ast.walk(writer)
            if isinstance(item, ast.Expr)
        ),
        "evidence records must be serialized as deterministic JSON",
    )

    environment = _ast_dict(assignments.get("environment"))
    environment_timeouts = _ast_dict(environment.get("timeouts_seconds")) if environment else None
    environment_tools = _ast_dict(environment.get("tools")) if environment else None
    expected_variables = (
        '{key: common_env[key] for key in ("CARGO_INCREMENTAL", '
        '"CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "LANG", "LC_ALL", '
        '"SOURCE_DATE_EPOCH", "TZ")}'
    )
    require(
        environment is not None
        and set(environment) == {
            "cargo_lock_sha256", "platform", "remap_flags", "schema_version",
            "timeouts_seconds", "tools", "variables",
        }
        and _ast_same(environment["cargo_lock_sha256"], "lock_digest")
        and _ast_same(environment["remap_flags"], "remap_flags")
        and _ast_same(environment["schema_version"], "1")
        and environment_timeouts is not None
        and set(environment_timeouts) == {"build", "fetch", "probe"}
        and _ast_same(environment_timeouts["build"], "BUILD_TIMEOUT")
        and _ast_same(environment_timeouts["fetch"], "FETCH_TIMEOUT")
        and _ast_same(environment_timeouts["probe"], "PROBE_TIMEOUT")
        and environment_tools is not None
        and set(environment_tools) == {"cargo", "rustc"}
        and _ast_same(environment_tools["cargo"], "cargo_version")
        and _ast_same(environment_tools["rustc"], "rustc_version")
        and _ast_same(environment["variables"], expected_variables)
        and (lock_digest := assignments.get("lock_digest")) is not None
        and _ast_same(lock_digest, 'sha256(ROOT / "Cargo.lock")')
        and any(_call_matches(call, '["rustc", "--version"]', cwd="ROOT", env="common_env",
                              timeout="PROBE_TIMEOUT", capture="True") for call in run_calls)
        and any(_call_matches(call, '["cargo", "--version"]', cwd="ROOT", env="common_env",
                              timeout="PROBE_TIMEOUT", capture="True") for call in run_calls),
        "environment JSON must bind the lock, remaps, fixed variables, timeouts, platform, and tools",
    )

    environment_record = _ast_dict(assignments.get("environment_record"))
    require(
        (environment_digest := assignments.get("environment_digest")) is not None
        and _ast_same(environment_digest, "canonical_digest(environment)")
        and environment_record is not None
        and set(environment_record) == {"environment", "sha256", "type"}
        and _ast_same(environment_record["environment"], "environment")
        and _ast_same(environment_record["sha256"], "environment_digest")
        and _ast_same(environment_record["type"], '"reproducible_build_environment"')
        and any(_call_matches(call, "ENVIRONMENT_FILE") and len(call.args) == 2
                and _ast_same(call.args[1], "environment_record") for call in _calls(main, "write_json")),
        "environment contract and canonical digest must be written as JSON",
    )
    artifact_record = _ast_dict(assignments.get("artifact_record"))
    binaries = _ast_dict(artifact_record.get("binaries")) if artifact_record else None
    closure = assignments.get("closure")
    closure_b = assignments.get("closure_b")
    closure_writes = [
        call for call in _calls(main, "write_json")
        if _call_matches(call, "CLOSURE_FILE") and len(call.args) == 2
        and _ast_same(call.args[1], "closure")
    ]
    build_lines = [
        call.lineno for call in run_calls
        if build_command_ok and call.args and _ast_same(call.args[0], "command")
    ]
    require(
        (binary_a := assignments.get("binary_a")) is not None
        and _ast_same(binary_a, 'TARGET_A / "release" / "kit"')
        and (binary_b := assignments.get("binary_b")) is not None
        and _ast_same(binary_b, 'TARGET_B / "release" / "kit"')
        and (digest_a := assignments.get("digest_a")) is not None
        and _ast_same(digest_a, "sha256(binary_a)")
        and (digest_b := assignments.get("digest_b")) is not None
        and _ast_same(digest_b, "sha256(binary_b)")
        and artifact_record is not None
        and set(artifact_record) == {
            "binaries", "build_input_closure_sha256", "byte_identical",
            "environment_sha256", "schema_version", "type"
        }
        and binaries is not None
        and set(binaries) == {"source_a_sha256", "source_b_sha256"}
        and _ast_same(binaries["source_a_sha256"], "digest_a")
        and _ast_same(binaries["source_b_sha256"], "digest_b")
        and _ast_same(artifact_record["byte_identical"], "True")
        and _ast_same(artifact_record["build_input_closure_sha256"], 'closure["sha256"]')
        and _ast_same(artifact_record["environment_sha256"], "environment_digest")
        and _ast_same(artifact_record["type"], '"reproducible_build_artifact"')
        and any(_call_matches(call, "ARTIFACT_FILE") and len(call.args) == 2
                and _ast_same(call.args[1], "artifact_record") for call in _calls(main, "write_json")),
        "artifact digests and environment digest must be written as JSON",
    )
    require(
        closure is not None
        and _ast_same(closure, "build_input_closure(SOURCE_A, common_env)")
        and closure_b is not None
        and _ast_same(closure_b, "build_input_closure(SOURCE_B, common_env)")
        and closure_writes
        and build_lines
        and closure_writes[0].lineno < min(build_lines),
        "both source closures must match and be recorded before building",
    )

    clean_calls = _calls(main, "clean_work")
    first_copy_line = min((call.lineno for call in copy_calls), default=10**9)
    try_blocks = [item for item in main.body if isinstance(item, ast.Try)]
    require(
        any(_ast_same(item.test, "WORK.exists()")
            and any(_call_matches(call, "WORK") for call in _calls(item, "shutil.rmtree"))
            for item in ast.walk(cleaner) if isinstance(item, ast.If))
        and any(call.lineno < first_copy_line for call in clean_calls)
        and len(try_blocks) == 1
        and any(_calls(item, "clean_work") for item in try_blocks[0].finalbody),
        "transient work must be cleaned before staging and in finally",
    )


def validate_repository(document, pins, errors, execute_candidate_tools=True):
    actual_rfc = sha256(ROOT / "RFC.md")
    if document["source_rfc_sha256"] != actual_rfc:
        errors.append(f"source_rfc_sha256 does not match RFC.md: expected {actual_rfc}")
    for evidence in [ROOT / "IMPLEMENTATION_PLAN.md", ROOT / "docs/decisions/PRE-0001-rfc-digest.md"]:
        if document["source_rfc_sha256"] not in evidence.read_text(encoding="utf-8"):
            errors.append(f"source_rfc_sha256 is absent from {evidence.relative_to(ROOT)}")

    cargo_toml = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    toolchain = tomllib.loads((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"))
    rust_pin = pins["rust.toolchain"]["value"]
    if toolchain.get("toolchain", {}).get("channel") != rust_pin:
        errors.append("rust.toolchain does not match rust-toolchain.toml")
    package = cargo_toml.get("package", {})
    if package.get("rust-version") != rust_pin:
        errors.append("rust.toolchain does not match Cargo.toml package.rust-version")
    minimum = pins["rust.agentkit_minimum"]["value"]
    if SEMVER.fullmatch(rust_pin) and SEMVER.fullmatch(minimum) and semver_tuple(rust_pin) < semver_tuple(minimum):
        errors.append(f"Rust {rust_pin} is below agentkit minimum {minimum}")

    lock_path = ROOT / "Cargo.lock"
    if pins["build.cargo_lock_sha256"]["value"] != sha256(lock_path):
        errors.append("build.cargo_lock_sha256 does not match Cargo.lock")
    closure_digest = pins["build.input_closure_sha256"]["value"]
    phase0_report = (ROOT / "requirements/reports/phase0-exit.md").read_text(encoding="utf-8")
    if f"Build-input closure SHA-256: `{closure_digest}`" not in phase0_report:
        errors.append("build.input_closure_sha256 is not bound by the Phase 0 report")
    cargo_lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    lock_packages = cargo_lock.get("package", [])
    root_lock = [item for item in lock_packages if item.get("name") == package.get("name") and item.get("version") == package.get("version")]
    if len(root_lock) != 1:
        errors.append("Cargo.lock does not contain exactly one Kit root package")
    for item in lock_packages:
        if str(item.get("source", "")).startswith("registry+") and not HEX64.fullmatch(str(item.get("checksum", ""))):
            errors.append(f"Cargo.lock registry package lacks a SHA-256 checksum: {item.get('name')} {item.get('version')}")
    structural_crates = {
        "ast-grep-core": (
            "tree-sitter",
            "cd97b166e4a9b45b0337cad09f849607018c820489865488dd1cd0e7059c4f16",
            "structural.ast_grep_core",
        ),
        "ast-grep-language": (
            "tree-sitter-rust",
            "79b5df85d2ad1dbd19ae78e73ef77e8f1eccd798ec918efd48d81a7072d55c46",
            "structural.ast_grep_language",
        ),
    }
    dependencies = cargo_toml.get("dependencies", {})
    for name, (feature, checksum, pin_id) in structural_crates.items():
        dependency = dependencies.get(name)
        if dependency != {
            "version": "=0.40.1",
            "default-features": False,
            "features": [feature],
        }:
            errors.append(f"Cargo.toml {name} must be exact 0.40.1 with only {feature} and default features disabled")
        locked = [
            item for item in lock_packages
            if item.get("name") == name and item.get("version") == "0.40.1"
        ]
        if len(locked) != 1 or locked[0].get("checksum") != checksum:
            errors.append(f"Cargo.lock {name} 0.40.1 checksum does not match the structural pin")
        value = pins[pin_id]["value"]
        if not all(token in value for token in (f"{name}=0.40.1", f"features={feature}", "default_features=false", f"crate_sha256={checksum}")):
            errors.append(f"{pin_id} does not bind the exact Cargo dependency and checksum")

    metadata = run_metadata(errors) if execute_candidate_tools else None
    if metadata is not None:
        packages = metadata.get("packages", [])
        if len(packages) != 1 or packages[0].get("name") != package.get("name"):
            errors.append("cargo metadata does not describe the single Kit package")
        elif packages[0].get("rust_version") != rust_pin or packages[0].get("version") != package.get("version"):
            errors.append("cargo metadata disagrees with the pinned Rust or package version")

    decision = (ROOT / "docs/decisions/PRE-0002-agentkit-pin.md").read_text(encoding="utf-8")
    agentkit_metadata = load_manifest(ROOT / "vendor/agentkit/SNAPSHOT-METADATA.yaml")
    runlet_metadata = load_manifest(ROOT / "vendor/runlet/SNAPSHOT-METADATA.yaml")
    validate_url(agentkit_metadata.get("source", {}).get("repository", ""), "agentkit source repository", errors, {"github.com"})
    validate_url(runlet_metadata.get("source", {}).get("repository", ""), "runlet source repository", errors, {"github.com"})
    for pin_id in ("agentkit.commit", "agentkit.tree"):
        if not HEX40.fullmatch(pins[pin_id]["value"]):
            errors.append(f"{pin_id} must be a lowercase 40-character Git object ID")
        elif pins[pin_id]["value"] not in decision:
            errors.append(f"{pin_id} does not match PRE-0002 recorded evidence")
    if agentkit_metadata.get("source", {}).get("base_commit") != pins["agentkit.commit"]["value"]:
        errors.append("agentkit.commit differs from the vendored snapshot metadata")
    if agentkit_metadata.get("source", {}).get("base_tree") != pins["agentkit.tree"]["value"]:
        errors.append("agentkit.tree differs from the vendored snapshot metadata")
    if sha256(ROOT / "vendor/agentkit/Cargo.lock") != pins["agentkit.lock_sha256"]["value"]:
        errors.append("agentkit.lock_sha256 differs from the vendored snapshot")
    validate_snapshot("agentkit", pins, errors)
    validate_snapshot("runlet", pins, errors)
    if pins["runlet.snapshot_sha256"]["value"] not in pins["runlet.application_source"]["value"]:
        errors.append("runlet.application_source is not bound to runlet.snapshot_sha256")
    validate_act(pins, errors, execute_installed=execute_candidate_tools)

    for pin_id, relative_path in EVIDENCE_DIGESTS.items():
        digests = re.findall(r"[0-9a-f]{64}", pins[pin_id]["value"])
        if len(digests) != 1:
            errors.append(f"{pin_id} must contain exactly one SHA-256 digest")
            continue
        evidence = (ROOT / relative_path).read_text(encoding="utf-8")
        if digests[0] not in evidence:
            errors.append(f"{pin_id} checksum is absent from {relative_path}")

    workflow = load_yaml_text((ROOT / ".github/workflows/ci.yaml").read_text(encoding="utf-8"), ".github/workflows/ci.yaml")
    validate_workflow(workflow, pins, errors)
    lane_job = workflow.get("jobs", {}).get("lane", {})
    matrix_rows = lane_job.get("strategy", {}).get("matrix", {}).get("include", [])
    matrix_names = [row.get("name") for row in matrix_rows if isinstance(row, dict)]
    if len(matrix_names) != 12 or set(matrix_names) != REQUIRED_LANES:
        errors.append("workflow matrix must map each of the exact 12 lane manifests once")
    workflow_text = (ROOT / ".github/workflows/ci.yaml").read_text(encoding="utf-8")
    for required in (
        "github.event.repository.default_branch",
        "github.workflow_ref",
        "ref: ${{ github.workflow_sha }}",
        "path: validator",
        "path: candidate",
        "../validator/scripts/verify_pins.py --root",
        "../validator/scripts/req_lint.py",
        "--candidate-ref \"$CANDIDATE_SHA\"",
        "--baseline-ref \"$BASELINE_SHA\"",
        "gh release create",
    ):
        if required not in workflow_text:
            errors.append(f"release workflow is missing {required!r}")
    if "ubuntu-latest" in workflow_text:
        errors.append("workflow must not use ubuntu-latest as a reproducible image pin")
    repro_lane = load_yaml_text(
        (ROOT / "ci/lanes/reproducible-build.yaml").read_text(encoding="utf-8"),
        "ci/lanes/reproducible-build.yaml",
    )
    validate_reproducible_build(
        (ROOT / "scripts/reproducible_build.py").read_text(encoding="utf-8"),
        repro_lane,
        errors,
    )

    requirements = (ROOT / "requirements/ci-requirements.txt").read_text(encoding="utf-8")
    lock = (ROOT / "requirements/ci-requirements.lock").read_text(encoding="utf-8")
    for dependency in ("PyYAML", "jsonschema", "openapi-spec-validator"):
        if not re.search(rf"(?mi)^{re.escape(dependency)}==[^\s]+$", requirements):
            errors.append(f"{dependency} must be exactly pinned in ci-requirements.txt")
        if not re.search(rf"(?mi)^{re.escape(dependency)}==[^\s]+", lock):
            errors.append(f"{dependency} is absent from ci-requirements.lock")
    if "--hash=sha256:" not in lock:
        errors.append("ci-requirements.lock contains no hashes")


def validate(document, release=False, repository=True):
    errors = []
    if not isinstance(document, dict):
        return ["manifest root must be a mapping"]
    for error in Draft202012Validator(MANIFEST_SCHEMA).iter_errors(document):
        location = ".".join(str(part) for part in error.absolute_path) or "manifest"
        errors.append(f"{location}: {error.message}")
    if errors:
        return errors

    seen = set()
    pins = pin_map(document)
    for record in document["pins"]:
        pin_id = record["id"]
        if not PIN_ID.fullmatch(pin_id):
            errors.append(f"invalid pin id: {pin_id!r}")
        if pin_id in seen:
            errors.append(f"duplicate pin id: {pin_id}")
        seen.add(pin_id)
        status, value, blocker = record["status"], record["value"], record["blocked_by"]
        validate_pin_evidence(record, errors)
        if "://" in value:
            validate_url(value, f"{pin_id} value URL", errors)
        if status == "pinned":
            if not value:
                errors.append(f"{pin_id}: pinned value is empty")
            if blocker:
                errors.append(f"{pin_id}: pinned record carries a blocker")
            if FLOATING.search(value):
                errors.append(f"{pin_id}: pinned value is floating: {value!r}")
        else:
            if not blocker:
                errors.append(f"{pin_id}: blocked record has no blocker")
            else:
                validate_blocker(blocker, errors)
            if release:
                errors.append(f"{pin_id}: unresolved blocker is forbidden in release mode")
        if pin_id.endswith(("sha256", "checksum")) and value and not HEX64.fullmatch(value):
            errors.append(f"{pin_id}: value must be a lowercase SHA-256 digest")

    missing = REQUIRED_IDS - seen
    if missing:
        errors.append(f"missing required pins: {', '.join(sorted(missing))}")
    if "rust.toolchain" in pins and not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", pins["rust.toolchain"]["value"]):
        errors.append("rust.toolchain must be an exact x.y.z release")
    for pin_id in ("toon.spec_commit", "build.action_checkout"):
        if pin_id in pins and not HEX40.fullmatch(pins[pin_id]["value"]):
            errors.append(f"{pin_id} must be a lowercase 40-character Git object ID")
    if repository and not errors:
        validate_repository(document, pins, errors, execute_candidate_tools=not release)
    return errors


def self_test(path):
    text = path.read_text(encoding="utf-8")
    document = load_yaml_text(text, path)
    baseline = validate(document)
    if baseline:
        return baseline
    failures = []

    workflow = load_yaml_text(
        (ROOT / ".github/workflows/ci.yaml").read_text(encoding="utf-8"),
        ".github/workflows/ci.yaml",
    )
    rogue_job = copy.deepcopy(workflow)
    rogue_job["jobs"]["rogue-write"] = {
        "runs-on": "ubuntu-24.04",
        "permissions": {"contents": "write"},
        "steps": [{"run": "gh release create rogue"}],
    }
    rogue_errors = []
    validate_workflow(rogue_job, pin_map(document), rogue_errors)
    if not any("job set must be exactly" in error for error in rogue_errors) or not any(
        "may not have contents: write" in error for error in rogue_errors
    ):
        failures.append("negative test failed: rogue write job was accepted")

    rogue_command = copy.deepcopy(workflow)
    rogue_command["jobs"]["lane"]["steps"].append({"run": "gh release create rogue"})
    rogue_errors = []
    validate_workflow(rogue_command, pin_map(document), rogue_errors)
    if not any("forbidden release-affecting command" in error for error in rogue_errors):
        failures.append("negative test failed: rogue release command was accepted")

    release_errors = validate(document, release=True, repository=False)
    if not any("forbidden in release mode" in error for error in release_errors):
        failures.append("negative test failed: release mode accepted blockers")

    corrupt = copy.deepcopy(document)
    corrupt["source_rfc_sha256"] = "0" * 64
    if not validate(corrupt):
        failures.append("negative test failed: corrupt RFC digest was accepted")

    floating = copy.deepcopy(document)
    pin_map(floating)["rust.toolchain"]["value"] = "stable"
    if not validate(floating, repository=False):
        failures.append("negative test failed: floating Rust channel was accepted")

    for pin_id in ("build.reproducible_artifact_sha256", "build.reproducible_environment_sha256"):
        missing_pin = copy.deepcopy(document)
        missing_pin["pins"] = [record for record in missing_pin["pins"] if record["id"] != pin_id]
        if not any("missing required pins" in error for error in validate(missing_pin, repository=False)):
            failures.append(f"negative test failed: removal of {pin_id} was accepted")

    repro_path = ROOT / "scripts/reproducible_build.py"
    repro_source = repro_path.read_text(encoding="utf-8")
    repro_lane = load_yaml_text(
        (ROOT / "ci/lanes/reproducible-build.yaml").read_text(encoding="utf-8"),
        "ci/lanes/reproducible-build.yaml",
    )
    altered_lane = copy.deepcopy(repro_lane)
    altered_lane["commands"].append("true")
    repro_errors = []
    validate_reproducible_build(repro_source, altered_lane, repro_errors)
    if not any("lane must exactly invoke" in error for error in repro_errors):
        failures.append("negative test failed: reproducible-build lane accepted an extra command")

    repro_mutations = (
        (
            "distinct sources",
            "shutil.copytree(ROOT, SOURCE_B, symlinks=True, ignore=ignored)",
            "shutil.copytree(ROOT, SOURCE_A, symlinks=True, ignore=ignored)",
            "two distinct clean source copies",
        ),
        (
            "clean sources",
            '".git", ".evidence-tmp", "target", "__pycache__", "*.pyc", ".DS_Store"',
            '".git", ".evidence-tmp", "__pycache__", "*.pyc", ".DS_Store"',
            "source copies must exclude",
        ),
        (
            "distinct targets",
            '"CARGO_TARGET_DIR": str(TARGET_B)',
            '"CARGO_TARGET_DIR": str(TARGET_A)',
            "offline builds must use distinct target directories",
        ),
        (
            "shared Cargo home",
            '            "CARGO_HOME": str(CARGO_HOME),\n',
            "",
            "share one Cargo home",
        ),
        (
            "locked fetch",
            '["cargo", "fetch", "--locked"]',
            '["cargo", "fetch"]',
            "dependency fetch must be locked",
        ),
        (
            "offline locked builds",
            '["cargo", "build", "--locked", "--offline", "--release"]',
            '["cargo", "build", "--release"]',
            "both builds must be locked, offline",
        ),
        (
            "source remaps",
            '            f"--remap-path-prefix={SOURCE_B}=/workspace",\n',
            "",
            "all source paths must be remapped",
        ),
        (
            "target remaps",
            '            f"--remap-path-prefix={TARGET_B}=/target",\n',
            "",
            "both target paths must be remapped",
        ),
        (
            "cache remap",
            '            f"--remap-path-prefix={CARGO_HOME}=/cargo-home",\n',
            "",
            "shared Cargo cache path must be remapped",
        ),
        (
            "fixed environment",
            '            "TZ": "UTC",\n',
            "",
            "fixed environment contract",
        ),
        (
            "fixed codegen",
            '            "codegen-units=1",\n',
            '            "codegen-units=2",\n',
            "code generation must be fixed",
        ),
        (
            "per-process timeout",
            "        output, _ = process.communicate(timeout=timeout)",
            "        output, _ = process.communicate()",
            "every process must use the timeout runner",
        ),
        (
            "process-group kill",
            "            os.killpg(process.pid, signal.SIGKILL)",
            "            process.kill()",
            "kill the process group",
        ),
        (
            "byte comparison",
            "filecmp.cmp(binary_a, binary_b, shallow=False)",
            "filecmp.cmp(binary_a, binary_b, shallow=True)",
            "compared byte-for-byte",
        ),
        (
            "environment digest JSON",
            "environment_digest = canonical_digest(environment)",
            'environment_digest = "unbound"',
            "canonical digest must be written as JSON",
        ),
        (
            "canonical environment digest",
            'json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")',
            'json.dumps(value, sort_keys=False, separators=(",", ":")).encode("utf-8")',
            "environment digest must use canonical JSON",
        ),
        (
            "complete environment evidence",
            '            "cargo_lock_sha256": lock_digest,\n',
            "",
            "environment JSON must bind the lock",
        ),
        (
            "deterministic JSON evidence",
            'path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\\n", encoding="utf-8")',
            'path.write_text(str(value), encoding="utf-8")',
            "serialized as deterministic JSON",
        ),
        (
            "artifact digest JSON",
            '            "environment_sha256": environment_digest,\n',
            "",
            "artifact digests and environment digest must be written as JSON",
        ),
        (
            "distinct artifact digest",
            "digest_b = sha256(binary_b)",
            "digest_b = sha256(binary_a)",
            "artifact digests and environment digest must be written as JSON",
        ),
        (
            "cleanup implementation",
            "        shutil.rmtree(WORK)",
            "        return",
            "cleaned before staging and in finally",
        ),
        (
            "cleanup",
            '        stage("cleanup-transient-work")\n        clean_work()',
            '        stage("cleanup-transient-work")',
            "cleaned before staging and in finally",
        ),
    )
    for label, old, new, diagnostic in repro_mutations:
        if repro_source.count(old) != 1:
            failures.append(f"negative test setup failed: {label} mutation source was not unique")
            continue
        altered_source = repro_source.replace(old, new, 1)
        repro_errors = []
        validate_reproducible_build(altered_source, repro_lane, repro_errors)
        if not any(diagnostic in error for error in repro_errors):
            failures.append(f"negative test failed: removal of reproducible-build {label} was accepted")

    bad_url = copy.deepcopy(document)
    pin_map(bad_url)["tool.act"]["evidence"] = "file:///tmp/act"
    if not any("authoritative host" in error for error in validate(bad_url, repository=False)):
        failures.append("negative test failed: file URL evidence was accepted")

    act = load_manifest(ROOT / "docs/compatibility/pins/act.yaml")
    for unsafe_url in ("file:///tmp/act.tar.gz", "https://example.com/act.tar.gz"):
        unsafe_act = copy.deepcopy(act)
        unsafe_act["platforms"][0]["source_url"] = unsafe_url
        act_errors = []
        validate_act(pin_map(document), act_errors, unsafe_act)
        if not any("authoritative host" in error for error in act_errors):
            failures.append(f"negative test failed: act accepted unsafe source URL {unsafe_url!r}")

    with tempfile.TemporaryDirectory() as directory:
        snapshot = Path(directory)
        (snapshot / "listed.rs").write_text("fn listed() {}\n", encoding="utf-8")
        expected = {"listed.rs"}
        if set(snapshot_entries(snapshot)) != expected:
            failures.append("negative test setup failed: snapshot enumeration missed listed source")
        (snapshot / "target").mkdir()
        (snapshot / "target" / "generated").write_bytes(b"build output")
        if set(snapshot_entries(snapshot, ["target"])) != expected:
            failures.append("negative test failed: declared generated target root was not ignored")
        (snapshot / "build.rs").write_text("fn main() {}\n", encoding="utf-8")
        (snapshot / "asset.bin").write_bytes(b"asset")
        if set(snapshot_entries(snapshot, ["target"])) == expected:
            failures.append("negative test failed: unlisted snapshot payload was accepted")

    fake = copy.deepcopy(document)
    next(record for record in fake["pins"] if record["status"] == "blocked")["blocked_by"] = (
        "BLK-99@docs/compatibility/pins/unresolved.md#missing-anchor"
    )
    if not validate(fake, repository=False):
        failures.append("negative test failed: fake blocker was accepted")

    try:
        load_yaml_text(text + "\nschema_version: 1\n", "duplicate-key fixture")
    except ValueError:
        pass
    else:
        failures.append("negative test failed: duplicate YAML key was accepted")
    for label, malicious in (
        ("merge key", "base: &base {status: pinned}\nrecord: {<<: *base, status: blocked}\n"),
        ("boolean-equivalent key", "mapping: {yes: 1, true: 2}\n"),
        ("numeric-equivalent key", "mapping: {1: one, 01: octal}\n"),
    ):
        try:
            load_yaml_text(malicious, f"{label} fixture")
        except ValueError:
            pass
        else:
            failures.append(f"negative test failed: {label} was accepted")
    alias = load_yaml_text("value: &value [one, two]\ncopy: *value\n", "safe alias fixture")
    if alias["value"] != alias["copy"]:
        failures.append("positive test failed: unambiguous YAML value alias was rejected")
    return failures


def main(argv=None):
    global ROOT
    parser = argparse.ArgumentParser()
    parser.add_argument("manifest", nargs="?", type=Path)
    parser.add_argument("--root", type=Path, help="candidate source root (release validator use)")
    parser.add_argument("--release", action="store_true", help="reject every unresolved blocker")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.root:
        ROOT = args.root.resolve()
    manifest = args.manifest or ROOT / "docs/compatibility/build-manifest.yaml"
    try:
        errors = self_test(manifest) if args.self_test else validate(load_manifest(manifest), args.release)
    except (OSError, UnicodeError, ValueError, tomllib.TOMLDecodeError) as error:
        print(error, file=sys.stderr)
        return 2
    if errors:
        for error in errors:
            print(f"{manifest}: {error}", file=sys.stderr)
        return 1
    if args.self_test:
        print("pin verifier self-test passed: rogue write job/command, corrupt digest, floating Rust, reproducibility-pin removal, reproducible-build contract mutations, unlisted payload, unsafe URL, unsafe act URL, fake blocker, release blockers, duplicate/equivalent/merge keys rejected, and safe value alias accepted")
    else:
        print(f"pin manifest verified ({'release' if args.release else 'normal'} mode): {manifest}")
        print("unpinned.mutable_protocols=0")
        print("unpinned.mutable_datasets=0")
        print("unpinned.mutable_models=0")
        print("unpinned.mutable_harnesses=0")
    return 0


if __name__ == "__main__":
    sys.exit(main())
