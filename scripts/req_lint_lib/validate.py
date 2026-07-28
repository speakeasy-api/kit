"""JSON Schema and shard consistency validation for registry records."""

import json
import os
from functools import lru_cache

from .loader import parse_source_anchor
from .model import AREAS, ID_PATTERN, SPECIAL_SHARDS


def record_label(record):
    return record.get("id") or "<no id>@%s" % record.get("_source_path", "?")


@lru_cache(maxsize=1)
def _schema_validator():
    try:
        from jsonschema import Draft202012Validator
    except ImportError as exc:
        raise RuntimeError("jsonschema is required to validate registry records") from exc

    schema_path = os.path.join(
        os.path.dirname(os.path.dirname(os.path.dirname(__file__))),
        "requirements",
        "schema",
        "record.schema.json",
    )
    with open(schema_path, "r", encoding="utf-8") as handle:
        schema = json.load(handle)
    Draft202012Validator.check_schema(schema)
    return Draft202012Validator(schema)


def validate_record_schema(record, shard_token):
    """Return a list of problem strings; empty means the record is valid."""
    instance = {key: value for key, value in record.items() if not key.startswith("_")}
    problems = [
        "%s: %s" % (record_label(record), error.message)
        for error in sorted(_schema_validator().iter_errors(instance), key=str)
    ]

    area = record.get("area")
    if area in AREAS:
        if shard_token not in SPECIAL_SHARDS and area != shard_token:
            problems.append(
                "%s: area %r does not match owning shard %r"
                % (record_label(record), area, shard_token)
            )
        rec_id = record.get("id")
        if isinstance(rec_id, str) and ID_PATTERN.fullmatch(rec_id):
            if not rec_id.startswith(area + "-"):
                problems.append(
                    "%s: id prefix does not match area %r"
                    % (record_label(record), area)
                )
    elif area is not None:
        problems.append(
            "%s: area %r is not one of the 29 normalized areas"
            % (record_label(record), area)
        )

    if parse_source_anchor(record.get("source_anchor")) is None:
        problems.append("%s: invalid source_anchor" % record_label(record))

    if shard_token not in SPECIAL_SHARDS:
        rec_id = record.get("id")
        if isinstance(rec_id, str) and rec_id and not ID_PATTERN.fullmatch(rec_id):
            problems.append(
                "%s: id does not match KIT-<AREA>-NNN" % record_label(record)
            )

    return problems
