use crate::CanonicalValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Runtime value schema used to validate inputs and analyze programs.
pub enum Schema {
    /// The `null` value.
    Null,
    /// A Boolean value.
    Boolean,
    /// A signed 64-bit integer with optional inclusive bounds.
    Integer {
        /// Inclusive lower bound.
        min: Option<i64>,
        /// Inclusive upper bound.
        max: Option<i64>,
    },
    /// A finite binary64 number with optional inclusive bounds.
    Number {
        /// Inclusive lower bound.
        min: Option<f64>,
        /// Inclusive upper bound.
        max: Option<f64>,
    },
    /// A Unicode string with optional semantic and size constraints.
    String {
        /// Host-defined semantic format name.
        format: Option<String>,
        /// Allowed values; empty means unrestricted.
        enumeration: Vec<String>,
        /// Minimum number of Unicode scalar values.
        min_len: Option<usize>,
        /// Maximum number of Unicode scalar values.
        max_len: Option<usize>,
    },
    /// An opaque byte sequence.
    Bytes,
    /// A homogeneous ordered list.
    List {
        /// Schema shared by every element.
        items: Box<Schema>,
        /// Minimum element count.
        min_len: Option<usize>,
        /// Maximum element count.
        max_len: Option<usize>,
    },
    /// An object with named properties.
    Object {
        /// Known property definitions.
        properties: BTreeMap<String, Property>,
        /// Properties that must be present.
        required: BTreeSet<String>,
        /// Whether properties absent from `properties` are accepted.
        additional: bool,
    },
    /// An object whose arbitrary keys share one value schema.
    Map {
        /// Schema accepted by every map value.
        values: Box<Schema>,
    },
    /// A value accepted by any of several schemas.
    Union {
        /// Alternative accepted schemas.
        variants: Vec<Schema>,
        /// Optional object property used to distinguish variants.
        discriminator: Option<String>,
    },
    /// Any canonical Runlet value.
    Any,
    /// The type of expressions that never produce a value (`fail(...)`).
    /// Never unifies with anything: a branch that fails contributes nothing
    /// to the surrounding schema. No runtime value has this schema.
    Never,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Schema and host metadata for an object property.
pub struct Property {
    /// Value schema for the property.
    pub schema: Schema,
    #[serde(default)]
    /// Human-readable guidance for tool callers.
    pub documentation: String,
    #[serde(default)]
    /// Whether observability systems should treat the value as sensitive.
    pub sensitive: bool,
    #[serde(default)]
    /// Whether the value is a secret that must not be exposed.
    pub secret: bool,
    #[serde(default)]
    /// Alternative names accepted by a host adapter.
    pub aliases: Vec<String>,
}

impl Property {
    /// Creates a property with no metadata flags or aliases.
    pub fn new(schema: Schema) -> Self {
        Self {
            schema,
            documentation: String::new(),
            sensitive: false,
            secret: false,
            aliases: vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Ordered parameter schema for a tool call.
pub struct CallSchema {
    /// Schemas for positional parameters in call order.
    pub parameters: Vec<Schema>,
    /// How many leading parameters a call must provide; the rest are
    /// optional trailing parameters. `None` means every parameter is
    /// required.
    #[serde(default)]
    pub required: Option<usize>,
}
impl CallSchema {
    /// Creates a call schema in which every parameter is required.
    pub fn positional(parameters: Vec<Schema>) -> Self {
        Self {
            parameters,
            required: None,
        }
    }
    /// Creates a required single-parameter call schema.
    pub fn one(schema: Schema) -> Self {
        Self::positional(vec![schema])
    }
    /// A schema whose trailing parameters past `required` may be omitted.
    pub fn optional_trailing(parameters: Vec<Schema>, required: usize) -> Self {
        debug_assert!(required <= parameters.len());
        Self {
            parameters,
            required: Some(required),
        }
    }
    /// The minimum number of arguments a call must provide.
    pub fn required_count(&self) -> usize {
        self.required.unwrap_or(self.parameters.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Effect and retry semantics declared by a host tool.
pub enum ExecutionPolicy {
    /// No external effects; unused calls may be pruned and results reused.
    Pure,
    /// Repeating the operation is safe and produces the same external effect.
    Idempotent,
    /// The host can recover or reconcile an interrupted operation.
    Recoverable,
    /// The operation must not be dispatched more than once.
    AtMostOnce,
    /// The operation has effects without stronger retry guarantees.
    Unsafe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Host-facing declaration of a callable tool.
pub struct ToolDescriptor {
    /// Fully qualified source name, such as `profile.lookup`.
    pub name: String,
    /// Concise description for callers and generated tool catalogs.
    pub summary: String,
    /// Positional input schema.
    pub input: CallSchema,
    /// Successful output schema.
    pub output: Schema,
    /// Effect and retry policy.
    pub execution: ExecutionPolicy,
    /// Host-controlled version included in operation identity.
    pub schema_version: String,
}

#[derive(Debug, Clone, Default)]
/// Deterministically ordered collection of tool descriptors.
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolDescriptor>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }
    /// Registers a descriptor, rejecting malformed or duplicate names.
    pub fn register(&mut self, descriptor: ToolDescriptor) -> Result<(), String> {
        if descriptor.name.split('.').any(|x| x.is_empty()) {
            return Err("tool names must contain non-empty path segments".into());
        }
        if self.tools.contains_key(&descriptor.name) {
            return Err(format!("duplicate tool `{}`", descriptor.name));
        }
        self.tools.insert(descriptor.name.clone(), descriptor);
        Ok(())
    }
    /// Looks up a descriptor by its fully qualified name.
    pub fn get(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools.get(name)
    }
    /// Iterates over fully qualified tool names in lexical order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
    /// Returns the top-level namespace segments used by registered tools.
    pub fn roots(&self) -> BTreeSet<&str> {
        self.tools
            .keys()
            .filter_map(|n| n.split('.').next())
            .collect()
    }
    /// Returns a deterministic hex digest of all descriptors.
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(&self.tools).expect("serializable");
        hex::encode(Sha256::digest(bytes))
    }
}

impl Schema {
    /// Unbounded signed 64-bit integer schema.
    pub const INTEGER: Self = Self::Integer {
        min: None,
        max: None,
    };
    /// Unbounded finite binary64 number schema.
    pub const NUMBER: Self = Self::Number {
        min: None,
        max: None,
    };
    /// Creates an unconstrained string schema.
    pub fn string() -> Self {
        Self::String {
            format: None,
            enumeration: vec![],
            min_len: None,
            max_len: None,
        }
    }
    /// Creates an unconstrained list with the given element schema.
    pub fn list(items: Schema) -> Self {
        Self::List {
            items: Box::new(items),
            min_len: None,
            max_len: None,
        }
    }
    /// Compact human-readable type notation, for diagnostics: `string`,
    /// `int[]`, `{ id: string, total?: int }`, `"a" | "b"`,
    /// `{ [key]: number }`. Deliberately lossy — bounds and formats are
    /// omitted; nesting deeper than two levels collapses to kind names.
    pub fn type_notation(&self) -> String {
        self.notation(0)
    }

    fn notation(&self, depth: usize) -> String {
        const MAX_DEPTH: usize = 2;
        match self {
            Self::Integer { .. } => "int".to_string(),
            Self::String { enumeration, .. } if !enumeration.is_empty() => enumeration
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(" | "),
            Self::List { items, .. } => match items.as_ref() {
                Self::Object { .. } | Self::Union { .. } if depth < MAX_DEPTH => {
                    format!("{}[]", items.notation(depth + 1))
                }
                Self::Object { .. } | Self::Union { .. } => "object[]".to_string(),
                other => format!("{}[]", other.notation(depth + 1)),
            },
            Self::Object {
                properties,
                required,
                ..
            } => {
                if depth >= MAX_DEPTH || properties.is_empty() {
                    return "object".to_string();
                }
                let fields = properties
                    .iter()
                    .map(|(name, property)| {
                        let optional = if required.contains(name) { "" } else { "?" };
                        format!("{name}{optional}: {}", property.schema.notation(depth + 1))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{ {fields} }}")
            }
            Self::Map { values } => format!("{{ [key]: {} }}", values.notation(depth + 1)),
            Self::Union { variants, .. } => variants
                .iter()
                .map(|v| v.notation(depth + 1))
                .collect::<Vec<_>>()
                .join(" | "),
            other => other.kind_name().to_string(),
        }
    }

    /// Short lowercase name of the schema's kind, for diagnostics.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Integer { .. } => "integer",
            Self::Number { .. } => "number",
            Self::String { .. } => "string",
            Self::Bytes => "bytes",
            Self::List { .. } => "list",
            Self::Map { .. } => "map",
            Self::Object { .. } => "object",
            Self::Union { .. } => "union",
            Self::Never => "never",
        }
    }

    /// Returns whether a canonical value satisfies this schema.
    pub fn accepts(&self, v: &CanonicalValue) -> bool {
        match (self, v) {
            (Self::Never, _) => false,
            (Self::Any, _) => true,
            (Self::Null, CanonicalValue::Null) => true,
            (Self::Boolean, CanonicalValue::Boolean(_)) => true,
            (Self::Integer { min, max }, CanonicalValue::Integer(x)) => {
                min.is_none_or(|m| *x >= m) && max.is_none_or(|m| *x <= m)
            }
            (Self::Number { min, max }, CanonicalValue::Number(x)) => {
                x.is_finite() && min.is_none_or(|m| *x >= m) && max.is_none_or(|m| *x <= m)
            }
            (
                Self::String {
                    enumeration,
                    min_len,
                    max_len,
                    ..
                },
                CanonicalValue::String(x),
            ) => {
                (enumeration.is_empty() || enumeration.contains(x))
                    && min_len.is_none_or(|m| x.chars().count() >= m)
                    && max_len.is_none_or(|m| x.chars().count() <= m)
            }
            (Self::Bytes, CanonicalValue::Bytes(_)) => true,
            (
                Self::List {
                    items,
                    min_len,
                    max_len,
                },
                CanonicalValue::List(xs),
            ) => {
                min_len.is_none_or(|m| xs.len() >= m)
                    && max_len.is_none_or(|m| xs.len() <= m)
                    && xs.iter().all(|x| items.accepts(x))
            }
            (
                Self::Object {
                    properties,
                    required,
                    additional,
                },
                CanonicalValue::Object(o),
            ) => {
                required.iter().all(|k| o.contains_key(k))
                    && (*additional || o.keys().all(|k| properties.contains_key(k)))
                    && o.iter()
                        .all(|(k, v)| properties.get(k).is_none_or(|p| p.schema.accepts(v)))
            }
            (Self::Map { values }, CanonicalValue::Object(o)) => {
                o.values().all(|v| values.accepts(v))
            }
            (Self::Union { variants, .. }, v) => variants.iter().any(|s| s.accepts(v)),
            _ => false,
        }
    }
}
