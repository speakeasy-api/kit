use std::sync::OnceLock;

use agentkit_core::MetadataMap;
use agentkit_tools_core::{ToolAnnotations, ToolName, ToolOutputLimit, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    capabilities::kernel::{
        grant::EffectClass,
        identity::{
            CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
            CapabilityVersion, Digest, DigestAlgorithm, SourceSchema,
        },
        invoke::{ApprovalState, MAX_INVOCATION_ARGUMENT_BYTES, RetrySafety},
    },
    capabilities::schema::NormalizedSchema,
    domain::config::{Grant, RunConfigSnapshot},
    runtime::scheduler::limits::Spend,
};

pub const JSON_SCHEMA_DIALECT: &str = crate::capabilities::schema::JSON_SCHEMA_2020_12;
pub const MAX_NATIVE_INPUT_BYTES: usize = MAX_INVOCATION_ARGUMENT_BYTES;
pub const MAX_NATIVE_OUTPUT_BYTES: usize = 64 * 1024;
const _: () = assert!(
    crate::domain::secret::JsonProjectionState::MAX_SERIALIZED_BYTES * 2 + 4096
        <= MAX_NATIVE_INPUT_BYTES
);
const VERSION: &str = "1.0.0";
pub(crate) const NATIVE_MAP_MAX_ITEMS: usize = 200;
pub(crate) const NATIVE_MAP_MAX_ESTIMATED_TOKENS: usize = 16_384;
pub(crate) const NATIVE_MAP_MAX_HOPS: usize = 4;
pub(crate) const NATIVE_MAP_MAX_DEGREE: usize = 64;
pub(crate) const NATIVE_MAP_MAX_RESULT_BYTES: usize = 60 * 1024;
pub(crate) const NATIVE_MAP_MAX_RELATIONSHIPS: usize = 16;
pub(crate) const NATIVE_MAP_MAX_EXPANSION_SELECTORS: usize = 128;
pub(crate) const NATIVE_MAP_MAX_SEMANTIC_RELATIONSHIPS: usize = 128;
pub(crate) const NATIVE_MAP_MAX_SEMANTIC_EVIDENCE_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeTool {
    Discover,
    Search,
    Read,
    Edit,
    Run,
}

impl NativeTool {
    pub const ALL: [Self; 5] = [
        Self::Discover,
        Self::Search,
        Self::Read,
        Self::Edit,
        Self::Run,
    ];

    pub const fn short_name(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::Search => "search",
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Run => "run",
        }
    }

    pub fn canonical_name(self) -> String {
        format!("kit.{}", self.short_name())
    }

    pub fn provider_alias(self) -> String {
        format!("kit_{}", self.short_name())
    }
}

#[derive(Clone)]
pub struct NativeToolDescriptor {
    tool: NativeTool,
    spec: ToolSpec,
    schema: NormalizedSchema,
    identity: CapabilityIdentity,
    effect: EffectClass,
    required_grants: &'static [Grant],
    reservation: Spend,
    retry_safety: RetrySafety,
    approval: ApprovalState,
}

impl NativeToolDescriptor {
    pub const fn tool(&self) -> NativeTool {
        self.tool
    }

    pub const fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    pub const fn schema(&self) -> &SourceSchema {
        self.schema.source()
    }

    pub const fn normalized_schema(&self) -> &NormalizedSchema {
        &self.schema
    }

    pub const fn identity(&self) -> &CapabilityIdentity {
        &self.identity
    }

    pub const fn effect(&self) -> EffectClass {
        self.effect
    }

    pub const fn required_grants(&self) -> &'static [Grant] {
        self.required_grants
    }

    pub const fn reservation(&self) -> Spend {
        self.reservation
    }

    pub const fn retry_safety(&self) -> RetrySafety {
        self.retry_safety
    }

    pub const fn approval(&self) -> ApprovalState {
        self.approval
    }

    pub fn canonical_name(&self) -> String {
        self.tool.canonical_name()
    }
}

pub struct NativeCatalog;

impl NativeCatalog {
    pub fn all() -> &'static [NativeToolDescriptor; 5] {
        static CATALOG: OnceLock<[NativeToolDescriptor; 5]> = OnceLock::new();
        CATALOG.get_or_init(|| NativeTool::ALL.map(descriptor))
    }

    pub fn enabled(config: &RunConfigSnapshot) -> Vec<NativeToolDescriptor> {
        Self::all()
            .iter()
            .filter(|descriptor| {
                descriptor
                    .required_grants
                    .iter()
                    .all(|grant| config.effective_authority().contains(grant))
            })
            .cloned()
            .collect()
    }

    pub fn by_tool_name(name: &str) -> Option<&'static NativeToolDescriptor> {
        Self::all()
            .iter()
            .find(|descriptor| descriptor.spec.name.0 == name)
    }

    pub fn by_canonical_name(name: &str) -> Option<&'static NativeToolDescriptor> {
        Self::all()
            .iter()
            .find(|descriptor| descriptor.canonical_name() == name)
    }

    pub(crate) fn by_identity(
        identity: &CapabilityIdentity,
    ) -> Option<&'static NativeToolDescriptor> {
        Self::all()
            .iter()
            .find(|descriptor| descriptor.identity() == identity)
    }
}

fn descriptor(tool: NativeTool) -> NativeToolDescriptor {
    let schema_value = input_schema(tool);
    let source = serde_json::to_vec(&schema_value).expect("native schemas serialize");
    let schema = NormalizedSchema::ingest(
        &source,
        JSON_SCHEMA_DIALECT,
        description(tool).as_bytes(),
        DigestAlgorithm::Sha256,
    )
    .expect("native schemas are valid JSON Schema");
    let (effect, grants, annotations, retry_safety, reservation, approval) = match tool {
        NativeTool::Discover | NativeTool::Search | NativeTool::Read => (
            EffectClass::WorkspaceRead,
            &[Grant::WorkspaceRead][..],
            ToolAnnotations::read_only().with_idempotent(true),
            RetrySafety::Idempotent,
            Spend::new(0, 0, 0, 1, 0),
            ApprovalState::NotRequired,
        ),
        NativeTool::Edit => (
            EffectClass::WorkspaceWrite,
            &[Grant::WorkspaceWrite][..],
            ToolAnnotations::destructive().with_needs_approval(true),
            RetrySafety::NonIdempotent,
            Spend::new(0, 0, 0, 1, 0),
            ApprovalState::Pending,
        ),
        NativeTool::Run => (
            EffectClass::ProcessSpawn,
            &[Grant::ProcessSpawn][..],
            ToolAnnotations::destructive()
                .with_needs_approval(true)
                .with_supports_streaming(true),
            RetrySafety::NonIdempotent,
            Spend::new(0, 0, 0, 1, 1),
            ApprovalState::Pending,
        ),
    };
    let mut metadata = MetadataMap::new();
    metadata.insert("kit.native.version".to_owned(), json!(VERSION));
    metadata.insert("kit.schema.dialect".to_owned(), json!(JSON_SCHEMA_DIALECT));
    metadata.insert(
        "kit.schema.digest".to_owned(),
        json!(schema.source().normalized_digest().to_string()),
    );
    metadata.insert(
        "kit.output.max_bytes".to_owned(),
        json!(MAX_NATIVE_OUTPUT_BYTES),
    );
    let spec = ToolSpec::new(
        ToolName::new(tool.provider_alias()),
        description(tool),
        schema_value,
    )
    .with_output_schema(output_schema(tool))
    .with_annotations(annotations)
    .with_metadata(metadata)
    .with_output_limit(ToolOutputLimit::fail(MAX_NATIVE_OUTPUT_BYTES));
    let implementation = if tool == NativeTool::Discover {
        format!("kit-native-discover-map-graph-history-blame-{VERSION}")
    } else {
        format!("kit-native-{}-{VERSION}", tool.short_name())
    };
    NativeToolDescriptor {
        tool,
        spec,
        schema,
        identity: CapabilityIdentity::new(
            CapabilitySource::new("native").expect("static source"),
            CapabilityNamespace::new("kit.native").expect("static namespace"),
            CapabilityName::new(tool.short_name()).expect("static name"),
            CapabilityVersion::new(VERSION).expect("static version"),
            Digest::of(DigestAlgorithm::Blake3, implementation.as_bytes()),
        ),
        effect,
        required_grants: grants,
        reservation,
        retry_safety,
        approval,
    }
}

fn description(tool: NativeTool) -> &'static str {
    match tool {
        NativeTool::Discover => {
            "Select for ranked repository tree, symbol, relationship, or personalized declaration-map discovery at an expected revision; do not select for exact text lookup. Saves no workspace changes and returns at most 64 KiB with mode-specific revision-bound cursors. Example: {\"expected_revision\":\"r:<64 hex>\",\"map\":{\"taskTerms\":[\"Config\"]}}."
        }
        NativeTool::Search => {
            "Select for one or more exact lexical or Rust structural lookups and read-only rewrite previews at an expected revision; do not select to apply changes. Batched calls accept 2 to 8 independent search-only queries at one revision. Saves no workspace changes and returns at most 64 KiB; changed structural rewrites include an opaque single-use apply token and change diff. Example: {\"expected_revision\":\"r:<64 hex>\",\"text\":\"Some($A)\",\"mode\":\"structural\",\"rewrite\":\"Ok($A)\",\"path_prefixes\":[],\"languages\":[\"rust\"]}."
        }
        NativeTool::Read => {
            "Select for one bounded file or line/byte range at an expected revision; do not select for repository-wide search. Saves large/binary content as an authorized artifact and returns at most 64 KiB. Example: {\"expected_revision\":\"r:<64 hex>\",\"path\":\"src/lib.rs\",\"range\":{\"kind\":\"lines\",\"start\":1,\"end\":80}}."
        }
        NativeTool::Edit => {
            "Select for a transactional multi-file patch anchored by exact line hunks against current file content; do not select for process execution. Each edit operation is {op:\"edit\",path,hunks:[{context_before,old,new,context_after}]} over exact UTF-8 lines (file CRLF is normalized to LF for matching); context_before+old+context_after must occur exactly once: zero matches fail edit_anchor_not_found (re-read the file), several fail edit_anchor_ambiguous (add context lines). Empty old inserts new between the contexts; empty new deletes old. add_file creates a file from a content string; delete_file removes one. Verifies then atomically commits or aborts. Saves diff and verification artifacts and returns at most 64 KiB. Example: {\"version\":2,\"operations\":[{\"op\":\"edit\",\"path\":\"src/lib.rs\",\"hunks\":[{\"context_before\":[\"fn main() {\"],\"old\":[\"    old();\"],\"new\":[\"    new();\"],\"context_after\":[\"}\"]}]}]}."
        }
        NativeTool::Run => {
            "Select for an explicit argv process in the configured M003 executor profile; never select for trusted project checks or shell strings. Saves bounded sanitized stream and process-evidence artifacts and returns at most 64 KiB. Example: {\"argv\":[\"cargo\",\"metadata\"],\"working_directory\":\".\",\"mounts\":{\"source\":\"read_only\",\"build\":\"read_write\",\"temp\":\"read_write\"},\"environment\":{},\"network\":\"deny\",\"host_compatibility\":false,\"background\":\"foreground\",\"limits\":{\"cpu_millis\":1000,\"memory_bytes\":268435456,\"pids\":64,\"file_bytes\":16777216,\"disk_bytes\":268435456,\"io_bytes\":67108864,\"output_bytes\":65536,\"wall_time_millis\":10000}}."
        }
    }
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "$schema": JSON_SCHEMA_DIALECT,
        "additionalProperties": false,
        "properties": properties,
        "required": required,
        "type": "object"
    })
}

fn revision() -> Value {
    json!({"pattern": "^r:[0-9a-f]{64}$", "type": "string"})
}

fn relative_path() -> Value {
    json!({"maxLength": 4096, "minLength": 1, "type": "string"})
}

fn expansion_path() -> Value {
    json!({
        "allOf": [
            {"not": {"pattern": "^/"}},
            {"not": {"pattern": "^[A-Za-z]:"}},
            {"not": {"pattern": r"\\"}},
            // Unicode C0/DEL/C1 controls spelled as ranges: \p{Cc} is not portable to
            // ECMA-262 pattern validators (e.g. the OpenAI tool-schema checker).
            {"not": {"pattern": r"[\x00-\x1F\x7F-\x9F]"}},
            {"not": {"pattern": r#"[?*"<>|:]"#}},
            {"not": {"pattern": "(?:^|/)(?:/|$)"}},
            {"not": {"pattern": r"(?:^|/)\.{1,2}(?:/|$)"}},
            {"not": {"pattern": r"[. ](?:/|$)"}},
            {"not": {"pattern": r"(?:^|/)(?:[cC][oO][nN](?:[iI][nN]\$|[oO][uU][tT]\$)?|[pP][rR][nN]|[aA][uU][xX]|[nN][uU][lL]|[cC][oO][mM][1-9¹²³]|[lL][pP][tT][1-9¹²³])(?:\.|/|$)"}}
        ],
        "description": "Lexically safe canonical root-relative path. maxLength is a conservative character prefilter; runtime validation authoritatively enforces the 4096-byte UTF-8 limit and portable path rules.",
        "maxLength": 4096,
        "minLength": 1,
        "type": "string"
    })
}

fn input_schema(tool: NativeTool) -> Value {
    match tool {
        NativeTool::Discover => {
            let mut legacy = object(
                json!({
                    "cursor": {"type": ["object", "null"]},
                    "expected_revision": revision(),
                    "languages": {"items": {"type": "string"}, "maxItems": 32, "type": "array"},
                    "roots": {"items": relative_path(), "maxItems": 32, "type": "array"},
                    "terms": {"items": {"maxLength": 256, "minLength": 1, "type": "string"}, "maxItems": 32, "type": "array"}
                }),
                &["expected_revision", "terms", "roots", "languages"],
            );
            legacy
                .as_object_mut()
                .expect("object schema")
                .remove("$schema");
            let mut map = object(
                json!({
                    "expected_revision": revision(),
                    "map": {
                        "additionalProperties": false,
                        "allOf": [{
                            "if": {
                                "properties": {"relationships": {"contains": {"const": "changed_with"}}},
                                "required": ["relationships"]
                            },
                            "then": {
                                "properties": {"purpose": {"const": "neighborhood"}},
                                "required": ["purpose"]
                            }
                        }],
                        "properties": {
                            "budgets": {
                                "additionalProperties": false,
                                "properties": {
                                    "degree": {"maximum": NATIVE_MAP_MAX_DEGREE, "minimum": 1, "type": "integer"},
                                    "estimatedTokens": {"maximum": NATIVE_MAP_MAX_ESTIMATED_TOKENS, "minimum": 1, "type": "integer"},
                                    "hops": {"maximum": NATIVE_MAP_MAX_HOPS, "minimum": 0, "type": "integer"},
                                    "items": {"maximum": NATIVE_MAP_MAX_ITEMS, "minimum": 1, "type": "integer"},
                                    "resultBytes": {"maximum": NATIVE_MAP_MAX_RESULT_BYTES, "minimum": 1, "type": "integer"}
                                },
                                "type": "object"
                            },
                            "cursor": {"maxLength": crate::workspace::map::MAP_CURSOR_TOKEN_LENGTH, "pattern": "^kitmap1_[0-9a-f]{400}$", "type": ["string", "null"]},
                            "currentEditPaths": {"items": relative_path(), "maxItems": 32, "type": "array"},
                            "exactIdentifiers": {"items": {"pattern": "^[0-9a-f]{64}$", "type": "string"}, "maxItems": 128, "type": "array"},
                            "expandPaths": {"description": "Exact canonical indexed file or directory paths. Every path must match. Repository-tree expansion returns path nodes and direct path edges; an exact file also deterministically seeds its indexed declarations.", "items": expansion_path(), "maxItems": NATIVE_MAP_MAX_EXPANSION_SELECTORS, "type": "array", "uniqueItems": true},
                            "expandSymbols": {"description": "Exact case-sensitive qualified or display symbols. An ambiguous exact display symbol selects all matches deterministically. maxLength is a conservative character prefilter; runtime validation authoritatively enforces the 256-byte UTF-8 limit.", "items": {"maxLength": 256, "minLength": 1, "type": "string"}, "maxItems": NATIVE_MAP_MAX_EXPANSION_SELECTORS, "type": "array", "uniqueItems": true},
                            "expandPackages": {"description": "Exact structure-graph package name or canonical manifest path.", "items": {"maxLength": 4096, "minLength": 1, "type": "string"}, "maxItems": NATIVE_MAP_MAX_EXPANSION_SELECTORS, "type": "array", "uniqueItems": true},
                            "expandTests": {"description": "Exact structure-graph test name or canonical source path.", "items": {"maxLength": 4096, "minLength": 1, "type": "string"}, "maxItems": NATIVE_MAP_MAX_EXPANSION_SELECTORS, "type": "array", "uniqueItems": true},
                            "expansionSeeds": {"items": {"pattern": "^[0-9a-f]{64}$", "type": "string"}, "maxItems": NATIVE_MAP_MAX_EXPANSION_SELECTORS, "type": "array", "uniqueItems": true},
                            "graphSeeds": {"description": "Exact structure-graph node IDs.", "items": {"pattern": "^[0-9a-f]{64}$", "type": "string"}, "maxItems": NATIVE_MAP_MAX_EXPANSION_SELECTORS, "type": "array", "uniqueItems": true},
                            "languages": {"items": {"maxLength": 64, "minLength": 1, "type": "string"}, "maxItems": 32, "type": "array"},
                            "historyPaths": {"description": "Optional exact bounded Git co-change scope. Omit to use all indexed file paths when changed_with is requested.", "items": expansion_path(), "maxItems": NATIVE_MAP_MAX_EXPANSION_SELECTORS, "type": "array", "uniqueItems": true},
                            "blamePaths": {"description": "Optional exact bounded blame scope. Requested paths produce canonical digest-only blame hunks in map.blame; raw line text is never returned. Blame is never extracted when omitted.", "items": expansion_path(), "maxItems": NATIVE_MAP_MAX_EXPANSION_SELECTORS, "type": "array", "uniqueItems": true},
                            "pathPrefixes": {"items": relative_path(), "maxItems": 32, "type": "array"},
                            "purpose": {"enum": ["dependencies", "dependents", "neighborhood"]},
                            "recentlyReadPaths": {"items": relative_path(), "maxItems": 32, "type": "array"},
                            "relationships": {"items": {"enum": ["contains", "contained_by", "semantic_declaration", "semantic_definition", "semantic_type_definition", "semantic_implementation", "semantic_reference", "defines", "imports", "exports", "references", "calls", "implements", "inherits", "overrides", "tests", "changed_with"]}, "maxItems": NATIVE_MAP_MAX_RELATIONSHIPS, "type": "array", "uniqueItems": true},
                            "scoreBand": {
                                "additionalProperties": false,
                                "description": "Inclusive u64 declaration rank under kit-repository-map-v1. Every band must match at least one declaration.",
                                "properties": {
                                    "max": {"maximum": 18446744073709551615_u64, "minimum": 0, "type": "integer"},
                                    "min": {"maximum": 18446744073709551615_u64, "minimum": 0, "type": "integer"}
                                },
                                "required": ["min", "max"],
                                "type": ["object", "null"]
                            },
                            "stackFrames": {
                                "items": {
                                    "additionalProperties": false,
                                    "properties": {
                                        "line": {"maximum": 10000000, "minimum": 1, "type": ["integer", "null"]},
                                        "path": relative_path(),
                                        "symbol": {"maxLength": 256, "minLength": 1, "type": ["string", "null"]}
                                    },
                                    "required": ["path"],
                                    "type": "object"
                                },
                                "maxItems": 32,
                                "type": "array"
                            },
                            "taskTerms": {"items": {"maxLength": 256, "minLength": 1, "type": "string"}, "maxItems": 32, "type": "array"}
                        },
                        "type": "object"
                    }
                }),
                &["expected_revision", "map"],
            );
            map.as_object_mut()
                .expect("object schema")
                .remove("$schema");
            json!({"$schema": JSON_SCHEMA_DIALECT, "oneOf": [legacy, map]})
        }
        NativeTool::Search => {
            let mut lexical = object(
                json!({
                    "cursor": {
                        "additionalProperties": false,
                        "properties": {
                            "custody_revision": {"minimum": 0, "type": "integer"},
                            "digest": {"type": "string"},
                            "epoch": {"type": "string"},
                            "frontier": {"minimum": 0, "type": "integer"},
                            "index_digest": {"pattern": "^[0-9a-f]{64}$", "type": "string"},
                            "options_digest": {"pattern": "^[0-9a-f]{64}$", "type": "string"},
                            "projection_state": {"maxLength": crate::domain::secret::JsonProjectionState::MAX_SERIALIZED_BYTES * 2, "minLength": 1, "pattern": "^[0-9a-f]+$", "type": "string"},
                            "projection_state_tag": {"pattern": "^[0-9a-f]{64}$", "type": "string"},
                            "projection_state_version": {"const": 1},
                            "query_digest": {"pattern": "^[0-9a-f]{64}$", "type": "string"},
                            "revision": revision()
                        },
                        "required": ["epoch", "revision", "digest", "index_digest", "query_digest", "options_digest", "frontier"],
                        "type": ["object", "null"]
                    },
                    "expected_revision": revision(),
                    "languages": {"items": {"type": "string"}, "maxItems": 32, "type": "array"},
                    "mode": {"enum": ["path", "content", "path_and_content"]},
                    "path_prefixes": {"items": relative_path(), "maxItems": 32, "type": "array"},
                    "text": {"maxLength": 4096, "minLength": 1, "type": "string"}
                }),
                &[
                    "expected_revision",
                    "text",
                    "mode",
                    "path_prefixes",
                    "languages",
                ],
            );
            let mut structural = object(
                json!({
                    "expected_revision": revision(),
                    "languages": {"items": {"type": "string"}, "maxItems": 32, "type": "array"},
                    "mode": {"const": "structural"},
                    "path_prefixes": {"items": relative_path(), "maxItems": 32, "type": "array"},
                    "rewrite": {"maxLength": 4096, "minLength": 1, "type": ["string", "null"]},
                    "text": {"maxLength": 4096, "minLength": 1, "type": "string"}
                }),
                &[
                    "expected_revision",
                    "text",
                    "mode",
                    "path_prefixes",
                    "languages",
                ],
            );
            let mut batched_lexical = object(
                json!({
                    "languages": {"items": {"type": "string"}, "maxItems": 32, "type": "array"},
                    "mode": {"enum": ["path", "content", "path_and_content"]},
                    "path_prefixes": {"items": relative_path(), "maxItems": 32, "type": "array"},
                    "text": {"maxLength": 4096, "minLength": 1, "type": "string"}
                }),
                &["text", "mode", "path_prefixes", "languages"],
            );
            let mut batched_structural = object(
                json!({
                    "languages": {"items": {"type": "string"}, "maxItems": 32, "type": "array"},
                    "mode": {"const": "structural"},
                    "path_prefixes": {"items": relative_path(), "maxItems": 32, "type": "array"},
                    "text": {"maxLength": 4096, "minLength": 1, "type": "string"}
                }),
                &["text", "mode", "path_prefixes", "languages"],
            );
            batched_lexical.as_object_mut().unwrap().remove("$schema");
            batched_structural.as_object_mut().unwrap().remove("$schema");
            let mut batched = object(
                json!({
                    "expected_revision": revision(),
                    "queries": {
                        "items": {"oneOf": [batched_lexical, batched_structural]},
                        "minItems": 2,
                        "maxItems": 8,
                        "type": "array"
                    }
                }),
                &["expected_revision", "queries"],
            );
            batched.as_object_mut().unwrap().remove("$schema");
            lexical.as_object_mut().unwrap().remove("$schema");
            structural.as_object_mut().unwrap().remove("$schema");
            json!({
                "$schema": JSON_SCHEMA_DIALECT,
                "oneOf": [lexical, structural, batched]
            })
        }
        NativeTool::Read => object(
            json!({
                "cursor": {
                    "additionalProperties": false,
                    "properties": {
                        "custody_revision": {"minimum": 0, "type": "integer"},
                        "projection_state": {"maxLength": crate::domain::secret::JsonProjectionState::MAX_SERIALIZED_BYTES * 2, "minLength": 1, "pattern": "^[0-9a-f]+$", "type": "string"},
                        "tag": {"pattern": "^[0-9a-f]{64}$", "type": "string"},
                        "version": {"const": 1}
                    },
                    "required": ["version", "projection_state", "custody_revision", "tag"],
                    "type": ["object", "null"]
                },
                "expected_revision": revision(),
                "path": relative_path(),
                "range": {
                    "oneOf": [
                        {"additionalProperties": false, "properties": {"kind": {"const": "full"}}, "required": ["kind"], "type": "object"},
                        {"additionalProperties": false, "properties": {"end": {"minimum": 1, "type": "integer"}, "kind": {"const": "bytes"}, "start": {"minimum": 0, "type": "integer"}}, "required": ["kind", "start", "end"], "type": "object"},
                        {"additionalProperties": false, "properties": {"end": {"minimum": 1, "type": "integer"}, "kind": {"const": "lines"}, "start": {"minimum": 1, "type": "integer"}}, "required": ["kind", "start", "end"], "type": "object"}
                    ]
                }
            }),
            &["expected_revision", "path", "range"],
        ),
        NativeTool::Edit => {
            let mut schema = crate::workspace::edit::normalize::native_edit_schema();
            schema["$schema"] = json!(JSON_SCHEMA_DIALECT);
            schema
        }
        NativeTool::Run => object(
            json!({
                "argv": {"items": {"maxLength": 16384, "minLength": 1, "type": "string"}, "maxItems": 256, "minItems": 1, "type": "array"},
                "background": {"const": "foreground"},
                "environment": {"additionalProperties": {"maxLength": 16384, "type": "string"}, "maxProperties": 128, "type": "object"},
                "host_compatibility": {"type": "boolean"},
                "limits": {"additionalProperties": false, "properties": {
                    "cpu_millis": {"minimum": 1, "type": "integer"}, "disk_bytes": {"minimum": 1, "type": "integer"}, "file_bytes": {"minimum": 1, "type": "integer"}, "io_bytes": {"minimum": 1, "type": "integer"}, "memory_bytes": {"minimum": 1, "type": "integer"}, "output_bytes": {"maximum": 1048576, "minimum": 1, "type": "integer"}, "pids": {"minimum": 1, "type": "integer"}, "wall_time_millis": {"minimum": 1, "type": "integer"}
                }, "required": ["cpu_millis", "memory_bytes", "pids", "file_bytes", "disk_bytes", "io_bytes", "output_bytes", "wall_time_millis"], "type": "object"},
                "network": {"enum": ["deny", "profile_grants"]},
                "mounts": {"const": {"build": "read_write", "source": "read_only", "temp": "read_write"}},
                "working_directory": {"const": "."}
            }),
            &[
                "argv",
                "working_directory",
                "mounts",
                "environment",
                "network",
                "limits",
                "host_compatibility",
                "background",
            ],
        ),
    }
}

fn output_schema(tool: NativeTool) -> Value {
    let generic = || {
        object(
            json!({
                "artifacts": {"items": {"type": "string"}, "type": "array"},
                "data": {},
                "truncated": {"type": "boolean"},
                "version": {"const": 1}
            }),
            &["version", "data", "artifacts", "truncated"],
        )
    };
    if tool != NativeTool::Discover {
        return generic();
    }

    let openapi: Value = serde_json::to_value(
        serde_yaml::from_str::<serde_yaml::Value>(include_str!("../../../docs/api/openapi.yaml"))
            .expect("OpenAPI schema parses"),
    )
    .expect("OpenAPI schema converts to JSON");
    let mut components = serde_json::Map::new();
    for name in [
        "RepositoryDiscoverMapOutput",
        "RepositoryMapResponse",
        "RepositoryRelativePath",
        "RepositoryRevisionId",
        "RepositoryBlameHunk",
        "RepositoryGraphRange",
        "RepositoryGraphEdgeProvenance",
        "RepositoryGraphSemanticProvenance",
        "RepositoryGraphHistoryProvenance",
    ] {
        components.insert(
            name.to_owned(),
            openapi["components"]["schemas"][name].clone(),
        );
    }
    let wrapper = |data: Value| {
        json!({
            "additionalProperties": false,
            "properties": {
                "artifacts": {"items": {"type": "string"}, "type": "array"},
                "data": data,
                "truncated": {"const": false},
                "version": {"const": 1}
            },
            "required": ["version", "data", "artifacts", "truncated"],
            "type": "object"
        })
    };
    json!({
        "$schema": JSON_SCHEMA_DIALECT,
        "components": {"schemas": components},
        "oneOf": [
            wrapper(json!({
                "not": {
                    "properties": {"mode": {"const": "map"}},
                    "required": ["mode"],
                    "type": "object"
                }
            })),
            wrapper(json!({"$ref": "#/components/schemas/RepositoryDiscoverMapOutput"}))
        ]
    })
}
