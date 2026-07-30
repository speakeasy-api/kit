use std::sync::OnceLock;

use agentkit_core::MetadataMap;
use agentkit_tools_core::{ToolAnnotations, ToolName, ToolOutputLimit, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::auth::contract::GrantSnapshot,
    capabilities::kernel::{
        grant::EffectClass,
        identity::{
            CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
            CapabilityVersion, Digest, DigestAlgorithm, SourceSchema,
        },
        invoke::{ApprovalState, RetrySafety},
    },
    domain::config::{Grant, RunConfigSnapshot},
    runtime::scheduler::limits::Spend,
};

pub const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
pub const MAX_NATIVE_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_NATIVE_OUTPUT_BYTES: usize = 64 * 1024;
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
    Check,
}

impl NativeTool {
    pub const ALL: [Self; 6] = [
        Self::Discover,
        Self::Search,
        Self::Read,
        Self::Edit,
        Self::Run,
        Self::Check,
    ];

    pub const fn short_name(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::Search => "search",
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Run => "run",
            Self::Check => "check",
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
    schema: SourceSchema,
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

    pub fn estimate_reservation(
        &self,
        input: &Value,
        registry: &crate::verify::profiles::VerificationRegistry,
        grants: &GrantSnapshot,
        config: &RunConfigSnapshot,
    ) -> Result<Spend, String> {
        if self.tool != NativeTool::Check {
            return Ok(self.reservation);
        }
        let profile = input
            .get("profile")
            .and_then(Value::as_str)
            .ok_or_else(|| "check profile is missing".to_owned())?;
        let selection = match profile {
            "syntax" => crate::verify::profiles::ProfileSelection::Syntax,
            "fast" => crate::verify::profiles::ProfileSelection::Fast,
            "full" => crate::verify::profiles::ProfileSelection::Full,
            "targeted" => crate::verify::profiles::ProfileSelection::Targeted {
                exact_targets: input
                    .get("targets")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            },
            _ => return Err("check profile is invalid".to_owned()),
        };
        let count = registry
            .select_native(&selection, grants, config)
            .map_err(|error| error.to_string())?
            .len();
        if count == 0 {
            return Err("check profile has no trusted commands".to_owned());
        }
        Ok(Spend::new(
            0,
            0,
            0,
            1,
            u64::try_from(count).map_err(|_| "check process count overflowed")?,
        ))
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
    pub fn all() -> &'static [NativeToolDescriptor; 6] {
        static CATALOG: OnceLock<[NativeToolDescriptor; 6]> = OnceLock::new();
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
}

fn descriptor(tool: NativeTool) -> NativeToolDescriptor {
    let schema_value = input_schema(tool);
    let source = serde_json::to_vec(&schema_value).expect("native schemas serialize");
    let schema = SourceSchema::new(
        source.clone(),
        JSON_SCHEMA_DIALECT,
        description(tool).as_bytes(),
        source,
        DigestAlgorithm::Sha256,
    )
    .expect("native schemas are non-empty");
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
        NativeTool::Check => (
            EffectClass::ProcessSpawn,
            &[Grant::ProcessSpawn, Grant::VerificationTargeted][..],
            ToolAnnotations::read_only().with_idempotent(true),
            RetrySafety::Idempotent,
            Spend::new(0, 0, 0, 1, 1),
            ApprovalState::NotRequired,
        ),
    };
    let mut metadata = MetadataMap::new();
    metadata.insert("kit.native.version".to_owned(), json!(VERSION));
    metadata.insert("kit.schema.dialect".to_owned(), json!(JSON_SCHEMA_DIALECT));
    metadata.insert(
        "kit.schema.digest".to_owned(),
        json!(schema.normalized_digest().to_string()),
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
    .with_output_schema(output_schema())
    .with_annotations(annotations)
    .with_metadata(metadata)
    .with_output_limit(ToolOutputLimit::fail(MAX_NATIVE_OUTPUT_BYTES));
    let implementation = if tool == NativeTool::Discover {
        format!("kit-native-discover-map-graph-{VERSION}")
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
            "Select for exact lexical or Rust structural lookup and read-only rewrite previews at an expected revision; do not select to apply changes. Saves no workspace changes and returns at most 64 KiB; changed structural rewrites include an opaque single-use apply token and change diff. Example: {\"expected_revision\":\"r:<64 hex>\",\"text\":\"Some($A)\",\"mode\":\"structural\",\"rewrite\":\"Ok($A)\",\"path_prefixes\":[],\"languages\":[\"rust\"]}."
        }
        NativeTool::Read => {
            "Select for one bounded file or line/byte range at an expected revision; do not select for repository-wide search. Saves large/binary content as an authorized artifact and returns at most 64 KiB. Example: {\"expected_revision\":\"r:<64 hex>\",\"path\":\"src/lib.rs\",\"range\":{\"kind\":\"lines\",\"start\":1,\"end\":80}}."
        }
        NativeTool::Edit => {
            "Select for a transactional multi-file structured patch against an expected revision; do not select for process execution. Verifies then atomically commits or aborts. Saves diff and verification artifacts and returns at most 64 KiB. Example: {\"version\":1,\"expected_revision\":\"r:<64 hex>\",\"operations\":[]}."
        }
        NativeTool::Run => {
            "Select for an explicit argv process in the configured M003 executor profile; never select for trusted project checks or shell strings. Saves bounded sanitized stream and process-evidence artifacts and returns at most 64 KiB. Example: {\"argv\":[\"cargo\",\"metadata\"],\"working_directory\":\".\",\"mounts\":{\"source\":\"read_only\",\"build\":\"read_write\",\"temp\":\"read_write\"},\"environment\":{},\"network\":\"deny\",\"host_compatibility\":false,\"background\":\"foreground\",\"limits\":{\"cpu_millis\":1000,\"memory_bytes\":268435456,\"pids\":64,\"file_bytes\":16777216,\"disk_bytes\":268435456,\"io_bytes\":67108864,\"output_bytes\":65536,\"wall_time_millis\":10000}}."
        }
        NativeTool::Check => {
            "Select for trusted diagnostics, build, test, lint, affected, or full verification profiles; never select for an arbitrary command. Runs only sealed registry entries, saves verification/process artifacts, and returns at most 64 KiB. Example: {\"profile\":\"fast\",\"targets\":[]}."
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
            {"not": {"pattern": r"\p{Cc}"}},
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
                            "pathPrefixes": {"items": relative_path(), "maxItems": 32, "type": "array"},
                            "purpose": {"enum": ["dependencies", "dependents", "neighborhood"]},
                            "recentlyReadPaths": {"items": relative_path(), "maxItems": 32, "type": "array"},
                            "relationships": {"items": {"enum": ["contains", "contained_by", "semantic_declaration", "semantic_definition", "semantic_type_definition", "semantic_implementation", "semantic_reference", "defines", "imports", "exports", "references", "calls", "implements", "inherits", "overrides", "tests"]}, "maxItems": NATIVE_MAP_MAX_RELATIONSHIPS, "type": "array", "uniqueItems": true},
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
                    "cursor": {"type": ["object", "null"]},
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
            lexical.as_object_mut().unwrap().remove("$schema");
            structural.as_object_mut().unwrap().remove("$schema");
            json!({
                "$schema": JSON_SCHEMA_DIALECT,
                "oneOf": [lexical, structural]
            })
        }
        NativeTool::Read => object(
            json!({
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
        NativeTool::Check => object(
            json!({
                "profile": {"enum": ["syntax", "fast", "targeted", "full"]},
                "targets": {"items": {"maxLength": 128, "minLength": 1, "type": "string"}, "maxItems": 64, "type": "array"}
            }),
            &["profile", "targets"],
        ),
    }
}

fn output_schema() -> Value {
    object(
        json!({
            "artifacts": {"items": {"type": "string"}, "type": "array"},
            "data": {},
            "truncated": {"type": "boolean"},
            "version": {"const": 1}
        }),
        &["version", "data", "artifacts", "truncated"],
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{
        api::auth::contract::GrantSnapshot,
        domain::{
            config::{LayerStack, Provider, RunConfigContext},
            ids::{PrincipalId, ProjectId, RunId},
        },
        executor::{check::CheckCommand, profile::ResourceLimits},
        verify::profiles::{CheckClass, CheckRequirement, DeclaredCheck, VerificationRegistry},
    };

    #[test]
    fn shared_estimator_charges_one_process_per_selected_check_and_one_for_run() {
        let principal = PrincipalId::from_stable_bytes(b"native-estimator-principal");
        let project = ProjectId::from_stable_bytes(b"native-estimator-project");
        let grants = BTreeSet::from([
            Grant::WorkspaceRead,
            Grant::ProcessSpawn,
            Grant::VerificationTargeted,
        ]);
        let config = LayerStack::safe_defaults_for(Provider::OpenAi)
            .materialize(
                RunConfigContext {
                    principal_id: principal,
                    project_id: project,
                    run_id: RunId::from_stable_bytes(b"native-estimator-run"),
                },
                &grants,
            )
            .unwrap();
        let grants = GrantSnapshot::new(principal, project, grants);
        let limits = ResourceLimits::new(1_000, 1024, 8, 1024, 1024, 1024, 1024, 1_000);
        let checks = [CheckClass::Diagnostics, CheckClass::Typecheck]
            .into_iter()
            .enumerate()
            .map(|(index, class)| {
                DeclaredCheck::new(
                    class,
                    CheckCommand::new(
                        format!("check-{index}"),
                        "cargo",
                        vec!["check".to_owned()],
                        format!("example.invalid/check@sha256:{}", "a".repeat(64)),
                        format!("sha256:{}", "b".repeat(64)),
                        format!("sha256:{}", "c".repeat(64)),
                        limits,
                    )
                    .unwrap(),
                    CheckRequirement::Required,
                    BTreeSet::new(),
                    false,
                )
                .unwrap()
            })
            .collect();
        let registry = VerificationRegistry::new(checks).unwrap();
        let check = NativeCatalog::all()
            .iter()
            .find(|descriptor| descriptor.tool() == NativeTool::Check)
            .unwrap();
        assert_eq!(
            check
                .estimate_reservation(
                    &json!({"profile":"fast","targets":[]}),
                    &registry,
                    &grants,
                    &config,
                )
                .unwrap()
                .processes(),
            2
        );
        let run = NativeCatalog::all()
            .iter()
            .find(|descriptor| descriptor.tool() == NativeTool::Run)
            .unwrap();
        assert_eq!(
            run.estimate_reservation(&json!({}), &registry, &grants, &config)
                .unwrap()
                .processes(),
            1
        );
    }
}
