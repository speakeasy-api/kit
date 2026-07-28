//! The deterministic standard prelude (DESIGN.md section 7.3, STDLIB.md).
//!
//! Prelude entries are ordinary [`ToolDescriptor`]s with in-process pure
//! handlers: they share the call syntax, schema machinery, and graph
//! semantics of host tools, but never leave the process. Hosts install them
//! with [`crate::RuntimeBuilder::with_prelude`]; a host registration with the
//! same name takes precedence.
//!
//! The surface is the 28-function tier 1 accepted in STDLIB.md. Aggregation
//! is not a prelude concern: `fold acc = init for x in xs { ... }` is the
//! single way to reduce, `skip` the single way to filter, computed keys the
//! single way to accumulate by key, and operators (`in`, `+`) are never
//! duplicated as functions. The prelude carries only what those cannot
//! express clearly: string/regex manipulation, ordering, ranges, parsing,
//! and formatting.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use crate::runtime::{ToolContext, ToolError};
use crate::{CallSchema, CanonicalValue as V, ExecutionPolicy, Schema, ToolDescriptor};

pub(crate) type PreludeHandler = fn(&[V], &ToolContext) -> Result<V, ToolError>;

const STDLIB_VERSION: &str = "stdlib/1";
/// Upper bound for `list.range` output length: generous for pagination and
/// index sequences while keeping a runaway range from exhausting memory.
const RANGE_CAP: i64 = 65_536;
/// Upper bound for dynamically-compiled regex pattern length.
const PATTERN_CAP: usize = 1_000;

fn descriptor(name: &str, summary: &str, input: CallSchema, output: Schema) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        summary: summary.into(),
        input,
        output,
        execution: ExecutionPolicy::Pure,
        schema_version: STDLIB_VERSION.into(),
    }
}

fn nullable(schema: Schema) -> Schema {
    Schema::Union {
        variants: vec![schema, Schema::Null],
        discriminator: None,
    }
}

fn captures_schema() -> Schema {
    let mut properties = BTreeMap::new();
    properties.insert("full".to_string(), crate::Property::new(Schema::string()));
    properties.insert(
        "groups".to_string(),
        crate::Property::new(Schema::list(nullable(Schema::string()))),
    );
    properties.insert(
        "names".to_string(),
        crate::Property::new(Schema::Map {
            values: Box::new(nullable(Schema::string())),
        }),
    );
    nullable(Schema::Object {
        required: properties.keys().cloned().collect(),
        properties,
        additional: false,
    })
}

pub(crate) fn entries() -> Vec<(ToolDescriptor, PreludeHandler)> {
    let s = Schema::string;
    vec![
        // text ------------------------------------------------------------
        (
            descriptor(
                "text.length",
                "Number of characters in a string.",
                CallSchema::one(s()),
                Schema::INTEGER,
            ),
            text_length,
        ),
        (
            descriptor(
                "text.lower",
                "Lowercase a string.",
                CallSchema::one(s()),
                s(),
            ),
            text_lower,
        ),
        (
            descriptor(
                "text.upper",
                "Uppercase a string.",
                CallSchema::one(s()),
                s(),
            ),
            text_upper,
        ),
        (
            descriptor(
                "text.trim",
                "Remove leading and trailing whitespace.",
                CallSchema::one(s()),
                s(),
            ),
            text_trim,
        ),
        (
            descriptor(
                "text.starts_with",
                "Whether the string starts with the prefix.",
                CallSchema::positional(vec![s(), s()]),
                Schema::Boolean,
            ),
            text_starts_with,
        ),
        (
            descriptor(
                "text.ends_with",
                "Whether the string ends with the suffix.",
                CallSchema::positional(vec![s(), s()]),
                Schema::Boolean,
            ),
            text_ends_with,
        ),
        (
            descriptor(
                "text.slice",
                "Characters from start (inclusive) to end (exclusive); negative indices count from the end; out-of-range indices clamp. End defaults to the string length.",
                CallSchema::optional_trailing(vec![s(), Schema::INTEGER, Schema::INTEGER], 2),
                s(),
            ),
            text_slice,
        ),
        (
            descriptor(
                "text.split",
                "Split a string on a non-empty literal separator.",
                CallSchema::positional(vec![s(), s()]),
                Schema::list(s()),
            ),
            text_split,
        ),
        (
            descriptor(
                "text.join",
                "Join a list of strings with a separator.",
                CallSchema::positional(vec![Schema::list(s()), s()]),
                s(),
            ),
            text_join,
        ),
        (
            descriptor(
                "text.replace",
                "Replace every occurrence of a literal substring with a replacement.",
                CallSchema::positional(vec![s(), s(), s()]),
                s(),
            ),
            text_replace,
        ),
        // regex -----------------------------------------------------------
        (
            descriptor(
                "regex.test",
                "Whether the string matches the pattern anywhere; anchor with ^ and $ for a full match. No lookaround or backreferences.",
                CallSchema::positional(vec![s(), s()]),
                Schema::Boolean,
            ),
            regex_test,
        ),
        (
            descriptor(
                "regex.find_all",
                "Every non-overlapping full match of the pattern, in order.",
                CallSchema::positional(vec![s(), s()]),
                Schema::list(s()),
            ),
            regex_find_all,
        ),
        (
            descriptor(
                "regex.captures",
                "First match of the pattern as { full, groups, names }, or null when nothing matches. groups are the numbered captures (null for unmatched optionals); names the named ones.",
                CallSchema::positional(vec![s(), s()]),
                captures_schema(),
            ),
            regex_captures,
        ),
        (
            descriptor(
                "regex.replace",
                "Replace every match of the pattern; $1 and $name reference capture groups in the replacement.",
                CallSchema::positional(vec![s(), s(), s()]),
                s(),
            ),
            regex_replace,
        ),
        (
            descriptor(
                "regex.split",
                "Split the string on every match of the pattern.",
                CallSchema::positional(vec![s(), s()]),
                Schema::list(s()),
            ),
            regex_split,
        ),
        // list ------------------------------------------------------------
        (
            descriptor(
                "list.length",
                "Number of elements in a list.",
                CallSchema::one(Schema::list(Schema::Any)),
                Schema::INTEGER,
            ),
            list_length,
        ),
        (
            descriptor(
                "list.sort",
                "Sort scalars in natural ascending order; every element must be the same kind (all strings or all numbers).",
                CallSchema::one(Schema::list(Schema::Any)),
                Schema::list(Schema::Any),
            ),
            list_sort,
        ),
        (
            descriptor(
                "list.sort_by",
                "Sort objects by a dotted key path, e.g. list.sort_by(orders, \"customer.tier\"); pass \"desc\" to reverse. Null or missing values sort last; the sort is stable.",
                CallSchema::optional_trailing(vec![Schema::list(Schema::Any), s(), s()], 2),
                Schema::list(Schema::Any),
            ),
            list_sort_by,
        ),
        (
            descriptor(
                "list.slice",
                "Elements from start (inclusive) to end (exclusive); negative indices count from the end; out-of-range indices clamp. End defaults to the list length.",
                CallSchema::optional_trailing(
                    vec![Schema::list(Schema::Any), Schema::INTEGER, Schema::INTEGER],
                    2,
                ),
                Schema::list(Schema::Any),
            ),
            list_slice,
        ),
        (
            descriptor(
                "list.range",
                "Integers from start (inclusive) to end (exclusive), by an optional step: list.range(1, 4) is [1, 2, 3]; list.range(0, 90, 30) is [0, 30, 60]. A negative step counts down.",
                CallSchema::optional_trailing(
                    vec![Schema::INTEGER, Schema::INTEGER, Schema::INTEGER],
                    2,
                ),
                Schema::list(Schema::INTEGER),
            ),
            list_range,
        ),
        // json ------------------------------------------------------------
        (
            descriptor(
                "json.parse",
                "Parse a JSON string into a value.",
                CallSchema::one(s()),
                Schema::Any,
            ),
            json_parse,
        ),
        (
            descriptor(
                "json.encode",
                "Encode a value as compact canonical JSON.",
                CallSchema::one(Schema::Any),
                s(),
            ),
            json_encode,
        ),
        // number ----------------------------------------------------------
        (
            descriptor(
                "number.round",
                "Round a number to the nearest integer.",
                CallSchema::one(Schema::NUMBER),
                Schema::INTEGER,
            ),
            number_round,
        ),
        (
            descriptor(
                "number.floor",
                "Round a number down to an integer.",
                CallSchema::one(Schema::NUMBER),
                Schema::INTEGER,
            ),
            number_floor,
        ),
        (
            descriptor(
                "number.ceil",
                "Round a number up to an integer.",
                CallSchema::one(Schema::NUMBER),
                Schema::INTEGER,
            ),
            number_ceil,
        ),
        (
            descriptor(
                "number.parse",
                "Parse a decimal string into an integer or number.",
                CallSchema::one(s()),
                Schema::Union {
                    variants: vec![Schema::INTEGER, Schema::NUMBER],
                    discriminator: None,
                },
            ),
            number_parse,
        ),
        // time ------------------------------------------------------------
        (
            descriptor(
                "time.parse",
                "Parse an RFC 3339 / ISO 8601 timestamp (or date) into epoch milliseconds; do arithmetic with plain +/- (86400000 ms per day).",
                CallSchema::one(s()),
                Schema::INTEGER,
            ),
            time_parse,
        ),
        (
            descriptor(
                "time.format",
                "Format epoch milliseconds as an RFC 3339 UTC timestamp.",
                CallSchema::one(Schema::INTEGER),
                s(),
            ),
            time_format,
        ),
    ]
}

fn err(code: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(code, message.into())
}

fn string_arg(args: &[V], index: usize) -> Result<&str, ToolError> {
    match args.get(index) {
        Some(V::String(value)) => Ok(value),
        _ => Err(err(
            "RL5201",
            format!("expected a string at argument {index}"),
        )),
    }
}

fn list_arg(args: &[V], index: usize) -> Result<&Vec<V>, ToolError> {
    match args.get(index) {
        Some(V::List(x)) => Ok(x),
        _ => Err(err(
            "RL5201",
            format!("expected a list at argument {index}"),
        )),
    }
}

fn integer_arg(args: &[V], index: usize) -> Result<i64, ToolError> {
    match args.get(index) {
        Some(V::Integer(x)) => Ok(*x),
        _ => Err(err(
            "RL5201",
            format!("expected an integer at argument {index}"),
        )),
    }
}

/// Normalizes a `slice`-style [start, end) pair against a length: negative
/// indices count from the end and everything clamps into range.
fn slice_bounds(len: usize, start: i64, end: Option<i64>) -> (usize, usize) {
    let normalize = |i: i64| -> usize {
        let n = if i < 0 { len as i64 + i } else { i };
        n.clamp(0, len as i64) as usize
    };
    let start = normalize(start);
    let end = normalize(end.unwrap_or(len as i64));
    (start, end.max(start))
}

// text ---------------------------------------------------------------------

fn text_length(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    Ok(V::Integer(string_arg(args, 0)?.chars().count() as i64))
}

fn text_lower(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    Ok(V::String(string_arg(args, 0)?.to_lowercase()))
}

fn text_upper(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    Ok(V::String(string_arg(args, 0)?.to_uppercase()))
}

fn text_trim(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    Ok(V::String(string_arg(args, 0)?.trim().to_string()))
}

fn text_starts_with(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    Ok(V::Boolean(
        string_arg(args, 0)?.starts_with(string_arg(args, 1)?),
    ))
}

fn text_ends_with(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    Ok(V::Boolean(
        string_arg(args, 0)?.ends_with(string_arg(args, 1)?),
    ))
}

fn text_slice(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let chars: Vec<char> = string_arg(args, 0)?.chars().collect();
    let end = args.get(2).map(|_| integer_arg(args, 2)).transpose()?;
    let (start, end) = slice_bounds(chars.len(), integer_arg(args, 1)?, end);
    Ok(V::String(chars[start..end].iter().collect()))
}

fn text_split(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let separator = string_arg(args, 1)?;
    if separator.is_empty() {
        return Err(err("RL5201", "text.split separator must not be empty"));
    }
    Ok(V::List(
        string_arg(args, 0)?
            .split(separator)
            .map(|part| V::String(part.into()))
            .collect(),
    ))
}

fn text_join(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let separator = string_arg(args, 1)?;
    let parts = list_arg(args, 0)?
        .iter()
        .map(|value| match value {
            V::String(value) => Ok(value.as_str()),
            _ => Err(err("RL5201", "text.join expects a list of strings")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(V::String(parts.join(separator)))
}

fn text_replace(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let pattern = string_arg(args, 1)?;
    if pattern.is_empty() {
        return Err(err("RL5201", "text.replace pattern must not be empty"));
    }
    Ok(V::String(
        string_arg(args, 0)?.replace(pattern, string_arg(args, 2)?),
    ))
}

// regex ---------------------------------------------------------------------

/// The catchable error for an invalid pattern, with a targeted hint for the
/// constructs the linear-time engine deliberately lacks.
pub(crate) fn regex_error(pattern: &str, detail: &str) -> ToolError {
    let hint = if ["(?=", "(?!", "(?<=", "(?<!"]
        .iter()
        .any(|l| pattern.contains(l))
    {
        " (lookaround is not supported; match the surrounding text and use regex.captures to extract the part you need)"
    } else if pattern.contains('\\') && detail.contains("backreference") {
        " (backreferences are not supported)"
    } else {
        ""
    };
    err("RL5210", format!("INVALID_REGEX: {detail}{hint}"))
}

/// Compile-time validation for literal patterns (analyzer use): same rules
/// and message as the runtime path, without touching the cache.
pub(crate) fn validate_pattern(pattern: &str) -> Result<(), ToolError> {
    if pattern.len() > PATTERN_CAP {
        return Err(err(
            "RL5210",
            format!("INVALID_REGEX: pattern exceeds {PATTERN_CAP} bytes"),
        ));
    }
    regex::Regex::new(pattern)
        .map(|_| ())
        .map_err(|e| regex_error(pattern, &e.to_string()))
}

fn compiled(pattern: &str) -> Result<regex::Regex, ToolError> {
    if pattern.len() > PATTERN_CAP {
        return Err(err(
            "RL5210",
            format!("INVALID_REGEX: pattern exceeds {PATTERN_CAP} bytes"),
        ));
    }
    thread_local! {
        static CACHE: RefCell<HashMap<String, regex::Regex>> = RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        if let Some(re) = cache.borrow().get(pattern) {
            return Ok(re.clone());
        }
        let re = regex::Regex::new(pattern).map_err(|e| regex_error(pattern, &e.to_string()))?;
        let mut cache = cache.borrow_mut();
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

fn regex_test(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let haystack = string_arg(args, 0)?;
    Ok(V::Boolean(
        compiled(string_arg(args, 1)?)?.is_match(haystack),
    ))
}

fn regex_find_all(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let haystack = string_arg(args, 0)?;
    Ok(V::List(
        compiled(string_arg(args, 1)?)?
            .find_iter(haystack)
            .map(|m| V::String(m.as_str().into()))
            .collect(),
    ))
}

fn regex_captures(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let haystack = string_arg(args, 0)?;
    let re = compiled(string_arg(args, 1)?)?;
    let Some(captures) = re.captures(haystack) else {
        return Ok(V::Null);
    };
    let capture = |m: Option<regex::Match>| match m {
        Some(m) => V::String(m.as_str().into()),
        None => V::Null,
    };
    let groups = (1..captures.len())
        .map(|i| capture(captures.get(i)))
        .collect();
    let names = re
        .capture_names()
        .flatten()
        .map(|name| (name.to_string(), capture(captures.name(name))))
        .collect::<BTreeMap<_, _>>();
    Ok(V::Object(BTreeMap::from([
        ("full".to_string(), capture(captures.get(0))),
        ("groups".to_string(), V::List(groups)),
        ("names".to_string(), V::Object(names)),
    ])))
}

fn regex_replace(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let haystack = string_arg(args, 0)?;
    let replacement = string_arg(args, 2)?;
    Ok(V::String(
        compiled(string_arg(args, 1)?)?
            .replace_all(haystack, replacement)
            .into_owned(),
    ))
}

fn regex_split(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let haystack = string_arg(args, 0)?;
    Ok(V::List(
        compiled(string_arg(args, 1)?)?
            .split(haystack)
            .map(|part| V::String(part.into()))
            .collect(),
    ))
}

// list ----------------------------------------------------------------------

fn list_length(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    Ok(V::Integer(list_arg(args, 0)?.len() as i64))
}

/// A scalar sort key. Every element in one sort must map to the same variant;
/// mixing kinds is an error rather than an arbitrary cross-kind order.
enum SortKey {
    Number(f64),
    Text(String),
}

fn sort_key(value: &V, context: &str) -> Result<SortKey, ToolError> {
    match value {
        V::Integer(x) => Ok(SortKey::Number(*x as f64)),
        V::Number(x) => Ok(SortKey::Number(*x)),
        V::String(x) => Ok(SortKey::Text(x.clone())),
        other => Err(err(
            "RL5201",
            format!(
                "{context} cannot order a {} value; sort keys must be strings or numbers",
                kind(other)
            ),
        )),
    }
}

fn kind(v: &V) -> &'static str {
    match v {
        V::Null => "null",
        V::Boolean(_) => "boolean",
        V::Integer(_) => "integer",
        V::Number(_) => "number",
        V::String(_) => "string",
        V::Bytes(_) => "bytes",
        V::List(_) => "list",
        V::Object(_) => "object",
    }
}

fn sort_pairs(
    mut pairs: Vec<(Option<SortKey>, V)>,
    descending: bool,
    context: &str,
) -> Result<Vec<V>, ToolError> {
    let mixed = pairs
        .iter()
        .filter_map(|(k, _)| k.as_ref())
        .try_fold(None::<bool>, |seen, key| {
            let textual = matches!(key, SortKey::Text(_));
            match seen {
                Some(prior) if prior != textual => None,
                _ => Some(Some(textual)),
            }
        })
        .is_none();
    if mixed {
        return Err(err(
            "RL5201",
            format!("{context} cannot order strings against numbers; make the keys one kind first"),
        ));
    }
    pairs.sort_by(|(a, _), (b, _)| {
        let ordering = match (a, b) {
            (Some(SortKey::Number(a)), Some(SortKey::Number(b))) => {
                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
            }
            (Some(SortKey::Text(a)), Some(SortKey::Text(b))) => a.cmp(b),
            // Null/missing keys sort last regardless of direction.
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (Some(_), None) => return std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Equal,
        };
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Ok(pairs.into_iter().map(|(_, v)| v).collect())
}

fn list_sort(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let pairs = list_arg(args, 0)?
        .iter()
        .map(|v| Ok((Some(sort_key(v, "list.sort")?), v.clone())))
        .collect::<Result<Vec<_>, ToolError>>()?;
    Ok(V::List(sort_pairs(pairs, false, "list.sort")?))
}

fn list_sort_by(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let path: Vec<&str> = string_arg(args, 1)?.split('.').collect();
    let descending = match args.get(2) {
        None => false,
        Some(V::String(direction)) if direction == "asc" => false,
        Some(V::String(direction)) if direction == "desc" => true,
        Some(other) => {
            return Err(err(
                "RL5201",
                format!(
                    "list.sort_by direction must be \"asc\" or \"desc\", got {}",
                    kind(other)
                ),
            ));
        }
    };
    let pairs = list_arg(args, 0)?
        .iter()
        .map(|item| {
            let mut cursor = item;
            for segment in &path {
                match cursor {
                    V::Object(o) => match o.get(*segment) {
                        Some(next) => cursor = next,
                        None => return Ok((None, item.clone())),
                    },
                    _ => return Ok((None, item.clone())),
                }
            }
            match cursor {
                V::Null => Ok((None, item.clone())),
                scalar => Ok((Some(sort_key(scalar, "list.sort_by")?), item.clone())),
            }
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    Ok(V::List(sort_pairs(pairs, descending, "list.sort_by")?))
}

fn list_slice(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let items = list_arg(args, 0)?;
    let end = args.get(2).map(|_| integer_arg(args, 2)).transpose()?;
    let (start, end) = slice_bounds(items.len(), integer_arg(args, 1)?, end);
    Ok(V::List(items[start..end].to_vec()))
}

fn list_range(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let (start, end) = (integer_arg(args, 0)?, integer_arg(args, 1)?);
    let step = args.get(2).map(|_| integer_arg(args, 2)).transpose()?;
    let step = step.unwrap_or(1);
    if step == 0 {
        return Err(err("RL5214", "list.range step cannot be 0".to_string()));
    }
    let span = if step > 0 {
        end.saturating_sub(start)
    } else {
        start.saturating_sub(end)
    }
    .max(0);
    let length = span
        .saturating_add(step.abs() - 1)
        .saturating_div(step.abs());
    if length > RANGE_CAP {
        return Err(err(
            "RL5201",
            format!("list.range produces {length} elements; the cap is {RANGE_CAP}"),
        ));
    }
    let mut out = Vec::with_capacity(length as usize);
    let mut x = start;
    while if step > 0 { x < end } else { x > end } {
        out.push(V::Integer(x));
        x += step;
    }
    Ok(V::List(out))
}

// json ----------------------------------------------------------------------

fn from_json(value: serde_json::Value) -> Result<V, ToolError> {
    Ok(match value {
        serde_json::Value::Null => V::Null,
        serde_json::Value::Bool(x) => V::Boolean(x),
        serde_json::Value::Number(x) => match x.as_i64() {
            Some(i) => V::Integer(i),
            None => V::number(x.as_f64().unwrap_or(f64::NAN))
                .map_err(|_| err("RL5103", "NON_FINITE_NUMBER"))?,
        },
        serde_json::Value::String(x) => V::String(x),
        serde_json::Value::Array(xs) => {
            V::List(xs.into_iter().map(from_json).collect::<Result<_, _>>()?)
        }
        serde_json::Value::Object(o) => V::Object(
            o.into_iter()
                .map(|(k, v)| Ok((k, from_json(v)?)))
                .collect::<Result<_, ToolError>>()?,
        ),
    })
}

fn json_parse(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let text = string_arg(args, 0)?;
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| err("RL5211", format!("INVALID_JSON: {e}")))?;
    from_json(value)
}

fn json_encode(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let value = args
        .first()
        .ok_or_else(|| err("RL5201", "expected a value argument"))?;
    value
        .presentation_json()
        .map(V::String)
        .map_err(|e| err("RL5207", e.to_string()))
}

// number --------------------------------------------------------------------

fn rounded(args: &[V], round: fn(f64) -> f64) -> Result<V, ToolError> {
    let value = match args.first() {
        Some(V::Number(v)) => *v,
        Some(V::Integer(v)) => return Ok(V::Integer(*v)),
        _ => return Err(err("RL5201", "expected a number argument")),
    };
    let rounded = round(value);
    if !rounded.is_finite() || rounded < i64::MIN as f64 || rounded > i64::MAX as f64 {
        return Err(err("RL5101", "NUMERIC_OVERFLOW"));
    }
    Ok(V::Integer(rounded as i64))
}

fn number_round(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    rounded(args, f64::round)
}

fn number_floor(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    rounded(args, f64::floor)
}

fn number_ceil(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    rounded(args, f64::ceil)
}

fn number_parse(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let text = string_arg(args, 0)?.trim();
    if let Ok(integer) = text.parse::<i64>() {
        return Ok(V::Integer(integer));
    }
    match text.parse::<f64>() {
        Ok(number) if number.is_finite() => {
            V::number(number).map_err(|_| err("RL5103", "NON_FINITE_NUMBER"))
        }
        _ => Err(err(
            "RL5212",
            format!("INVALID_NUMBER: `{text}` is not a decimal number"),
        )),
    }
}

// time ----------------------------------------------------------------------
//
// Epoch milliseconds with hand-rolled proleptic-Gregorian civil conversion
// (Howard Hinnant's days_from_civil/civil_from_days), so the prelude stays
// dependency-free and the semantics are fully pinned.

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_adjusted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_adjusted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_adjusted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_adjusted + 2) / 5 + 1;
    let month = if month_adjusted < 10 {
        month_adjusted + 3
    } else {
        month_adjusted - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
    }
}

fn invalid_time(text: &str, reason: &str) -> ToolError {
    err(
        "RL5213",
        format!(
            "INVALID_TIMESTAMP: `{text}` — {reason}; expected RFC 3339 like \
             \"2026-07-12T09:30:00Z\" or a date like \"2026-07-12\""
        ),
    )
}

fn time_parse(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let text = string_arg(args, 0)?.trim();
    let digits = |s: &str, range: std::ops::Range<usize>| -> Option<i64> {
        let slice = s.get(range)?;
        (!slice.is_empty() && slice.bytes().all(|b| b.is_ascii_digit()))
            .then(|| slice.parse().ok())
            .flatten()
    };
    let date_ok = text.len() >= 10 && &text[4..5] == "-" && &text[7..8] == "-";
    let (year, month, day) = match (
        date_ok,
        digits(text, 0..4),
        digits(text, 5..7),
        digits(text, 8..10),
    ) {
        (true, Some(y), Some(m), Some(d)) => (y, m, d),
        _ => return Err(invalid_time(text, "malformed date")),
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err(invalid_time(text, "no such calendar date"));
    }
    let mut millis = days_from_civil(year, month, day) * 86_400_000;
    let rest = &text[10..];
    if !rest.is_empty() {
        let time = match rest.as_bytes()[0] {
            b'T' | b't' | b' ' => &rest[1..],
            _ => return Err(invalid_time(text, "expected `T` between date and time")),
        };
        let (hour, minute, second) = match (
            time.len() >= 8 && &time[2..3] == ":" && &time[5..6] == ":",
            digits(time, 0..2),
            digits(time, 3..5),
            digits(time, 6..8),
        ) {
            (true, Some(h), Some(m), Some(s)) => (h, m, s),
            _ => return Err(invalid_time(text, "malformed time")),
        };
        if hour > 23 || minute > 59 || second > 60 {
            return Err(invalid_time(text, "no such time of day"));
        }
        millis += (hour * 3_600 + minute * 60 + second.min(59)) * 1_000;
        let mut tail = &time[8..];
        if let Some(fraction) = tail.strip_prefix('.') {
            let end = fraction
                .bytes()
                .position(|b| !b.is_ascii_digit())
                .unwrap_or(fraction.len());
            if end == 0 {
                return Err(invalid_time(text, "empty fractional seconds"));
            }
            let padded = format!("{:0<3}", &fraction[..end.min(3)]);
            millis += padded.parse::<i64>().unwrap_or(0);
            tail = &fraction[end..];
        }
        match tail {
            "Z" | "z" => {}
            offset
                if offset.len() == 6
                    && (offset.starts_with('+') || offset.starts_with('-'))
                    && &offset[3..4] == ":" =>
            {
                let (hours, minutes) = match (digits(offset, 1..3), digits(offset, 4..6)) {
                    (Some(h), Some(m)) if h <= 23 && m <= 59 => (h, m),
                    _ => return Err(invalid_time(text, "malformed UTC offset")),
                };
                let offset_ms = (hours * 3_600 + minutes * 60) * 1_000;
                if offset.starts_with('+') {
                    millis -= offset_ms;
                } else {
                    millis += offset_ms;
                }
            }
            "" => return Err(invalid_time(text, "missing timezone (append `Z`)")),
            _ => return Err(invalid_time(text, "malformed timezone")),
        }
    }
    Ok(V::Integer(millis))
}

fn time_format(args: &[V], _: &ToolContext) -> Result<V, ToolError> {
    let millis = integer_arg(args, 0)?;
    let days = millis.div_euclid(86_400_000);
    let in_day = millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return Err(err(
            "RL5213",
            format!("INVALID_TIMESTAMP: {millis} ms is outside years 0000-9999"),
        ));
    }
    let (second_of_day, ms) = (in_day / 1_000, in_day % 1_000);
    // Whole seconds render without a fractional part: `14:00:00Z`, not
    // `14:00:00.000Z` — the form APIs and humans expect back.
    let fraction = if ms == 0 {
        String::new()
    } else {
        format!(".{ms:03}")
    };
    Ok(V::String(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{fraction}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
    )))
}
