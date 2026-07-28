use runlet::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn object(fields: Vec<(&str, Schema)>) -> Schema {
    let properties = fields
        .into_iter()
        .map(|(k, s)| (k.into(), Property::new(s)))
        .collect::<BTreeMap<_, _>>();
    Schema::Object {
        required: properties.keys().cloned().collect::<BTreeSet<_>>(),
        properties,
        additional: false,
    }
}

fn descriptor(name: &str, input: Vec<Schema>, output: Schema) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        summary: String::new(),
        input: CallSchema::positional(input),
        output,
        execution: ExecutionPolicy::Pure,
        schema_version: "1".into(),
    }
}

#[test]
fn unresolved_outputs_form_a_lazy_reachable_graph() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor(
            "github.me",
            vec![],
            object(vec![("login", Schema::string())]),
        ))
        .unwrap();
    registry
        .register(descriptor(
            "linear.me",
            vec![],
            object(vec![("id", Schema::string())]),
        ))
        .unwrap();
    registry
        .register(descriptor(
            "github.prs",
            vec![object(vec![("owner", Schema::string())])],
            Schema::list(Schema::string()),
        ))
        .unwrap();
    registry
        .register(descriptor(
            "linear.issues",
            vec![object(vec![("owner", Schema::string())])],
            Schema::list(Schema::string()),
        ))
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("github.me", |_, _| Ok(VObj::one("login", "octo")))
        .tool("linear.me", |_, _| Ok(VObj::one("id", "usr-1")))
        .tool("github.prs", {
            let calls = calls.clone();
            move |_, _| {
                calls.lock().unwrap().push("prs");
                Ok(CanonicalValue::List(vec!["pr".into()]))
            }
        })
        .tool("linear.issues", {
            let calls = calls.clone();
            move |_, _| {
                calls.lock().unwrap().push("issues");
                Ok(CanonicalValue::List(vec!["issue".into()]))
            }
        })
        .build()
        .unwrap();
    let source = "me = github.me()\nlinear_me = linear.me()\nmy_issues = linear.issues({ owner: linear_me.id })\nmy_prs = github.prs({ owner: me.login })\nreturn my_issues + my_prs";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value,
        CanonicalValue::List(vec!["issue".into(), "pr".into()])
    );
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(
        execution
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Call)
            .count(),
        4
    );
}

#[test]
fn unused_pure_call_is_pruned_and_never_runs() {
    // Pure dead code is allowed: execution is root-reachable, so the call
    // never dispatches. The compiled program carries an RL1205 warning
    // making the prune visible to hosts.
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("math.slow", vec![], Schema::Boolean))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("math.slow", |_, _| panic!("must not run"))
        .build()
        .unwrap();
    let program = runtime
        .compile("unused = math.slow()\nreturn true")
        .expect("pure dead code must compile");
    assert!(program.diagnostics.iter().any(|d| d.code == "RL1205"));
    let execution = runtime.run(&program).unwrap();
    assert_eq!(execution.value, CanonicalValue::Boolean(true));
}

fn effect_descriptor(name: &str, input: Vec<Schema>, output: Schema) -> ToolDescriptor {
    ToolDescriptor {
        execution: ExecutionPolicy::AtMostOnce,
        ..descriptor(name, input, output)
    }
}

#[test]
fn unused_effectful_calls_execute_as_implicit_roots_in_order() {
    // A statement containing an effectful call is an implicit root: it runs
    // when its block runs, in statement order, whether or not the return
    // references it. Fire-and-forget writes are never silently dropped.
    let mut registry = ToolRegistry::new();
    registry
        .register(effect_descriptor("audit.first", vec![], Schema::Boolean))
        .unwrap();
    registry
        .register(effect_descriptor("audit.second", vec![], Schema::Boolean))
        .unwrap();
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let record = |log: &Arc<Mutex<Vec<&'static str>>>, name: &'static str| {
        let log = log.clone();
        move |_: &[CanonicalValue], _: &ToolContext| {
            log.lock().unwrap().push(name);
            Ok(CanonicalValue::Boolean(true))
        }
    };
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("audit.first", record(&log, "first"))
        .tool("audit.second", record(&log, "second"))
        .build()
        .unwrap();
    let program = runtime
        .compile("a = audit.first()\nb = audit.second()\nreturn true")
        .expect("fire-and-forget writes must compile");
    assert!(
        program.diagnostics.is_empty(),
        "effect roots are not dead code: {:?}",
        program.diagnostics
    );
    let execution = runtime.run(&program).unwrap();
    assert_eq!(execution.value, CanonicalValue::Boolean(true));
    assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);
}

#[test]
fn conditional_effect_bindings_dispatch_only_selected_writes() {
    // The shape agents actually produce: per-item conditional writes bound
    // but never referenced, with the loop returning summary flags only. Each
    // iteration roots its own effect binding; the postfix conditional still
    // selects which branch dispatches.
    let mut registry = ToolRegistry::new();
    registry
        .register(effect_descriptor(
            "crm.update",
            vec![Schema::INTEGER],
            Schema::Boolean,
        ))
        .unwrap();
    let updated = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("crm.update", {
            let updated = updated.clone();
            move |args, _| {
                let CanonicalValue::Integer(id) = args[0] else {
                    unreachable!()
                };
                updated.lock().unwrap().push(id);
                Ok(CanonicalValue::Boolean(true))
            }
        })
        .build()
        .unwrap();
    let source = "flags = for id in [1, 2, 3, 4] {\n  \
                  result = crm.update(id) if id > 2 else null\n  \
                  return id > 2\n}\nreturn flags";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value,
        CanonicalValue::List(vec![
            CanonicalValue::Boolean(false),
            CanonicalValue::Boolean(false),
            CanonicalValue::Boolean(true),
            CanonicalValue::Boolean(true),
        ])
    );
    let mut ids = updated.lock().unwrap().clone();
    ids.sort();
    assert_eq!(ids, vec![3, 4]);
}

#[test]
fn conditional_without_else_defaults_to_null() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let source =
        "kept = 42 if 2 > 1\ndropped = 42 if 1 > 2\nreturn { kept: kept, dropped: dropped }";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value,
        CanonicalValue::Object(BTreeMap::from([
            ("kept".to_string(), CanonicalValue::Integer(42)),
            ("dropped".to_string(), CanonicalValue::Null),
        ]))
    );
}

#[test]
fn runtime_errors_carry_the_failing_expression_span() {
    // A non-boolean condition points at the exact source expression, and the
    // message explains the no-truthiness rule instead of `EXPECTED_BOOLEAN`.
    // The condition schema must be Any so the failure is a runtime one.
    let runtime = Runtime::builder()
        .with_prelude()
        .input("flag", Schema::Any, "ada".into())
        .build()
        .unwrap();
    let source = "label = \"yes\" if flag else \"no\"\nreturn label";
    let error = runtime
        .run(&runtime.compile(source).unwrap())
        .expect_err("string condition must fail");
    assert_eq!(error.code, "RL5202");
    assert!(
        error.message.contains("string") && error.message.contains("truthiness"),
        "message should name the value kind and the rule: {}",
        error.message
    );
    let span = error.span.expect("runtime error carries a span");
    assert_eq!(&source[span.start..span.end], "flag");
}

#[test]
fn null_for_optional_property_means_omit_the_key() {
    // `key: value if cond else null` is the only way to express a conditional
    // object property, so null for an optional non-nullable property omits
    // the key (JSON null-as-absent) instead of failing conversion. Pure
    // helpers referenced only by the effect root are reachable, not dead.
    let input_schema = Schema::Object {
        properties: BTreeMap::from([
            ("id".to_string(), Property::new(Schema::string())),
            ("phone".to_string(), Property::new(Schema::string())),
            ("company".to_string(), Property::new(Schema::string())),
        ]),
        required: BTreeSet::from(["id".to_string()]),
        additional: false,
    };
    let mut registry = ToolRegistry::new();
    registry
        .register(ToolDescriptor {
            execution: ExecutionPolicy::AtMostOnce,
            ..descriptor("update_contact", vec![input_schema], Schema::Any)
        })
        .unwrap();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::builder()
        .registry(registry)
        .with_prelude()
        .tool("update_contact", {
            let updates = updates.clone();
            move |args, _| {
                updates.lock().unwrap().push(args[0].clone());
                Ok(CanonicalValue::Boolean(true))
            }
        })
        .build()
        .unwrap();
    let source = r#"contacts = [
  { id: "CT-1", phone: "415.555.0102", company: "Globex" },
  { id: "CT-2", phone: "+14155550103", company: "Initech" },
  { id: "CT-3", phone: "+14155550104", company: "" }
]
results = for contact in contacts {
  phone = contact.phone
  needs_fix = not (text.length(phone) == 12 and "+1" in phone)
  needs_company = contact.company == ""
  cleaned = text.replace(text.replace(phone, ".", ""), "-", "")
  update_result = boundary retry 2 {
    return update_contact({
      id: contact.id,
      phone: ("+1" + cleaned) if needs_fix else null,
      company: "Umbrella" if needs_company else null
    })
  } catch err {
    return null
  } if needs_fix or needs_company else null
  return { id: contact.id, fixed: 1 if needs_fix else 0 }
}
return { fixed: fold n = 0 for r in results { return n + r.fixed } }"#;
    let program = runtime.compile(source).unwrap();
    assert!(
        program.diagnostics.is_empty(),
        "helpers feeding effect roots are reachable: {:?}",
        program.diagnostics
    );
    let execution = runtime.run(&program).unwrap();
    assert_eq!(
        execution.value,
        CanonicalValue::Object(BTreeMap::from([(
            "fixed".to_string(),
            CanonicalValue::Integer(1)
        )]))
    );
    let dispatched = updates.lock().unwrap().clone();
    assert_eq!(dispatched.len(), 2, "CT-1 phone fix and CT-3 company fill");
    let CanonicalValue::Object(first) = &dispatched[0] else {
        panic!()
    };
    assert_eq!(
        first["phone"],
        CanonicalValue::String("+14155550102".into())
    );
    assert!(!first.contains_key("company"), "null company omitted");
    let CanonicalValue::Object(second) = &dispatched[1] else {
        panic!()
    };
    assert_eq!(second["company"], CanonicalValue::String("Umbrella".into()));
    assert!(!second.contains_key("phone"), "null phone omitted");
}

#[test]
fn failing_effect_root_fails_the_block_and_boundary_catches() {
    let mut registry = ToolRegistry::new();
    registry
        .register(effect_descriptor("audit.log", vec![], Schema::Boolean))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("audit.log", |_, _| {
            Err(ToolError::new("AUDIT_DOWN", "audit sink unavailable"))
        })
        .build()
        .unwrap();
    let source = "outcome = boundary {\n  logged = audit.log()\n  return \"ok\"\n} \
                  catch err {\n  return err.code\n}\nreturn outcome";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::String("AUDIT_DOWN".into()));
}

#[test]
fn prelude_text_replace_normalizes_strings() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let source = "step1 = text.replace(\"(415) 555-0101\", \"(\", \"\")\n\
                  step2 = text.replace(step1, \")\", \"\")\n\
                  step3 = text.replace(step2, \" \", \"\")\n\
                  return text.replace(step3, \"-\", \"\")";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::String("4155550101".into()));
}

#[test]
fn loops_are_ordered_and_boundaries_catch_failures() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("work", vec![Schema::INTEGER], Schema::INTEGER))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("work", |args, _| match args[0] {
            CanonicalValue::Integer(2) => Err(ToolError::new("BOOM", "failed")),
            CanonicalValue::Integer(x) => Ok(CanonicalValue::Integer(x * 2)),
            _ => unreachable!(),
        })
        .build()
        .unwrap();
    let source = "result = boundary {\n values = for item in [1, 2, 3] { return work(item) }\n return values\n} catch err { return [err.code] }\nreturn result";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::List(vec!["BOOM".into()]));
}

#[test]
fn boundary_retry_reuses_successful_operations() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("stable", vec![], Schema::INTEGER))
        .unwrap();
    registry
        .register(descriptor("flaky", vec![], Schema::INTEGER))
        .unwrap();
    let stable_calls = Arc::new(Mutex::new(0));
    let flaky_calls = Arc::new(Mutex::new(0));
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("stable", {
            let calls = stable_calls.clone();
            move |_, _| {
                *calls.lock().unwrap() += 1;
                Ok(1.into())
            }
        })
        .tool("flaky", {
            let calls = flaky_calls.clone();
            move |_, _| {
                let mut n = calls.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    Err(ToolError::new("TEMP", "try again").retryable(true))
                } else {
                    Ok(2.into())
                }
            }
        })
        .build()
        .unwrap();
    let source = "result = boundary retry 1 { a = stable()\n b = flaky()\n return a + b } catch err { return -1 }\nreturn result";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, 3.into());
    assert_eq!(*stable_calls.lock().unwrap(), 1);
    assert_eq!(*flaky_calls.lock().unwrap(), 2);
    assert!(
        execution
            .graph
            .edges
            .iter()
            .any(|edge| edge.kind == EdgeKind::RetryOf)
    );
}

#[test]
fn conversions_and_short_circuiting_are_observable() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("accept", vec![Schema::NUMBER], Schema::NUMBER))
        .unwrap();
    registry
        .register(descriptor("must_not_run", vec![], Schema::Boolean))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("accept", |args, _| Ok(args[0].clone()))
        .tool("must_not_run", |_, _| panic!("short-circuited call ran"))
        .build()
        .unwrap();
    let execution=runtime.run(&runtime.compile("number = accept(42)\nflag = false and must_not_run()\nreturn { number, label: \"count: \" + 3, flag }").unwrap()).unwrap();
    let CanonicalValue::Object(value) = execution.value else {
        panic!()
    };
    assert_eq!(value["number"], CanonicalValue::Number(42.0));
    assert_eq!(value["label"], CanonicalValue::String("count: 3".into()));
    assert!(
        execution
            .graph
            .nodes
            .iter()
            .any(|n| n.kind == NodeKind::Convert)
    );
}

#[test]
fn host_loop_concurrency_bounds_parallel_execution_and_streams_graph_events() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("slow", vec![Schema::INTEGER], Schema::INTEGER))
        .unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::builder()
        .registry(registry)
        .loop_concurrency(3)
        .tool("slow", {
            let active = active.clone();
            let maximum = maximum.clone();
            move |args, _| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(args[0].clone())
            }
        })
        .build()
        .unwrap();
    let program = runtime
        .compile("values = for x in [1, 2, 3, 4, 5, 6] { return slow(x) }\nreturn values")
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let execution = runtime
        .run_observed(&program, {
            let events = events.clone();
            move |event| events.lock().unwrap().push(event.clone())
        })
        .unwrap();

    assert_eq!(
        execution.value,
        CanonicalValue::List((1..=6).map(CanonicalValue::Integer).collect())
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 3);
    let events = events.lock().unwrap();
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert!(events.iter().any(|event| matches!(
        event.change,
        GraphChange::NodeUpdated(ref node) if node.state == NodeState::Running
    )));
}

#[test]
fn producer_consumer_edges_are_recorded_for_tool_chains() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("step", vec![Schema::INTEGER], Schema::INTEGER))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("step", |args, _| Ok(args[0].clone()))
        .build()
        .unwrap();
    let execution = runtime
        .run(
            &runtime
                .compile("first = step(1)\nsecond = step(first)\nreturn second")
                .unwrap(),
        )
        .unwrap();
    let calls = execution
        .graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Call)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert!(execution.graph.edges.iter().any(|edge| {
        calls.iter().any(|node| node.id == edge.from)
            && calls.iter().any(|node| node.id == edge.to)
            && edge.from != edge.to
            && matches!(edge.kind, EdgeKind::Data { .. })
    }));
}

struct VObj;
impl VObj {
    fn one(key: &str, value: &str) -> CanonicalValue {
        CanonicalValue::Object(BTreeMap::from([(key.into(), value.into())]))
    }
}

#[test]
fn agent_style_programs_with_unions_and_constrained_schemas_compile() {
    // Regression: programs LLM agents actually write — conditional fallbacks
    // (which synthesize union schemas), loops over those unions, literals
    // passed to enum/bounded parameters, and arithmetic on projected union
    // members — must pass analysis; value-level checks belong to the runtime.
    let mut registry = ToolRegistry::new();
    let page = object(vec![
        (
            "items",
            Schema::list(object(vec![("id", Schema::string())])),
        ),
        ("total_pages", Schema::INTEGER),
    ]);
    registry
        .register(ToolDescriptor {
            name: "list_orders".into(),
            summary: String::new(),
            input: CallSchema::one(object(vec![
                (
                    "status",
                    Schema::String {
                        format: None,
                        enumeration: vec!["open".into(), "closed".into()],
                        min_len: None,
                        max_len: None,
                    },
                ),
                (
                    "page",
                    Schema::Integer {
                        min: Some(1),
                        max: Some(100),
                    },
                ),
            ])),
            output: page.clone(),
            execution: ExecutionPolicy::Pure,
            schema_version: "1".into(),
        })
        .unwrap();
    registry
        .register(descriptor(
            "get_order",
            vec![object(vec![("id", Schema::string())])],
            object(vec![("amount_cents", Schema::INTEGER)]),
        ))
        .unwrap();

    let source = r#"
page1 = list_orders({ status: "open", page: 1 })
page2 = list_orders({ status: "open", page: 2 })
extra = page2 if page1.total_pages > 1 else { items: [], total_pages: 1 }
amounts = for order in page1.items + extra.items {
    detail = get_order({ id: order.id })
    return detail.amount_cents if detail.amount_cents > 0 else 0
}
first = amounts[0] + 0
return { amounts, first }
"#;

    let runtime = Runtime::builder()
        .registry(registry)
        .tool("list_orders", |_, _| {
            Ok(CanonicalValue::Object(BTreeMap::from([
                ("items".into(), CanonicalValue::List(vec![])),
                ("total_pages".into(), CanonicalValue::Integer(1)),
            ])))
        })
        .tool("get_order", |_, _| {
            Ok(CanonicalValue::Object(BTreeMap::from([(
                "amount_cents".into(),
                CanonicalValue::Integer(5),
            )])))
        })
        .build()
        .unwrap();
    runtime.compile(source).unwrap_or_else(|diagnostics| {
        panic!("agent-style program must compile, got: {diagnostics:#?}")
    });
}

#[test]
fn deeply_nested_unions_stay_bounded() {
    // Regression: union synthesis must flatten, dedup, and cap — nested
    // conditionals inside loops previously grew schema trees combinatorially,
    // which could exhaust memory during analysis of pathological programs.
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor(
            "fetch",
            vec![object(vec![("id", Schema::INTEGER)])],
            object(vec![("value", Schema::INTEGER)]),
        ))
        .unwrap();

    let mut source = String::from("base = [1, 2, 3]\n");
    let mut previous = "base".to_string();
    for depth in 0..24 {
        let name = format!("level{depth}");
        source.push_str(&format!(
            "{name} = for item in {previous} {{\n\
                 detail = fetch({{ id: 1 }})\n\
                 branch = detail if detail.value > 0 else {{ value: {depth} }}\n\
                 return [branch] if detail.value > {depth} else []\n\
             }}\n"
        ));
        previous = name;
    }
    source.push_str(&format!("return {previous}\n"));

    let runtime = Runtime::builder()
        .registry(registry)
        .tool("fetch", |_, _| {
            Ok(CanonicalValue::Object(BTreeMap::from([(
                "value".into(),
                CanonicalValue::Integer(1),
            )])))
        })
        .build()
        .unwrap();
    runtime.compile(&source).unwrap_or_else(|diagnostics| {
        panic!("nested-union program must compile, got: {diagnostics:#?}")
    });
}

#[test]
fn prelude_enables_filter_and_aggregate() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor(
            "get_order",
            vec![object(vec![("id", Schema::INTEGER)])],
            object(vec![
                ("amount_cents", Schema::INTEGER),
                ("status", Schema::string()),
            ]),
        ))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .with_prelude()
        .input(
            "ids",
            Schema::list(Schema::INTEGER),
            CanonicalValue::List((1..=4).map(CanonicalValue::Integer).collect()),
        )
        .tool("get_order", |args, _| {
            let CanonicalValue::Object(input) = &args[0] else {
                panic!()
            };
            let CanonicalValue::Integer(id) = input["id"] else {
                panic!()
            };
            Ok(CanonicalValue::Object(BTreeMap::from([
                ("amount_cents".into(), CanonicalValue::Integer(id * 100)),
                (
                    "status".into(),
                    CanonicalValue::String(
                        if id % 2 == 0 { "completed" } else { "refunded" }.into(),
                    ),
                ),
            ])))
        })
        .build()
        .unwrap();

    let source = r#"
amounts = for id in ids {
    detail = get_order({ id: id })
    skip if not ("completed" in text.lower(detail.status))
    return detail.amount_cents
}
return {
    total: fold t = 0 for a in amounts { return t + a },
    count: fold n = 0 for a in amounts { return n + 1 },
    largest: fold m = 0 for a in amounts { return a if a > m else m }
}
"#;
    let program = runtime
        .compile(source)
        .unwrap_or_else(|d| panic!("prelude program must compile: {d:#?}"));
    let execution = runtime.run(&program).unwrap();
    let CanonicalValue::Object(value) = execution.value else {
        panic!()
    };
    assert_eq!(value["total"], CanonicalValue::Integer(600));
    assert_eq!(value["count"], CanonicalValue::Integer(2));
    assert_eq!(value["largest"], CanonicalValue::Integer(400));
}

#[test]
fn chained_lazy_loop_bindings_evaluate_linearly() {
    // Regression: a binding referenced only inside a loop body used to be
    // re-evaluated by every iteration; chains of such loops re-evaluated
    // exponentially (width^depth) and could exhaust memory. The shared
    // binding cache plus the sequential first iteration keep it linear.
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("fetch", vec![Schema::INTEGER], Schema::INTEGER))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("fetch", |args, _| Ok(args[0].clone()))
        .build()
        .unwrap();

    let depth = 9;
    let mut source = String::from("l0 = for i in [1, 2, 3] { v = fetch(i)\n return v }\n");
    for level in 1..depth {
        source.push_str(&format!(
            "l{level} = for i in [1, 2, 3] {{ return l{}[0] + i }}\n",
            level - 1
        ));
    }
    source.push_str(&format!("return l{}[0]\n", depth - 1));

    let program = runtime
        .compile(&source)
        .unwrap_or_else(|d| panic!("{d:#?}"));
    let nodes = Arc::new(AtomicUsize::new(0));
    let counter = nodes.clone();
    let execution = runtime
        .run_observed(&program, move |event| {
            if matches!(event.change, GraphChange::NodeAdded(_)) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
        .unwrap();
    assert_eq!(execution.value, CanonicalValue::Integer(9));
    let total = nodes.load(Ordering::Relaxed);
    assert!(
        total < 500,
        "chained loops must evaluate linearly, created {total} nodes"
    );
}

#[test]
fn eval_depth_limit_fails_cleanly_instead_of_overflowing() {
    let runtime = Runtime::builder().eval_depth_limit(8).build().unwrap();
    let source = format!("return {}1{}", "(1 + ".repeat(20), ")".repeat(20));
    let program = runtime
        .compile(&source)
        .unwrap_or_else(|d| panic!("{d:#?}"));
    let error = runtime.run(&program).unwrap_err();
    assert_eq!(error.code, "RL4106");
}

#[test]
fn deeply_nested_source_is_rejected_at_parse_time() {
    let runtime = Runtime::builder().build().unwrap();
    let source = format!("return {}1{}", "(".repeat(2000), ")".repeat(2000));
    let diagnostics = runtime.compile(&source).unwrap_err();
    assert!(
        diagnostics.iter().any(|d| d.code == "RL1015"),
        "expected RL1015, got {diagnostics:#?}"
    );
}

#[test]
fn unknown_property_diagnostic_names_the_value_type() {
    // The trained-habit collision: `.items` projected on a value that is
    // already the list. The message must say what the value IS so the model
    // can repair in one attempt instead of guessing.
    let runtime = Runtime::builder().build().unwrap();
    let diagnostics = runtime
        .compile("services = [\"a\", \"b\"]\nreturn services.items")
        .unwrap_err();
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == "RL2103")
        .unwrap_or_else(|| panic!("expected RL2103, got {diagnostics:#?}"));
    assert!(
        diagnostic.message.contains("of type string[]")
            && diagnostic
                .message
                .contains("iterate or index the list itself"),
        "message must name the type and the repair: {}",
        diagnostic.message
    );
}

#[test]
fn dispatch_limit_bounds_active_tool_executions() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("slow", vec![Schema::INTEGER], Schema::INTEGER))
        .unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let handler_active = active.clone();
    let handler_peak = peak.clone();
    let runtime = Runtime::builder()
        .registry(registry)
        .dispatch_limit(2)
        .tool("slow", move |args, _| {
            let now = handler_active.fetch_add(1, Ordering::SeqCst) + 1;
            handler_peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(15));
            handler_active.fetch_sub(1, Ordering::SeqCst);
            Ok(args[0].clone())
        })
        .build()
        .unwrap();
    let program = runtime
        .compile("out = for i in [1, 2, 3, 4, 5, 6, 7, 8] { return slow(i) }\nreturn out")
        .unwrap_or_else(|d| panic!("{d:#?}"));
    let execution = runtime.run(&program).unwrap();
    let CanonicalValue::List(values) = execution.value else {
        panic!()
    };
    assert_eq!(values.len(), 8);
    assert!(
        peak.load(Ordering::SeqCst) <= 2,
        "dispatch semaphore must bound active handlers, peak was {}",
        peak.load(Ordering::SeqCst)
    );
}

#[test]
fn nested_cross_product_loops_degrade_to_sequential_under_thread_budget() {
    // Regression: nested loops multiply worker threads (an N^3 cross-product
    // over 24 items used to create ~14k concurrent threads and gigabytes of
    // stacks). Loops now draw extra threads from a run-wide budget and the
    // evaluating thread always works its own loop, so a tiny budget must
    // still complete correctly — just more sequentially.
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("probe", vec![Schema::INTEGER], Schema::INTEGER))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .with_prelude()
        .worker_thread_limit(4)
        .tool("probe", |args, _| Ok(args[0].clone()))
        .build()
        .unwrap();
    let source = "
pairs = for a in [1, 2, 3, 4, 5, 6, 7, 8] {
    inner = for b in [1, 2, 3, 4, 5, 6, 7, 8] {
        deepest = for c in [1, 2] {
            return probe(c)
        }
        return (fold s = 0 for d in deepest { return s + d }) + b
    }
    return (fold s = 0 for i in inner { return s + i }) + a
}
return fold s = 0 for p in pairs { return s + p }
";
    let program = runtime.compile(source).unwrap_or_else(|d| panic!("{d:#?}"));
    let execution = runtime.run(&program).unwrap();
    // sum over a of (sum over b of (3 + b)) + a = sum over a of (24 + 36 + a) = 8*60 + 36
    assert_eq!(execution.value, CanonicalValue::Integer(516));
}

#[test]
fn parser_error_recovery_always_makes_progress() {
    // Regression: a parse error inside a block used as a call argument left
    // recovery stuck on the trailing `}`, looping forever and accumulating
    // gigabytes of diagnostics. Recovery must terminate quickly with a
    // bounded diagnostic list, and prefix `if` gets a targeted hint.
    let source = "x = list.flatten(for p in [1] { return if p then 1 else 2 })\nreturn x";
    let diagnostics = parse(source).unwrap_err();
    assert!(
        diagnostics.len() < 32,
        "recovery must be bounded, got {} diagnostics",
        diagnostics.len()
    );
}

#[test]
fn bindings_used_inside_nested_block_statements_are_reachable() {
    // Regression: reachability walked only block results, not block-local
    // statements, so an outer binding referenced from a statement inside a
    // loop body was falsely rejected as an unreachable effect (RL1204).
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor(
            "list_companies",
            vec![object(vec![])],
            object(vec![("items", Schema::list(Schema::Any))]),
        ))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("list_companies", |_, _| {
            Ok(CanonicalValue::Object(BTreeMap::from([(
                "items".into(),
                CanonicalValue::List(vec![CanonicalValue::Integer(7)]),
            )])))
        })
        .build()
        .unwrap();
    let source = "
companies = list_companies({})
mapped = for x in [1, 2] {
    inner = for c in companies.items {
        return c
    }
    return inner
}
return mapped
";
    let program = runtime
        .compile(source)
        .unwrap_or_else(|d| panic!("nested use must be reachable: {d:#?}"));
    runtime.run(&program).unwrap();
}

#[test]
fn fold_reduces_sequentially_and_skip_leaves_accumulator_unchanged() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let source = "total = fold acc = 0 for x in [1, 2, 3, 4, 5] {\n  \
                  skip if x == 3\n  return acc + x\n}\nreturn total";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::Integer(12));
}

#[test]
fn fold_over_empty_collection_yields_the_initial_value() {
    let runtime = Runtime::builder().build().unwrap();
    let source = "empty = []\nreturn fold acc = 42 for x in empty { return acc + 1 }";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::Integer(42));
}

#[test]
fn fold_chains_dependent_tool_calls_in_order() {
    // The cursor-pagination shape: each call depends on the previous
    // result, which only fold can express. Calls must run in order and
    // no RL1206 warning fires because the call references the accumulator.
    let mut registry = ToolRegistry::new();
    registry
        .register(effect_descriptor(
            "queue.pop",
            vec![Schema::INTEGER],
            Schema::INTEGER,
        ))
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("queue.pop", {
            let seen = seen.clone();
            move |args, _| {
                let CanonicalValue::Integer(cursor) = args[0] else {
                    panic!()
                };
                seen.lock().unwrap().push(cursor);
                Ok(CanonicalValue::Integer(cursor + 10))
            }
        })
        .build()
        .unwrap();
    let source = "last = fold cursor = 0 for step in [1, 2, 3] {\n  \
                  return queue.pop(cursor)\n}\nreturn last";
    let program = runtime.compile(source).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.code == "RL1206"),
        "accumulator-dependent calls are legitimately sequential: {:?}",
        program.diagnostics
    );
    let execution = runtime.run(&program).unwrap();
    assert_eq!(execution.value, CanonicalValue::Integer(30));
    assert_eq!(*seen.lock().unwrap(), vec![0, 10, 20]);
}

#[test]
fn fold_warns_when_effects_do_not_use_the_accumulator() {
    let mut registry = ToolRegistry::new();
    registry
        .register(effect_descriptor(
            "notify.send",
            vec![Schema::INTEGER],
            Schema::Boolean,
        ))
        .unwrap();
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("notify.send", |_, _| Ok(CanonicalValue::Boolean(true)))
        .build()
        .unwrap();
    let source = "n = fold acc = 0 for x in [1, 2] {\n  \
                  sent = notify.send(x)\n  return acc + 1\n}\nreturn n";
    let program = runtime.compile(source).unwrap();
    assert!(
        program.diagnostics.iter().any(|d| d.code == "RL1206"),
        "independent effects in a fold should suggest `for`: {:?}",
        program.diagnostics
    );
}

#[test]
fn fold_accumulator_must_keep_one_schema() {
    let runtime = Runtime::builder().build().unwrap();
    let diagnostics = runtime
        .compile("x = fold acc = 0 for v in [1, 2] { return \"nope\" }\nreturn x")
        .expect_err("accumulator schema change must be rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "RL2313"),
        "{diagnostics:#?}"
    );
}

#[test]
fn skip_filters_for_loop_elements_in_order() {
    let runtime = Runtime::builder().build().unwrap();
    let source = "evens = for x in [1, 2, 3, 4, 5, 6] {\n  \
                  skip if x % 2 == 1\n  return x\n}\nreturn evens";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value,
        CanonicalValue::List(vec![
            CanonicalValue::Integer(2),
            CanonicalValue::Integer(4),
            CanonicalValue::Integer(6),
        ])
    );
}

#[test]
fn skip_is_rejected_outside_loops_and_across_boundaries() {
    let runtime = Runtime::builder().build().unwrap();
    let top = runtime
        .compile("skip\nreturn 1")
        .expect_err("top-level skip must be rejected");
    assert!(top.iter().any(|d| d.code == "RL1018"), "{top:#?}");
    let crossing = runtime
        .compile(
            "x = for v in [1] {\n  y = boundary {\n    skip\n    return 1\n  } \
             catch err {\n    return 2\n  }\n  return y\n}\nreturn x",
        )
        .expect_err("skip inside a boundary must be rejected");
    assert!(crossing.iter().any(|d| d.code == "RL1018"), "{crossing:#?}");
}

#[test]
fn assertions_force_checks_without_returning_intermediate_values() {
    let mut registry = ToolRegistry::new();
    registry
        .register(descriptor("count_items", vec![], Schema::INTEGER))
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("count_items", {
            let calls = calls.clone();
            move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(CanonicalValue::Integer(3))
            }
        })
        .build()
        .unwrap();

    let source =
        "count = count_items()\nassert(count == 3, \"unexpected item count\")\nreturn { ok: true }";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        execution.value,
        CanonicalValue::Object(BTreeMap::from([(
            "ok".into(),
            CanonicalValue::Boolean(true)
        )]))
    );

    let failing = "assert(false, \"items are missing\")\nreturn true";
    let error = runtime
        .run(&runtime.compile(failing).unwrap())
        .expect_err("false assertion must fail");
    assert_eq!(error.code, "ASSERTION_FAILED");
    assert_eq!(error.message, "items are missing");
    let span = error.span.expect("assertion failure carries its span");
    assert_eq!(
        &failing[span.start..span.end],
        "assert(false, \"items are missing\")"
    );
}

#[test]
fn assertion_inputs_are_checked_statically() {
    let runtime = Runtime::builder().build().unwrap();
    let diagnostics = runtime
        .compile("assert(1, false)\nreturn true")
        .expect_err("assertion inputs must have the right schemas");
    assert!(diagnostics.iter().any(|d| d.code == "RL2305"));
    assert!(diagnostics.iter().any(|d| d.code == "RL2315"));
}

#[test]
fn fail_raises_a_catchable_error_and_types_as_never() {
    let runtime = Runtime::builder().build().unwrap();
    // Expression position: the else arm fails, so the result schema is the
    // then arm's. Boundary catches and reads code/message/details.
    let source = "outcome = boundary {\n  items = []\n  first = items[0] if items != [] \
                  else fail(\"EMPTY\", \"expected matches\", { hint: \"check filters\" })\n  \
                  return first\n} catch err {\n  return { code: err.code, hint: err.hint }\n}\n\
                  return outcome";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    let CanonicalValue::Object(o) = execution.value else {
        panic!()
    };
    assert_eq!(o["code"], CanonicalValue::String("EMPTY".into()));
    assert_eq!(o["hint"], CanonicalValue::String("check filters".into()));
}

#[test]
fn fail_guard_statements_always_run_and_carry_spans() {
    let runtime = Runtime::builder().build().unwrap();
    let source = "guard = fail(\"NO_MATCH\", \"nothing matched\") if 2 > 1\nreturn true";
    let program = runtime
        .compile(source)
        .expect("fail guards must compile even when unused");
    let error = runtime.run(&program).expect_err("guard must fire");
    assert_eq!(error.code, "NO_MATCH");
    assert!(error.span.is_some(), "fail carries the failing span");
}

#[test]
fn computed_keys_auto_stringify_scalars_and_last_entry_wins() {
    let runtime = Runtime::builder().build().unwrap();
    let source = "key = \"a\"\nreturn { [key]: 1, [\"a\"]: 2, [40 + 2]: true, fixed: \"x\" }";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value.presentation_json().unwrap(),
        "{\"42\":true,\"a\":2,\"fixed\":\"x\"}"
    );
}

#[test]
fn object_merge_operator_is_shallow_and_right_biased() {
    let runtime = Runtime::builder().build().unwrap();
    let source =
        "base = { a: 1, b: 2 }\nmerged = base + { b: 3, c: 4 }\nreturn merged.b + merged.c";
    // `merged.b`/`merged.c` also prove the analyzer computes the merged
    // object schema (a projection on an unknown property is RL2103).
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::Integer(7));
}

#[test]
fn fold_accumulates_keyed_maps_from_an_empty_object_seed() {
    // The group-by idiom: `{}` widens to the map the body merges into, so
    // RL2313 must not fire, and the missing-key branch never evaluates.
    let runtime = Runtime::builder().build().unwrap();
    let source = "
people = [
    { name: \"ada\", team: \"eng\" },
    { name: \"bo\", team: \"ops\" },
    { name: \"cy\", team: \"eng\" }
]
groups = fold acc = {} for c in people {
    return acc + { [c.team]: (acc[c.team] if c.team in acc else []) + [c.name] }
}
return groups
";
    let program = runtime
        .compile(source)
        .unwrap_or_else(|d| panic!("keyed accumulation must compile: {d:#?}"));
    let execution = runtime.run(&program).unwrap();
    assert_eq!(
        execution.value.presentation_json().unwrap(),
        "{\"eng\":[\"ada\",\"cy\"],\"ops\":[\"bo\"]}"
    );
}

#[test]
fn fold_indexes_by_computed_key_with_precise_map_schema() {
    // The join-by-id idiom; integer ids stringify, and the resulting map
    // schema stays precise enough to project `.name` on an indexed value.
    let runtime = Runtime::builder().build().unwrap();
    let source = "
users = [{ id: 1, name: \"ada\" }, { id: 2, name: \"bo\" }]
by_id = fold acc = {} for u in users { return acc + { [u.id]: u } }
return by_id[\"2\"].name
";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::String("bo".into()));
}

#[test]
fn computed_keys_reject_lists_and_objects() {
    let runtime = Runtime::builder().build().unwrap();
    let diagnostics = runtime
        .compile("return { [[1]]: 1 }")
        .expect_err("a list key must be rejected");
    assert!(
        diagnostics.iter().any(|d| d.code == "RL2315"),
        "{diagnostics:#?}"
    );

    // Statically-unknown keys defer to the runtime, which names the kind
    // and carries the key expression's span.
    let runtime = Runtime::builder()
        .input("k", Schema::Any, CanonicalValue::List(vec![]))
        .build()
        .unwrap();
    let program = runtime.compile("return { [k]: 1 }").unwrap();
    let error = runtime
        .run(&program)
        .expect_err("list key fails at runtime");
    assert_eq!(error.code, "RL5209");
    assert!(error.span.is_some(), "key errors carry the key span");
}

#[test]
fn stdlib_text_and_number_namespaces_cover_the_tier_one_surface() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let source = r#"
return {
    trimmed: text.trim("  hi  "),
    sliced: text.slice("composable", 0, 7),
    tail: text.slice("composable", -4),
    prefix: text.starts_with("runlet", "run"),
    suffix: text.ends_with("runlet", "let"),
    n: number.parse("42"),
    f: number.parse("2.5"),
    len: list.length([1, 2, 3])
}
"#;
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value.presentation_json().unwrap(),
        "{\"f\":2.5,\"len\":3,\"n\":42,\"prefix\":true,\"sliced\":\"composa\",\"suffix\":true,\"tail\":\"able\",\"trimmed\":\"hi\"}"
    );
}

#[test]
fn stdlib_regex_namespace_matches_extracts_and_rewrites() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let source = r#"
phone = "+1-415-555-0103"
digits = regex.replace(phone, "[^0-9]", "")
m = regex.captures("order AB-123", "(?<team>[A-Z]+)-([0-9]+)")
return {
    valid: regex.test("+14155550103", "^\\+1[0-9]{10}$"),
    dashed: regex.test(phone, "^\\+1[0-9]{10}$"),
    e164: "+" + digits,
    words: regex.find_all("a1 b2 c3", "[a-z][0-9]"),
    parts: regex.split("a, b,c", ",\\s*"),
    team: m.names["team"] if m != null,
    number: m.groups[1] if m != null
}
"#;
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value.presentation_json().unwrap(),
        "{\"dashed\":false,\"e164\":\"+14155550103\",\"number\":\"123\",\"parts\":[\"a\",\"b\",\"c\"],\"team\":\"AB\",\"valid\":true,\"words\":[\"a1\",\"b2\",\"c3\"]}"
    );
}

#[test]
fn stdlib_invalid_literal_regex_is_a_compile_error_with_lookaround_hint() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let diagnostics = runtime
        .compile("return regex.test(\"x\", \"(?=abc)\")")
        .expect_err("lookahead must be rejected before execution");
    let diagnostic = diagnostics
        .iter()
        .find(|d| d.code == "RL2316")
        .unwrap_or_else(|| panic!("{diagnostics:#?}"));
    assert!(
        diagnostic.message.contains("lookaround is not supported"),
        "{diagnostic:#?}"
    );
}

#[test]
fn stdlib_list_namespace_sorts_slices_and_ranges() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let source = r#"
orders = [
    { id: "o1", customer: { tier: 2 } },
    { id: "o2", customer: { tier: 1 } },
    { id: "o3" }
]
ranked = list.sort_by(orders, "customer.tier")
top = list.sort_by(orders, "customer.tier", "desc")
return {
    sorted: list.sort([3, 1, 2]),
    names: list.sort(["b", "a"]),
    first: ranked[0].id,
    missing_last: ranked[2].id,
    top: top[0].id,
    window: list.slice([1, 2, 3, 4, 5], 1, -1),
    pages: list.range(1, 4),
    stepped: list.range(540, 961, 30),
    down: list.range(3, 0, -1)
}
"#;
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value.presentation_json().unwrap(),
        "{\"down\":[3,2,1],\"first\":\"o2\",\"missing_last\":\"o3\",\"names\":[\"a\",\"b\"],\"pages\":[1,2,3],\"sorted\":[1,2,3],\"stepped\":[540,570,600,630,660,690,720,750,780,810,840,870,900,930,960],\"top\":\"o1\",\"window\":[2,3,4]}"
    );
    let mixed = runtime
        .compile("return list.sort([1, \"a\"])")
        .map(|p| runtime.run(&p))
        .expect("mixed kinds defer to runtime")
        .expect_err("mixed kinds must fail");
    assert_eq!(mixed.code, "RL5201");
    let zero_step = runtime
        .compile("return list.range(0, 10, 0)")
        .map(|p| runtime.run(&p))
        .expect("zero step defers to runtime")
        .expect_err("zero step must fail");
    assert_eq!(zero_step.code, "RL5214");
}

#[test]
fn stdlib_json_and_time_round_trip() {
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let source = r#"
parsed = json.parse("{\"a\": [1, 2.5, null], \"b\": \"x\"}")
t = time.parse("2026-07-12T09:30:00.250+02:00")
day = time.parse("2026-07-12")
return {
    a1: parsed.a[1],
    b: parsed.b,
    encoded: json.encode({ z: true }),
    t: t,
    t_text: time.format(t),
    day_text: time.format(day)
}
"#;
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value.presentation_json().unwrap(),
        "{\"a1\":2.5,\"b\":\"x\",\"day_text\":\"2026-07-12T00:00:00Z\",\"encoded\":\"{\\\"z\\\":true}\",\"t\":1783841400250,\"t_text\":\"2026-07-12T07:30:00.250Z\"}"
    );
    let bad = runtime
        .compile("return time.parse(\"tomorrow\")")
        .map(|p| runtime.run(&p))
        .expect("dynamic-looking input defers to runtime")
        .expect_err("non-timestamp must fail");
    assert_eq!(bad.code, "RL5213");
}

#[test]
fn newlines_terminate_statements_inside_parenthesized_blocks() {
    // Regression: parentheses suppress newline tokens (implicit line
    // joining), but a block nested inside `( ... )` still needs newlines as
    // statement terminators — `{` must reset the suppression.
    let runtime = Runtime::builder().build().unwrap();
    let source = "
flag = true
out = 0 if flag else (
    fold acc = 0 for x in [1, 2] {
        doubled = x * 2
        return acc + doubled
    }
)
return out
";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::Integer(0));
}

#[test]
fn fold_null_seed_widens_to_a_find_first_result() {
    // The find-first idiom: a `null` seed adopts `Item | Null`, so the
    // caller can guard with `!= null` and project the found object.
    let runtime = Runtime::builder().build().unwrap();
    let source = "
slots = [{ day: \"mon\", free: false }, { day: \"tue\", free: true }]
found = fold hit = null for s in slots {
    return s if s.free and hit == null else hit
}
return found.day if found != null else \"none\"
";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::String("tue".into()));

    // Formatting conversions must NOT widen: an integer seed with a string
    // body is still a schema change.
    let diagnostics = runtime
        .compile("x = fold acc = 0 for v in [1] { return \"nope\" }\nreturn x")
        .expect_err("formatting widening must stay rejected");
    assert!(diagnostics.iter().any(|d| d.code == "RL2313"));
}

#[test]
fn statement_form_if_gets_one_construct_level_diagnostic() {
    let runtime = Runtime::builder().build().unwrap();
    // Statement position: the whole construct (else-if chain included)
    // is consumed; its body must not re-parse as loose statements.
    let source = "
flag = 2 > 1
if flag {
    a = 1
    b = a + 1
} else if flag {
    c = 3
} else {
    d = 4
}
result = 5
return result
";
    let diagnostics = runtime
        .compile(source)
        .expect_err("statement-form if is rejected");
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "one construct, one diagnostic: {errors:#?}"
    );
    assert_eq!(errors[0].code, "RL1014");

    // Expression position with return-less branches (the JS habit): one
    // fixable missing-return diagnostic per block, no cascade.
    let source = "x = if 2 > 1 { 1 } else { 2 }\ny = 3\nreturn y";
    let diagnostics = runtime
        .compile(source)
        .expect_err("blocks still end with return");
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert_eq!(errors.len(), 2, "one diagnostic per block: {errors:#?}");
    assert!(errors.iter().all(|d| d.code == "RL1017"), "{errors:#?}");
    assert!(
        errors.iter().all(|d| !d.fixes.is_empty()),
        "missing returns carry machine fixes: {errors:#?}"
    );
}

#[test]
fn block_if_is_a_lazy_expression_with_optional_else() {
    let runtime = Runtime::builder().build().unwrap();
    let source = "
score = 62
grade = if score >= 90 {
    label = \"gold\"
    return label
} else if score >= 60 {
    return \"silver\"
} else {
    return \"bronze\"
}
missing = if score > 100 { return \"impossible\" }
return { grade: grade, missing: missing }
";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(
        execution.value.presentation_json().unwrap(),
        "{\"grade\":\"silver\",\"missing\":null}"
    );
}

#[test]
fn block_if_gates_effects_to_the_selected_branch() {
    let mut registry = ToolRegistry::new();
    registry
        .register(effect_descriptor(
            "audit.log",
            vec![Schema::string()],
            Schema::Boolean,
        ))
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::builder()
        .registry(registry)
        .tool("audit.log", {
            let seen = seen.clone();
            move |args, _| {
                let CanonicalValue::String(tag) = &args[0] else {
                    unreachable!()
                };
                seen.lock().unwrap().push(tag.clone());
                Ok(CanonicalValue::Boolean(true))
            }
        })
        .build()
        .unwrap();
    // The if binding is never referenced: it is still an effect root, and
    // only the selected branch's effects dispatch.
    let source = "
flag = 1 > 2
logged = if flag {
    a = audit.log(\"then\")
    return true
} else {
    b = audit.log(\"else\")
    return false
}
return 1
";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::Integer(1));
    assert_eq!(*seen.lock().unwrap(), vec!["else".to_string()]);
}

#[test]
fn fold_state_object_seeds_widen_null_fields() {
    // Models simulate mutable state with an object accumulator whose fields
    // start null: the field widens to `T | null` and body projections on it
    // type-check (bench v15 calendar shape).
    let runtime = Runtime::builder().build().unwrap();
    let source = "
slots = [{ day: \"mon\", score: 3 }, { day: \"tue\", score: 5 }, { day: \"wed\", score: 4 }]
best = fold state = { found: null, count: 0 } for s in slots {
    better = state.found == null or s.score > state.found.score
    return {
        found: s if better else state.found,
        count: state.count + 1
    }
}
return { day: best.found.day if best.found != null else \"none\", seen: best.count }
";
    let program = runtime
        .compile(source)
        .unwrap_or_else(|d| panic!("state-object folds must compile: {d:#?}"));
    let execution = runtime.run(&program).unwrap();
    assert_eq!(
        execution.value.presentation_json().unwrap(),
        "{\"day\":\"tue\",\"seen\":3}"
    );

    // Last-wins over a null seed (body never references the accumulator).
    let source = "
last = fold acc = null for x in [{ n: 1 }, { n: 2 }] { return x }
return last.n if last != null else 0
";
    let execution = runtime.run(&runtime.compile(source).unwrap()).unwrap();
    assert_eq!(execution.value, CanonicalValue::Integer(2));
}

#[test]
fn runtime_errors_inside_loop_iterations_keep_their_spans() {
    // Bench v15 showed bare `RL5203: NOT_INDEXABLE` errors with no source
    // span; failures inside concurrent loop iterations must carry the
    // failing expression's span like top-level failures do.
    let runtime = Runtime::builder()
        .input(
            "input",
            Schema::Any,
            CanonicalValue::Object(BTreeMap::from([(
                "xs".to_string(),
                CanonicalValue::List(vec![CanonicalValue::Null]),
            )])),
        )
        .build()
        .unwrap();
    let source = "out = for x in input.xs {\n    return x[0]\n}\nreturn out";
    let program = runtime.compile(source).unwrap();
    let error = runtime.run(&program).expect_err("indexing null fails");
    assert_eq!(error.code, "RL5203");
    let span = error.span.expect("loop-iteration errors carry spans");
    assert_eq!(&source[span.start..span.end], "x[0]");
}
