#!/usr/bin/env python3
"""Regression tests for the ACP Swift generator's schema contract."""
import copy
import hashlib
import importlib.util
import json
import pathlib
import unittest
from unittest.mock import patch

GENERATOR_PATH = pathlib.Path(__file__).resolve().parents[1] / "generate-acp-swift.py"
SPEC = importlib.util.spec_from_file_location("generate_acp_swift", GENERATOR_PATH)
GENERATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GENERATOR)


class SchemaContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.schema = json.loads(GENERATOR.SCHEMA.read_bytes())

    def assert_rejected(self, mutate, message):
        schema = copy.deepcopy(self.schema)
        mutate(schema["$defs"])
        with self.assertRaisesRegex(ValueError, message):
            GENERATOR.validate_schema(schema)

    def test_pinned_schema_matches_emitted_subset(self):
        GENERATOR.validate_schema(self.schema)

    def test_header_tracks_schema_pin_and_bytes(self):
        revision = "0123456789abcdef" * 2 + "01234567"
        pin = f"[`agentkit-acp-schema` 1.0.0 source commit](https://github.com/danielkov/agent-client-protocol/tree/{revision})."
        with patch.object(GENERATOR, "SCHEMA_PIN") as source:
            source.read_text.return_value = pin
            header = GENERATOR.render().splitlines()[:3]
            source.read_text.return_value = pin.replace(revision, revision[:7])
            with self.assertRaisesRegex(SystemExit, "expected one full ACP schema source revision"):
                GENERATOR.render()
        self.assertEqual(header[1], f"// Source: ACP v2 unstable schema, agent-client-protocol rev {revision}")
        digest = hashlib.sha256(GENERATOR.SCHEMA.read_bytes()).hexdigest()
        self.assertEqual(header[2], f"// Schema SHA-256: {digest}")

    def test_emitted_field_change_is_rejected(self):
        self.assert_rejected(
            lambda defs: defs["PromptRequest"]["properties"]["prompt"].update(
                {"type": "string"}
            ),
            r"PromptRequest\.prompt",
        )

    def test_referenced_scalar_change_is_rejected(self):
        self.assert_rejected(
            lambda defs: defs["ProtocolVersion"].update({"type": "string"}),
            r"ProtocolVersion referenced kind: expected integer, got string",
        )

    def test_replay_from_start_payload_change_is_rejected(self):
        def add_required_cursor(defs):
            replay_start = defs["ReplayFromStart"]
            replay_start["properties"]["cursor"] = {"type": "string"}
            replay_start["required"] = ["cursor"]

        self.assert_rejected(add_required_cursor, r"ReplayFromStart fields")

    def test_requiredness_change_is_rejected(self):
        self.assert_rejected(
            lambda defs: defs["PromptRequest"]["required"].remove("prompt"),
            r"PromptRequest required",
        )

    def test_discriminator_change_is_rejected(self):
        def change_discriminator(defs):
            variants = defs["SetSessionConfigOptionRequest"]["anyOf"]
            variants[0]["properties"]["type"]["const"] = "select"

        self.assert_rejected(change_discriminator, r"discriminator type='id'")

    def test_method_change_is_rejected(self):
        self.assert_rejected(
            lambda defs: defs["PromptRequest"].update({"x-method": "prompt"}),
            r"PromptRequest x-method",
        )


if __name__ == "__main__":
    unittest.main()
