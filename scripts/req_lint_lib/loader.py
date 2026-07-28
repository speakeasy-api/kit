"""Loading of requirement registry shard files."""

import os

from yaml_utils import YamlLoadError, load_yaml_file, load_yaml_text

from .model import AREAS, SOURCE_ANCHOR_PATTERN, SPECIAL_SHARDS


class RegistryError(Exception):
    pass


def _shard_token_from_filename(filename):
    if not filename.endswith(".yaml"):
        return None
    return filename[: -len(".yaml")]


def discover_shards(registry_dir):
    """Return sorted list of shard tokens present on disk (area names or
    special `_promises`/`_decisions`/`_risks` shards). Missing directory
    yields an empty list."""
    if not os.path.isdir(registry_dir):
        return []
    tokens = []
    for entry in sorted(os.listdir(registry_dir)):
        token = _shard_token_from_filename(entry)
        if token is None:
            continue
        if token in AREAS or token in SPECIAL_SHARDS:
            tokens.append(token)
        elif token.startswith("KIT-") or token.startswith("_"):
            raise RegistryError("%s: unknown or non-normalized shard name" % entry)
    return tokens


def load_shard(registry_dir, shard_token):
    """Load one shard file's records. Missing file -> empty list."""
    path = os.path.join(registry_dir, shard_token + ".yaml")
    if not os.path.isfile(path):
        return []
    try:
        data = load_yaml_file(path)
    except YamlLoadError as exc:
        raise RegistryError(str(exc)) from exc
    if data is None:
        return []
    if not isinstance(data, list):
        raise RegistryError("%s: expected a list of records at top level" % path)
    records = []
    for item in data:
        if not isinstance(item, dict):
            raise RegistryError("%s: record is not a mapping: %r" % (path, item))
        record = dict(item)
        record["_shard"] = shard_token
        record["_source_path"] = path
        records.append(record)
    return records


def load_registry_dir(registry_dir, shard_tokens=None):
    """Load records for the given shard tokens (default: every shard found
    on disk). Returns (records, shards_seen)."""
    if shard_tokens is None:
        shard_tokens = discover_shards(registry_dir)
    records = []
    for token in shard_tokens:
        records.extend(load_shard(registry_dir, token))
    return records, list(shard_tokens)


def load_area_na(registry_dir):
    """Load the policy entries that permit exact area shards to be empty."""
    path = os.path.join(
        os.path.dirname(os.path.normpath(registry_dir)), "policy", "area-na.yaml"
    )
    if not os.path.isfile(path):
        return set()
    try:
        data = load_yaml_file(path)
    except YamlLoadError as exc:
        raise RegistryError(str(exc)) from exc
    if not isinstance(data, dict) or not isinstance(data.get("areas"), list):
        raise RegistryError("%s: expected an areas list" % path)

    areas = set()
    for entry in data["areas"]:
        if not isinstance(entry, dict):
            raise RegistryError("%s: area entry is not a mapping" % path)
        area = entry.get("area")
        if area not in AREAS:
            raise RegistryError("%s: unknown or non-normalized area %r" % (path, area))
        if area in areas:
            raise RegistryError("%s: duplicate area %s" % (path, area))
        if parse_source_anchor(entry.get("source")) is None or not entry.get("reason"):
            raise RegistryError("%s: %s requires a source citation and reason" % (path, area))
        areas.add(area)
    return areas


def parse_source_anchor(value):
    """Parse a `source_anchor` value like `RFC.md:138` or `RFC.md:138-143`.

    Returns (file, start, end) or None if unparseable.
    """
    if not isinstance(value, str):
        return None
    match = SOURCE_ANCHOR_PATTERN.match(value.strip())
    if not match:
        return None
    start = int(match.group("start"))
    end = int(match.group("end")) if match.group("end") else start
    if start < 1 or end < start:
        return None
    return match.group("file"), start, end


def load_yaml_document(path):
    """Load an arbitrary governance YAML document through the shared limits."""
    try:
        return load_yaml_file(path)
    except YamlLoadError as exc:
        raise RegistryError(str(exc)) from exc


def load_yaml_string(text, source):
    try:
        return load_yaml_text(text, source)
    except YamlLoadError as exc:
        raise RegistryError(str(exc)) from exc
