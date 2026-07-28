//! Mechanical source repair for the parse errors models actually make.
//!
//! `heal` is a pre-pass, not an execution feature: it applies only safe,
//! insertion-only rewrites (never deleting or reordering user code), re-parses,
//! and succeeds only when the result parses cleanly. Hosts run it when a
//! submitted program is rejected, execute the healed source, and surface the
//! notes as warnings — the model gets its result *and* learns the correction
//! without paying a retry round-trip.
//!
//! Healable today:
//! - a block whose final statement is a bare expression (`{ x }`) or that
//!   has no result at all — `return` / `return null` inserted (`RL1017`);
//! - a bare expression statement (`update(...)` fire-and-forget) — bound to
//!   a fresh name (`RL1019`);
//! - a statement-form control structure (`if cond { ... }` at statement
//!   level) — bound to a fresh name, turning it into the expression form
//!   (`RL1014`); missing branch returns then heal on the next pass;
//! - a missing statement separator — newline inserted (`RL1008`).

use crate::Diagnostic;
use crate::parser::parse;

/// Source produced by a successful mechanical repair pass.
pub struct Healed {
    /// The repaired source; parses cleanly (later phases may still reject it).
    pub source: String,
    /// One human/model-readable note per applied repair, in source order.
    pub notes: Vec<String>,
}

/// Attempts to repair `source` so it parses. Returns `None` when the source
/// already parses, when no rule applies, or when healing does not converge.
pub fn heal(source: &str) -> Option<Healed> {
    let mut current = source.to_string();
    let mut notes = Vec::new();
    if parse(&current).is_ok() {
        return None;
    }
    for _pass in 0..6 {
        let diagnostics = match parse(&current) {
            Ok(_) => {
                return (!notes.is_empty()).then_some(Healed {
                    source: current,
                    notes,
                });
            }
            Err(diagnostics) => diagnostics,
        };
        let mut edits = plan(&current, &diagnostics, &mut notes);
        if edits.is_empty() {
            return None;
        }
        // Apply back-to-front so earlier offsets stay valid.
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.0));
        edits.dedup_by_key(|e| e.0);
        for (offset, text) in edits {
            if current.is_char_boundary(offset) {
                current.insert_str(offset, &text);
            }
        }
    }
    None
}

/// Plans insertion edits for one pass and records their notes.
fn plan(source: &str, diagnostics: &[Diagnostic], notes: &mut Vec<String>) -> Vec<(usize, String)> {
    let mut edits: Vec<(usize, String)> = Vec::new();
    let mut names = 0usize;
    let mut fresh = || loop {
        names += 1;
        let name = format!("fixed_{names}");
        if !source.contains(&name) {
            return name;
        }
    };
    for d in diagnostics {
        let at = line_of(source, d.primary_span.start);
        match d.code.as_str() {
            "RL1017" => {
                let Some(fix) = d.fixes.first() else { continue };
                if fix.span.start == fix.span.end && !fix.replacement.is_empty() {
                    edits.push((fix.span.start, fix.replacement.clone()));
                    notes.push(format!(
                        "line {at}: a block must end with `return`; inserted `{}`",
                        fix.replacement.trim()
                    ));
                }
            }
            "RL1019" => {
                let name = fresh();
                edits.push((d.primary_span.start, format!("{name} = ")));
                notes.push(format!(
                    "line {at}: statements are bindings; bound the bare expression to `{name}`"
                ));
            }
            "RL1014" => {
                let name = fresh();
                edits.push((d.primary_span.start, format!("{name} = ")));
                notes.push(format!(
                    "line {at}: control structures are expressions; bound the construct to \
                     `{name}` — write `x = if condition {{ ... }}` directly next time"
                ));
            }
            "RL1008" if d.message.contains("expected a newline") => {
                edits.push((d.primary_span.start, "\n".to_string()));
                notes.push(format!("line {at}: inserted a missing statement separator"));
            }
            _ => {}
        }
    }
    edits
}

fn line_of(source: &str, offset: usize) -> usize {
    source
        .get(..offset)
        .map(|s| s.matches('\n').count() + 1)
        .unwrap_or(1)
}
