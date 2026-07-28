"""Inventory, semantic, history, and release governance checks."""

import hashlib
import json
import os
import re
import stat
import subprocess
from pathlib import Path

from .checks import (
    Finding,
    REASON_DUPLICATE_ID,
    REASON_MISSING_GOVERNANCE_FIELDS,
    REASON_MISSING_STALE_OR_FAILING_EVIDENCE,
    REASON_UNREGISTERED_NORMATIVE_TEXT,
)
from .loader import RegistryError, load_yaml_document, load_yaml_string, parse_source_anchor
from .model import EVIDENCE_CODES, TOMBSTONED_STATUSES
from .rfc_scan import fingerprint_line, read_lines
from .validate import record_label


MAPPED_CLASSIFICATIONS = {
    "normative_requirement": {"requirement"},
    "testable_promise": {"promise", "decision", "risk"},
}
COVERAGE_ONLY_CLASSIFICATIONS = {"context", "example", "observation", "reference"}
TERMINAL_STATUSES = {"implemented", "resolved_by_amendment", "not_selected", "mitigated"}
DECIDABLE_PATTERN = re.compile(
    r"^(?=.*(?:`|\b(?:cargo|python3|scripts/|checks?|reports?|evaluations?|audits?|suites?|tests?|commands?|queries?|builds?)\b))"
    r"(?=.*(?:\b(?:exit|exits|equals?|reports?|contains?|matches?|rejects?|denies?|denied|"
    r"passes?|fails?|present|absent|selected|true|false|every|all|none|no)\b|\d|sha-?256)).+$",
    re.IGNORECASE,
)
EVIDENCE_CODE_PATTERN = re.compile(r"-(C|E|O|M)-[0-9]+$")
SECTION_PATTERN = re.compile(r"^## (\d+)\.")
OPTIONAL_POLICY_FIELDS = {
    "id", "name", "requirement_id", "source", "gate", "disposition",
    "experiment_id", "selection_rule", "fallback", "evidence",
}
FULL_SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")


def identity_digest(record):
    identity = {
        key: record.get(key)
        for key in (
            "id",
            "record_class",
            "area",
            "source_anchor",
            "source_quote",
            "source_fingerprint",
            "atomic_text",
        )
    }
    encoded = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _finding(reason, message):
    return Finding(reason, message)


def _git_commit(ref, root):
    result = subprocess.run(
        ["git", "rev-parse", "--verify", f"{ref}^{{commit}}"],
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    return result.stdout.strip() if result.returncode == 0 else None


def _git_file(commit, path, root):
    result = subprocess.run(
        ["git", "show", f"{commit}:{path}"],
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    return result.stdout if result.returncode == 0 else None


def _git_is_ancestor(ancestor, candidate, root):
    return subprocess.run(
        ["git", "merge-base", "--is-ancestor", ancestor, candidate],
        cwd=root,
        capture_output=True,
        check=False,
    ).returncode == 0


def _git_status(root):
    result = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=all"],
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    return None if result.returncode else result.stdout


def check_inventory(records, rfc_path, inventory_path, root="."):
    document = load_yaml_document(inventory_path)
    if not isinstance(document, dict) or not isinstance(document.get("atoms"), list):
        raise RegistryError(f"{inventory_path}: expected an atoms list")
    lines = read_lines(rfc_path)
    findings = []
    source_revision = document.get("source_revision")
    source_commit = _git_commit(source_revision, root) if isinstance(source_revision, str) else None
    historical_rfc = _git_file(source_commit, document.get("source"), root) if source_commit else None
    if source_commit is None or historical_rfc is None:
        findings.append(
            _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{inventory_path}: source_revision does not resolve to an RFC commit")
        )
    elif hashlib.sha256(historical_rfc.encode("utf-8")).digest() != hashlib.sha256(
        "".join(lines).encode("utf-8")
    ).digest():
        findings.append(
            _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{inventory_path}: source_revision RFC digest differs from the inventoried RFC")
        )
    if document.get("source_line_count") != len(lines):
        findings.append(
            _finding(
                REASON_UNREGISTERED_NORMATIVE_TEXT,
                f"{inventory_path}: source_line_count does not match {rfc_path}",
            )
        )

    by_record = {}
    inventory_ids = set()
    covered_lines = set()
    counts = {}
    for index, atom in enumerate(document["atoms"], 1):
        label = f"{inventory_path}:atoms[{index}]"
        if not isinstance(atom, dict):
            findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: not a mapping"))
            continue
        inventory_id = atom.get("inventory_id")
        if not isinstance(inventory_id, str) or inventory_id in inventory_ids:
            findings.append(
                _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: duplicate or invalid inventory_id")
            )
        inventory_ids.add(inventory_id)
        classification = atom.get("classification")
        counts[classification] = counts.get(classification, 0) + 1
        parsed = parse_source_anchor(atom.get("source_anchor"))
        if parsed is None:
            findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: invalid source_anchor"))
            continue
        source_file, start, end = parsed
        if os.path.basename(source_file) != os.path.basename(rfc_path) or end > len(lines):
            findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: source_anchor is outside the RFC"))
            continue
        source_text = "".join(lines[start - 1 : end]).strip()
        covered_lines.update(range(start, end + 1))
        quote = atom.get("source_quote")
        if not isinstance(quote, str) or quote not in source_text:
            findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: source_quote is not verbatim within its anchor"))
        if atom.get("source_fingerprint") != fingerprint_line(source_text):
            findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: source_fingerprint is stale"))

        record_id = atom.get("record_id")
        if classification in MAPPED_CLASSIFICATIONS:
            if not isinstance(record_id, str):
                findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: mapped atom has no record_id"))
            else:
                by_record.setdefault(record_id, []).append(atom)
        elif classification in COVERAGE_ONLY_CLASSIFICATIONS:
            if start != end:
                findings.append(
                    _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: coverage-only atom must cover exactly one source line")
                )
            if not source_text:
                findings.append(
                    _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: coverage-only atom covers a blank source line")
                )
            if quote != source_text:
                findings.append(
                    _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: coverage-only source_quote must equal the complete source line")
                )
            if record_id is not None:
                findings.append(
                    _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: coverage-only atom maps a record")
                )
        else:
            findings.append(
                _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: unknown classification {classification!r}")
            )

    summary_counts = document.get("classification_counts")
    if summary_counts != counts:
        findings.append(
            _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{inventory_path}: classification_counts are stale")
        )
    uncovered = [number for number, text in enumerate(lines, 1) if text.strip() and number not in covered_lines]
    if uncovered:
        findings.append(
            _finding(
                REASON_UNREGISTERED_NORMATIVE_TEXT,
                f"{inventory_path}: {len(uncovered)} nonblank RFC lines are not inventoried",
            )
        )
    record_by_id = {
        record.get("id"): record
        for record in records
        if record.get("id") and record.get("status") not in TOMBSTONED_STATUSES
    }
    revision_rfcs = {}
    nonblank = sum(bool(line.strip()) for line in lines)
    expected_coverage = {
        "covered_nonblank_lines": nonblank - len(uncovered),
        "uncovered_nonblank_lines": len(uncovered),
        "registry_records_mapped": sum(len(atoms) for atoms in by_record.values()),
        "unmapped_registry_records": len(set(record_by_id) - set(by_record)),
    }
    if document.get("nonblank_line_count") != nonblank or document.get("coverage") != expected_coverage:
        findings.append(
            _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{inventory_path}: coverage summary is stale")
        )

    for record_id, record in record_by_id.items():
        atoms = by_record.get(record_id, [])
        if len(atoms) != 1:
            findings.append(
                _finding(
                    REASON_UNREGISTERED_NORMATIVE_TEXT,
                    f"{record_id}: expected exactly one inventory atom, found {len(atoms)}",
                )
            )
            continue
        atom = atoms[0]
        if record.get("record_class") not in MAPPED_CLASSIFICATIONS.get(atom.get("classification"), set()):
            findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{record_id}: incompatible inventory class"))
        for field in (
            "record_class",
            "source_section",
            "source_anchor",
            "source_quote",
            "source_fingerprint",
            "atomic_text",
        ):
            if atom.get(field) != record.get(field):
                findings.append(
                    _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{record_id}: inventory {field} differs")
                )
        introduced = record.get("introduced_revision")
        if introduced not in revision_rfcs:
            commit = _git_commit(introduced, root) if isinstance(introduced, str) else None
            revision_rfcs[introduced] = _git_file(commit, "RFC.md", root) if commit else None
        introduced_rfc = revision_rfcs[introduced]
        parsed = parse_source_anchor(record.get("source_anchor"))
        if introduced_rfc is None or parsed is None:
            findings.append(
                _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{record_id}: introduced_revision does not resolve to an RFC commit")
            )
        else:
            _source, start, end = parsed
            introduced_lines = introduced_rfc.splitlines(True)
            introduced_text = "".join(introduced_lines[start - 1 : end]).strip()
            if end > len(introduced_lines) or record.get("source_quote") not in introduced_text:
                findings.append(
                    _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{record_id}: requirement is absent at introduced_revision")
                )
    for record_id in sorted(set(by_record) - set(record_by_id)):
        findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{record_id}: inventory record is absent"))

    headings = {
        int(match.group(1))
        for line in lines
        if (match := SECTION_PATTERN.match(line)) is not None
    }
    if headings != set(range(1, 38)):
        findings.append(
            _finding(REASON_UNREGISTERED_NORMATIVE_TEXT, "RFC/inventory does not cover exactly sections 1 through 37")
        )
    return findings


def _dashboard_statuses(dashboard_dir):
    statuses = {}
    path = Path(dashboard_dir)
    if not path.is_dir():
        return statuses
    for dashboard in sorted(path.glob("G*.yaml")):
        document = load_yaml_document(dashboard)
        if isinstance(document, dict) and isinstance(document.get("gate"), str):
            statuses[document["gate"]] = document.get("current_status")
    return statuses


def check_semantics(records, rfc_path, optional_policy_path=None, dashboard_dir=None):
    findings = []
    lines = read_lines(rfc_path)
    known_ids = {record.get("id") for record in records}
    policy_by_requirement = {}
    mechanism_ids = set()
    experiment_ids = set()
    policy_decision = None
    if optional_policy_path and os.path.isfile(optional_policy_path):
        policy = load_yaml_document(optional_policy_path)
        if not isinstance(policy, dict) or set(policy) != {
            "schema_version", "decision_record", "allowed_dispositions", "mechanisms"
        } or policy.get("schema_version") != 1 or not isinstance(policy.get("mechanisms"), list):
            raise RegistryError(f"{optional_policy_path}: expected a mechanisms list")
        if policy.get("allowed_dispositions") != ["selected", "not_selected", "pending_voi"]:
            findings.append(
                _finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{optional_policy_path}: allowed_dispositions is invalid")
            )
        policy_decision = policy.get("decision_record")
        for mechanism in policy["mechanisms"]:
            requirement_id = mechanism.get("requirement_id") if isinstance(mechanism, dict) else None
            if not isinstance(mechanism, dict) or set(mechanism) != OPTIONAL_POLICY_FIELDS:
                findings.append(
                    _finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{optional_policy_path}: mechanism fields must be exactly {sorted(OPTIONAL_POLICY_FIELDS)}")
                )
            if not requirement_id or requirement_id in policy_by_requirement:
                findings.append(
                    _finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{optional_policy_path}: duplicate or missing requirement_id")
                )
            else:
                policy_by_requirement[requirement_id] = mechanism
            if isinstance(mechanism, dict):
                for field, seen in (("id", mechanism_ids), ("experiment_id", experiment_ids)):
                    value = mechanism.get(field)
                    if not isinstance(value, str) or not value.strip():
                        continue
                    if value in seen:
                        findings.append(
                            _finding(
                                REASON_MISSING_GOVERNANCE_FIELDS,
                                f"{optional_policy_path}: duplicate {field} {value!r}",
                            )
                        )
                    seen.add(value)
    dashboard_statuses = _dashboard_statuses(dashboard_dir) if dashboard_dir else {}

    optional_records = set()
    for record in records:
        label = record_label(record)
        parsed = parse_source_anchor(record.get("source_anchor"))
        if parsed is not None and record.get("status") not in TOMBSTONED_STATUSES:
            source_file, start, end = parsed
            if os.path.basename(source_file) == os.path.basename(rfc_path) and end <= len(lines):
                source_text = "".join(lines[start - 1 : end]).strip()
                if record.get("source_quote") not in source_text:
                    findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: source_quote is not verbatim within its anchor"))
                if record.get("source_fingerprint") != fingerprint_line(source_text):
                    findings.append(_finding(REASON_UNREGISTERED_NORMATIVE_TEXT, f"{label}: source_fingerprint is stale"))

        record_class = record.get("record_class")
        modality = record.get("modality")
        if (record_class == "requirement") != (modality != "declarative"):
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: record_class and modality disagree"))

        criteria = record.get("acceptance_criteria") or []
        for criterion in criteria:
            if not isinstance(criterion, str) or not DECIDABLE_PATTERN.search(criterion.replace("\n", " ")):
                findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: acceptance criterion is not machine-decidable"))
        expected = record.get("expected_result")
        if not isinstance(expected, str) or not DECIDABLE_PATTERN.search(expected.replace("\n", " ")):
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: expected_result is not machine-decidable"))

        code_match = EVIDENCE_CODE_PATTERN.search(str(record.get("evidence_id") or ""))
        expected_code = EVIDENCE_CODES.get(record.get("evidence_type"))
        if code_match is None or code_match.group(1) != expected_code:
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: evidence_type and evidence_id disagree"))

        for dependency in record.get("dependencies") or []:
            if dependency not in known_ids:
                findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: unknown dependency {dependency}"))

        status = record.get("status")
        applicability = record.get("applicability")
        if status == "not_selected" and applicability != "not_selected":
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: not_selected status/applicability disagree"))
        if applicability == "not_selected" and status != "not_selected":
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: not_selected applicability/status disagree"))
        if applicability in {"selected", "not_selected", "pending_voi"}:
            optional_records.add(record.get("id"))
            mechanism = policy_by_requirement.get(record.get("id"))
            if mechanism is None:
                findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: optional disposition has no policy row"))
            elif mechanism.get("disposition") != applicability:
                findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: optional policy disposition differs"))
            else:
                parsed_source = parse_source_anchor(mechanism.get("source"))
                required_text = ("id", "name", "experiment_id", "selection_rule", "fallback")
                if parsed_source is None or any(not isinstance(mechanism.get(field), str) or not mechanism[field].strip() for field in required_text):
                    findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: optional policy metadata is incomplete"))
                if mechanism.get("source") != record.get("source_anchor"):
                    findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: optional policy source differs"))
                if mechanism.get("gate") not in (record.get("release_gates") or []):
                    findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: optional policy gate differs"))
                if applicability == "pending_voi" and mechanism.get("evidence") is not None:
                    findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: pending optional disposition has premature evidence"))
                if applicability in {"selected", "not_selected"}:
                    if not (record.get("decision_record") or policy_decision):
                        findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: closed optional disposition has no decision"))
                    if not isinstance(mechanism.get("evidence"), str) or not mechanism["evidence"].strip():
                        findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: closed optional disposition has no evidence"))
            if applicability == "pending_voi" and status not in {"proposed", "active"}:
                findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: pending_voi has a terminal status"))
            if applicability == "pending_voi" and mechanism and dashboard_statuses.get(mechanism.get("gate")) == "passed":
                findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: pending_voi remains after owning gate"))
        if applicability == "mandatory" and status == "not_selected":
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: mandatory record is not_selected"))
        if applicability == "not_applicable" and not record.get("decision_record"):
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: not_applicable has no decision record"))
        if status == "resolved_by_amendment" and not record.get("decision_record"):
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: resolved_by_amendment has no decision record"))
        if status == "mitigated" and record_class != "risk":
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: only risk records may be mitigated"))
        if record.get("deviation_record") and modality not in {"SHOULD", "SHOULD NOT"}:
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: deviation is only valid for SHOULD modalities"))
        if status in TOMBSTONED_STATUSES and record.get("id") in (record.get("supersedes"), record.get("dependencies") or []):
            findings.append(_finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{label}: tombstone self-reference"))

    for requirement_id in sorted(set(policy_by_requirement) - optional_records):
        findings.append(
            _finding(REASON_MISSING_GOVERNANCE_FIELDS, f"{requirement_id}: optional policy has no matching optional record")
        )
    return findings


def _load_baseline(ref, candidate_ref, root):
    if not FULL_SHA_PATTERN.fullmatch(str(ref)):
        raise RegistryError("baseline ref must be an explicit lowercase 40-character commit SHA")
    if not FULL_SHA_PATTERN.fullmatch(str(candidate_ref)):
        raise RegistryError("candidate ref must be an explicit lowercase 40-character commit SHA")
    commit = _git_commit(ref, root)
    if commit is None:
        raise RegistryError(f"baseline ref {ref!r} does not resolve to a Git commit")
    candidate = _git_commit(candidate_ref, root)
    if candidate is None:
        raise RegistryError(f"candidate ref {candidate_ref!r} does not resolve to a Git commit")
    if commit == candidate:
        raise RegistryError("baseline and candidate commits must be distinct")
    if not _git_is_ancestor(commit, candidate, root):
        raise RegistryError("baseline commit must be an ancestor of the candidate commit")
    text = _git_file(commit, "requirements/registry.yaml", root)
    if text is None:
        raise RegistryError(f"baseline {ref!r} has no requirements/registry.yaml")
    document = load_yaml_string(text, f"{ref}:requirements/registry.yaml")
    if not isinstance(document, list) or not document:
        raise RegistryError(f"baseline {ref!r}: registry must be a non-empty list")
    return document


def check_history(
    records,
    ledger_path=None,
    baseline_ref=None,
    baseline_file=None,
    candidate_ref=None,
    release=False,
    root=".",
):
    findings = []
    current = {record.get("id"): record for record in records if record.get("id")}
    if ledger_path and os.path.isfile(ledger_path):
        ledger = load_yaml_document(ledger_path)
        if not isinstance(ledger, dict) or not isinstance(ledger.get("records"), list):
            raise RegistryError(f"{ledger_path}: expected a records list")
        seen = set()
        for entry in ledger["records"]:
            record_id = entry.get("id") if isinstance(entry, dict) else None
            if not record_id or record_id in seen:
                findings.append(_finding(REASON_DUPLICATE_ID, f"{ledger_path}: duplicate or invalid historical id"))
                continue
            seen.add(record_id)
            record = current.get(record_id)
            if record is None:
                findings.append(_finding(REASON_DUPLICATE_ID, f"{record_id}: historical id disappeared instead of being tombstoned"))
            elif entry.get("identity_digest") != identity_digest(record):
                findings.append(_finding(REASON_DUPLICATE_ID, f"{record_id}: historical identity was reused"))

    if baseline_ref and baseline_file:
        raise RegistryError("--baseline-ref and --baseline-file are mutually exclusive")
    if release and baseline_file:
        raise RegistryError("--baseline-file is local testing only and forbidden for release candidates")
    if baseline_ref and not candidate_ref:
        raise RegistryError("--baseline-ref requires an explicit --candidate-ref")
    baseline = _load_baseline(baseline_ref, candidate_ref, root) if baseline_ref else None
    if release and candidate_ref:
        head = _git_commit("HEAD", root)
        if head != candidate_ref:
            raise RegistryError("candidate SHA does not match the candidate checkout HEAD")
        status = _git_status(root)
        if status is None or status:
            raise RegistryError("release candidate checkout must be a clean committed worktree")
        candidate_registry = _git_file(candidate_ref, "requirements/registry.yaml", root)
        if candidate_registry is None:
            raise RegistryError("candidate commit has no requirements/registry.yaml")
        candidate_document = load_yaml_string(
            candidate_registry, f"{candidate_ref}:requirements/registry.yaml"
        )
        if not isinstance(candidate_document, list) or not candidate_document:
            raise RegistryError("candidate commit registry must be a non-empty list")
    if baseline_file:
        baseline = load_yaml_document(baseline_file)
        if not isinstance(baseline, list):
            raise RegistryError(f"baseline file {baseline_file!r}: registry must be a list")
    if baseline is not None:
        for previous in baseline:
            record_id = previous.get("id") if isinstance(previous, dict) else None
            if not record_id:
                continue
            record = current.get(record_id)
            if record is None:
                findings.append(_finding(REASON_DUPLICATE_ID, f"{record_id}: baseline id disappeared instead of being tombstoned"))
            elif identity_digest(previous) != identity_digest(record) and record.get("status") not in TOMBSTONED_STATUSES:
                findings.append(_finding(REASON_DUPLICATE_ID, f"{record_id}: baseline identity changed without a tombstone"))
    return findings


def _load_attestations(attestation_dir, root):
    directory = Path(attestation_dir).resolve()
    root = Path(root).resolve()
    if directory == root or root in directory.parents:
        raise RegistryError("release attestations must be outside the source checkout")
    trusted = {value.strip().lower() for value in os.environ.get("KIT_TRUSTED_ATTESTATION_SHA256", "").split(",") if value.strip()}
    if not trusted:
        raise RegistryError("KIT_TRUSTED_ATTESTATION_SHA256 is required for release attestations")
    attestations = {}
    for path in sorted(directory.glob("*.json")):
        if path.is_symlink() or not path.is_file():
            raise RegistryError(f"{path}: attestation must be a regular file")
        if stat.S_IMODE(path.stat().st_mode) & 0o022:
            raise RegistryError(f"{path}: attestation is group/world writable")
        raw = path.read_bytes()
        if len(raw) > 1024 * 1024:
            raise RegistryError(f"{path}: attestation exceeds 1048576 bytes")
        digest = hashlib.sha256(raw).hexdigest()
        if digest not in trusted:
            raise RegistryError(f"{path}: attestation digest is not trusted")
        try:
            pairs = json.loads(raw, object_pairs_hook=lambda value: value)
            if not isinstance(pairs, list):
                raise ValueError("top level must be an object")
            keys = [key for key, _value in pairs]
            if len(keys) != len(set(keys)):
                raise ValueError("duplicate JSON key")
            document = dict(pairs)
        except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
            raise RegistryError(f"{path}: invalid attestation JSON: {error}") from error
        required = {"evidence_id", "evidence_job", "commit_sha", "workflow_ref", "run_id", "artifact_digest", "environment_digest"}
        if set(document) != required or not all(document.get(key) for key in required):
            raise RegistryError(f"{path}: attestation fields must be exactly {sorted(required)}")
        if not re.fullmatch(r"[0-9a-f]{64}", str(document["artifact_digest"])) or not re.fullmatch(
            r"[0-9a-f]{64}", str(document["environment_digest"])
        ):
            raise RegistryError(f"{path}: artifact/environment digest must be SHA-256 hex")
        expected_commit = os.environ.get("GITHUB_SHA")
        if not expected_commit:
            raise RegistryError("GITHUB_SHA is required for release attestations")
        if document["commit_sha"] != expected_commit:
            raise RegistryError(f"{path}: commit_sha does not bind the checked-out commit")
        for env_name, field in (
            ("GITHUB_SHA", "commit_sha"),
            ("KIT_ATTESTATION_WORKFLOW_REF", "workflow_ref"),
            ("KIT_ATTESTATION_RUN_ID", "run_id"),
        ):
            expected = os.environ.get(env_name)
            if expected and str(document[field]) != expected:
                raise RegistryError(f"{path}: {field} does not bind {env_name}")
        if not all(os.environ.get(name) for name in ("KIT_ATTESTATION_WORKFLOW_REF", "KIT_ATTESTATION_RUN_ID")):
            raise RegistryError("KIT_ATTESTATION_WORKFLOW_REF and KIT_ATTESTATION_RUN_ID are required for release attestations")
        attestations.setdefault(document["evidence_id"], []).append(document)
    return attestations


def check_release(records, attestation_dir=None, root=".", dashboard_dir=None):
    findings = []
    attestations = {}
    if attestation_dir:
        attestations = _load_attestations(attestation_dir, root)
    else:
        findings.append(
            _finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, "external trusted attestations were not supplied")
        )

    for record in records:
        if record.get("status") in TOMBSTONED_STATUSES:
            continue
        label = record_label(record)
        record_class = record.get("record_class")
        status = record.get("status")
        if record_class == "promise" and status not in {"implemented", "resolved_by_amendment"}:
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, f"{label}: promise is unresolved"))
        if record_class == "risk" and status != "mitigated":
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, f"{label}: risk is not mitigated"))
        if record_class == "decision" and status not in {"implemented", "resolved_by_amendment"}:
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, f"{label}: decision is unresolved"))
        if record.get("applicability") == "pending_voi":
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, f"{label}: optional disposition is pending_voi"))
        if record.get("modality") in {"SHOULD", "SHOULD NOT"} and status not in TERMINAL_STATUSES and not record.get("deviation_record"):
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, f"{label}: SHOULD deviation is undocumented"))
        if record.get("criticality") == "blocking" and status not in TERMINAL_STATUSES:
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, f"{label}: release blocker remains open"))
        if record.get("area") == "KIT-SEC" and status not in TERMINAL_STATUSES:
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, f"{label}: security default remains open"))

        matching = [
            attestation
            for attestation in attestations.get(record.get("evidence_id"), [])
            if attestation.get("evidence_job") == record.get("evidence_job")
            and attestation.get("artifact_digest") == record.get("artifact_digest")
            and attestation.get("environment_digest") == record.get("environment_digest")
        ]
        if not matching:
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, f"{label}: no trusted exact evidence attestation"))

    if dashboard_dir and os.path.isdir(dashboard_dir):
        statuses = _dashboard_statuses(dashboard_dir)
        if len(statuses) != 12 or any(status != "passed" for status in statuses.values()):
            findings.append(_finding(REASON_MISSING_STALE_OR_FAILING_EVIDENCE, "all 12 dashboards must be passed"))
    return findings
