"""Scanning RFC source text for normative statements.

`fingerprint_line` is the canonical algorithm registry records must use to
populate `source_fingerprint`: SHA-256 of the stripped text covered by
`source_anchor`. The text may contain multiple lines.
"""

import hashlib

from .model import NORMATIVE_LINE_PATTERN


def fingerprint_line(text):
    return hashlib.sha256(text.strip().encode("utf-8")).hexdigest()


def read_lines(path):
    with open(path, "r", encoding="utf-8") as fh:
        return fh.readlines()


def scan_normative_lines(rfc_path, start, end):
    """Return normative (lineno, text) pairs in the 1-indexed range."""
    lines = read_lines(rfc_path)
    results = []
    for lineno in range(max(start, 1), min(end, len(lines)) + 1):
        text = lines[lineno - 1].rstrip("\n")
        if NORMATIVE_LINE_PATTERN.search(text):
            results.append((lineno, text))
    return results


def parse_range(value):
    """Parse a `START-END` CLI argument into (start, end) ints."""
    parts = value.split("-")
    if len(parts) != 2:
        raise ValueError("range must be START-END, got %r" % value)
    start, end = int(parts[0]), int(parts[1])
    if start < 1 or end < start:
        raise ValueError("invalid range %r" % value)
    return start, end
