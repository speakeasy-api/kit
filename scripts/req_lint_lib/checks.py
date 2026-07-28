"""The 6 named rejection checks required by IMPLEMENTATION_PLAN.md:136-143.

Each check has a single, distinct reason string. `--list-checks` prints
exactly these 6 strings, one per line; the same strings are attached to
findings emitted while linting a registry.
"""

import os
from collections import namedtuple

from .loader import parse_source_anchor
from .model import ID_TOKEN_PATTERN, TOMBSTONED_STATUSES
from .rfc_scan import fingerprint_line, read_lines, scan_normative_lines
from .validate import record_label

Finding = namedtuple("Finding", ["reason", "message"])

REASON_UNREGISTERED_NORMATIVE_TEXT = "unregistered-normative-text"
REASON_DUPLICATE_ID = "duplicate-requirement-id"
REASON_MISSING_GOVERNANCE_FIELDS = "missing-governance-fields"
REASON_UNKNOWN_CITATION = "unknown-requirement-citation"
REASON_TOMBSTONE_WITHOUT_REPLACEMENT = "tombstone-without-replacement"
REASON_MISSING_STALE_OR_FAILING_EVIDENCE = "missing-stale-or-failing-evidence"


def _is_live(record):
    return record.get("status") not in TOMBSTONED_STATUSES


def check_unregistered_normative_text(records, rfc_path, start, end):
    """IMPLEMENTATION_PLAN.md:138 — unregistered or changed normative text."""
    covering = {}
    live_fingerprints = {}
    source_lines = read_lines(rfc_path)
    for record in records:
        if not _is_live(record):
            continue
        parsed = parse_source_anchor(record.get("source_anchor"))
        if parsed is None:
            continue
        source_file, sec_start, sec_end = parsed
        if os.path.basename(source_file) != os.path.basename(rfc_path):
            continue
        anchored_text = "".join(source_lines[sec_start - 1 : sec_end])
        live_fingerprints[id(record)] = fingerprint_line(anchored_text)
        for lineno in range(sec_start, sec_end + 1):
            covering.setdefault(lineno, []).append(record)

    findings = []
    for lineno, text in scan_normative_lines(rfc_path, start, end):
        matches = covering.get(lineno, [])
        if not matches:
            findings.append(
                Finding(
                    REASON_UNREGISTERED_NORMATIVE_TEXT,
                    "%s:%d unregistered normative text: %r"
                    % (rfc_path, lineno, text.strip()),
                )
            )
            continue
        if not any(
            match.get("source_fingerprint") == live_fingerprints[id(match)]
            for match in matches
        ):
            findings.append(
                Finding(
                    REASON_UNREGISTERED_NORMATIVE_TEXT,
                    "%s:%d normative text changed since it was registered "
                    "(recorded by %s): %r"
                    % (
                        rfc_path,
                        lineno,
                        ", ".join(record_label(m) for m in matches),
                        text.strip(),
                    ),
                )
            )
    return findings


def check_duplicate_ids(records):
    """IMPLEMENTATION_PLAN.md:139 — duplicate or reused IDs."""
    by_id = {}
    for record in records:
        rec_id = record.get("id")
        if not rec_id:
            continue
        by_id.setdefault(rec_id, []).append(record)

    findings = []
    for rec_id, group in by_id.items():
        if len(group) > 1:
            findings.append(
                Finding(
                    REASON_DUPLICATE_ID,
                    "%s used by %d records (%s)"
                    % (
                        rec_id,
                        len(group),
                        ", ".join(g.get("_source_path", "?") for g in group),
                    ),
                )
            )
    return findings


def check_missing_governance_fields(records):
    """IMPLEMENTATION_PLAN.md:140 — live requirements without owner,
    milestone, acceptance criteria, and evidence plan."""
    findings = []
    for record in records:
        if not _is_live(record):
            continue
        missing = [
            field
            for field in ("owner", "primary_milestone", "acceptance_criteria")
            if not record.get(field)
        ]
        evidence_fields = (
            "evidence_type",
            "evidence_id",
            "evidence_job",
            "expected_result",
            "revalidation_rule",
        )
        if any(not record.get(field) for field in evidence_fields):
            missing.append("evidence_plan")
        if missing:
            findings.append(
                Finding(
                    REASON_MISSING_GOVERNANCE_FIELDS,
                    "%s: live requirement missing %s"
                    % (record_label(record), ", ".join(missing)),
                )
            )
    return findings


def check_unknown_citations(records, scan_dirs, excluded_dirs=()):
    """IMPLEMENTATION_PLAN.md:141 — tests/evaluations citing unknown IDs."""
    known_ids = {r.get("id") for r in records if r.get("id")}
    excluded = {os.path.realpath(path) for path in excluded_dirs}
    findings = []
    for scan_dir in scan_dirs:
        if not os.path.isdir(scan_dir):
            continue
        for root, dirs, files in os.walk(scan_dir):
            dirs[:] = [
                name
                for name in dirs
                if os.path.realpath(os.path.join(root, name)) not in excluded
            ]
            for name in files:
                path = os.path.join(root, name)
                try:
                    with open(path, "r", encoding="utf-8", errors="ignore") as fh:
                        text = fh.read()
                except OSError:
                    continue
                for token in set(ID_TOKEN_PATTERN.findall(text)):
                    if token not in known_ids:
                        findings.append(
                            Finding(
                                REASON_UNKNOWN_CITATION,
                                "%s cites unknown requirement id %s" % (path, token),
                            )
                        )
    return findings


def check_tombstone_without_replacement(records):
    """IMPLEMENTATION_PLAN.md:142 — tombstoned requirements without a
    replacement or decision record."""
    findings = []
    for record in records:
        if record.get("status") not in TOMBSTONED_STATUSES:
            continue
        if not record.get("supersedes") and not record.get("decision_record"):
            findings.append(
                Finding(
                    REASON_TOMBSTONE_WITHOUT_REPLACEMENT,
                    "%s: tombstoned without supersedes or decision_record"
                    % record_label(record),
                )
            )
    return findings


def check_missing_stale_or_failing_evidence(records):
    """IMPLEMENTATION_PLAN.md:143 — release candidates with missing, stale,
    or failing evidence."""
    findings = []
    for record in records:
        if not _is_live(record):
            continue
        missing = [
            field
            for field in (
                "evidence_type",
                "evidence_id",
                "evidence_job",
                "expected_result",
                "artifact_digest",
                "environment_digest",
                "versions",
            )
            if not record.get(field)
        ]
        if missing:
            findings.append(
                Finding(
                    REASON_MISSING_STALE_OR_FAILING_EVIDENCE,
                    "%s: missing evidence: %s"
                    % (record_label(record), ", ".join(missing)),
                )
            )
        result = str(record.get("latest_result") or "").strip().lower()
        if result == "pass":
            continue
        if result in ("stale", "expired"):
            findings.append(
                Finding(
                    REASON_MISSING_STALE_OR_FAILING_EVIDENCE,
                    "%s: latest_result is stale (%s)" % (record_label(record), result),
                )
            )
        elif result in ("fail", "failed", "failing"):
            findings.append(
                Finding(
                    REASON_MISSING_STALE_OR_FAILING_EVIDENCE,
                    "%s: latest_result is failing (%s)"
                    % (record_label(record), result),
                )
            )
        else:
            findings.append(
                Finding(
                    REASON_MISSING_STALE_OR_FAILING_EVIDENCE,
                    "%s: latest_result is missing or not passing (%s)"
                    % (record_label(record), result or "unset"),
                )
            )
    return findings


# Ordered (name, reason) pairs — the source of truth for `--list-checks`.
CHECK_REASONS = (
    ("unregistered-or-changed-normative-text", REASON_UNREGISTERED_NORMATIVE_TEXT),
    ("duplicate-or-reused-ids", REASON_DUPLICATE_ID),
    ("missing-governance-fields", REASON_MISSING_GOVERNANCE_FIELDS),
    ("unknown-requirement-citation", REASON_UNKNOWN_CITATION),
    ("tombstone-without-replacement", REASON_TOMBSTONE_WITHOUT_REPLACEMENT),
    ("missing-stale-or-failing-evidence", REASON_MISSING_STALE_OR_FAILING_EVIDENCE),
)

assert len(CHECK_REASONS) == 6
assert len({reason for _name, reason in CHECK_REASONS}) == 6
