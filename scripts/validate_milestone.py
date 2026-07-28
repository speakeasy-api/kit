#!/usr/bin/env python3
"""Generate or validate current-tree, local-only milestone attestations."""

import argparse
import json
import re
import subprocess
import sys
from collections import Counter
from datetime import date
from pathlib import Path

import yaml

import validate_g00_attestations as g00
from req_lint_lib.loader import discover_shards, load_registry_dir
from yaml_utils import load_yaml_file


ROOT = Path(__file__).resolve().parents[1]
MILESTONE = "M001"
GATE = "G01"
ATTESTATION_DIR = ROOT / "requirements/attestations/m001"
REPORT = ROOT / "requirements/reports/m001-exit.md"
DASHBOARD = ROOT / "requirements/dashboards/G01.yaml"
WORKFLOW_REF = "local://m001-worktree-attestation"
TRUST_SCOPE = "local_milestone_only"
EXTERNAL_BLOCKERS = {
    "EV-G01-012": ["EXT-08"],
    "EV-G01-014": ["EXT-05"],
    "EV-G01-015": ["EXT-05", "EXT-08"],
}

JOBS = (
    ("EV-G01-001", "cargo test --locked --test conformance store_append::sixty_four_real_connections_allocate_one_gapless_committed_prefix -- --ignored --exact --test-threads=1"),
    ("EV-G01-002", "cargo test --locked --test conformance store_projection::replay_is_byte_identical_across_twenty_restarts -- --exact"),
    ("EV-G01-003", "cargo test --locked --test conformance store_append::idempotency_replays_only_the_same_canonical_request_and_exposes_pending -- --exact && cargo test --locked --test conformance deletion_api"),
    ("EV-G01-004", "cargo test --locked --test conformance config_layering"),
    ("EV-G01-005", "cargo test --locked --test conformance sched_budget -- --test-threads=1 && cargo test --locked --test fault sched_crash -- --test-threads=1"),
    ("EV-G01-006", "cargo test --locked --test conformance cap_invoke && cargo test --locked --test adversarial cap_bypass"),
    ("EV-G01-007", "cargo test --locked --test fault fencing && cargo test --locked --test fault lifecycle_cas"),
    ("EV-G01-008", "cargo test --locked --test fault artifact_crash"),
    ("EV-G01-009", "cargo test --locked --test conformance sse_cursor"),
    ("EV-G01-010", "cargo test --locked --test conformance sse_cursor::cross_principal_and_nonexistent_streams_are_byte_identical -- --exact && cargo test --locked --test adversarial auth_local::authorization_denials_do_not_disclose_cross_resource_state -- --exact && cargo test --locked --test conformance deletion_api::http_returns_jobs_typed_hold_refusal_and_no_cross_principal_details -- --exact"),
    ("EV-G01-011", "cargo test --locked --test integration backup_restore && cargo test --locked --test conformance retention_model && cargo test --locked --test conformance deletion_api"),
    ("EV-G01-012", "cargo test --locked --test adversarial auth_local::exact_seven_required_denial_cases_are_closed -- --exact && cargo test --locked --test adversarial auth_local::readiness_requires_both_components_in_every_boot_order -- --exact && cargo test --locked --test adversarial auth_remote::operational_fake_pki_denies_exactly_seven_required_cases_and_accepts_valid_peers -- --exact"),
    ("EV-G01-013", "cargo test --locked --test conformance telemetry_export && cargo test --locked --test adversarial secret_leak"),
    ("EV-G01-014", "python3 -m openapi_spec_validator docs/api/openapi.yaml && cargo test --locked --test conformance cli_parity && cargo test --locked --test conformance handler_parity && cargo test --locked --test conformance http_contract"),
    ("EV-G01-015", "cargo test --locked --test conformance --test integration --test fault --test adversarial -- --test-threads=1"),
)
M002_JOBS = (
    ("EV-G02-001", "cargo test --locked --test integration cli_daemon::prompt_runs_to_completion_through_daemon_and_cli -- --exact && cargo test --locked --test integration agent_run::agent_run_tests::loopdriver_commits_completion_progress_usage_and_cost -- --exact"),
    ("EV-G02-002", "cargo test --locked --test fault model_intent_outcome::crash_windows_reconcile_without_duplicate_dispatch_or_invented_success -- --exact"),
    ("EV-G02-003", "cargo test --locked --test fault loop_restart::every_safe_boundary_restarts_without_duplicate_provider_or_transcript_items -- --exact && cargo test --locked --test fault provider_interrupt::input_approval_and_auth_interruptions_survive_100_restarts_each -- --exact"),
    ("EV-G02-004", "cargo test --locked --test conformance sched_budget -- --test-threads=1 && cargo test --locked --test fault sched_crash -- --test-threads=1 && cargo test --locked --test integration agent_run::agent_run_tests::budget_exhaustion_fails_before_provider_dispatch -- --exact"),
    ("EV-G02-005", "cargo test --locked --test conformance prompt_determinism && cargo test --locked --test conformance context_projection"),
    ("EV-G02-006", "cargo test --locked --test conformance usage_reconcile && cargo test --locked --test conformance run_telemetry::unavailable_provider_values_are_explicit_nulls -- --exact && cargo test --locked --test conformance run_telemetry::provider_cache_and_accounting_reconcile_without_inventing_values -- --exact"),
    ("EV-G02-007", "cargo test --locked --test fault loop_restart::input_approval_and_auth_waits_survive_and_require_authenticated_resolution -- --exact && cargo test --locked --test integration agent_run::agent_run_tests::approval_and_auth_resolutions_resume_real_waiting_paths -- --exact"),
    ("EV-G02-008", "cargo test --locked --test integration agent_run::agent_run_tests::loopdriver_commits_completion_progress_usage_and_cost -- --exact && cargo test --locked --test integration provider_stream::durable_stream_suppresses_reasoning_and_redacts_secret_forms_and_headers -- --exact && cargo test --locked --test conformance run_telemetry::private_reasoning_has_no_schema_path_and_summary_requires_retention -- --exact"),
    ("EV-G02-009", "cargo test --locked --test integration provider_stream::durable_stream_suppresses_reasoning_and_redacts_secret_forms_and_headers -- --exact && cargo test --locked --test conformance run_telemetry::provider_canaries_are_absent_on_every_capture_boundary -- --exact && cargo test --locked --test adversarial secret_leak::persistent_capture_boundaries_remove_raw_and_encoded_canaries -- --exact"),
    ("EV-G02-010", "sh scripts/verify_pins.sh && cargo test --locked --test conformance ext_m002 && python3 scripts/req_lint.py --aggregate"),
)
M003_JOBS = (
    ("EV-G03-001", "cargo test --locked --test adversarial local_sandbox && cargo test --locked --test adversarial container_fs && cargo test --locked --test adversarial container_net"),
    ("EV-G03-002", "cargo test --locked --test conformance container_limits && cargo test --locked --test conformance process_output"),
    ("EV-G03-003", "cargo test --locked --test fault process_reap && cargo test --locked --test fault exec_cancel"),
    ("EV-G03-004", "cargo test --locked --test conformance exec_profile && cargo test --locked --test adversarial local_sandbox"),
    ("EV-G03-005", "cargo test --locked --test conformance workspace_acquire"),
    ("EV-G03-006", "cargo test --locked --test fault exec_cancel && cargo test --locked --test fault fencing"),
    ("EV-G03-007", "cargo test --locked --test adversarial exec_secret_leak && cargo test --locked --test conformance terminal_lease::secret_absent_terminal_history -- --exact"),
    ("EV-G03-008", "cargo test --locked --test conformance exec_api && cargo test --locked --test conformance terminal_lease"),
    ("EV-G03-009", "cargo test --locked --test adversarial trial_grader_access"),
    ("EV-G03-010", "cargo test --locked --test conformance exec_contracts && cargo test --locked --test conformance exec_api && cargo test --locked --test adversarial trial_grader_access && cargo test --locked --test fault exec_cancel"),
)
M004_JOBS = (
    ("EV-G04-001", "python3 scripts/check_dogfood_harness.py && cargo test --locked --manifest-path dogfood-harness/Cargo.toml local_mechanical_provider_conformance_uses_public_cli_and_http -- --exact && cargo test --locked --manifest-path dogfood-harness/Cargo.toml direct_public_edit_failure_approval_and_artifact_contracts -- --exact"),
    ("EV-G04-002", "cargo test --locked --test fault edit_recovery -- --test-threads=1"),
    ("EV-G04-003", "cargo test --locked --test conformance edit_validate && cargo test --locked --test adversarial path_escape"),
    ("EV-G04-004", "cargo test --locked --test fault edit_recovery::cancellation -- --test-threads=1"),
    ("EV-G04-005", "cargo test --locked --test conformance edit_format"),
    ("EV-G04-006", "cargo test --locked --test conformance verify_profiles && cargo test --locked --test conformance verify_feedback"),
    ("EV-G04-007", "cargo test --locked --test conformance grammar_edit_path"),
    ("EV-G04-008", "cargo test --locked --test adversarial native_tool_bypass && cargo test --locked --test conformance native_tools"),
    ("EV-G04-009", "cargo test --locked --test conformance harness_selfcheck && KIT_M004_REPORT_DIR=requirements/reports/m004/source-semantics cargo test --locked --test conformance eval_stats_report::eval_stats_report_uses_exact_binary_primary_and_exploratory_point_estimates_only -- --exact && check-jsonschema --schemafile eval/preregistration/schema/v1/preregistration.schema.json requirements/reports/m004/source-semantics/preregistration.json && check-jsonschema --schemafile eval/preregistration/schema/v1/registration.schema.json requirements/reports/m004/source-semantics/registered-preregistration.json && check-jsonschema --schemafile eval/reports/schema/v1/statistical-report.schema.json requirements/reports/m004/source-semantics/statistical-report.json"),
    ("EV-G04-010", "cargo test --locked --test conformance --test integration --test fault --test adversarial -- --test-threads=1"),
)
M003_BLOCKERS = {
    "EV-G03-001": ["EXT-01", "EXT-04", "EXT-19", "EXT-20"],
    "EV-G03-002": ["EXT-01", "EXT-04", "EXT-19", "EXT-20"],
    "EV-G03-003": ["EXT-01", "EXT-04", "EXT-19", "EXT-20", "EXT-22"],
    "EV-G03-004": ["EXT-01", "EXT-04", "EXT-19", "EXT-20"],
    "EV-G03-005": None,
    "EV-G03-006": ["EXT-01", "EXT-04", "EXT-19", "EXT-20", "EXT-22"],
    "EV-G03-007": ["EXT-01", "EXT-04", "EXT-19", "EXT-20"],
    "EV-G03-008": ["EXT-19", "EXT-22"],
    "EV-G03-009": ["EXT-01", "EXT-04", "EXT-20"],
    "EV-G03-010": ["G01", "G02", "EXT-01", "EXT-04", "EXT-19", "EXT-20", "EXT-22"],
}
M003_ROW_STATUS = {
    "EV-G03-001": "blocked_external",
    "EV-G03-002": "blocked_external",
    "EV-G03-003": "blocked_external",
    "EV-G03-004": "blocked_external",
    "EV-G03-005": "in_progress",
    "EV-G03-006": "blocked_external",
    "EV-G03-007": "blocked_external",
    "EV-G03-008": "blocked_external",
    "EV-G03-009": "blocked_external",
    "EV-G03-010": "blocked_transitive",
}
M003_RECORD_LANE = {
    "EV-G03-001": "adversarial",
    "EV-G03-002": "integration",
    "EV-G03-003": "fault",
    "EV-G03-004": "adversarial",
    "EV-G03-005": "integration",
    "EV-G03-006": "fault",
    "EV-G03-007": "adversarial",
    "EV-G03-008": "integration",
    "EV-G03-009": "adversarial",
    "EV-G03-010": "evidence-report",
}
M004_BLOCKERS = {
    "EV-G04-001": ["EXT-01", "EXT-22"],
    "EV-G04-009": ["EXT-01", "EXT-04", "EXT-15", "EXT-22"],
    "EV-G04-010": ["G02", "G03", "EXT-01", "EXT-04", "EXT-15", "EXT-19", "EXT-20", "EXT-22"],
}
M004_ROW_STATUS = {
    "EV-G04-001": "blocked_external",
    "EV-G04-002": "passed",
    "EV-G04-003": "passed",
    "EV-G04-004": "passed",
    "EV-G04-005": "passed",
    "EV-G04-006": "passed",
    "EV-G04-007": "passed",
    "EV-G04-008": "passed",
    "EV-G04-009": "blocked_external",
    "EV-G04-010": "blocked_transitive",
}
COMMANDS = dict(JOBS)
BLOCKED_JOBS = set(EXTERNAL_BLOCKERS)
EXPECTED_DASHBOARD_STATUS = {
    evidence_id: "blocked" if evidence_id in BLOCKED_JOBS else "passed"
    for evidence_id in COMMANDS
}

# These promises describe mechanisms delivered by later milestones. M001 defines
# their extension boundary but does not implement or close them.
FUTURE_PRIMARY = {
    "KIT-DOMAIN-001": "M003",
    "KIT-DOMAIN-002": "M003",
    "KIT-DOMAIN-003": "M002",
    "KIT-DOMAIN-004": "M002",
    "KIT-CONFIG-807": "M002",
    "KIT-CONFIG-808": "M006",
    "KIT-CONFIG-809": "M006",
    "KIT-CONFIG-810": "M010",
    "KIT-CONFIG-811": "M010",
    "KIT-CONFIG-812": "M005",
    "KIT-CONFIG-813": "M005",
    "KIT-CONFIG-814": "M002",
    "KIT-CONFIG-815": "M012",
    "KIT-CONFIG-816": "M011",
    "KIT-CONFIG-817": "M012",
    "KIT-CONFIG-818": "M012",
    "KIT-DOMAIN-862": "M003",
    "KIT-DOMAIN-863": "M003",
    "KIT-DOMAIN-864": "M002",
    "KIT-STORE-801": "M005",
    "KIT-STORE-821": "M012",
    "KIT-STORE-822": "M002",
    "KIT-STORE-825": "M005",
    "KIT-STORE-826": "M005",
    "KIT-STORE-827": "M004",
    "KIT-STORE-828": "M002",
    "KIT-STORE-830": "M002",
    "KIT-API-808": "M002",
}
MILESTONE_GATE = {
    "M002": "G02",
    "M003": "G03",
    "M004": "G04",
    "M005": "G05",
    "M006": "G06",
    "M010": "G10",
    "M011": "G11",
    "M012": "G12",
}
EXTERNAL_RECORDS = {"KIT-API-804"}


def configure(milestone):
    global MILESTONE, GATE, ATTESTATION_DIR, REPORT, DASHBOARD, WORKFLOW_REF
    global JOBS, COMMANDS, BLOCKED_JOBS, EXPECTED_DASHBOARD_STATUS, EXTERNAL_BLOCKERS
    if milestone == "M001":
        return
    if milestone == "M002":
        MILESTONE = "M002"
        GATE = "G02"
        ATTESTATION_DIR = ROOT / "requirements/attestations/m002"
        REPORT = ROOT / "requirements/reports/m002-exit.md"
        DASHBOARD = ROOT / "requirements/dashboards/G02.yaml"
        WORKFLOW_REF = "local://m002-worktree-attestation"
        JOBS = M002_JOBS
        COMMANDS = dict(JOBS)
        BLOCKED_JOBS = set()
        EXPECTED_DASHBOARD_STATUS = {evidence_id: "passed" for evidence_id in COMMANDS}
        return
    if milestone == "M003":
        MILESTONE = "M003"
        GATE = "G03"
        ATTESTATION_DIR = ROOT / "requirements/attestations/m003"
        REPORT = ROOT / "requirements/reports/m003-exit.md"
        DASHBOARD = ROOT / "requirements/dashboards/G03.yaml"
        WORKFLOW_REF = "local://m003-worktree-source-conformance"
        JOBS = M003_JOBS
        COMMANDS = dict(JOBS)
        EXTERNAL_BLOCKERS = M003_BLOCKERS
        BLOCKED_JOBS = {key for key, value in M003_BLOCKERS.items() if value}
        EXPECTED_DASHBOARD_STATUS = M003_ROW_STATUS
        return
    MILESTONE = "M004"
    GATE = "G04"
    ATTESTATION_DIR = ROOT / "requirements/attestations/m004"
    REPORT = ROOT / "requirements/reports/m004-exit.md"
    DASHBOARD = ROOT / "requirements/dashboards/G04.yaml"
    WORKFLOW_REF = "local://m004-worktree-source-conformance"
    JOBS = M004_JOBS
    COMMANDS = dict(JOBS)
    EXTERNAL_BLOCKERS = M004_BLOCKERS
    BLOCKED_JOBS = set(M004_BLOCKERS)
    EXPECTED_DASHBOARD_STATUS = M004_ROW_STATUS

FIELDS = {
    "artifact",
    "artifact_digest",
    "base_commit_sha",
    "candidate_identity",
    "dashboard_row",
    "disposition",
    "environment",
    "environment_digest",
    "external_blocker",
    "gate",
    "milestone",
    "requirements",
    "run_id",
    "schema_version",
    "source_tree_digest",
    "trust_scope",
    "trusted_for_release",
    "versions",
    "workflow_ref",
}


def records():
    loaded, _ = load_registry_dir(
        ROOT / "requirements/registry.d",
        discover_shards(ROOT / "requirements/registry.d"),
    )
    return loaded


def dashboard_rows():
    document = load_yaml_file(DASHBOARD)
    rows = {row["evidence_id"]: row for row in document["evidence"]}
    expected_gate_status = {
        "M001": "blocked",
        "M002": "blocked_transitive",
        "M003": "blocked_external",
        "M004": "in_progress",
    }[MILESTONE]
    if document.get("current_status") != expected_gate_status:
        raise ValueError(f"{GATE} dashboard must be {expected_gate_status}")
    if set(rows) != set(COMMANDS):
        raise ValueError(f"{GATE} dashboard evidence set does not match the exit bullets")
    for evidence_id, expected in EXPECTED_DASHBOARD_STATUS.items():
        if rows[evidence_id].get("current_status") != expected:
            raise ValueError(f"{evidence_id}: dashboard status must be {expected}")
    return rows


def selected_records(all_records=None):
    all_records = records() if all_records is None else all_records
    selected = {
        record["id"]: record
        for record in all_records
        if record.get("primary_milestone") == MILESTONE
        and record["id"] not in EXTERNAL_RECORDS
        and (MILESTONE != "M002" or record.get("applicability") == "mandatory")
    }
    if not selected:
        raise ValueError(f"{MILESTONE} has no implemented record set")
    return selected


def job_for(record):
    record_id = record["id"]
    area = record["area"]
    number = int(record_id.rsplit("-", 1)[1])
    if MILESTONE == "M004":
        work_package = re.match(r"EV-M004-W(\d{2})", record["evidence_id"])
        work_package = int(work_package.group(1)) if work_package else None
        if work_package == 11:
            return "EV-G04-001"
        if work_package == 6:
            return "EV-G04-002"
        if work_package == 5:
            return "EV-G04-003"
        if work_package in {7, 8}:
            return "EV-G04-006"
        if work_package == 9:
            return "EV-G04-007"
        if work_package == 10:
            return "EV-G04-008"
        if work_package == 12:
            return "EV-G04-009"
        return "EV-G04-010"
    if MILESTONE == "M003":
        if record_id in {"KIT-EXEC-805", "KIT-EXEC-806", "KIT-EXEC-808", "KIT-EXEC-811", "KIT-SEC-804"}:
            return "EV-G03-001"
        if record_id in {"KIT-DOMAIN-862", "KIT-DOMAIN-863", "KIT-EXEC-809"}:
            return "EV-G03-002"
        if record_id in {"KIT-DOMAIN-001", "KIT-DOMAIN-002", "KIT-EXEC-001", "KIT-EXEC-002", "KIT-EXEC-810", "KIT-EXEC-980"}:
            return "EV-G03-003"
        if record_id in {"KIT-EXEC-800", "KIT-EXEC-801", "KIT-EXEC-802", "KIT-EXEC-803", "KIT-EXEC-804", "KIT-EXEC-812"}:
            return "EV-G03-004"
        if record_id in {"KIT-EXEC-813", "KIT-EXEC-814", "KIT-EXEC-815", "KIT-EXEC-816", "KIT-EXEC-817"}:
            return "EV-G03-005"
        if record_id == "KIT-RUNTIME-950":
            return "EV-G03-006"
        if record_id in {"KIT-SEC-005", "KIT-SEC-006", "KIT-SEC-007", "KIT-SEC-802", "KIT-SEC-803", "KIT-SEC-981", "KIT-EXEC-807"}:
            return "EV-G03-007"
        if record_id in {"KIT-EXEC-818", "KIT-EXEC-819", "KIT-EXEC-820", "KIT-EXEC-821"}:
            return "EV-G03-008"
        if record_id in {"KIT-SEC-800", "KIT-SEC-801"}:
            return "EV-G03-010"
        raise ValueError(f"{record_id}: no M003 exit-evidence binding")
    if MILESTONE == "M002":
        if record_id == "KIT-STORE-822":
            return "EV-G02-001"
        if record_id in {"KIT-STORE-018", "KIT-STORE-830", "KIT-PROMPT-043"}:
            return "EV-G02-008"
        if record_id == "KIT-STORE-828":
            return "EV-G02-006"
        if record_id == "KIT-AGENTKIT-804":
            return "EV-G02-007"
        if record["record_class"] == "risk":
            return "EV-G02-002"
        if area == "KIT-API":
            return "EV-G02-001"
        if area == "KIT-AGENTKIT":
            evidence_id = record["evidence_id"]
            if record["evidence_job"] == "reproducible-build" or "-W09-" in evidence_id:
                return "EV-G02-010"
            return "EV-G02-002" if "-W03-" in evidence_id else "EV-G02-003"
        if area in {"KIT-DOMAIN", "KIT-STORE"}:
            return "EV-G02-003"
        if area in {"KIT-PROMPT", "KIT-CONTEXT", "KIT-CONFIG"}:
            return "EV-G02-005"
        if area == "KIT-OBS":
            return "EV-G02-006"
        return "EV-G02-010"
    if record_id == "KIT-ARCH-005":
        return "EV-G01-009"
    if area == "KIT-CONFIG":
        return "EV-G01-004"
    if area == "KIT-SEC":
        return "EV-G01-013"
    if area == "KIT-DOMAIN":
        if number in {861, 865}:
            return "EV-G01-005"
        if number == 857:
            return "EV-G01-006"
        if number == 860:
            return "EV-G01-009"
        return "EV-G01-007" if number >= 821 else "EV-G01-015"
    if area == "KIT-ARCH":
        return "EV-G01-014"
    if area == "KIT-API":
        if record_id == "KIT-API-001" or 809 <= number <= 812:
            return "EV-G01-003"
        if number in {801}:
            return "EV-G01-010"
        if number in {802, 803}:
            return "EV-G01-012"
        if number in {805, 806}:
            return "EV-G01-009"
        if 817 <= number <= 819:
            return "EV-G01-011"
        return "EV-G01-014"
    if area == "KIT-STORE":
        if record_id in {"KIT-STORE-010", "KIT-STORE-011", "KIT-STORE-012", "KIT-STORE-013", "KIT-STORE-014", "KIT-STORE-015"} or number == 820:
            return "EV-G01-011"
        if number in {2, 803, 815, 816, 817, 818}:
            return "EV-G01-008"
        if number in {7, 809, 810}:
            return "EV-G01-003"
        if number in {5, 9, 804, 808, 823, 824, 829}:
            return "EV-G01-007"
        if number in {6, 805, 807}:
            return "EV-G01-002"
        if number in {814, 819}:
            return "EV-G01-009"
        if number in {980}:
            return "EV-G01-006"
        if number in {981}:
            return "EV-G01-011"
        return "EV-G01-001"
    return "EV-G01-015"


def record_bindings(all_records=None):
    grouped = {evidence_id: [] for evidence_id in COMMANDS}
    for record in selected_records(all_records).values():
        grouped[job_for(record)].append(
            {
                "record_id": record["id"],
                "evidence_id": record["evidence_id"],
                "evidence_job": record["evidence_job"],
            }
        )
    return {key: sorted(value, key=lambda item: item["record_id"]) for key, value in grouped.items()}


def relevant_test_count(command, artifact):
    count = g00.validate_artifact(command, artifact)
    if count is None:
        raise ValueError("milestone evidence command does not execute a test")
    return count


def run_jobs():
    results = {}
    for evidence_id, command in JOBS:
        artifact = g00.run_command(evidence_id, command)
        test_count = relevant_test_count(command, artifact)
        results[evidence_id] = {
            "artifact": artifact,
            "artifact_digest": g00.artifact_digest(artifact),
        }
        count = f", {test_count} relevant tests" if MILESTONE in {"M003", "M004"} else ""
        print(f"{evidence_id}: command passed{count}", flush=True)
    return results


def update_registry_contracts():
    if MILESTONE not in {"M003", "M004"}:
        return
    paths = sorted((ROOT / "requirements/registry.d").glob("*.yaml"))
    for path in paths:
        document = load_yaml_file(path)
        changed = False
        for record in document:
            if record.get("primary_milestone") != MILESTONE or record["id"] in EXTERNAL_RECORDS:
                continue
            evidence_id = job_for(record)
            command = COMMANDS[evidence_id]
            criterion = (
                f"{command} exits 0 with at least 1 relevant test per cargo invocation; "
                "this local-only result is not trusted release evidence"
            )
            lane = M003_RECORD_LANE[evidence_id] if MILESTONE == "M003" else "evidence-report"
            expected = ([criterion], lane, criterion)
            current = (record.get("acceptance_criteria"), record.get("evidence_job"), record.get("expected_result"))
            if current != expected:
                record["acceptance_criteria"], record["evidence_job"], record["expected_result"] = expected
                changed = True
        if changed:
            path.write_text(g00.dump_yaml(document), encoding="utf-8")


def update_registry(results, environment_digest, version_data):
    paths = sorted((ROOT / "requirements/registry.d").glob("*.yaml"))
    for path in paths:
        document = load_yaml_file(path)
        changed = False
        for record in document:
            record_id = record["id"]
            if MILESTONE == "M001" and record_id in FUTURE_PRIMARY:
                primary = FUTURE_PRIMARY[record_id]
                contributors = [
                    value
                    for value in record["contributing_milestones"]
                    if value not in {MILESTONE, primary}
                ]
                record["primary_milestone"] = primary
                record["contributing_milestones"] = [MILESTONE, *contributors]
                record["release_gates"] = [MILESTONE_GATE[primary]]
                changed = True
                continue
            if (
                record.get("primary_milestone") != MILESTONE
                or record_id in EXTERNAL_RECORDS
                or (MILESTONE == "M002" and record.get("applicability") != "mandatory")
            ):
                continue
            evidence_id = job_for(record)
            if MILESTONE in {"M003", "M004"}:
                command = COMMANDS[evidence_id]
                criterion = (
                    f"{command} exits 0 with at least 1 relevant test per cargo invocation; "
                    "this local-only result is not trusted release evidence"
                )
                record["acceptance_criteria"] = [criterion]
                record["evidence_job"] = (
                    M003_RECORD_LANE[evidence_id] if MILESTONE == "M003" else "evidence-report"
                )
                record["expected_result"] = criterion
                externally_blocked = MILESTONE == "M003" or evidence_id in {
                    "EV-G04-001", "EV-G04-009"
                }
                if externally_blocked:
                    record["status"] = "active"
                    record["artifact_digest"] = None
                    record["environment_digest"] = None
                    record["versions"] = None
                    record["latest_result"] = "pending"
                else:
                    record["status"] = "mitigated" if record["record_class"] == "risk" else "implemented"
                    record["artifact_digest"] = results[evidence_id]["artifact_digest"]
                    record["environment_digest"] = environment_digest
                    record["versions"] = version_data
                    record["latest_result"] = "pass"
                changed = True
                continue
            record["status"] = "mitigated" if record["record_class"] == "risk" else "implemented"
            record["artifact_digest"] = results[evidence_id]["artifact_digest"]
            record["environment_digest"] = environment_digest
            record["versions"] = version_data
            record["latest_result"] = "pass"
            changed = True
        if changed:
            path.write_text(g00.dump_yaml(document), encoding="utf-8")


def write_attestations(results, environment_text, environment_digest, version_data):
    rows = dashboard_rows()
    all_records = records()
    bindings = record_bindings(all_records)
    tree_digest = g00.source_tree_digest()
    identity = f"worktree:{tree_digest}"
    base_commit = g00.current_base_commit()
    ATTESTATION_DIR.mkdir(parents=True, exist_ok=True)
    for path in ATTESTATION_DIR.glob("*.json"):
        path.unlink()
    documents = []
    for evidence_id, _command in JOBS:
        result = results[evidence_id]
        document = {
            "artifact": result["artifact"],
            "artifact_digest": result["artifact_digest"],
            "base_commit_sha": base_commit,
            "candidate_identity": identity,
            "dashboard_row": rows[evidence_id],
            "disposition": (M003_ROW_STATUS if MILESTONE == "M003" else M004_ROW_STATUS)[evidence_id] if MILESTONE in {"M003", "M004"} else ("blocked_external" if evidence_id in BLOCKED_JOBS else "pass"),
            "environment": environment_text,
            "environment_digest": environment_digest,
            "external_blocker": EXTERNAL_BLOCKERS.get(evidence_id),
            "gate": GATE,
            "milestone": MILESTONE,
            "requirements": bindings[evidence_id],
            "run_id": f"local-{date.today().strftime('%Y%m%d')}-{MILESTONE.lower()}",
            "schema_version": 2,
            "source_tree_digest": tree_digest,
            "trust_scope": TRUST_SCOPE,
            "trusted_for_release": False,
            "versions": version_data,
            "workflow_ref": WORKFLOW_REF,
        }
        path = ATTESTATION_DIR / f"{evidence_id}.json"
        path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        documents.append(document)
    write_report(documents)


def write_report(documents):
    if MILESTONE == "M004":
        write_m004_report(documents)
        return
    if MILESTONE == "M003":
        write_m003_report(documents)
        return
    if MILESTONE == "M002":
        write_m002_report(documents)
        return
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    set_digest = g00.attestation_set_digest(paths)
    tree_digest = documents[0]["source_tree_digest"]
    all_records = records()
    selected = selected_records(all_records)
    classes = Counter(record["record_class"] for record in selected.values())
    manifest = load_yaml_file(ROOT / "docs/compatibility/build-manifest.yaml")
    pins = {record["id"]: record["value"] for record in manifest["pins"]}
    test_total = sum(
        relevant_test_count(document["artifact"]["command"], document["artifact"])
        for document in documents
    )
    projection_paths = [
        ROOT / "requirements/registry.yaml",
        ROOT / "requirements/evidence.yaml",
        ROOT / "requirements/tombstones.yaml",
        ROOT / "requirements/id-ledger.yaml",
        ROOT / "requirements/report.md",
    ]
    lines = [
        "# M001 Exit Report",
        "",
        f"- Gate: `{GATE}`",
        f"- Milestone: `{MILESTONE}`",
        f"- Run date: `{date.today().isoformat()}`",
        "- Result: **BLOCKED_EXTERNAL (EXT-05, EXT-08)**",
        "- Exit bullets: **12/15 passed; 3/15 blocked by external prerequisites**",
        f"- Evidence commands: **{len(documents)}/{len(JOBS)} exited 0; {test_total} relevant tests passed**",
        f"- Candidate identity: `worktree:{tree_digest}`",
        f"- Source-tree SHA-256: `{tree_digest}`",
        f"- Local attestation-set SHA-256: `{set_digest}`",
        "- Release result: **FAIL**",
        "",
        "The local implementation evidence is current for this worktree. Source-controlled",
        "attestations are local-only and are rejected by final release validation.",
        "Every authoritative Cargo invocation selected at least one passing test.",
        "",
        "`EXT-05` requires Windows CI for CLI/API parity. `EXT-08` requires a provisioned",
        "OIDC IdP and CA with live issuance and revocation.",
        "The real cryptographic fake-PKI fixture ran successfully but is operational evidence",
        "(`O`), not external conformance (`C`), so it does not close G01. The aggregate exit",
        "bullet remains blocked transitively; no internal G01 blocker is claimed.",
        "",
        "## Registry",
        "",
        f"- M001 implemented/mitigated records: {len(selected)}",
        f"- Requirements: {classes['requirement']}",
        f"- Promises: {classes['promise']}",
        f"- Decisions: {classes['decision']}",
        f"- Mitigated risks: {classes['risk']}",
        f"- External pending record: `{', '.join(sorted(EXTERNAL_RECORDS))}`",
        f"- Future mechanism promises reassigned from M001 primary ownership: {len(FUTURE_PRIMARY)}",
        "",
        "## Generated Projections",
        "",
        "| Projection | SHA-256 |",
        "| --- | --- |",
    ]
    lines.extend(
        f"| `{path.relative_to(ROOT).as_posix()}` | `{g00.file_digest(path)}` |"
        for path in projection_paths
    )
    lines.extend(
        [
            "",
            "## Exit Evidence",
            "",
            "| Bullet | Dashboard | Disposition | Command | Artifact SHA-256 | Requirement records |",
            "| --- | --- | --- | --- | --- | ---: |",
        ]
    )
    for document in documents:
        command = document["artifact"]["command"].replace("|", "\\|")
        lines.append(
            f"| `{document['dashboard_row']['source_ref']}` | `{document['dashboard_row']['evidence_id']}` | "
            f"`{document['disposition']}` | `{command}` | `{document['artifact_digest']}` | "
            f"{len(document['requirements'])} |"
        )
    lines.extend(
        [
            "",
            "Every attestation binds the current worktree/base commit, dashboard row, literal",
            "command and captured output, artifact/environment/version metadata, and each",
            "requirement record's ID, evidence ID, and evidence job. The report binds the full set.",
            "",
            "## External Blocker",
            "",
            "G01 remains `BLOCKED_EXTERNAL` until `EXT-05` supplies Windows CLI/API parity",
            "and `EXT-08` supplies live IdP/PKI issuance, validation, and revocation evidence.",
            "The operational cryptographic fixture cannot be relabeled as conformance evidence.",
            "",
            "## Build Provenance Limitation",
            "",
            f"The retained reproducible artifact is bound to build-input closure `{pins['build.input_closure_sha256']}`",
            "using pre-artifact mtimes and byte-identical retained source copies. The closure manifest was",
            "recorded after the run (`closure_manifest_recorded_post_run=true`), so this is not equivalent",
            "to a closure digest embedded by the completed run. Future reproducible builds embed that digest.",
            "",
        ]
    )
    REPORT.write_text("\n".join(lines), encoding="utf-8")


def write_m002_report(documents):
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    set_digest = g00.attestation_set_digest(paths)
    tree_digest = documents[0]["source_tree_digest"]
    all_records = records()
    selected = selected_records(all_records)
    classes = Counter(record["record_class"] for record in selected.values())
    optional = [
        record
        for record in all_records
        if record.get("primary_milestone") == MILESTONE
        and record.get("applicability") != "mandatory"
    ]
    test_total = sum(
        relevant_test_count(document["artifact"]["command"], document["artifact"])
        for document in documents
    )
    projection_paths = [
        ROOT / "requirements/registry.yaml",
        ROOT / "requirements/evidence.yaml",
        ROOT / "requirements/tombstones.yaml",
        ROOT / "requirements/id-ledger.yaml",
        ROOT / "requirements/report.md",
    ]
    lines = [
        "# M002 Exit Report",
        "",
        f"- Gate: `{GATE}`",
        f"- Milestone: `{MILESTONE}`",
        f"- Run date: `{date.today().isoformat()}`",
        "- Local mechanism result: **PASS**",
        "- Overall result: **BLOCKED_TRANSITIVE (G01)**",
        f"- Exit bullets: **{len(documents)}/{len(JOBS)} passed locally**",
        f"- Evidence commands: **{len(documents)}/{len(JOBS)} exited 0; {test_total} relevant tests passed**",
        f"- Candidate identity: `worktree:{tree_digest}`",
        f"- Source-tree SHA-256: `{tree_digest}`",
        f"- Local attestation-set SHA-256: `{set_digest}`",
        "- Release result: **FAIL**",
        "",
        "All M002 mechanisms pass in this worktree. The retained G00, G01, and G02 local",
        "attestations bind the current source tree, but G02 remains transitively blocked because",
        "G01 is externally blocked. Local G00 reproducibility does not replace release evidence.",
        "Every authoritative Cargo invocation selected at least one passing test.",
        "",
        "## Registry",
        "",
        f"- M002 implemented/mitigated mandatory records: {len(selected)}",
        f"- Requirements: {classes['requirement']}",
        f"- Promises: {classes['promise']}",
        f"- Decisions: {classes['decision']}",
        f"- Mitigated risks: {classes['risk']}",
        f"- Optional pending-VOI records left pending: {len(optional)}",
        "",
        "## Generated Projections",
        "",
        "| Projection | SHA-256 |",
        "| --- | --- |",
    ]
    lines.extend(
        f"| `{path.relative_to(ROOT).as_posix()}` | `{g00.file_digest(path)}` |"
        for path in projection_paths
    )
    lines.extend(
        [
            "",
            "## Exit Evidence",
            "",
            "| Bullet | Dashboard | Disposition | Command | Artifact SHA-256 | Requirement records |",
            "| --- | --- | --- | --- | --- | ---: |",
        ]
    )
    for document in documents:
        command = document["artifact"]["command"].replace("|", "\\|")
        lines.append(
            f"| `{document['dashboard_row']['source_ref']}` | `{document['dashboard_row']['evidence_id']}` | "
            f"`{document['disposition']}` | `{command}` | `{document['artifact_digest']}` | "
            f"{len(document['requirements'])} |"
        )
    lines.extend(
        [
            "",
            "Every attestation binds the current worktree/base commit, dashboard row, literal",
            "command and captured output, artifact/environment/version metadata, and each",
            "implemented M002 record's ID, evidence ID, and evidence job.",
            "",
            "## Transitive Blockers",
            "",
            "- G00, G01, and G02 local attestations are current source-controlled evidence only.",
            "- G01 remains `BLOCKED_EXTERNAL` on `EXT-05` and `EXT-08`.",
            "",
        ]
    )
    REPORT.write_text("\n".join(lines), encoding="utf-8")


def write_m003_report(documents):
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    set_digest = g00.attestation_set_digest(paths)
    tree_digest = documents[0]["source_tree_digest"]
    selected = selected_records()
    classes = Counter(record["record_class"] for record in selected.values())
    projection_paths = [
        ROOT / "requirements/registry.yaml",
        ROOT / "requirements/evidence.yaml",
        ROOT / "requirements/tombstones.yaml",
        ROOT / "requirements/id-ledger.yaml",
        ROOT / "requirements/report.md",
    ]
    test_total = sum(relevant_test_count(document["artifact"]["command"], document["artifact"]) for document in documents)
    lines = [
        "# M003 Exit Report",
        "",
        f"- Gate: `{GATE}`",
        f"- Milestone: `{MILESTONE}`",
        f"- Run date: `{date.today().isoformat()}`",
        "- Local source/conformance result: **PASS_LOCAL**",
        "- Overall result: **BLOCKED_EXTERNAL (EXT-01, EXT-04, EXT-19, EXT-20, EXT-22; G01/G02 transitive)**",
        "- Exit bullets: **10/10 exercised locally; 8/10 blocked externally; 1/10 in progress; 1/10 blocked transitively**",
        f"- Evidence commands: **{len(documents)}/{len(JOBS)} exited 0; {test_total} relevant tests passed**",
        f"- Candidate identity: `worktree:{tree_digest}`",
        f"- Source-tree SHA-256: `{tree_digest}`",
        f"- Local attestation-set SHA-256: `{set_digest}`",
        "- Release result: **FAIL**",
        "",
        "These attestations bind the current uncommitted worktree and prove only local source and",
        "conformance behavior. They are not trusted external runtime attestations and cannot pass",
        "Linux helper/cgroup/network/filesystem/daemon-SIGKILL, Windows, macOS VM, or architecture cells.",
        "",
        "## Registry",
        "",
        f"- M003 active records: {len(selected)}",
        f"- Requirements: {classes['requirement']}",
        f"- Promises: {classes['promise']}",
        f"- Decisions: {classes['decision']}",
        f"- Risks awaiting mitigation evidence: {classes['risk']}",
        "- Record evidence disposition: `latest_result: pending`; no M003 record is release-closed by local evidence.",
        "",
        "## Generated Projections",
        "",
        "| Projection | SHA-256 |",
        "| --- | --- |",
    ]
    lines.extend(
        f"| `{path.relative_to(ROOT).as_posix()}` | `{g00.file_digest(path)}` |"
        for path in projection_paths
    )
    lines.extend([
        "",
        "## Local Evidence",
        "",
        "| Bullet | Dashboard | Disposition | Command | Passed tests | Artifact SHA-256 | Requirement records | Blockers |",
        "| --- | --- | --- | --- | ---: | --- | ---: | --- |",
    ])
    for document in documents:
        command = document["artifact"]["command"].replace("|", "\\|")
        blockers = ", ".join(document["external_blocker"] or []) or "uncommitted/in progress"
        lines.append(
            f"| `{document['dashboard_row']['source_ref']}` | `{document['dashboard_row']['evidence_id']}` | "
            f"`{document['disposition']}` | `{command}` | {relevant_test_count(document['artifact']['command'], document['artifact'])} | "
            f"`{document['artifact_digest']}` | {len(document['requirements'])} | `{blockers}` |"
        )
    lines.extend([
        "",
        "Each command selected at least one relevant test in every Cargo invocation. The validator",
        "rejects exit-zero output containing zero selected tests and rechecks the captured count.",
        "",
        "## Blockers",
        "",
        "- `EXT-01`: trusted Linux x86_64 cgroup v2, Landlock, helper, filesystem, network, limits, and process runtime artifacts are absent.",
        "- `EXT-04`: the equivalent Linux aarch64 runtime artifacts are absent.",
        "- `EXT-19`: Windows Job Object, ConPTY, trusted runtime helper, and isolation-provider artifacts are absent.",
        "- `EXT-20`: macOS per-run VM escape and zero-survivor artifacts are absent.",
        "- `EXT-22`: production attempt-owned PTY helper daemon-SIGKILL/restart evidence does not yet exist.",
        "- `G01` remains blocked externally and `G02` remains blocked transitively; G03 cannot release over either dependency.",
        "",
        "No local source result is relabeled as external conformance, and no release PASS is claimed.",
        "",
    ])
    REPORT.write_text("\n".join(lines), encoding="utf-8")


def write_m004_report(documents):
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    set_digest = g00.attestation_set_digest(paths)
    tree_digest = documents[0]["source_tree_digest"]
    selected = selected_records()
    classes = Counter(record["record_class"] for record in selected.values())
    projection_paths = [
        ROOT / "requirements/registry.yaml",
        ROOT / "requirements/evidence.yaml",
        ROOT / "requirements/tombstones.yaml",
        ROOT / "requirements/id-ledger.yaml",
        ROOT / "requirements/report.md",
    ]
    test_total = sum(relevant_test_count(document["artifact"]["command"], document["artifact"]) for document in documents)
    pending = sum(record["latest_result"] == "pending" for record in selected.values())
    passed = len(selected) - pending
    lines = [
        "# M004 Exit Report",
        "",
        "- Gate: `G04`",
        "- Milestone: `M004`",
        f"- Run date: `{date.today().isoformat()}`",
        "- Local source/mechanical verdict: **PASS_LOCAL**",
        "- Trusted production verdict: **BLOCKED_EXTERNAL (EXT-01, EXT-04, EXT-15, EXT-22)**",
        "- Overall verdict: **IN_PROGRESS (G02/G03 transitive; EXT-19/EXT-20 transitive)**",
        "- Exit bullets: **10/10 exercised locally; 7/10 passed locally; 2/10 blocked externally; 1/10 blocked transitively**",
        f"- Evidence commands: **{len(documents)}/{len(JOBS)} exited 0; {test_total} relevant tests passed**",
        f"- Candidate identity: `worktree:{tree_digest}`",
        f"- Source-tree SHA-256: `{tree_digest}`",
        f"- Local attestation-set SHA-256: `{set_digest}`",
        "- Release result: **FAIL**",
        "",
        "This candidate is an uncommitted worktree. Its local attestations prove source and",
        "mechanical conformance only, are not the immutable candidate-commit evidence required",
        "for release, and cannot substitute for trusted production dogfood, core, or statistics runs.",
        "",
        "## Unit Status",
        "",
        "| Unit | Status | Evidence scope |",
        "| --- | --- | --- |",
        "| `5.01` | `PASS_LOCAL` | workspace revision source/conformance |",
        "| `5.02` | `PASS_LOCAL` | bounded lexical search source/conformance |",
        "| `5.03` | `PASS_LOCAL` | discover/read/cursor source/conformance |",
        "| `5.04` | `PASS_LOCAL` | edit IR normalization source/conformance |",
        "| `5.05` | `PASS_LOCAL` | path authorization adversarial source/conformance |",
        "| `5.06` | `PASS_LOCAL` | edit validation source/conformance |",
        "| `5.07` | `PASS_LOCAL` | staging/formatter source/conformance |",
        "| `5.08` | `PASS_LOCAL` | recovery crash/cancellation source/conformance |",
        "| `5.09` | `PASS_LOCAL` | verification-profile source/conformance |",
        "| `5.10` | `PASS_LOCAL` | diagnostic feedback source/conformance |",
        "| `5.11` | `PASS_LOCAL` | grammar edit-path source/conformance |",
        "| `5.12` | `PASS_LOCAL` | native capability-kernel adversarial source/conformance |",
        "| `5.13` | `BLOCKED_EXTERNAL` | local dogfood passed; trusted production dogfood absent |",
        "| `5.14` | `BLOCKED_EXTERNAL` | source semantics passed; trusted production core absent |",
        "| `5.15` | `BLOCKED_EXTERNAL` | ConformanceSourceSemantics report passed; ProductionTrusted statistics absent |",
        "",
        "## Registry",
        "",
        f"- M004 records: {len(selected)}",
        f"- Local pass records with current artifact/environment digests: {passed}",
        f"- Trusted-evidence pending records with null artifact/environment digests: {pending}",
        f"- Requirements: {classes['requirement']}",
        f"- Promises: {classes['promise']}",
        f"- Decisions: {classes['decision']}",
        f"- Risks: {classes['risk']}",
        "",
        "## Generated Projections",
        "",
        "| Projection | SHA-256 |",
        "| --- | --- |",
    ]
    lines.extend(
        f"| `{path.relative_to(ROOT).as_posix()}` | `{g00.file_digest(path)}` |"
        for path in projection_paths
    )
    lines.extend([
        "",
        "## Local Evidence",
        "",
        "| Bullet | Dashboard | Disposition | Command | Passed tests | Artifact SHA-256 | Requirement records | Blockers |",
        "| --- | --- | --- | --- | ---: | --- | ---: | --- |",
    ])
    for document in documents:
        command = document["artifact"]["command"].replace("|", "\\|")
        blockers = ", ".join(document["external_blocker"] or []) or "none (local/mechanical)"
        lines.append(
            f"| `{document['dashboard_row']['source_ref']}` | `{document['dashboard_row']['evidence_id']}` | "
            f"`{document['disposition']}` | `{command}` | {relevant_test_count(document['artifact']['command'], document['artifact'])} | "
            f"`{document['artifact_digest']}` | {len(document['requirements'])} | `{blockers}` |"
        )
    stats = ROOT / "requirements/reports/m004/source-semantics"
    lines.extend([
        "",
        "Every Cargo invocation selected at least one test. Exit-zero output with zero selected tests",
        "is rejected both while writing and while validating retained attestations.",
        "",
        "## Statistical Source-Semantics Artifacts",
        "",
        "These retained files are labelled `ConformanceSourceSemantics`; they are not production evidence.",
        "",
        "| Artifact | SHA-256 |",
        "| --- | --- |",
    ])
    for name in (
        "preregistration.json",
        "registered-preregistration.json",
        "statistical-report.json",
        "statistical-report-receipt.json",
    ):
        path = stats / name
        lines.append(f"| `{path.relative_to(ROOT).as_posix()}` | `{g00.file_digest(path)}` |")
    lines.extend([
        "",
        "## Blockers",
        "",
        "- Direct: `EXT-01`/`EXT-22` trusted Linux helper evidence for production dogfood; `EXT-01`/`EXT-04`/`EXT-22` trusted isolated core/statistical execution; `EXT-15` production provider credentials and approved spend for production statistics.",
        "- Transitive: `G02` depends on externally blocked `G01`; `G03` depends on `G01` and remains blocked by `EXT-01`, `EXT-04`, `EXT-19`, `EXT-20`, and `EXT-22`.",
        "- No source-controlled local attestation is accepted by release governance.",
        "",
    ])
    REPORT.write_text("\n".join(lines), encoding="utf-8")


def generate():
    dashboard_rows()
    update_registry_contracts()
    initial = subprocess.run(
        ["python3", "scripts/generate_registry.py"],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )
    if initial.returncode:
        raise ValueError(initial.stdout + initial.stderr)
    results = run_jobs()
    environment_text = g00.environment()
    environment_digest = g00.sha256_bytes(environment_text.encode("utf-8"))
    version_data = g00.versions(environment_text)
    update_registry(results, environment_digest, version_data)
    generated = subprocess.run(
        ["python3", "scripts/generate_registry.py"],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )
    if generated.returncode:
        raise ValueError(generated.stdout + generated.stderr)
    if MILESTONE == "M001" and g00.generate():
        raise ValueError("G00 regeneration failed")
    write_attestations(results, environment_text, environment_digest, version_data)
    return validate()


def rebind():
    dashboard_rows()
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    if len(paths) != len(JOBS):
        raise ValueError(f"expected {len(JOBS)} retained attestations, found {len(paths)}")
    results = {}
    environment_text = None
    tree_digest = g00.source_tree_digest()
    for path in paths:
        document = json.loads(path.read_text(encoding="utf-8"))
        g00.require_rebind_source(document, tree_digest, path)
        evidence_id = document.get("dashboard_row", {}).get("evidence_id")
        artifact = document.get("artifact")
        if evidence_id not in COMMANDS or not isinstance(artifact, dict):
            raise ValueError(f"{path}: retained artifact is invalid")
        relevant_test_count(COMMANDS[evidence_id], artifact)
        results[evidence_id] = {
            "artifact": artifact,
            "artifact_digest": g00.artifact_digest(artifact),
        }
        if environment_text is None:
            environment_text = document.get("environment")
        elif environment_text != document.get("environment"):
            raise ValueError("retained attestation environments differ")
    if set(results) != set(COMMANDS):
        raise ValueError("retained evidence set differs from milestone commands")
    environment_digest = g00.sha256_bytes(environment_text.encode("utf-8"))
    write_attestations(results, environment_text, environment_digest, g00.versions(environment_text))
    return validate()


def validate(release=False):
    if release:
        print(
            f"{MILESTONE} attestation error: source-controlled local attestations are rejected for release",
            file=sys.stderr,
        )
        return 1
    errors = []
    try:
        rows = dashboard_rows()
        all_records = records()
        selected = selected_records(all_records)
        bindings = record_bindings(all_records)
    except (KeyError, OSError, TypeError, ValueError, yaml.YAMLError) as error:
        print(f"{MILESTONE} attestation error: {error}", file=sys.stderr)
        return 1
    tree_digest = g00.source_tree_digest()
    identity = f"worktree:{tree_digest}"
    paths = sorted(ATTESTATION_DIR.glob("*.json"))
    if len(paths) != len(JOBS):
        errors.append(f"expected {len(JOBS)} attestations, found {len(paths)}")
    seen = set()
    seen_records = set()
    documents = []
    common = None
    for path in paths:
        try:
            document = json.loads(path.read_text(encoding="utf-8"))
            if not isinstance(document, dict) or set(document) != FIELDS:
                raise ValueError("fields differ from milestone schema v2")
        except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
            errors.append(f"{path}: invalid attestation: {error}")
            continue
        evidence_id = document.get("dashboard_row", {}).get("evidence_id")
        if evidence_id in seen or evidence_id not in COMMANDS:
            errors.append(f"{path}: duplicate or unknown dashboard evidence {evidence_id!r}")
            continue
        seen.add(evidence_id)
        if path.stem != evidence_id:
            errors.append(f"{path}: filename does not match dashboard evidence")
        artifact = document.get("artifact")
        expected_artifact = artifact
        if isinstance(artifact, dict):
            try:
                relevant_test_count(COMMANDS[evidence_id], artifact)
                if artifact.get("evidence_id") != evidence_id:
                    raise ValueError("artifact evidence_id does not match dashboard evidence")
            except (TypeError, ValueError) as error:
                errors.append(f"{path}: zero-test or invalid local evidence: {error}")
        expected = {
            "schema_version": 2,
            "milestone": MILESTONE,
            "gate": GATE,
            "candidate_identity": identity,
            "source_tree_digest": tree_digest,
            "base_commit_sha": g00.current_base_commit(),
            "dashboard_row": rows[evidence_id],
            "artifact": expected_artifact,
            "artifact_digest": g00.artifact_digest(expected_artifact),
            "environment_digest": g00.sha256_bytes(str(document.get("environment", "")).encode("utf-8")),
            "requirements": bindings[evidence_id],
            "disposition": (M003_ROW_STATUS if MILESTONE == "M003" else M004_ROW_STATUS)[evidence_id] if MILESTONE in {"M003", "M004"} else ("blocked_external" if evidence_id in BLOCKED_JOBS else "pass"),
            "external_blocker": EXTERNAL_BLOCKERS.get(evidence_id),
            "trust_scope": TRUST_SCOPE,
            "trusted_for_release": False,
            "workflow_ref": WORKFLOW_REF,
        }
        for field, value in expected.items():
            if document.get(field) != value:
                errors.append(f"{path}: {field} is not bound to current {MILESTONE} evidence")
        try:
            stored_versions = g00.versions(document.get("environment"))
        except (AttributeError, IndexError):
            stored_versions = None
        if document.get("versions") != stored_versions:
            errors.append(f"{path}: versions are not bound to the environment")
        for binding in document.get("requirements", []):
            record_id = binding.get("record_id")
            if record_id in seen_records:
                errors.append(f"{path}: requirement {record_id} appears more than once")
            seen_records.add(record_id)
            record = selected.get(record_id, {})
            if MILESTONE in {"M003", "M004"}:
                criterion = (
                    f"{COMMANDS[evidence_id]} exits 0 with at least 1 relevant test per cargo invocation; "
                    "this local-only result is not trusted release evidence"
                )
                external = MILESTONE == "M003" or evidence_id in {"EV-G04-001", "EV-G04-009"}
                valid_result = (
                    record.get("status") == "active"
                    and record.get("latest_result") == "pending"
                    and record.get("artifact_digest") is None
                    and record.get("environment_digest") is None
                    and record.get("versions") is None
                ) if external else (
                    record.get("status") == ("mitigated" if record.get("record_class") == "risk" else "implemented")
                    and record.get("latest_result") == "pass"
                    and record.get("artifact_digest") == document.get("artifact_digest")
                    and record.get("environment_digest") == document.get("environment_digest")
                    and record.get("versions") == document.get("versions")
                )
                lane = M003_RECORD_LANE[evidence_id] if MILESTONE == "M003" else "evidence-report"
                if not (
                    valid_result
                    and record.get("acceptance_criteria") == [criterion]
                    and record.get("evidence_job") == lane
                    and record.get("expected_result") == criterion
                ):
                    errors.append(f"{path}: {record_id} has an untruthful local/trusted evidence result")
            else:
                if record.get("artifact_digest") != document.get("artifact_digest"):
                    errors.append(f"{path}: {record_id} artifact digest differs from its job")
                if record.get("environment_digest") != document.get("environment_digest"):
                    errors.append(f"{path}: {record_id} environment digest differs from its job")
                expected_status = "mitigated" if record.get("record_class") == "risk" else "implemented"
                if record.get("status") != expected_status or record.get("latest_result") != "pass":
                    errors.append(f"{path}: {record_id} is not honestly closed by passing evidence")
                if record.get("versions") != document.get("versions"):
                    errors.append(f"{path}: {record_id} versions differ from its job")
        provenance = {
            key: document.get(key)
            for key in (
                "base_commit_sha",
                "candidate_identity",
                "environment",
                "environment_digest",
                "run_id",
                "source_tree_digest",
                "versions",
            )
        }
        if common is None:
            common = provenance
        elif common != provenance:
            errors.append(f"{path}: provenance differs within the attestation set")
        documents.append(document)
    if seen != set(COMMANDS):
        errors.append(f"attestation evidence set mismatch: {sorted(set(COMMANDS) - seen)}")
    if seen_records != set(selected):
        errors.append(f"attested requirement set mismatch: {sorted(set(selected) - seen_records)}")
    if MILESTONE == "M001":
        external = {record_id: record for record_id, record in ((r["id"], r) for r in all_records) if record_id in EXTERNAL_RECORDS}
        for record_id, record in external.items():
            if record.get("status") == "implemented" or record.get("latest_result") == "pass":
                errors.append(f"{record_id}: EXT-08 conformance cannot be passed by local fake-PKI evidence")
        for record_id in FUTURE_PRIMARY:
            record = next((item for item in all_records if item["id"] == record_id), None)
            if not record or record.get("primary_milestone") != FUTURE_PRIMARY[record_id]:
                errors.append(f"{record_id}: future mechanism primary milestone is incorrect")
    elif MILESTONE == "M002":
        for record in all_records:
            if record.get("primary_milestone") == MILESTONE and record.get("applicability") != "mandatory":
                if record.get("status") == "implemented" or record.get("latest_result") == "pass":
                    errors.append(f"{record['id']}: pending-VOI mechanism was prematurely promoted")
    if paths:
        set_digest = g00.attestation_set_digest(paths)
        report = REPORT.read_text(encoding="utf-8") if REPORT.is_file() else ""
        result_checks = (
            "Result: **BLOCKED_EXTERNAL (EXT-05, EXT-08)**",
            "Exit bullets: **12/15 passed; 3/15 blocked by external prerequisites**",
        ) if MILESTONE == "M001" else (
            "Local mechanism result: **PASS**",
            "Overall result: **BLOCKED_TRANSITIVE (G01)**",
            "Exit bullets: **10/10 passed locally**",
        ) if MILESTONE == "M002" else (
            "Local source/conformance result: **PASS_LOCAL**",
            "Overall result: **BLOCKED_EXTERNAL (EXT-01, EXT-04, EXT-19, EXT-20, EXT-22; G01/G02 transitive)**",
            "Exit bullets: **10/10 exercised locally; 8/10 blocked externally; 1/10 in progress; 1/10 blocked transitively**",
        ) if MILESTONE == "M003" else (
            "Local source/mechanical verdict: **PASS_LOCAL**",
            "Trusted production verdict: **BLOCKED_EXTERNAL (EXT-01, EXT-04, EXT-15, EXT-22)**",
            "Overall verdict: **IN_PROGRESS (G02/G03 transitive; EXT-19/EXT-20 transitive)**",
            "Exit bullets: **10/10 exercised locally; 7/10 passed locally; 2/10 blocked externally; 1/10 blocked transitively**",
        )
        checks = result_checks + (
            f"Candidate identity: `{identity}`",
            f"Source-tree SHA-256: `{tree_digest}`",
            f"Local attestation-set SHA-256: `{set_digest}`",
        )
        for check in checks:
            if check not in report:
                errors.append(f"{MILESTONE} report does not bind {check}")
        for document in documents:
            command = document["artifact"]["command"].replace("|", "\\|")
            if MILESTONE in {"M003", "M004"}:
                blockers = ", ".join(document["external_blocker"] or []) or (
                    "none (local/mechanical)" if MILESTONE == "M004" else "uncommitted/in progress"
                )
                row = (
                    f"| `{document['dashboard_row']['source_ref']}` | `{document['dashboard_row']['evidence_id']}` | "
                    f"`{document['disposition']}` | `{command}` | {relevant_test_count(document['artifact']['command'], document['artifact'])} | "
                    f"`{document['artifact_digest']}` | {len(document['requirements'])} | `{blockers}` |"
                )
            else:
                row = (
                    f"| `{document['dashboard_row']['source_ref']}` | `{document['dashboard_row']['evidence_id']}` | "
                    f"`{document['disposition']}` | `{command}` | `{document['artifact_digest']}` | "
                    f"{len(document['requirements'])} |"
                )
            if row not in report:
                errors.append(f"{MILESTONE} report does not bind {document['dashboard_row']['evidence_id']}")
        if MILESTONE == "M004":
            stats = ROOT / "requirements/reports/m004/source-semantics"
            for name in (
                "preregistration.json",
                "registered-preregistration.json",
                "statistical-report.json",
                "statistical-report-receipt.json",
            ):
                path = stats / name
                if not path.is_file():
                    errors.append(f"M004 retained source-semantics artifact is missing: {name}")
                elif f"| `{path.relative_to(ROOT).as_posix()}` | `{g00.file_digest(path)}` |" not in report:
                    errors.append(f"M004 report does not bind retained source-semantics artifact: {name}")
            try:
                statistical = json.loads((stats / "statistical-report.json").read_text(encoding="utf-8"))
                receipt = json.loads((stats / "statistical-report-receipt.json").read_text(encoding="utf-8"))
                if statistical.get("evidence_source") != "conformance_source_semantics":
                    errors.append("M004 local statistical report must be ConformanceSourceSemantics")
                if receipt.get("evidence_source") != "conformance_source_semantics":
                    errors.append("M004 local statistical receipt must be ConformanceSourceSemantics")
            except (OSError, TypeError, json.JSONDecodeError) as error:
                errors.append(f"M004 retained statistical evidence is invalid: {error}")
    if errors:
        for error in errors:
            print(f"{MILESTONE} attestation error: {error}", file=sys.stderr)
        return 1
    status = "IN_PROGRESS" if MILESTONE == "M004" else ("BLOCKED_EXTERNAL" if MILESTONE in {"M001", "M003"} else "BLOCKED_TRANSITIVE")
    blockers = {
        "M001": "EXT-05,EXT-08",
        "M002": "G01_BLOCKED_EXTERNAL",
        "M003": "EXT-01,EXT-04,EXT-19,EXT-20,EXT-22,G01,G02",
        "M004": "EXT-01,EXT-04,EXT-15,EXT-22,G02,G03,EXT-19_TRANSITIVE,EXT-20_TRANSITIVE",
    }[MILESTONE]
    print(
        f"{MILESTONE} local attestations valid: {len(paths)} jobs, {len(selected)} records, "
        f"local=PASS, status={status}, blockers={blockers}, candidate={identity}, "
        f"set_sha256={g00.attestation_set_digest(paths)}"
    )
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("milestone", nargs="?", default=MILESTONE, choices=["M001", "M002", "M003", "M004"])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true", help="run jobs and replace local evidence")
    mode.add_argument("--rebind", action="store_true", help="bind retained passing output to the current tree")
    mode.add_argument("--release", action="store_true", help="reject source-controlled local evidence")
    mode.add_argument("--self-test", action="store_true", help="verify adversarial evidence is rejected")
    args = parser.parse_args(argv)
    if args.self_test:
        for label, command in (
            ("hidden semicolon", "true ; cargo test --test conformance"),
            ("comment spoof", "true # cargo test --test conformance"),
            ("printf spoof", "printf 'cargo test --test conformance'"),
        ):
            try:
                g00.command_steps(command)
            except ValueError:
                continue
            raise ValueError(f"{label} command was accepted")

        command = "cargo test first && cargo test second"
        outputs = (
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n",
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out;\n",
        )
        steps = []
        for manifest, output in zip(g00.command_steps(command), outputs):
            counts = [int(value) for value in re.findall(r"test result: ok\. ([0-9]+) passed;", output)]
            steps.append({
                "argv": manifest["argv"],
                "environment": manifest["environment"],
                "exit_code": 0,
                "output": output,
                "output_digest": g00.sha256_bytes(output.encode("utf-8")),
                "proof_kind": None,
                "test_count": sum(counts),
            })
        artifact = {"command": command, "evidence_id": "SELF-TEST", "exit_code": 0, "steps": steps}
        try:
            g00.validate_artifact(command, artifact)
        except ValueError:
            pass
        else:
            raise ValueError("multiple-command evidence with one zero-test step was accepted")
        try:
            g00.require_rebind_source({"source_tree_digest": "0" * 64}, "1" * 64, "self-test")
        except ValueError:
            pass
        else:
            raise ValueError("old-source retained output was accepted for rebind")

        valid_proofs = (
            (["python3", "scripts/req_lint.py", "--coverage", "8-1597"], "0 unmapped\n"),
            (
                ["python3", "scripts/req_lint.py", "--aggregate"],
                f"{len(g00.records())} record(s) across "
                f"{len(g00.discover_shards(g00.ROOT / 'requirements/registry.d'))} shard(s), 0 finding(s)\n",
            ),
            (["python3", "scripts/check_architecture.py", "binary"], "cargo metadata: exactly 1 binary target: kit\n"),
            (
                ["python3", "scripts/generate_registry.py", "--check"],
                f"generated 5 projections from {len(g00.records())} records\n",
            ),
            (["python3", "-m", "openapi_spec_validator", "docs/api/openapi.yaml"], "docs/api/openapi.yaml: OK\n"),
            (["python3", "scripts/check_dogfood_harness.py"], "dogfood harness: separate black-box package using only the Kit executable and public surfaces\n"),
            (
                [
                    "check-jsonschema", "--schemafile",
                    "eval/preregistration/schema/v1/preregistration.schema.json",
                    "requirements/reports/m004/source-semantics/preregistration.json",
                ],
                "ok -- validation done\n",
            ),
        )
        for argv, output in valid_proofs:
            g00.validate_proof(g00.proof_kind(argv), argv, output)

        invalid_outputs = (
            "10 unmapped\n",
            "prefix 0 unmapped\n",
            "0 unmapped suffix\n",
            "0 unmapped\n0 unmapped\n",
            "",
        )
        coverage = ["python3", "scripts/req_lint.py", "--coverage", "8-1597"]
        for output in invalid_outputs:
            try:
                g00.validate_proof("requirement_lint", coverage, output)
            except ValueError:
                pass
            else:
                raise ValueError(f"adversarial coverage summary was accepted: {output!r}")
        aggregate = ["python3", "scripts/req_lint.py", "--aggregate"]
        for output in (
            f"{len(g00.records())} record(s) across {len(g00.discover_shards(g00.ROOT / 'requirements/registry.d'))} shard(s), 10 finding(s)\n",
            f"{len(g00.records())} record(s) across {len(g00.discover_shards(g00.ROOT / 'requirements/registry.d'))} shard(s), 0 finding(s)\n0 finding(s)\n",
        ):
            try:
                g00.validate_proof("requirement_lint", aggregate, output)
            except ValueError:
                pass
            else:
                raise ValueError(f"adversarial aggregate summary was accepted: {output!r}")

        for argv in (
            ["check-jsonschema", "--schemafile", "Cargo.lock", "requirements/reports/m004/source-semantics/preregistration.json"],
            ["check-jsonschema", "--schemafile", "eval/preregistration/schema/v1/preregistration.schema.json", "Cargo.lock"],
            ["check-jsonschema", "--schemafile", "eval/preregistration/schema/v1/registration.schema.json", "requirements/reports/m004/source-semantics/preregistration.json"],
        ):
            try:
                g00.proof_kind(argv)
            except ValueError:
                pass
            else:
                raise ValueError(f"arbitrary schema/target proof was accepted: {argv!r}")
        schema_argv = valid_proofs[-1][0]
        for output in ("", "prefix ok -- validation done\n", "ok -- validation done\nok -- validation done\n"):
            try:
                g00.validate_proof("json_schema", schema_argv, output)
            except ValueError:
                pass
            else:
                raise ValueError(f"invalid schema validation summary was accepted: {output!r}")

        print("milestone validator self-test passed: command, summary, schema-target, zero-test, and source-binding spoofs rejected")
        return 0
    configure(args.milestone)
    try:
        return generate() if args.write else rebind() if args.rebind else validate(release=args.release)
    except (KeyError, OSError, TypeError, ValueError, yaml.YAMLError) as error:
        print(f"{MILESTONE} attestation error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
