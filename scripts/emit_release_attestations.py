#!/usr/bin/env python3
"""Emit exact seven-field release attestations for retained external evidence."""

import argparse
import hashlib
import json
import os
import re
import sys
from pathlib import Path

from req_lint_lib.loader import discover_shards, load_registry_dir


def digest_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def digest_artifact(path):
    if path.is_symlink():
        raise ValueError("artifact must not be a symlink")
    if path.is_file():
        return digest_file(path)
    if not path.is_dir():
        raise ValueError("artifact must be a file or directory")
    records = []
    for child in sorted(path.rglob("*")):
        if child.is_symlink():
            raise ValueError(f"artifact entry must not be a symlink: {child}")
        if child.is_file():
            records.append(f"{digest_file(child)}  {child.relative_to(path).as_posix()}")
    if not records:
        raise ValueError("artifact directory is empty")
    return hashlib.sha256(("\n".join(records) + "\n").encode()).hexdigest()


def require_production_stats(path):
    names = {
        "preregistration.json",
        "registered-preregistration.json",
        "statistical-report.json",
        "statistical-report-receipt.json",
    }
    missing = sorted(name for name in names if not (path / name).is_file())
    if missing:
        raise ValueError(f"production statistics bundle is missing {missing}")
    report = json.loads((path / "statistical-report.json").read_text(encoding="utf-8"))
    receipt = json.loads(
        (path / "statistical-report-receipt.json").read_text(encoding="utf-8")
    )
    if report.get("evidence_source") != "production_trusted":
        raise ValueError("statistical report is not ProductionTrusted")
    if receipt.get("evidence_source") != "production_trusted":
        raise ValueError("statistical report receipt is not ProductionTrusted")
    expected = f"sha256:{digest_file(path / 'statistical-report.json')}"
    if receipt.get("report_digest") != expected:
        raise ValueError("statistical report receipt does not bind report bytes")


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path("."))
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--environment", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--commit-sha", required=True)
    parser.add_argument("--workflow-ref", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--evidence-prefix")
    parser.add_argument("--evidence-id", action="append", default=[])
    parser.add_argument("--filename-prefix", default="external")
    parser.add_argument("--require-production-stats", action="store_true")
    args = parser.parse_args(argv)

    root = args.root.resolve()
    artifact = args.artifact.resolve()
    environment = args.environment.resolve()
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit_sha):
        raise ValueError("commit SHA must be 40 lowercase hexadecimal characters")
    if not environment.is_file() or environment.is_symlink():
        raise ValueError("environment must be a regular file")
    if args.require_production_stats:
        require_production_stats(artifact)

    records, _ = load_registry_dir(
        root / "requirements/registry.d",
        discover_shards(root / "requirements/registry.d"),
    )
    selected_ids = set(args.evidence_id)
    selected = {
        (record["evidence_id"], record["evidence_job"])
        for record in records
        if record.get("primary_milestone") == "M004"
        and (
            record.get("evidence_id") in selected_ids
            or args.evidence_prefix
            and record.get("evidence_id", "").startswith(args.evidence_prefix)
        )
    }
    if not selected:
        raise ValueError("no M004 registry evidence matched")

    artifact_digest = digest_artifact(artifact)
    environment_digest = digest_file(environment)
    args.output.mkdir(parents=True, exist_ok=True)
    for evidence_id, evidence_job in sorted(selected):
        document = {
            "artifact_digest": artifact_digest,
            "commit_sha": args.commit_sha,
            "environment_digest": environment_digest,
            "evidence_id": evidence_id,
            "evidence_job": evidence_job,
            "run_id": args.run_id,
            "workflow_ref": args.workflow_ref,
        }
        path = args.output / f"{args.filename_prefix}-{evidence_id}.json"
        path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(
        f"emitted {len(selected)} release attestations; artifact_sha256={artifact_digest}; "
        f"environment_sha256={environment_digest}"
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (KeyError, OSError, TypeError, ValueError) as error:
        print(f"release attestation error: {error}", file=sys.stderr)
        sys.exit(1)
