//! End-to-end corpus tests: complete, model-shaped programs run against a
//! mock tool registry with golden outputs.
//!
//! Unit tests exercise features one at a time; the bugs that reach models
//! live in *compositions* (a block inside a parenthesized expression, a
//! `null` fold seed projected later). Every program in `tests/programs/`
//! is compiled and executed exactly like a compose-submitted program: tools
//! return `Any`-schema JSON-ish values, the `input` host value is `Any`,
//! and the result is compared structurally.
//!
//! Directive header (runlet comments, so the files stay valid programs):
//!   #! input: <json>          bind as the `input` host value (schema Any)
//!   #! expect: <json>         structural equality with the program result
//!   #! expect_error: <code>   compilation or execution must fail with code
//!   #! writes: <n>            exact number of write.update dispatches
//!
//! Add a corpus file for every new grammar construct or stdlib namespace,
//! composed with at least two existing features — not in isolation.

use runlet::{
    CallSchema, CanonicalValue, ExecutionPolicy, Property, Runtime, Schema, ToolDescriptor,
    ToolRegistry,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn json_to_value(v: serde_json::Value) -> CanonicalValue {
    match v {
        serde_json::Value::Null => CanonicalValue::Null,
        serde_json::Value::Bool(x) => CanonicalValue::Boolean(x),
        serde_json::Value::Number(x) => match x.as_i64() {
            Some(i) => CanonicalValue::Integer(i),
            None => CanonicalValue::number(x.as_f64().unwrap()).unwrap(),
        },
        serde_json::Value::String(x) => CanonicalValue::String(x),
        serde_json::Value::Array(xs) => {
            CanonicalValue::List(xs.into_iter().map(json_to_value).collect())
        }
        serde_json::Value::Object(o) => {
            CanonicalValue::Object(o.into_iter().map(|(k, v)| (k, json_to_value(v))).collect())
        }
    }
}

fn fixture(json: serde_json::Value) -> CanonicalValue {
    json_to_value(json)
}

fn any_tool(name: &str, arity: usize) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        summary: String::new(),
        input: CallSchema::positional(vec![Schema::Any; arity]),
        output: Schema::Any,
        execution: ExecutionPolicy::Pure,
        schema_version: "e2e/1".into(),
    }
}

/// The registry mirrors what the AgentKit compose backend produces from
/// JSON tools: `Any` in, `Any` out — except `write.update`, whose object
/// input schema has optional properties so null-omission is exercised.
fn harness(input: Option<serde_json::Value>) -> (Runtime, Arc<AtomicUsize>) {
    let mut registry = ToolRegistry::new();
    for (name, arity) in [
        ("data.users", 0),
        ("data.orders", 0),
        ("data.contacts", 0),
        ("data.payload", 0),
        ("data.availability", 1),
    ] {
        registry.register(any_tool(name, arity)).unwrap();
    }
    let mut update_properties = BTreeMap::new();
    update_properties.insert("id".to_string(), Property::new(Schema::string()));
    update_properties.insert("phone".to_string(), Property::new(Schema::string()));
    update_properties.insert("company".to_string(), Property::new(Schema::string()));
    registry
        .register(ToolDescriptor {
            name: "write.update".into(),
            summary: String::new(),
            input: CallSchema::one(Schema::Object {
                properties: update_properties,
                required: BTreeSet::from(["id".to_string()]),
                additional: false,
            }),
            output: Schema::Boolean,
            execution: ExecutionPolicy::AtMostOnce,
            schema_version: "e2e/1".into(),
        })
        .unwrap();

    let writes = Arc::new(AtomicUsize::new(0));
    let mut builder = Runtime::builder()
        .registry(registry)
        .with_prelude()
        .tool("data.users", |_, _| {
            Ok(fixture(serde_json::json!([
                { "id": 1, "name": "ada" },
                { "id": 2, "name": "bo" },
                { "id": 3, "name": "cy" }
            ])))
        })
        .tool("data.orders", |_, _| {
            Ok(fixture(serde_json::json!([
                { "id": "o1", "status": "completed", "amount_cents": 500, "customer": { "tier": 2 } },
                { "id": "o2", "status": "open", "amount_cents": 250, "customer": { "tier": 3 } },
                { "id": "o3", "status": "open", "amount_cents": 100, "customer": { "tier": 1 } }
            ])))
        })
        .tool("data.contacts", |_, _| {
            Ok(fixture(serde_json::json!([
                { "id": "CT-1", "phone": "+14155550100", "company": "Globex" },
                { "id": "CT-2", "phone": "+1-415-555-0101", "company": "Globex" },
                { "id": "CT-3", "phone": "415.555.0102", "company": "Initech" }
            ])))
        })
        .tool("data.payload", |_, _| {
            Ok(fixture(serde_json::json!({
                "body": "{\"when\": \"2026-06-15T12:00:00Z\", \"n\": 5}"
            })))
        })
        .tool("data.availability", |args, _| {
            let user_id = match &args[0] {
                CanonicalValue::Object(o) => o.get("user_id").cloned(),
                _ => None,
            };
            let busy = if user_id == Some(CanonicalValue::Integer(1)) {
                serde_json::json!([{ "start": "09:00", "end": "10:30" }])
            } else {
                serde_json::json!([])
            };
            let date = match &args[0] {
                CanonicalValue::Object(o) => o.get("date").cloned().unwrap_or(CanonicalValue::Null),
                _ => CanonicalValue::Null,
            };
            let mut result = BTreeMap::new();
            result.insert("date".to_string(), date);
            result.insert("busy".to_string(), fixture(busy));
            Ok(CanonicalValue::Object(result))
        })
        .tool("write.update", {
            let writes = writes.clone();
            move |_, _| {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(CanonicalValue::Boolean(true))
            }
        });
    if let Some(input) = input {
        builder = builder.input("input", Schema::Any, json_to_value(input));
    }
    (builder.build().unwrap(), writes)
}

struct Directives {
    input: Option<serde_json::Value>,
    expect: Option<serde_json::Value>,
    expect_error: Option<String>,
    writes: Option<usize>,
}

fn directives(source: &str) -> Directives {
    let mut d = Directives {
        input: None,
        expect: None,
        expect_error: None,
        writes: None,
    };
    for line in source.lines() {
        let Some(rest) = line.strip_prefix("#!") else {
            continue;
        };
        let Some((key, value)) = rest.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "input" => d.input = Some(serde_json::from_str(value).expect("input directive")),
            "expect" => d.expect = Some(serde_json::from_str(value).expect("expect directive")),
            "expect_error" => d.expect_error = Some(value.to_string()),
            "writes" => d.writes = Some(value.parse().expect("writes directive")),
            other => panic!("unknown directive `{other}`"),
        }
    }
    d
}

#[test]
fn corpus_programs_run_end_to_end() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("tests/programs")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rnlt"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "corpus must not be empty");
    let mut failures = Vec::new();
    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let source = std::fs::read_to_string(path).unwrap();
        let d = directives(&source);
        let (runtime, writes) = harness(d.input.clone());
        let compiled = runtime.compile(&source);
        let outcome: Result<(), String> = (|| {
            let program = match (compiled, &d.expect_error) {
                (Err(diagnostics), Some(code)) => {
                    return if diagnostics.iter().any(|x| &x.code == code) {
                        Ok(())
                    } else {
                        Err(format!("expected {code} at compile, got {diagnostics:#?}"))
                    };
                }
                (Err(diagnostics), None) => {
                    return Err(format!("failed to compile: {diagnostics:#?}"));
                }
                (Ok(program), _) => program,
            };
            match (runtime.run(&program), &d.expect_error) {
                (Err(error), Some(code)) => {
                    if &error.code == code {
                        Ok(())
                    } else {
                        Err(format!("expected error {code}, got {error:?}"))
                    }
                }
                (Err(error), None) => Err(format!("execution failed: {error:?}")),
                (Ok(_), Some(code)) => Err(format!("expected error {code}, but it succeeded")),
                (Ok(execution), None) => {
                    if let Some(expected) = &d.expect {
                        let got: serde_json::Value =
                            serde_json::from_str(&execution.value.presentation_json().unwrap())
                                .unwrap();
                        if &got != expected {
                            return Err(format!("expected {expected}, got {got}"));
                        }
                    }
                    if let Some(count) = d.writes {
                        let dispatched = writes.load(Ordering::SeqCst);
                        if dispatched != count {
                            return Err(format!("expected {count} writes, got {dispatched}"));
                        }
                    }
                    Ok(())
                }
            }
        })();
        if let Err(reason) = outcome {
            failures.push(format!("{name}: {reason}"));
        }
    }
    assert!(
        failures.is_empty(),
        "corpus failures:\n{}",
        failures.join("\n")
    );
}

/// Every block-bearing construct, nested inside every bracketing context,
/// with multi-statement bodies. This is the class both v11 bugs lived in:
/// features that work at the top level but break inside another grouping.
#[test]
fn block_constructs_parse_inside_every_grouping_context() {
    let blocks = [
        "for x in [1, 2] {\n  y = x * 2\n  skip if y > 3\n  return y\n}",
        "fold acc = 0 for x in [1, 2] {\n  y = x * 2\n  return acc + y\n}",
        "boundary retry 2 {\n  v = 1\n  return v\n} catch err {\n  w = 0\n  return w\n}",
        "if 1 > 0 {\n  y = 1\n  return y\n} else if 2 > 1 {\n  return 2\n} else {\n  z = 0\n  return z\n}",
    ];
    let contexts = [
        ("binding", "value = {B}\nreturn value"),
        ("parenthesized", "value = ({B})\nreturn value"),
        (
            "conditional else",
            "value = 0 if false else ({B})\nreturn value",
        ),
        ("list element", "value = [{B}]\nreturn value[0]"),
        ("object value", "value = { key: {B} }\nreturn value.key"),
        (
            "computed key value",
            "value = { [\"k\"]: {B} }\nreturn value[\"k\"]",
        ),
        (
            "call argument",
            "value = text.join([\"a\"], \"\" + list.length([{B}]))\nreturn value",
        ),
        (
            "nested loop body",
            "value = for outer in [1] {\n  inner = {B}\n  return inner\n}\nreturn value",
        ),
        (
            "fold body",
            "value = fold acc = 0 for outer in [1] {\n  inner = {B}\n  return acc + list.length([inner])\n}\nreturn value",
        ),
        (
            "boundary body",
            "value = boundary {\n  inner = {B}\n  return inner\n} catch err {\n  return null\n}\nreturn value",
        ),
    ];
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let mut failures = Vec::new();
    for block in &blocks {
        for (label, template) in &contexts {
            let source = template.replace("{B}", block);
            if let Err(diagnostics) = runtime.compile(&source) {
                let head = &diagnostics[0];
                failures.push(format!(
                    "{label} × `{}`: {} {}",
                    block.split_whitespace().next().unwrap(),
                    head.code,
                    head.message
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "constructs must parse in every grouping context:\n{}",
        failures.join("\n")
    );
}
