"""Bounded, duplicate-key-rejecting YAML loading for governance inputs."""

from pathlib import Path

import yaml
from yaml.events import AliasEvent
from yaml.nodes import MappingNode, ScalarNode


MAX_YAML_BYTES = 64 * 1024 * 1024
MAX_YAML_DEPTH = 64
MAX_YAML_ALIASES = 10_000
MAX_YAML_NODES = 1_000_000


class YamlLoadError(ValueError):
    pass


class _BoundedSafeLoader(yaml.SafeLoader):
    def __init__(self, stream):
        super().__init__(stream)
        self._depth = 0
        self._aliases = 0
        self._nodes = 0

    def compose_node(self, parent, index):
        if self.check_event(AliasEvent):
            self._aliases += 1
            if self._aliases > MAX_YAML_ALIASES:
                raise YamlLoadError(f"YAML alias limit exceeds {MAX_YAML_ALIASES}")
        self._depth += 1
        if self._depth > MAX_YAML_DEPTH:
            raise YamlLoadError(f"YAML nesting depth exceeds {MAX_YAML_DEPTH}")
        try:
            node = super().compose_node(parent, index)
            self._nodes += 1
            if self._nodes > MAX_YAML_NODES:
                raise YamlLoadError(f"YAML node limit exceeds {MAX_YAML_NODES}")
            return node
        finally:
            self._depth -= 1

    def construct_mapping(self, node, deep=False):
        if not isinstance(node, MappingNode):
            return super().construct_mapping(node, deep=deep)
        seen = {}
        for key_node, _value_node in node.value:
            if not isinstance(key_node, ScalarNode):
                raise YamlLoadError("governance YAML mapping keys must be scalars")
            if key_node.tag == "tag:yaml.org,2002:merge":
                raise YamlLoadError("YAML merge keys are forbidden")
            key = self.construct_object(key_node, deep=True)
            try:
                previous = seen.get(key)
            except TypeError as error:
                raise YamlLoadError("governance YAML mapping keys must be scalar values") from error
            if key in seen:
                mark = key_node.start_mark
                raise YamlLoadError(
                    f"duplicate or semantically equivalent YAML key {key_node.value!r} "
                    f"at line {mark.line + 1} (first at line {previous})"
                )
            seen[key] = key_node.start_mark.line + 1
        return super().construct_mapping(node, deep=deep)


def load_yaml_text(text, source="<string>"):
    if not isinstance(text, str):
        raise YamlLoadError(f"{source}: YAML input must be text")
    size = len(text.encode("utf-8"))
    if size > MAX_YAML_BYTES:
        raise YamlLoadError(f"{source}: YAML size {size} exceeds {MAX_YAML_BYTES} bytes")
    try:
        return yaml.load(text, Loader=_BoundedSafeLoader)
    except (yaml.YAMLError, YamlLoadError) as error:
        raise YamlLoadError(f"{source}: invalid YAML: {error}") from error


def load_yaml_file(path):
    path = Path(path)
    try:
        if path.is_symlink() or not path.is_file():
            raise YamlLoadError(f"{path}: governance YAML must be a regular non-symlink file")
        size = path.stat().st_size
        if size > MAX_YAML_BYTES:
            raise YamlLoadError(
                f"{path}: YAML size {size} exceeds {MAX_YAML_BYTES} bytes"
            )
        return load_yaml_text(path.read_text(encoding="utf-8"), str(path))
    except OSError as error:
        raise YamlLoadError(f"{path}: {error}") from error
