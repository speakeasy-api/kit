#!/usr/bin/env python3
"""Validate the twelve milestone evidence dashboards against the plan."""

import argparse
from pathlib import Path
import re
import sys

from yaml_utils import YamlLoadError, load_yaml_file


ROOT = Path(__file__).resolve().parents[1]
PLAN = ROOT / "IMPLEMENTATION_PLAN.md"
DASHBOARDS = ROOT / "requirements" / "dashboards"
GATES = {
    "G01": ("KIT-MILESTONE-001", 322, 336),
    "G02": ("KIT-MILESTONE-002", 366, 375),
    "G03": ("KIT-MILESTONE-003", 407, 416),
    "G04": ("KIT-MILESTONE-004", 449, 458),
    "G05": ("KIT-MILESTONE-005", 491, 498),
    "G06": ("KIT-MILESTONE-006", 530, 537),
    "G07": ("KIT-MILESTONE-007", 569, 576),
    "G08": ("KIT-MILESTONE-008", 609, 617),
    "G09": ("KIT-MILESTONE-009", 647, 654),
    "G10": ("KIT-MILESTONE-010", 685, 692),
    "G11": ("KIT-MILESTONE-011", 725, 734),
    "G12": ("KIT-MILESTONE-012", 769, 780),
}
ROW_KEYS = {
    "source_line",
    "source_ref",
    "requirement_areas",
    "evidence_id",
    "evidence_job",
    "expected_result",
    "current_status",
}
LIFECYCLE_STATUSES = {
    "pending",
    "in_progress",
    "passed",
    "failed",
    "blocked",
    "blocked_external",
    "blocked_transitive",
}


def fail(errors, path, message):
    errors.append(f"{path}: {message}")


def main(argv=None):
    global ROOT, PLAN, DASHBOARDS
    parser = argparse.ArgumentParser()
    parser.add_argument("--release", action="store_true", help="require every gate and row to be passed")
    parser.add_argument("--root", type=Path, help="candidate source root (release validator use)")
    args = parser.parse_args(argv)
    if args.root:
        ROOT = args.root.resolve()
        PLAN = ROOT / "IMPLEMENTATION_PLAN.md"
        DASHBOARDS = ROOT / "requirements" / "dashboards"
    errors = []
    plan_lines = PLAN.read_text(encoding="utf-8").splitlines()
    known_areas = set(re.findall(r"KIT-[A-Z0-9]+", "\n".join(plan_lines[86:117])))
    expected_files = {f"{gate}.yaml" for gate in GATES}
    actual_files = {path.name for path in DASHBOARDS.glob("*.yaml")}
    if actual_files != expected_files:
        missing = sorted(expected_files - actual_files)
        extra = sorted(actual_files - expected_files)
        fail(errors, DASHBOARDS, f"expected exactly 12 gates; missing={missing}, extra={extra}")

    seen_ids = set()
    evidence_rows = 0
    for gate, (milestone, start, end) in GATES.items():
        path = DASHBOARDS / f"{gate}.yaml"
        if not path.is_file():
            continue
        try:
            document = load_yaml_file(path)
        except (OSError, YamlLoadError) as exc:
            fail(errors, path, f"cannot load YAML: {exc}")
            continue
        if not isinstance(document, dict):
            fail(errors, path, "dashboard must be a mapping")
            continue

        expected_header = {
            "gate": gate,
            "milestone": milestone,
            "source_range": {
                "source": "IMPLEMENTATION_PLAN.md",
                "start_line": start,
                "end_line": end,
            },
        }
        for key, expected in expected_header.items():
            if document.get(key) != expected:
                fail(errors, path, f"{key} must be {expected!r}")
        gate_status = document.get("current_status")
        if gate_status not in LIFECYCLE_STATUSES:
            fail(errors, path, f"current_status must be one of {sorted(LIFECYCLE_STATUSES)}")

        expected_bullets = []
        for line_number in range(start, end + 1):
            text = plan_lines[line_number - 1]
            if text.startswith("- "):
                expected_bullets.append((line_number, text[2:]))
        rows = document.get("evidence")
        if not isinstance(rows, list):
            fail(errors, path, "evidence must be a list")
            continue
        evidence_rows += len(rows)
        if len(rows) != len(expected_bullets):
            fail(errors, path, f"expected {len(expected_bullets)} evidence rows, found {len(rows)}")

        gate_areas = set(re.findall(r"KIT-[A-Z0-9]+", expected_bullets[-1][1]))
        represented_areas = set()
        for index, expected in enumerate(expected_bullets, 1):
            if index > len(rows):
                break
            row = rows[index - 1]
            label = f"{path}:evidence[{index}]"
            if not isinstance(row, dict):
                fail(errors, label, "row must be a mapping")
                continue
            if set(row) != ROW_KEYS:
                fail(errors, label, f"fields must be exactly {sorted(ROW_KEYS)}")
            line_number, bullet = expected
            checks = {
                "source_line": line_number,
                "source_ref": f"IMPLEMENTATION_PLAN.md:{line_number}",
                "evidence_id": f"EV-{gate}-{index:03d}",
                "evidence_job": f"dashboard-{gate.lower()}",
                "expected_result": bullet,
            }
            for key, value in checks.items():
                if row.get(key) != value:
                    fail(errors, label, f"{key} must be {value!r}")
            row_status = row.get("current_status")
            if row_status not in LIFECYCLE_STATUSES:
                fail(errors, label, f"current_status must be one of {sorted(LIFECYCLE_STATUSES)}")
            if args.release and row_status != "passed":
                fail(errors, label, "current_status must be 'passed' in release mode")
            evidence_id = row.get("evidence_id")
            if evidence_id in seen_ids:
                fail(errors, label, f"duplicate evidence_id {evidence_id!r}")
            elif isinstance(evidence_id, str):
                seen_ids.add(evidence_id)
            areas = row.get("requirement_areas")
            if not isinstance(areas, list) or not areas or any(not isinstance(area, str) for area in areas):
                fail(errors, label, "requirement_areas must be a non-empty string list")
            else:
                area_set = set(areas)
                represented_areas.update(area_set)
                if len(area_set) != len(areas):
                    fail(errors, label, "requirement_areas contains duplicates")
                if not area_set <= known_areas:
                    fail(errors, label, f"unknown requirement areas: {sorted(area_set - known_areas)}")
                if index == len(expected_bullets) and area_set != gate_areas:
                    fail(errors, label, "final applicability row must map every declared gate area")
        if not gate_areas <= represented_areas:
            fail(errors, path, f"missing declared areas: {sorted(gate_areas - represented_areas)}")
        row_statuses = {row.get("current_status") for row in rows if isinstance(row, dict)}
        if gate_status == "passed" and row_statuses != {"passed"}:
            fail(errors, path, "a passed gate requires every evidence row to be passed")
        if gate_status == "pending" and row_statuses - {"pending"}:
            fail(errors, path, "a pending gate cannot contain started or terminal evidence")
        if gate_status in {"failed", "blocked", "blocked_external"} and gate_status not in row_statuses:
            fail(errors, path, f"a {gate_status} gate requires a {gate_status} evidence row")
        if gate_status == "blocked_transitive" and row_statuses != {"passed"}:
            fail(errors, path, "a transitively blocked gate requires all local evidence to pass")
        if gate_status == "in_progress" and not row_statuses & {
            "in_progress",
            "passed",
            "blocked_external",
        }:
            fail(errors, path, "an in_progress gate requires started evidence")
        if args.release and gate_status != "passed":
            fail(errors, path, "current_status must be 'passed' in release mode")

    if errors:
        print("dashboard lint failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    mode = "release" if args.release else "lifecycle"
    print(f"dashboard lint passed: {len(GATES)} gates, {evidence_rows} exit-evidence bullets, {mode} mode")
    return 0


if __name__ == "__main__":
    sys.exit(main())
