//! File-backed config migration — the only scenario with a true Bash arm,
//! anchoring the comparison the thesis is about: granular fs tools vs the
//! same tools under compose vs a raw shell pipeline.
//!
//! Task: across a scratch directory of `.cfg` files, rename the standalone
//! `timeout_ms` key to `request_timeout_ms`. `connect_timeout_ms` appears in
//! some files as a trap for careless substring replacement.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agentkit_core::MetadataMap;
use agentkit_tools_core::{
    CommandPolicy, CompositePermissionChecker, PathPolicy, PermissionCode, PermissionDecision,
    PermissionDenial,
};
use serde_json::{Value, json};

use crate::scenario::{Arm, BenchError, Scenario, ScenarioInstance, Score, submit_result_tool};

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

fn file_body(i: u64) -> String {
    let mut body = format!(
        "service: app{i:02}\nendpoint: https://svc-{i:02}.internal\nretries: {}\n",
        i % 5
    );
    if !i.is_multiple_of(6) {
        // 10 of 12 files carry the key to migrate.
        body.push_str(&format!("timeout_ms: {}\n", 1000 + i * 250));
        if i.is_multiple_of(3) {
            body.push_str(&format!(
                "fallback_timeout_ms_note: keep\nidle:\n  timeout_ms: {}\n",
                500 + i * 100
            ));
        }
    }
    if i.is_multiple_of(4) {
        // The trap: must NOT be renamed.
        body.push_str("connect_timeout_ms: 300\n");
    }
    body.push_str("log_level: info\n");
    body
}

/// Ground-truth transform: rename only keys whose trimmed line starts with
/// `timeout_ms:` (top-level or nested), preserving indentation and values.
fn expected_body(original: &str) -> String {
    original
        .lines()
        .map(|line| {
            let indent_len = line.len() - line.trim_start().len();
            let (indent, rest) = line.split_at(indent_len);
            if let Some(value) = rest.strip_prefix("timeout_ms:") {
                format!("{indent}request_timeout_ms:{value}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect()
}

fn migration_stats(files: &[(String, String)]) -> (u64, u64) {
    let mut files_changed = 0u64;
    let mut replacements = 0u64;
    for (_, body) in files {
        let count = body
            .lines()
            .filter(|line| line.trim_start().starts_with("timeout_ms:"))
            .count() as u64;
        if count > 0 {
            files_changed += 1;
            replacements += count;
        }
    }
    (files_changed, replacements)
}

pub struct ConfigMigration;

impl Scenario for ConfigMigration {
    fn name(&self) -> &'static str {
        "config-migration"
    }

    fn arms(&self) -> Vec<Arm> {
        vec![Arm::Granular, Arm::Compose, Arm::Bash]
    }

    fn setup(&self, arm: Arm) -> Result<ScenarioInstance, BenchError> {
        let run = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let scratch: PathBuf = std::env::current_dir()?
            .join("target")
            .join("compose-bench-scratch")
            .join(format!("run-{}-{run}", std::process::id()));
        if scratch.exists() {
            std::fs::remove_dir_all(&scratch)?;
        }
        std::fs::create_dir_all(&scratch)?;

        let files: Vec<(String, String)> = (1..=12u64)
            .map(|i| (format!("app{i:02}.cfg"), file_body(i)))
            .collect();
        for (name, body) in &files {
            std::fs::write(scratch.join(name), body)?;
        }
        let scratch = scratch.canonicalize()?;

        let (submit, submission) = submit_result_tool(json!({
            "type": "object",
            "properties": {
                "files_changed": { "type": "integer" },
                "replacements": { "type": "integer" }
            },
            "required": ["files_changed", "replacements"]
        }));

        let tools = match arm {
            Arm::Bash => agentkit_tool_shell::registry().with(submit),
            Arm::Granular | Arm::Compose | Arm::RunletCompose => {
                agentkit_tool_fs::registry().with(submit)
            }
        };

        let permissions =
            CompositePermissionChecker::new(PermissionDecision::Deny(PermissionDenial {
                code: PermissionCode::UnknownRequest,
                message: "tool request is not covered by any benchmark policy".into(),
                metadata: MetadataMap::new(),
            }))
            .with_policy(
                PathPolicy::new()
                    .allow_root(scratch.clone())
                    .require_approval_outside_allowed(true),
            )
            .with_policy(
                CommandPolicy::new()
                    .allow_cwd(scratch.clone())
                    .require_approval_for_unknown(true),
            );

        let user_prompt = format!(
            "In the directory `{scratch}` there are 12 `.cfg` files. Migrate them: every config \
             key named exactly `timeout_ms` (at any indentation level) must be renamed to \
             `request_timeout_ms`, keeping its value and indentation unchanged. Keys that merely \
             contain the substring, such as `connect_timeout_ms`, must NOT be touched, and no \
             other content may change. When done, submit \
             {{\"files_changed\": <files that needed at least one rename>, \
             \"replacements\": <total keys renamed>}} via submit_result.",
            scratch = scratch.display()
        );

        let scorer_submission = submission.clone();
        let scorer_scratch = scratch.clone();
        let scorer_files = files;
        let scorer = Box::new(move || {
            let (expected_files_changed, expected_replacements) = migration_stats(&scorer_files);
            let mut correct_files = 0usize;
            let mut notes = Vec::new();
            for (name, original) in &scorer_files {
                let expected = expected_body(original);
                match std::fs::read_to_string(scorer_scratch.join(name)) {
                    Ok(actual) if actual == expected => correct_files += 1,
                    Ok(_) if notes.len() < 5 => {
                        notes.push(format!("{name}: content differs from expected migration"));
                    }
                    Ok(_) => {}
                    Err(error) => notes.push(format!("{name}: unreadable ({error})")),
                }
            }
            let state_score = correct_files as f64 / scorer_files.len() as f64;

            let submitted = scorer_submission.lock().expect("submission lock").clone();
            let count = |key: &str| -> Option<u64> {
                submitted
                    .as_ref()
                    .and_then(|v| v.get(key))
                    .and_then(Value::as_u64)
            };
            let counts_score = (u64::from(count("files_changed") == Some(expected_files_changed))
                + u64::from(count("replacements") == Some(expected_replacements)))
                as f64
                / 2.0;

            notes.insert(
                0,
                format!(
                    "state: {correct_files}/{} files correct; expected files_changed={expected_files_changed} replacements={expected_replacements}; submitted {submitted:?}",
                    scorer_files.len()
                ),
            );
            Score {
                accuracy: 0.7 * state_score + 0.3 * counts_score,
                notes,
            }
        });

        Ok(ScenarioInstance {
            tools,
            user_prompt,
            permissions: Some(permissions),
            submission,
            scorer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_contains_renames_nested_keys_and_traps() {
        let files: Vec<(String, String)> = (1..=12u64)
            .map(|i| (format!("app{i:02}.cfg"), file_body(i)))
            .collect();
        let (files_changed, replacements) = migration_stats(&files);
        assert_eq!(files_changed, 10);
        assert!(
            replacements > files_changed,
            "nested keys add extra renames"
        );
        // Trap files keep connect_timeout_ms verbatim after the expected transform.
        let trapped = files
            .iter()
            .filter(|(_, body)| body.contains("connect_timeout_ms"))
            .count();
        assert!(trapped >= 3);
        for (_, body) in &files {
            let migrated = expected_body(body);
            assert_eq!(
                body.matches("connect_timeout_ms").count(),
                migrated.matches("connect_timeout_ms").count()
            );
            assert!(!migrated.contains("\ntimeout_ms:"));
        }
    }
}
