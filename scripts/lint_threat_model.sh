#!/bin/sh
# Unit 1.03 content lint. Optional paths allow adversarial seeded copies.

set -eu

threat_model=${1:-docs/decisions/threat-model.md}
fault_matrix=${2:-docs/decisions/fault-matrix.md}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(dirname -- "$script_dir")

expected_titles='Local and remote API ingress|State root, SQLite and artifacts|Workspace mutation and hostile repositories|Local, container and VM execution|Model/provider calls and prompt injection|Native, MCP and composed capabilities|ACP children and A2A peers|Secrets, URLs, redirects and egress|Telemetry, retention, backups and deletion|Clustered control plane and executors'
expected_classes='authority|persistence|authority + persistence|authority|authority|authority|authority + persistence|authority|persistence|authority + persistence'
required_subboundaries='CI and release evidence authority|Hidden-grader custody|Configuration and extension loading'

for file in "$threat_model" "$fault_matrix"; do
    if [ ! -f "$file" ]; then
        printf 'FAIL: missing file: %s\n' "$file" >&2
        exit 1
    fi
done

check_document() {
    mode=$1
    file=$2

    awk -v mode="$mode" -v expected_titles="$expected_titles" \
        -v expected_classes="$expected_classes" \
        -v required_subboundaries="$required_subboundaries" '
        BEGIN {
            expected_count = split(expected_titles, titles, "[|]")
            split(expected_classes, classes, "[|]")
            required_sub_count = split(required_subboundaries, required_subs, "[|]")
            fixture_count = split("providers repos protocol_sim clock crashpoints storefault sandbox_probe", fixtures, " ")
        }

        function fail(message) {
            printf "FAIL: %s: %s\n", mode, message > "/dev/stderr"
            failed = 1
        }

        function trim(value) {
            gsub(/^[ \t]+|[ \t]+$/, "", value)
            return value
        }

        function concrete(value, normalized) {
            normalized = tolower(trim(value))
            gsub(/[`*_#.:-]/, "", normalized)
            normalized = trim(normalized)
            return length(normalized) >= 12 && normalized !~ /^(x|xx|n\/a|na|none|unknown|placeholder|pending|later|test|owner)$/
        }

        function value_after(label, value) {
            value = $0
            sub("^\\*\\*" label ":\\*\\*[ \t]*", "", value)
            return trim(value)
        }

        function accountable_owner(value) {
            return concrete(value) && value ~ /M[0-9][0-9][0-9]-W[0-9][0-9]/
        }

        function has_fixture(value, i) {
            for (i = 1; i <= fixture_count; i++)
                if (index(value, fixtures[i])) return 1
            return 0
        }

        function finish_abuse() {
            if (current_abuse == "") return
            abuse_count++
            if (!concrete(current_abuse))
                fail(section_label " has a placeholder/non-concrete abuse case")
            current_abuse = ""
        }

        function finish_fault() {
            if (current_fault == "") return
            fault_count++
            if (!concrete(current_fault))
                fail(section_label " has a placeholder/non-concrete fault case")
            if (!has_fixture(current_fault) || index(tolower(current_fault), "assert") == 0)
                fail(section_label " fault cases must name a registered fixture and asserted outcome")
            current_fault = ""
        }

        function mitigation_count(value, pieces, count, i) {
            count = split(value, pieces, ";")
            mitigation_cases = 0
            for (i = 1; i <= count; i++)
                if (concrete(pieces[i])) mitigation_cases++
            return mitigation_cases
        }

        function finish_section( expected_number, expected_parent) {
            if (!in_section) return
            finish_abuse()
            finish_fault()

            if (section_kind == "boundary") {
                expected_number = boundary_count
                if (number != expected_number)
                    fail("boundary numbers must be exactly 1..10; expected " expected_number ", found " number)
                if (title != titles[expected_number])
                    fail("Boundary " number " title must be exactly \"" titles[expected_number] "\"; found \"" title "\"")
                if (boundary_class != classes[expected_number])
                    fail("Boundary " number " must declare class \"" classes[expected_number] "\"")
            } else {
                if (sub_seen[title]++) fail("duplicate documented sub-boundary \"" title "\"")
                expected_parent = 0
                if (title == "CI and release evidence authority") expected_parent = 9
                if (title == "Hidden-grader custody") expected_parent = 4
                if (title == "Configuration and extension loading") expected_parent = 6
                if (expected_parent && parent_number != expected_parent)
                    fail("sub-boundary \"" title "\" must map to Boundary " expected_parent)
                if (parent_number !~ /^([1-9]|10)$/)
                    fail("sub-boundary \"" title "\" has an invalid canonical parent")
                if (boundary_class != "authority" && boundary_class != "persistence" && boundary_class != "authority + persistence")
                    fail("sub-boundary \"" title "\" has an invalid Boundary class")
            }

            if (!accountable_owner(evidence_owner_text))
                fail(section_label " needs an accountable MNNN-WNN Evidence owner")

            if (mode == "threat-model") {
                if (!concrete(assets_text)) fail(section_label " is missing concrete Assets")
                if (!concrete(trust_text)) fail(section_label " is missing a concrete Trust transition")
                if (!concrete(actors_text)) fail(section_label " is missing concrete Actors")
                if (mitigation_count(mitigation_text) < 2)
                    fail(section_label " needs at least two concrete mitigation cases separated by semicolons")
                if (!concrete(fail_closed_text)) fail(section_label " is missing concrete Fail-closed behavior")
                if (!concrete(rfc_text) || index(rfc_text, "RFC.md") == 0)
                    fail(section_label " needs a concrete RFC.md reference")
                if (!abuse_heading || abuse_count < 2)
                    fail(section_label " needs at least two concrete Abuse cases")
                if (!fault_heading || fault_count < 2)
                    fail(section_label " needs at least two concrete Fault-injection points")
            } else {
                if (!concrete(intent_text) || index(tolower(intent_text), "fixed") == 0 || index(tolower(intent_text), "replay") == 0)
                    fail(section_label " needs deterministic fixture intent naming fixed inputs and replay")
                if (!table_header || data_rows < 2)
                    fail(section_label " needs the fault table header and at least two fixture rows")
            }
        }

        function reset_section() {
            boundary_class = evidence_owner_text = ""
            assets_text = trust_text = actors_text = mitigation_text = ""
            fail_closed_text = rfc_text = intent_text = ""
            abuse_heading = fault_heading = abuse_count = fault_count = 0
            abuse_section = fault_section = in_mitigation = in_intent = 0
            active_field = ""
            current_abuse = current_fault = ""
            table_header = data_rows = 0
        }

        {
            lower = tolower($0)
            if (lower ~ /(^|[^a-z])(todo|fixme|tbd)([^a-z]|$)/)
                fail("line " NR " contains unfinished text")
        }

        /^## Boundary / {
            finish_section()
            if ($0 !~ /^## Boundary [0-9]+: .+$/) {
                fail("line " NR " has a malformed canonical boundary heading")
                next
            }
            boundary_count++
            in_section = 1
            section_kind = "boundary"
            number = $0
            sub(/^## Boundary /, "", number)
            sub(/:.*/, "", number)
            title = $0
            sub(/^## Boundary [0-9]+: /, "", title)
            section_label = "Boundary " number
            reset_section()
            next
        }

        /^### Sub-boundary:/ {
            finish_section()
            if ($0 !~ /^### Sub-boundary: .+ \(maps to Boundary [0-9]+\)$/) {
                fail("line " NR " has a malformed sub-boundary heading")
                next
            }
            in_section = 1
            section_kind = "sub-boundary"
            title = $0
            sub(/^### Sub-boundary: /, "", title)
            sub(/ \(maps to Boundary [0-9]+\)$/, "", title)
            parent_number = $0
            sub(/^.*\(maps to Boundary /, "", parent_number)
            sub(/\)$/, "", parent_number)
            section_label = "Sub-boundary \"" title "\""
            reset_section()
            next
        }

        !in_section { next }

        /^#### Abuse cases[ \t]*$/ {
            finish_fault()
            abuse_heading = abuse_section = 1
            fault_section = in_mitigation = in_intent = 0
            active_field = ""
            next
        }
        /^#### Fault-injection points[ \t]*$/ {
            finish_abuse()
            fault_heading = fault_section = 1
            abuse_section = in_mitigation = in_intent = 0
            active_field = ""
            next
        }

        /^\*\*Boundary class:\*\*/ {
            boundary_class = value_after("Boundary class")
            sub(/\.[ \t]*$/, "", boundary_class)
            active_field = ""
            next
        }
        /^\*\*Assets:\*\*/ { assets_text = value_after("Assets"); active_field = "assets"; next }
        /^\*\*Trust transition:\*\*/ { trust_text = value_after("Trust transition"); active_field = "trust"; next }
        /^\*\*Actors:\*\*/ { actors_text = value_after("Actors"); active_field = "actors"; next }
        /^\*\*Mitigations:\*\*/ {
            mitigation_text = value_after("Mitigations")
            in_mitigation = 1
            active_field = "mitigations"
            next
        }
        /^\*\*Fail-closed behavior:\*\*/ {
            fail_closed_text = value_after("Fail-closed behavior")
            in_mitigation = 0
            active_field = "fail_closed"
            next
        }
        /^\*\*RFC references:\*\*/ { rfc_text = value_after("RFC references"); active_field = "rfc"; next }
        /^\*\*Evidence owner:\*\*/ { evidence_owner_text = value_after("Evidence owner"); active_field = "owner"; next }
        /^\*\*Deterministic fixture intent:\*\*/ {
            intent_text = value_after("Deterministic fixture intent")
            in_intent = 1
            active_field = "intent"
            next
        }

        abuse_section && /^- Abuse:/ {
            finish_abuse()
            current_abuse = $0
            active_field = ""
            next
        }
        fault_section && /^- Fault([ :(])/ {
            finish_fault()
            current_fault = $0
            active_field = ""
            next
        }

        /^\| Fixture Family \| Fault Injection \| Expected Invariant \| Evidence \|$/ {
            table_header = 1
            active_field = ""
            next
        }
        /^\|/ {
            for (i = 1; i <= fixture_count; i++) {
                if ($0 ~ ("^\\| *" fixtures[i] " *\\|")) {
                    data_rows++
                    fixture_seen[fixtures[i]] = 1
                    split($0, cells, "\\|")
                    if (!concrete(cells[3])) fail(section_label " has a non-concrete fault-table injection")
                    if (!concrete(cells[4]) || index(cells[4], "RFC.md") == 0)
                        fail(section_label " has a fault-table invariant without an RFC.md reference")
                    evidence = trim(cells[5])
                    if (!concrete(evidence)) {
                        fail(section_label " has non-concrete fault-table evidence")
                    } else if (evidence ~ /^Implemented: /) {
                        quoted_count = split(evidence, quoted, "`")
                        if (quoted_count != 3 || quoted[1] != "Implemented: " ||
                            quoted[2] !~ /^(tests\/(fault|adversarial|integration)\/main\.rs::[a-z][a-z0-9_]*|scripts\/[A-Za-z0-9_.\/-]+#[A-Za-z0-9_.-]+)$/)
                            fail(section_label " has a malformed implemented evidence reference")
                        implemented_rows++
                    } else if (evidence ~ /^Planned: /) {
                        quoted_count = split(evidence, quoted, "`")
                        if (quoted_count != 5 || quoted[1] != "Planned: owner " ||
                            quoted[2] !~ /^M[0-9][0-9][0-9]-W[0-9][0-9]$/ ||
                            quoted[3] != "; gate " || !concrete(quoted[4]))
                            fail(section_label " planned evidence must name an MNNN-WNN owner and concrete gate")
                        if (evidence ~ /tests\/(fault|adversarial|integration)|[Vv]erified/)
                            fail(section_label " planned evidence must not claim test verification")
                        planned_rows++
                    } else {
                        fail(section_label " evidence must start with Implemented: or Planned:")
                    }
                }
            }
            next
        }

        /^[ \t]*$/ {
            in_mitigation = in_intent = 0
            active_field = ""
            finish_abuse()
            finish_fault()
            next
        }

        {
            if (active_field == "assets") assets_text = assets_text " " trim($0)
            if (active_field == "trust") trust_text = trust_text " " trim($0)
            if (active_field == "actors") actors_text = actors_text " " trim($0)
            if (active_field == "mitigations") mitigation_text = mitigation_text " " trim($0)
            if (active_field == "fail_closed") fail_closed_text = fail_closed_text " " trim($0)
            if (active_field == "rfc") rfc_text = rfc_text " " trim($0)
            if (active_field == "owner") evidence_owner_text = evidence_owner_text " " trim($0)
            if (active_field == "intent") intent_text = intent_text " " trim($0)
            if (abuse_section && current_abuse != "") current_abuse = current_abuse " " trim($0)
            if (fault_section && current_fault != "") current_fault = current_fault " " trim($0)
        }

        END {
            finish_section()
            if (boundary_count != expected_count)
                fail("expected exactly " expected_count " canonical boundaries, found " boundary_count)
            for (i = 1; i <= required_sub_count; i++)
                if (!sub_seen[required_subs[i]])
                    fail("missing required documented sub-boundary \"" required_subs[i] "\"")
            if (mode == "fault-matrix")
                for (i = 1; i <= fixture_count; i++)
                    if (!fixture_seen[fixtures[i]])
                        fail("fixture family \"" fixtures[i] "\" is not covered")
            if (mode == "fault-matrix" && (!implemented_rows || !planned_rows))
                fail("fault evidence must distinguish implemented and planned rows")
            if (failed) exit 1
        }
    ' "$file"
}

check_document threat-model "$threat_model"
check_document fault-matrix "$fault_matrix"

references_file=$(mktemp "${TMPDIR:-/tmp}/kit-threat-model-references.XXXXXX")
trap 'rm -f "$references_file"' EXIT HUP INT TERM

awk -F '|' '
    function trim(value) {
        gsub(/^[ \t]+|[ \t]+$/, "", value)
        return value
    }
    /^\|/ {
        evidence = trim($5)
        if (evidence ~ /^Implemented: /) {
            split(evidence, quoted, "`")
            print quoted[2]
        }
    }
' "$fault_matrix" > "$references_file"

while IFS= read -r reference; do
    case "$reference" in
        tests/fault/main.rs::*|tests/adversarial/main.rs::*|tests/integration/main.rs::*)
            test_file=${reference%%::*}
            symbol=${reference#*::}
            target=${test_file#tests/}
            target=${target%/main.rs}

            if [ ! -f "$repo_root/$test_file" ] || [ ! -f "$repo_root/Cargo.toml" ]; then
                printf 'FAIL: implemented Cargo target does not exist: %s\n' "$reference" >&2
                exit 1
            fi
            if grep -Eq '^[[:space:]]*autotests[[:space:]]*=[[:space:]]*false' "$repo_root/Cargo.toml"; then
                printf 'FAIL: Cargo automatic test target %s is disabled\n' "$target" >&2
                exit 1
            fi
            if ! awk -v symbol="$symbol" '
                /^[ \t]*#\[(tokio::)?test\][ \t]*$/ { test_attribute = 1; next }
                test_attribute {
                    if ($0 ~ "^[ \\t]*(pub([ \\t]+\([^)]*\))?[ \\t]+)?fn[ \\t]+" symbol "[ \\t]*\\(") found = 1
                    if ($0 !~ /^[ \t]*#\[/) test_attribute = 0
                }
                END { exit !found }
            ' "$repo_root/$test_file"; then
                printf 'FAIL: implemented test symbol does not exist as an exact #[test] fn: %s\n' "$reference" >&2
                exit 1
            fi
            ;;
        scripts/*#*)
            script=${reference%%#*}
            check=${reference#*#}
            if [ ! -f "$repo_root/$script" ] || ! grep -F -e "$check" "$repo_root/$script" >/dev/null; then
                printf 'FAIL: implemented script check does not exist: %s\n' "$reference" >&2
                exit 1
            fi
            ;;
        *)
            printf 'FAIL: unsupported implemented evidence reference: %s\n' "$reference" >&2
            exit 1
            ;;
    esac
done < "$references_file"

counts=$(awk -F '|' '
    /^\|/ {
        if ($5 ~ /^[ \t]*Implemented: /) implemented++
        if ($5 ~ /^[ \t]*Planned: /) planned++
    }
    END { print implemented + 0, planned + 0 }
' "$fault_matrix")
implemented_count=${counts%% *}
planned_count=${counts#* }

printf 'PASS: 10 canonical boundaries, 3 required sub-boundaries; %s implemented references verified; %s planned cases not reported as verified\n' \
    "$implemented_count" "$planned_count"
