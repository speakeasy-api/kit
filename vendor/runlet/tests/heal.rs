//! The auto-healing pre-pass: models' most common invalid-but-unambiguous
//! programs repair mechanically, run, and report what was fixed.

use runlet::{CanonicalValue, Runtime, heal};

#[test]
fn statement_form_if_with_returnless_branches_heals_and_runs() {
    // The exact shape models write from Python/JS muscle memory: a
    // statement-form if whose branches never return.
    let source = "
flag = 2 > 1
if flag {
    a = 1
} else {
    b = 2
}
return flag
";
    let healed = heal(source).expect("statement-form if is healable");
    assert!(
        healed
            .notes
            .iter()
            .any(|n| n.contains("control structures")),
        "{:?}",
        healed.notes
    );
    let runtime = Runtime::builder().build().unwrap();
    let program = runtime
        .compile(&healed.source)
        .unwrap_or_else(|d| panic!("healed source must compile: {d:#?}\n{}", healed.source));
    let execution = runtime.run(&program).unwrap();
    assert_eq!(execution.value, CanonicalValue::Boolean(true));
}

#[test]
fn bare_final_expression_and_bare_statement_heal() {
    // `{ expr }` blocks (Rust habit) and fire-and-forget bare calls.
    let source = "
doubled = for x in [1, 2] {
    y = x * 2
    y
}
return doubled
";
    let healed = heal(source).expect("missing return is healable");
    assert!(
        healed.notes.iter().any(|n| n.contains("return")),
        "{:?}",
        healed.notes
    );
    let runtime = Runtime::builder().build().unwrap();
    let execution = runtime
        .run(&runtime.compile(&healed.source).unwrap())
        .unwrap();
    assert_eq!(
        execution.value,
        CanonicalValue::List(vec![CanonicalValue::Integer(2), CanonicalValue::Integer(4)])
    );
}

#[test]
fn valid_and_unhealable_sources_return_none() {
    assert!(
        heal("x = 1\nreturn x").is_none(),
        "valid source: no healing"
    );
    assert!(
        heal("x = ((((\nreturn x").is_none(),
        "garbage stays unhealable"
    );
}

#[test]
fn v14_crm_transcript_shape_heals_to_the_semantic_errors() {
    // Distilled from a real bench transcript: block-if with Rust-style
    // final expressions (no `return`) in several branches, plus a genuine
    // semantic error (rebinding). Heal must fix all the syntax so the
    // remaining diagnostics are the real ones.
    let source = r#"
digits = "4155550100"
final_digits = if text.length(digits) == 11 and text.starts_with(digits, "1") {
    text.slice(digits, 1)
} else if text.length(digits) == 10 {
    digits
} else {
    fail("INVALID", "cannot parse")
}
final_digits = "+1" + final_digits
return final_digits
"#;
    let healed = heal(source).expect("missing returns must heal");
    let runtime = Runtime::builder().with_prelude().build().unwrap();
    let diagnostics = runtime
        .compile(&healed.source)
        .expect_err("the rebinding is a real error that must survive healing");
    assert!(
        diagnostics.iter().all(|d| d.code != "RL1017"),
        "syntax errors must be gone: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == "RL2106" && d.message.contains("immutable")),
        "the rebinding surfaces with the immutability hint: {diagnostics:#?}"
    );
}
