"""Requirement registry tokens shared by the linter."""

import re

# IMPLEMENTATION_PLAN.md:87-117 — 29 normalized area prefixes.
AREAS = (
    "KIT-GOV", "KIT-OUTCOME", "KIT-ARCH", "KIT-AGENTKIT", "KIT-DOMAIN",
    "KIT-STORE", "KIT-PROMPT", "KIT-CONTEXT", "KIT-REPO", "KIT-TOOL",
    "KIT-CAP", "KIT-COMPOSE", "KIT-ENCODE", "KIT-EDIT", "KIT-VERIFY",
    "KIT-ROUTE", "KIT-RUNTIME", "KIT-COMPACT", "KIT-ACP", "KIT-A2A",
    "KIT-MCP", "KIT-SEC", "KIT-EXEC", "KIT-API", "KIT-OBS", "KIT-EVAL",
    "KIT-CONFIG", "KIT-VERSION", "KIT-RELEASE",
)

assert len(AREAS) == 29

# `_promises.yaml` / `_decisions.yaml` / `_risks.yaml` (IMPLEMENTATION_PLAN.md:145-147).
SPECIAL_SHARDS = ("_promises", "_decisions", "_risks")

# Requirement ID shape: KIT-<AREA>-NNN.
_AREA_ALTERNATION = "|".join(a[len("KIT-"):] for a in AREAS)
ID_PATTERN = re.compile(r"^KIT-(?:%s)-\d+$" % _AREA_ALTERNATION)

# Broad on purpose: malformed and unknown area citations must not evade the scan.
ID_TOKEN_PATTERN = re.compile(r"\bKIT-[A-Z][A-Z0-9]*-\d+\b")

# RFC 2119 normative keywords (matched as whole words).
NORMATIVE_KEYWORDS = (
    "MUST NOT", "MUST", "SHALL NOT", "SHALL", "SHOULD NOT", "SHOULD",
    "REQUIRED", "RECOMMENDED", "MAY NOT", "MAY", "OPTIONAL",
)
NORMATIVE_LINE_PATTERN = re.compile(
    r"\b(?:%s)\b" % "|".join(re.escape(k) for k in NORMATIVE_KEYWORDS)
)

TOMBSTONED_STATUSES = ("tombstoned", "tombstone")

EVIDENCE_CODES = {
    "conformance": "C",
    "evaluation": "E",
    "operational_assertion": "O",
    "manual_review": "M",
}

SOURCE_ANCHOR_PATTERN = re.compile(
    r"^(?P<file>[^:]+):(?P<start>\d+)(?:-(?P<end>\d+))?$"
)
