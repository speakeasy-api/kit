#!/usr/bin/env python3
"""Generate the dependency-free Swift ACP v2 wire subset used by Kit Desktop.

The input is the pinned upstream unstable v2 JSON schema. No network access or
third-party generator is used, so CI output is deterministic.
"""
import argparse, hashlib, json, pathlib, sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCHEMA = ROOT / "macos/ACP/Schema/acp-v2.schema.json"
OUTPUT = ROOT / "macos/KitDesktop/Generated/ACPWireModels.generated.swift"

# This is the schema contract for every property emitted below. The boolean is
# the property's requiredness in ACP, not whether the Swift convenience model
# supplies a default when encoding it.
MODEL_FIELDS = {
    "Implementation": {
        "name": ("string", True), "title": ("string|null", False),
        "version": ("string", True),
    },
    "ClientCapabilities": {
        "auth": ("anyOf($ref:AuthCapabilities,null)", False),
        "elicitation": ("anyOf($ref:ElicitationCapabilities,null)", False),
    },
    "AuthCapabilities": {
        "terminal": ("anyOf($ref:TerminalAuthCapabilities,null)", False),
    },
    "ElicitationCapabilities": {
        "form": ("anyOf($ref:ElicitationFormCapabilities,null)", False),
        "url": ("anyOf($ref:ElicitationUrlCapabilities,null)", False),
    },
    "PromptCapabilities": {
        "image": ("anyOf($ref:PromptImageCapabilities,null)", False),
        "audio": ("anyOf($ref:PromptAudioCapabilities,null)", False),
        "embeddedContext": ("anyOf($ref:PromptEmbeddedContextCapabilities,null)", False),
    },
    "SessionInjectCapabilities": {
        "modes": ("array[$ref:SessionInjectMode]", True),
        "steerInStream": ("array|null", False),
    },
    "SessionCapabilities": {
        "prompt": ("anyOf($ref:PromptCapabilities,null)", False),
        "mcp": ("anyOf($ref:McpCapabilities,null)", False),
        "delete": ("anyOf($ref:SessionDeleteCapabilities,null)", False),
        "additionalDirectories": ("anyOf($ref:SessionAdditionalDirectoriesCapabilities,null)", False),
        "inject": ("anyOf($ref:SessionInjectCapabilities,null)", False),
        "fork": ("anyOf($ref:SessionForkCapabilities,null)", False),
    },
    "AgentCapabilities": {
        "session": ("anyOf($ref:SessionCapabilities,null)", False),
        "auth": ("anyOf($ref:AgentAuthCapabilities,null)", False),
        "providers": ("anyOf($ref:ProvidersCapabilities,null)", False),
        "nes": ("anyOf($ref:NesCapabilities,null)", False),
        "positionEncoding": ("anyOf($ref:PositionEncodingKind,null)", False),
    },
    "InitializeRequest": {
        "protocolVersion": ("allOf($ref:ProtocolVersion)", True),
        "info": ("allOf($ref:Implementation)", True),
        "capabilities": ("allOf($ref:ClientCapabilities)", False),
    },
    "InitializeResponse": {
        "protocolVersion": ("allOf($ref:ProtocolVersion)", True),
        "info": ("allOf($ref:Implementation)", True),
        "capabilities": ("allOf($ref:AgentCapabilities)", False),
    },
    "NewSessionRequest": {
        "cwd": ("allOf($ref:AbsolutePath)", True),
        "additionalDirectories": ("array[$ref:AbsolutePath]", False),
        "mcpServers": ("array[$ref:McpServer]", False),
    },
    "NewSessionResponse": {
        "sessionId": ("allOf($ref:SessionId)", True),
        "configOptions": ("array[$ref:SessionConfigOption]", False),
    },
    "ResumeSessionRequest": {
        "sessionId": ("allOf($ref:SessionId)", True),
        "cwd": ("allOf($ref:AbsolutePath)", True),
        "additionalDirectories": ("array[$ref:AbsolutePath]", False),
        "mcpServers": ("array[$ref:McpServer]", False),
        "replayFrom": ("anyOf($ref:ReplayFrom,null)", False),
    },
    "ResumeSessionResponse": {
        "configOptions": ("array[$ref:SessionConfigOption]", False),
    },
    "ListSessionsRequest": {
        "cwd": ("anyOf($ref:AbsolutePath,null)", False),
        "cursor": ("anyOf($ref:SessionListCursor,null)", False),
    },
    "SessionInfo": {
        "sessionId": ("allOf($ref:SessionId)", True),
        "cwd": ("allOf($ref:AbsolutePath)", True),
        "additionalDirectories": ("array[$ref:AbsolutePath]", False),
        "title": ("string|null", False), "updatedAt": ("string|null", False),
    },
    "ListSessionsResponse": {
        "sessions": ("array[$ref:SessionInfo]", True),
        "nextCursor": ("anyOf($ref:SessionListCursor,null)", False),
    },
    "TextContent": {"text": ("string", True)},
    "PromptRequest": {
        "sessionId": ("allOf($ref:SessionId)", True),
        "prompt": ("array[$ref:ContentBlock]", True),
    },
    "PromptResponse": {},
    "InjectSessionRequest": {
        "sessionId": ("allOf($ref:SessionId)", True),
        "mode": ("allOf($ref:SessionInjectMode)", True),
        "content": ("array[$ref:ContentBlock]", True),
    },
    "InjectSessionResponse": {
        "messageId": ("allOf($ref:MessageId)", True),
    },
    "SetSessionConfigOptionRequest": {
        "sessionId": ("allOf($ref:SessionId)", True),
        "configId": ("allOf($ref:SessionConfigId)", True),
    },
    "SetSessionConfigOptionResponse": {
        "configOptions": ("array[$ref:SessionConfigOption]", True),
    },
    "CloseSessionRequest": {"sessionId": ("allOf($ref:SessionId)", True)},
    "CancelSessionNotification": {"sessionId": ("allOf($ref:SessionId)", True)},
    "CancelRequestNotification": {"requestId": ("allOf($ref:RequestId)", True)},
    "UpdateSessionNotification": {
        "sessionId": ("allOf($ref:SessionId)", True),
        "update": ("allOf($ref:SessionUpdate)", True),
    },
    "RequestPermissionRequest": {
        "sessionId": ("allOf($ref:SessionId)", True),
        "title": ("string", True), "description": ("string|null", False),
        "subject": ("anyOf($ref:RequestPermissionSubject,null)", False),
        "options": ("array[$ref:PermissionOption]", True),
    },
    "RequestPermissionResponse": {
        "outcome": ("allOf($ref:RequestPermissionOutcome)", True),
    },
}

# Source fields intentionally not represented by the dependency-free subset.
# Keeping these explicit makes an upstream field addition fail until reviewed.
OMITTED_FIELDS = {
    "ClientCapabilities": {"nes", "positionEncodings"},
    "InitializeResponse": {"authMethods"},
    "TextContent": {"annotations"},
    "SessionInjectCapabilities": {"pending"},
}

METHODS = {
    "InitializeRequest": "initialize", "InitializeResponse": "initialize",
    "NewSessionRequest": "session/new", "NewSessionResponse": "session/new",
    "ResumeSessionRequest": "session/resume", "ResumeSessionResponse": "session/resume",
    "ListSessionsRequest": "session/list", "ListSessionsResponse": "session/list",
    "PromptRequest": "session/prompt", "PromptResponse": "session/prompt",
    "InjectSessionRequest": "session/inject", "InjectSessionResponse": "session/inject",
    "SetSessionConfigOptionRequest": "session/set_config_option",
    "SetSessionConfigOptionResponse": "session/set_config_option",
    "CloseSessionRequest": "session/close",
    "CancelSessionNotification": "session/cancel",
    "CancelRequestNotification": "$/cancel_request",
    "UpdateSessionNotification": "session/update",
    "RequestPermissionRequest": "session/request_permission",
    "RequestPermissionResponse": "session/request_permission",
}

# (definition, discriminator, literal): (required fields, field signatures, allOf)
DISCRIMINATORS = {
    ("ContentBlock", "type", "text"): (
        {"type"}, {"type": "string:const='text'"}, ("$ref:TextContent",),
    ),
    ("ReplayFrom", "type", "start"): (
        {"type"}, {"type": "string:const='start'"}, ("$ref:ReplayFromStart",),
    ),
    ("SetSessionConfigOptionRequest", "type", "id"): (
        {"type", "value"},
        {"type": "string:const='id'", "value": "allOf($ref:SessionConfigValueId)"}, (),
    ),
    ("SetSessionConfigOptionRequest", "type", "boolean"): (
        {"type", "value"},
        {"type": "string:const='boolean'", "value": "boolean"}, (),
    ),
    ("RequestPermissionOutcome", "outcome", "cancelled"): (
        {"outcome"}, {"outcome": "string:const='cancelled'"}, (),
    ),
    ("RequestPermissionOutcome", "outcome", "selected"): (
        {"outcome"}, {"outcome": "string:const='selected'"},
        ("$ref:SelectedPermissionOutcome",),
    ),
}

# Object payloads composed into tracked discriminator variants need exact
# contracts; validating only their top-level object kind misses new payload data.
DISCRIMINATOR_BASE_FIELDS = {
    "ReplayFromStart": {},
    "SelectedPermissionOutcome": {
        "optionId": ("allOf($ref:PermissionOptionId)", True),
    },
}

OBJECT_FIELDS = {**MODEL_FIELDS, **DISCRIMINATOR_BASE_FIELDS}

EMPTY_OBJECT_DEFS = {
    "TerminalAuthCapabilities", "ElicitationFormCapabilities",
    "ElicitationUrlCapabilities", "PromptImageCapabilities",
    "PromptAudioCapabilities", "PromptEmbeddedContextCapabilities",
    "SessionAdditionalDirectoriesCapabilities",
}

# Resolved JSON kinds for every definition referenced by an emitted field or
# discriminator. This ties aliases such as ProtocolVersion to the Swift scalar
# used below instead of validating only that the $ref spelling is unchanged.
REFERENCE_KINDS = {
    "AbsolutePath": "string",
    "AgentAuthCapabilities": "object",
    "AgentCapabilities": "object",
    "AuthCapabilities": "object",
    "ClientCapabilities": "object",
    "ContentBlock": "object",
    "ElicitationCapabilities": "object",
    "ElicitationFormCapabilities": "object",
    "ElicitationUrlCapabilities": "object",
    "Implementation": "object",
    "McpCapabilities": "object",
    "MessageId": "string",
    "McpServer": "object",
    "NesCapabilities": "object",
    "PermissionOption": "object",
    "PermissionOptionId": "string",
    "PositionEncodingKind": "string",
    "PromptAudioCapabilities": "object",
    "PromptCapabilities": "object",
    "PromptEmbeddedContextCapabilities": "object",
    "PromptImageCapabilities": "object",
    "ProtocolVersion": "integer",
    "ProvidersCapabilities": "object",
    "ReplayFrom": "object",
    "ReplayFromStart": "object",
    "RequestId": "integer|null|string",
    "RequestPermissionOutcome": "object",
    "RequestPermissionSubject": "object",
    "SelectedPermissionOutcome": "object",
    "SessionAdditionalDirectoriesCapabilities": "object",
    "SessionCapabilities": "object",
    "SessionConfigId": "string",
    "SessionConfigOption": "object",
    "SessionConfigValueId": "string",
    "SessionDeleteCapabilities": "object",
    "SessionForkCapabilities": "object",
    "SessionId": "string",
    "SessionInfo": "object",
    "SessionInjectMode": "string",
    "SessionInjectSteerInStream": "string",
    "SessionInjectCapabilities": "object",
    "SessionListCursor": "string",
    "SessionUpdate": "object",
    "TerminalAuthCapabilities": "object",
    "TextContent": "object",
}

BODY = r'''// swiftlint:disable file_length
import Foundation

struct ACPImplementation: Codable, Equatable {
    var name: String
    var title: String?
    var version: String
}

struct ACPClientCapabilities: Codable, Equatable {
    var auth: ACPAuthCapabilities?
    var elicitation: ACPElicitationCapabilities?
}

struct ACPAuthCapabilities: Codable, Equatable {
    var terminal: ACPEmptyObject?
}

struct ACPElicitationCapabilities: Codable, Equatable {
    var form: ACPEmptyObject?
    var url: ACPEmptyObject?
}

struct ACPEmptyObject: Codable, Equatable {}

struct ACPPromptCapabilities: Codable, Equatable {
    var image: ACPEmptyObject?
    var audio: ACPEmptyObject?
    var embeddedContext: ACPEmptyObject?
}

struct ACPSessionInjectCapabilities: Codable, Equatable {
    var modes: [String]
    var steerInStream: [String]?
}

struct ACPSessionCapabilities: Codable, Equatable {
    var prompt: ACPPromptCapabilities?
    var mcp: JSONValue?
    var delete: JSONValue?
    var additionalDirectories: ACPEmptyObject?
    var inject: ACPSessionInjectCapabilities?
    var fork: JSONValue?
}

struct ACPAgentCapabilities: Codable, Equatable {
    var session: ACPSessionCapabilities?
    var auth: JSONValue?
    var providers: JSONValue?
    var nes: JSONValue?
    var positionEncoding: String?
}

struct ACPInitializeRequest: Codable, Equatable {
    var protocolVersion: Int
    var info: ACPImplementation
    var capabilities: ACPClientCapabilities
}

struct ACPInitializeResponse: Codable, Equatable {
    var protocolVersion: Int
    var info: ACPImplementation
    var capabilities: ACPAgentCapabilities?
}

struct ACPNewSessionRequest: Codable, Equatable {
    var cwd: String
    var additionalDirectories: [String] = []
    var mcpServers: [JSONValue] = []
}

struct ACPNewSessionResponse: Codable, Equatable {
    var sessionId: String
    var configOptions: [JSONValue] = []
}

struct ACPReplayFrom: Codable, Equatable {
    var type: String = "start"
}

struct ACPResumeSessionRequest: Codable, Equatable {
    var sessionId: String
    var cwd: String
    var additionalDirectories: [String] = []
    var mcpServers: [JSONValue] = []
    var replayFrom: ACPReplayFrom?
}

struct ACPResumeSessionResponse: Codable, Equatable {
    var configOptions: [JSONValue] = []
}

struct ACPListSessionsRequest: Codable, Equatable {
    var cwd: String?
    var cursor: String?
}

struct ACPSessionWireInfo: Codable, Equatable {
    var sessionId: String
    var cwd: String
    var additionalDirectories: [String] = []
    var title: String?
    var updatedAt: String?
}

struct ACPListSessionsResponse: Codable, Equatable {
    var sessions: [ACPSessionWireInfo]
    var nextCursor: String?
}

struct ACPTextContent: Codable, Equatable {
    var type: String = "text"
    var text: String
}

struct ACPPromptRequest: Codable, Equatable {
    var sessionId: String
    var prompt: [JSONValue]
}

struct ACPPromptResponse: Codable, Equatable {}

struct ACPInjectSessionRequest: Codable, Equatable {
    var sessionId: String
    var mode: String
    var content: [JSONValue]
}

struct ACPInjectSessionResponse: Codable, Equatable {
    var messageId: String
}

enum ACPSessionConfigValue: Codable, Equatable {
    case select(String)
    case boolean(Bool)

    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer()
        if let boolean = try? value.decode(Bool.self) { self = .boolean(boolean) }
        else { self = .select(try value.decode(String.self)) }
    }

    func encode(to encoder: Encoder) throws {
        var value = encoder.singleValueContainer()
        switch self {
        case .select(let selected): try value.encode(selected)
        case .boolean(let selected): try value.encode(selected)
        }
    }
}

struct ACPSetSessionConfigOptionRequest: Encodable, Equatable {
    var sessionId: String
    var configId: String
    var value: ACPSessionConfigValue

    private enum CodingKeys: String, CodingKey { case sessionId, configId, type, value }

    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(sessionId, forKey: .sessionId)
        try values.encode(configId, forKey: .configId)
        switch value {
        case .select(let selected):
            try values.encode("id", forKey: .type)
            try values.encode(selected, forKey: .value)
        case .boolean(let selected):
            try values.encode("boolean", forKey: .type)
            try values.encode(selected, forKey: .value)
        }
    }
}

struct ACPSetSessionConfigOptionResponse: Codable, Equatable {
    var configOptions: [JSONValue]
}

struct ACPCloseSessionRequest: Codable, Equatable { var sessionId: String }
struct ACPCancelSessionNotification: Codable, Equatable { var sessionId: String }
struct ACPCancelRequestNotification: Codable, Equatable { var requestId: JSONValue }

struct ACPUpdateSessionNotification: Codable, Equatable {
    var sessionId: String
    var update: JSONValue
}

struct ACPRequestPermissionRequest: Codable, Equatable {
    var sessionId: String
    var title: String
    var description: String?
    var subject: JSONValue?
    var options: [JSONValue]
}

struct ACPRequestPermissionOutcome: Codable, Equatable {
    var outcome: String = "cancelled"
}

struct ACPRequestPermissionResponse: Codable, Equatable {
    var outcome = ACPRequestPermissionOutcome()
}
'''

def _type_signature(node: dict) -> str:
    if "$ref" in node:
        return "$ref:" + node["$ref"].rsplit("/", 1)[-1]
    if "allOf" in node:
        return "allOf(" + ",".join(_type_signature(item) for item in node["allOf"]) + ")"
    if "anyOf" in node:
        return "anyOf(" + ",".join(_type_signature(item) for item in node["anyOf"]) + ")"
    schema_type = node.get("type", "untyped")
    if isinstance(schema_type, list):
        schema_type = "|".join(schema_type)
    if schema_type == "array":
        return "array[" + _type_signature(node.get("items", {})) + "]"
    if "const" in node:
        return f"{schema_type}:const={node['const']!r}"
    return str(schema_type)


def _collect_refs(node, refs: set) -> None:
    if isinstance(node, dict):
        if "$ref" in node:
            refs.add(node["$ref"].rsplit("/", 1)[-1])
        for value in node.values():
            _collect_refs(value, refs)
    elif isinstance(node, list):
        for value in node:
            _collect_refs(value, refs)


def _resolved_kind(node: dict, definitions: dict, resolving=()) -> str:
    if "$ref" in node:
        name = node["$ref"].rsplit("/", 1)[-1]
        if name in resolving:
            return "recursive"
        definition = definitions.get(name)
        if not isinstance(definition, dict):
            return "missing"
        return _resolved_kind(definition, definitions, resolving + (name,))

    schema_type = node.get("type")
    if isinstance(schema_type, list):
        return "|".join(sorted(schema_type))
    if schema_type == "array":
        item_kind = _resolved_kind(node.get("items", {}), definitions, resolving)
        return f"array[{item_kind}]"
    if isinstance(schema_type, str):
        return schema_type

    for union in ("anyOf", "oneOf"):
        if union in node:
            kinds = {_resolved_kind(item, definitions, resolving) for item in node[union]}
            return "|".join(sorted(kinds))
    if "allOf" in node:
        kinds = {_resolved_kind(item, definitions, resolving) for item in node["allOf"]}
        return "|".join(sorted(kinds))
    return "untyped"


def validate_schema(schema: dict) -> None:
    definitions = schema.get("$defs")
    if not isinstance(definitions, dict):
        raise ValueError("pinned ACP schema has no $defs object")
    errors = []
    referenced = set()

    for name, fields in OBJECT_FIELDS.items():
        definition = definitions.get(name)
        if not isinstance(definition, dict):
            errors.append(f"missing definition {name}")
            continue
        properties = definition.get("properties", {})
        actual_fields = set(properties) - {"_meta"}
        expected_fields = set(fields) | OMITTED_FIELDS.get(name, set())
        if actual_fields != expected_fields:
            errors.append(
                f"{name} fields: expected {sorted(expected_fields)}, got {sorted(actual_fields)}"
            )
        expected_required = {field for field, (_, required) in fields.items() if required}
        actual_required = set(definition.get("required", []))
        if actual_required != expected_required:
            errors.append(
                f"{name} required: expected {sorted(expected_required)}, got {sorted(actual_required)}"
            )
        for field, (expected_type, _) in fields.items():
            if field not in properties:
                continue
            _collect_refs(properties[field], referenced)
            actual_type = _type_signature(properties[field])
            if actual_type != expected_type:
                errors.append(f"{name}.{field}: expected {expected_type}, got {actual_type}")

    for name, method in METHODS.items():
        definition = definitions.get(name, {})
        if definition.get("x-method") != method:
            errors.append(
                f"{name} x-method: expected {method!r}, got {definition.get('x-method')!r}"
            )

    for (name, key, literal), expected in DISCRIMINATORS.items():
        definition = definitions.get(name, {})
        variants = [
            variant for variant in definition.get("anyOf", [])
            if variant.get("properties", {}).get(key, {}).get("const") == literal
        ]
        if len(variants) != 1:
            errors.append(f"{name} discriminator {key}={literal!r}: expected one variant")
            continue
        variant = variants[0]
        _collect_refs(variant, referenced)
        expected_required, expected_properties, expected_all_of = expected
        actual_properties = {
            field: _type_signature(value)
            for field, value in variant.get("properties", {}).items()
            if field != "_meta"
        }
        actual_all_of = tuple(_type_signature(item) for item in variant.get("allOf", []))
        for signature in expected_all_of:
            if not signature.startswith("$ref:"):
                continue
            base = signature.removeprefix("$ref:")
            if REFERENCE_KINDS.get(base) == "object" and base not in OBJECT_FIELDS:
                errors.append(f"{name} discriminator base {base} has no structural contract")
        if set(variant.get("required", [])) != expected_required:
            errors.append(f"{name} discriminator {literal!r} required fields changed")
        if actual_properties != expected_properties:
            errors.append(f"{name} discriminator {literal!r} properties changed")
        if actual_all_of != expected_all_of:
            errors.append(f"{name} discriminator {literal!r} allOf changed")

    uncontracted_refs = referenced - set(REFERENCE_KINDS)
    stale_refs = set(REFERENCE_KINDS) - referenced
    if uncontracted_refs:
        errors.append(f"references missing kind contracts: {sorted(uncontracted_refs)}")
    if stale_refs:
        errors.append(f"unused reference kind contracts: {sorted(stale_refs)}")
    for name in sorted(referenced & set(REFERENCE_KINDS)):
        definition = definitions.get(name)
        if not isinstance(definition, dict):
            errors.append(f"missing referenced definition {name}")
            continue
        actual_kind = _resolved_kind({"$ref": f"#/$defs/{name}"}, definitions)
        expected_kind = REFERENCE_KINDS[name]
        if actual_kind != expected_kind:
            errors.append(
                f"{name} referenced kind: expected {expected_kind}, got {actual_kind}"
            )

    for name in sorted(EMPTY_OBJECT_DEFS):
        definition = definitions.get(name)
        if not isinstance(definition, dict):
            errors.append(f"missing definition {name}")
            continue
        fields = set(definition.get("properties", {})) - {"_meta"}
        if definition.get("type") != "object" or fields or definition.get("required", []):
            errors.append(f"{name} is no longer an empty capability object")

    if errors:
        raise ValueError("pinned ACP schema does not match emitted Swift subset:\n- " + "\n- ".join(errors))


def render() -> str:
    raw = SCHEMA.read_bytes()
    schema = json.loads(raw)
    try:
        validate_schema(schema)
    except ValueError as error:
        raise SystemExit(str(error)) from error
    digest = hashlib.sha256(raw).hexdigest()
    return (
        "// Generated by scripts/generate-acp-swift.py; DO NOT EDIT.\n"
        "// Source: ACP v2 unstable schema, agent-client-protocol rev "
        "6e7e044f9464c4fd652d90699a09e9edc8b3bbad\n"
        f"// Schema SHA-256: {digest}\n\n" + BODY
    )

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    generated = render()
    if args.check:
        if not OUTPUT.exists() or OUTPUT.read_text() != generated:
            print(f"generated ACP Swift models drifted: run {pathlib.Path(__file__).name}", file=sys.stderr)
            return 1
        return 0
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(generated)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
