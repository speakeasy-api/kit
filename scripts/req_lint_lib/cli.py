"""req_lint command-line interface.

Modes (mutually exclusive, exactly one required unless --list-checks/-h):
  --range START-END    unmapped normative-text count over an RFC line range
  --areas A,B,C         schema-validate the named registry shards
  --coverage START-END  unmapped normative-text count over the full registry
  --aggregate           merge every shard and run the Phase 0 checks
  --release-candidate   with --aggregate, require passing current evidence
  --list-checks         print the 6 distinct check reason strings, one per line
"""

import argparse
import os
import sys

from .checks import (
    CHECK_REASONS,
    Finding,
    REASON_MISSING_STALE_OR_FAILING_EVIDENCE,
    check_duplicate_ids,
    check_missing_governance_fields,
    check_missing_stale_or_failing_evidence,
    check_tombstone_without_replacement,
    check_unknown_citations,
    check_unregistered_normative_text,
)
from .loader import RegistryError, discover_shards, load_area_na, load_registry_dir
from .model import AREAS, SPECIAL_SHARDS
from .rfc_scan import parse_range, read_lines
from .validate import validate_record_schema
from .governance import check_history, check_inventory, check_release, check_semantics

NO_RECORDS_REASON = "no-records"
DEFAULT_SCAN_DIRS = ("tests", "eval")


def build_parser():
    parser = argparse.ArgumentParser(
        prog="req_lint.py",
        description=(
            "Lint the requirement registry: unmapped normative text, "
            "duplicate ids, governance gaps, unknown citations, dangling "
            "tombstones, and missing/stale/failing evidence."
        ),
    )
    parser.add_argument(
        "--registry-dir",
        default="requirements/registry.d",
        help="directory containing KIT-<AREA>.yaml (and _promises/_decisions/"
        "_risks.yaml) shard files (default: %(default)s)",
    )
    parser.add_argument(
        "--rfc",
        default="RFC.md",
        help="path to the RFC source scanned for normative text (default: %(default)s)",
    )
    parser.add_argument(
        "--scan-dir",
        action="append",
        default=None,
        help="directory to scan for requirement-id citations (repeatable; "
        "default: tests, eval)",
    )
    parser.add_argument(
        "--release-candidate",
        action="store_true",
        help="with --aggregate, reject missing, pending, stale, or failing evidence",
    )
    parser.add_argument(
        "--inventory",
        help="source inventory (default: source-inventory.yaml beside registry.d)",
    )
    parser.add_argument(
        "--id-ledger",
        help="historical id ledger (default: id-ledger.yaml beside registry.d)",
    )
    parser.add_argument(
        "--baseline-ref",
        default=os.environ.get("GOVERNANCE_BASE_REF"),
        help="explicit baseline Git commit SHA used for historical id comparison",
    )
    parser.add_argument(
        "--candidate-ref",
        default=os.environ.get("GOVERNANCE_CANDIDATE_SHA"),
        help="explicit candidate Git commit SHA paired with --baseline-ref",
    )
    parser.add_argument(
        "--baseline-file",
        help="explicit generated registry file used for historical id comparison",
    )
    parser.add_argument(
        "--attestation-dir",
        default=os.environ.get("KIT_ATTESTATION_DIR"),
        help="external directory of trusted release attestation JSON files",
    )

    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--range",
        metavar="START-END",
        help="report normative RFC lines in this range with no matching, "
        "unchanged registry record",
    )
    mode.add_argument(
        "--areas",
        metavar="AREA[,AREA...]",
        help="schema-validate the named shard(s), e.g. KIT-GOV,KIT-OUTCOME "
        "or _promises,_decisions,_risks",
    )
    mode.add_argument(
        "--coverage",
        metavar="START-END",
        help="like --range but always scans the full registry directory",
    )
    mode.add_argument(
        "--aggregate",
        action="store_true",
        help="merge every shard and run Phase 0 structure/governance checks",
    )
    mode.add_argument(
        "--list-checks",
        action="store_true",
        help="print the 6 distinct rejection-check reason strings, one per line",
    )
    return parser


def _resolve_scan_dirs(args):
    if args.scan_dir:
        return list(args.scan_dir)
    return [d for d in DEFAULT_SCAN_DIRS if os.path.isdir(d)]


def _citation_exclusions(args):
    if args.scan_dir:
        return []
    return [os.path.join("tests", "conformance", "req_lint_cases")]


def _print_findings(findings, stream):
    for finding in findings:
        print("%s: %s" % (finding.reason, finding.message), file=stream)


def _governance_paths(args):
    registry = os.path.normpath(args.registry_dir)
    base = os.path.dirname(registry) if os.path.basename(registry) == "registry.d" else registry
    inventory = args.inventory or os.path.join(base, "source-inventory.yaml")
    ledger = args.id_ledger or os.path.join(base, "id-ledger.yaml")
    policy = os.path.join(base, "policy", "optional.yaml")
    dashboards = os.path.join(base, "dashboards")
    return inventory, ledger, policy, dashboards


def _cmd_list_checks(stream):
    for _name, reason in CHECK_REASONS:
        print(reason, file=stream)
    return 0


def _cmd_range_or_coverage(args, value, stream):
    try:
        start, end = parse_range(value)
    except ValueError as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 2

    records, _shards = load_registry_dir(args.registry_dir)
    if not records:
        print(NO_RECORDS_REASON, file=stream)
        return 1

    findings = check_unregistered_normative_text(records, args.rfc, start, end)
    print("%d unmapped" % len(findings), file=stream)
    for finding in findings:
        print("  %s" % finding.message, file=stream)
    return 0 if not findings else 1


def _cmd_areas(args, value, stream):
    tokens = []
    for raw in value.split(","):
        raw = raw.strip()
        if not raw:
            continue
        if raw.endswith("-"):
            print(
                "error: %r is not normalized; trailing '-' is forbidden" % raw,
                file=sys.stderr,
            )
            return 2
        token = raw
        if token not in AREAS and token not in SPECIAL_SHARDS:
            print("error: %r is not a known area or special shard" % raw, file=sys.stderr)
            return 2
        if token in tokens:
            print("error: duplicate area token %r" % raw, file=sys.stderr)
            return 2
        tokens.append(token)

    if not tokens:
        print("error: --areas requires at least one area token", file=sys.stderr)
        return 2

    records = []
    empty_areas = load_area_na(args.registry_dir)
    for token in tokens:
        shard_records, _shards = load_registry_dir(
            args.registry_dir, shard_tokens=[token]
        )
        if not shard_records:
            shard_path = os.path.join(args.registry_dir, token + ".yaml")
            if token in empty_areas and os.path.isfile(shard_path):
                continue
            print("%s: %s" % (NO_RECORDS_REASON, token), file=stream)
            return 1
        records.extend(shard_records)

    problems = []
    for record in records:
        problems.extend(validate_record_schema(record, record["_shard"]))

    if problems:
        for problem in problems:
            print(problem, file=stream)
        return 1

    print("%d record(s) across %d area(s) valid" % (len(records), len(tokens)), file=stream)
    return 0


def _cmd_aggregate(args, stream):
    if args.release_candidate:
        if os.path.islink(args.rfc) or not os.path.isfile(args.rfc):
            raise RegistryError("release RFC must be a regular non-symlink file")
        if os.path.islink(args.registry_dir) or not os.path.isdir(args.registry_dir):
            raise RegistryError("release registry directory must be a regular non-symlink directory")
    shard_tokens = discover_shards(args.registry_dir)
    records, _shards = load_registry_dir(args.registry_dir, shard_tokens=shard_tokens)
    if not records:
        print(NO_RECORDS_REASON, file=stream)
        return 1

    schema_problems = []
    for record in records:
        schema_problems.extend(validate_record_schema(record, record["_shard"]))

    findings = []
    total_lines = len(read_lines(args.rfc))
    if total_lines:
        findings.extend(
            check_unregistered_normative_text(records, args.rfc, 1, total_lines)
        )
    findings.extend(check_duplicate_ids(records))
    findings.extend(check_missing_governance_fields(records))
    findings.extend(
        check_unknown_citations(
            records, _resolve_scan_dirs(args), _citation_exclusions(args)
        )
    )
    findings.extend(check_tombstone_without_replacement(records))
    inventory, ledger, policy, dashboards = _governance_paths(args)
    findings.extend(
        check_semantics(
            records,
            args.rfc,
            optional_policy_path=policy if os.path.isfile(policy) else None,
            dashboard_dir=dashboards if os.path.isdir(dashboards) else None,
        )
    )
    if os.path.isfile(inventory):
        findings.extend(
            check_inventory(
                records,
                args.rfc,
                inventory,
                root=os.path.dirname(os.path.abspath(args.rfc)) or ".",
            )
        )
    elif args.inventory:
        raise RegistryError("%s: inventory does not exist" % inventory)
    findings.extend(
        check_history(
            records,
            ledger_path=ledger if os.path.isfile(ledger) else None,
            baseline_ref=args.baseline_ref,
            baseline_file=args.baseline_file,
            candidate_ref=args.candidate_ref,
            release=args.release_candidate,
            root=os.path.dirname(os.path.abspath(args.rfc)) or ".",
        )
    )
    if args.release_candidate:
        if not args.baseline_ref or not args.candidate_ref:
            findings.append(
                Finding(
                    REASON_MISSING_STALE_OR_FAILING_EVIDENCE,
                    "release candidate requires explicit --baseline-ref and --candidate-ref commit SHAs",
                )
            )
        findings.extend(check_missing_stale_or_failing_evidence(records))
        findings.extend(
            check_release(
                records,
                attestation_dir=args.attestation_dir,
                root=os.path.dirname(os.path.abspath(args.rfc)) or ".",
                dashboard_dir=dashboards if os.path.isdir(dashboards) else None,
            )
        )

    print(
        "%d record(s) across %d shard(s), %d finding(s)"
        % (len(records), len(shard_tokens), len(schema_problems) + len(findings)),
        file=stream,
    )
    for problem in schema_problems:
        print("invalid-record: %s" % problem, file=stream)
    _print_findings(findings, stream)
    return 0 if not schema_problems and not findings else 1


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.release_candidate and not args.aggregate:
        parser.error("--release-candidate requires --aggregate")
    if args.baseline_ref and args.baseline_file:
        parser.error("--baseline-ref and --baseline-file are mutually exclusive")
    if args.release_candidate and args.baseline_file:
        parser.error("--baseline-file is local testing only and forbidden with --release-candidate")
    stream = sys.stdout

    try:
        if args.list_checks:
            return _cmd_list_checks(stream)
        if args.range is not None:
            return _cmd_range_or_coverage(args, args.range, stream)
        if args.coverage is not None:
            return _cmd_range_or_coverage(args, args.coverage, stream)
        if args.areas is not None:
            return _cmd_areas(args, args.areas, stream)
        if args.aggregate:
            return _cmd_aggregate(args, stream)
    except (OSError, RegistryError, RuntimeError) as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 2

    parser.error("no mode selected")
    return 2


if __name__ == "__main__":
    sys.exit(main())
