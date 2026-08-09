use std::{fs, path::PathBuf, process::Command};

use serde_json::Value;

use kit::{
    api::service::PromptInput,
    cli::{
        auth::AuthCommand,
        core::{ClientRequest, Invocation, parse},
    },
};

const PROJECT: &str = "project_00000000000000000000000001";
const THREAD: &str = "thread_00000000000000000000000001";
const RUN: &str = "run_00000000000000000000000001";
const APPROVAL: &str = "approval_00000000000000000000000001";
const ARTIFACT: &str = "artifact_00000000000000000000000001";
const PROCESS: &str = "process_00000000000000000000000001";
const TERMINAL: &str = "terminal_00000000000000000000000001";
const ATTACHMENT: &str = "attachment_00000000000000000000000000000001";
const REFERENCE: &str = "blake3:0000000000000000000000000000000000000000000000000000000000000000";

fn kit() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kit"))
}

fn unused_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kit-cli-{label}-{}", std::process::id()))
}

#[test]
fn auth_logout_local_only_is_explicitly_parsed() {
    let cli = parse(["auth", "logout", "openai", "--local-only"]).unwrap();
    assert!(matches!(
        cli.invocation,
        Invocation::Auth(AuthCommand::Logout { local_only: true })
    ));
}

#[test]
fn top_group_and_leaf_help_are_structured() {
    let top = kit().arg("--help").output().unwrap();
    assert!(top.status.success());
    let top = String::from_utf8(top.stdout).unwrap();
    assert!(top.contains("Usage: kit [OPTIONS] <COMMAND>"));
    assert!(top.contains("Examples:"));

    let group = kit().args(["repo", "--help"]).output().unwrap();
    assert!(group.status.success());
    let group = String::from_utf8(group.stdout).unwrap();
    assert!(group.contains("Usage: kit repo [OPTIONS] <COMMAND>"));
    assert!(group.contains("edit"));

    let leaf = kit().args(["repo", "edit", "--help"]).output().unwrap();
    assert!(leaf.status.success());
    let leaf = String::from_utf8(leaf.stdout).unwrap();
    assert!(leaf.contains("Usage: kit repo edit"));
    assert!(leaf.contains("--input-file <PATH>"));
    assert!(leaf.contains("--idempotency-key <KEY>"));

    for arguments in [
        vec!["--help"],
        vec!["repo", "--help"],
        vec!["repo", "read", "--help"],
        vec!["project", "show", "--help"],
    ] {
        let help = kit().args(arguments).output().unwrap();
        assert!(help.status.success());
        assert!(
            !String::from_utf8(help.stdout)
                .unwrap()
                .contains("--idempotency-key")
        );
    }
    let optional = kit()
        .args(["project", "create", "--help"])
        .output()
        .unwrap();
    assert!(optional.status.success());
    assert!(
        String::from_utf8(optional.stdout)
            .unwrap()
            .contains("--idempotency-key <KEY>")
    );
}

#[test]
fn prompt_help_documents_every_form_and_positional_role() {
    for (arguments, prefix) in [
        (vec!["prompt", "--help"], "kit prompt"),
        (vec!["run", "start", "--help"], "kit run start"),
    ] {
        let help = kit().args(arguments).output().unwrap();
        assert!(help.status.success());
        let help = String::from_utf8(help.stdout).unwrap();
        for form in [
            format!("{prefix} THREAD_ID MESSAGE"),
            format!("{prefix} THREAD_ID --message MESSAGE"),
            format!("{prefix} THREAD_ID --input ARTIFACT_REF"),
            format!("{prefix} --thread THREAD_ID MESSAGE"),
            format!("{prefix} --thread THREAD_ID --message MESSAGE"),
            format!("{prefix} --thread THREAD_ID --input ARTIFACT_REF"),
        ] {
            assert!(help.contains(&form), "missing prompt help form: {form}");
        }
        assert!(help.contains(
            "When --thread is present, the single positional value is the prompt message."
        ));
    }
}

#[test]
fn version_and_help_exit_without_starting_a_daemon() {
    let version = kit().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        format!("kit {}", env!("CARGO_PKG_VERSION"))
    );

    let root = unused_root("help-no-daemon");
    let _ = fs::remove_dir_all(&root);
    let help = kit()
        .args(["repo", "edit", "--auto-start", "--state-root"])
        .arg(&root)
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(!root.exists());
}

#[test]
fn typos_and_missing_subcommands_include_guidance() {
    let typo = kit().args(["repo", "edti"]).output().unwrap();
    assert_eq!(typo.status.code(), Some(2));
    let typo = String::from_utf8(typo.stderr).unwrap();
    assert!(typo.contains("similar subcommand exists: 'edit'"));
    assert!(typo.contains("Usage: kit repo"));
    assert!(!typo.contains("\u{1b}["));

    let missing = kit().arg("repo").output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    let missing = String::from_utf8(missing.stderr).unwrap();
    assert!(missing.contains("Usage: kit repo [OPTIONS] <COMMAND>"));
    assert!(missing.contains("Commands:"));
}

#[test]
fn globals_after_leaf_and_json_errors_are_supported() {
    let semantic = kit()
        .args([
            "project",
            "show",
            "bad-id",
            "--state-root=/definitely/missing",
            "--format=json",
        ])
        .output()
        .unwrap();
    assert_eq!(semantic.status.code(), Some(2));
    let problem: Value = serde_json::from_slice(&semantic.stderr).unwrap();
    assert_eq!(problem["code"], "invalid_request");

    let lexical = kit().args(["repo", "edti", "--json"]).output().unwrap();
    assert_eq!(lexical.status.code(), Some(2));
    assert!(serde_json::from_slice::<Value>(&lexical.stderr).is_err());
    let lexical = String::from_utf8(lexical.stderr).unwrap();
    assert!(lexical.starts_with("error: "));
    assert!(lexical.contains("Usage: kit repo"));

    let owned_value = kit()
        .args(["project", "show", "--project", "--json"])
        .output()
        .unwrap();
    assert_eq!(owned_value.status.code(), Some(2));
    assert!(
        String::from_utf8(owned_value.stderr)
            .unwrap()
            .starts_with("error: invalid value for --project: --json")
    );

    let lexical_owned_value = kit()
        .args(["project", "show", "--project", "--json", "extra"])
        .output()
        .unwrap();
    assert_eq!(lexical_owned_value.status.code(), Some(2));
    assert!(serde_json::from_slice::<Value>(&lexical_owned_value.stderr).is_err());
    assert!(
        String::from_utf8(lexical_owned_value.stderr)
            .unwrap()
            .starts_with("error: ")
    );

    let recovery = kit().args(["repo", "edit", PROJECT]).output().unwrap();
    assert_eq!(recovery.status.code(), Some(2));
    let recovery = String::from_utf8(recovery.stderr).unwrap();
    assert!(recovery.contains("required arguments were not provided"));
    assert!(recovery.contains("--idempotency-key <KEY>"));

    let recovery = parse(["kit", "repo", "edit", PROJECT]).unwrap_err();
    assert!(recovery.message.contains("--idempotency-key <KEY>"));
}

#[test]
fn globals_are_singletons_across_command_depths() {
    let valid = parse([
        "kit",
        "--state-root=state",
        "run",
        "--timeout-ms=10",
        "start",
        "--thread",
        THREAD,
        "--message=message",
        "--format=json",
        "--idempotency-key=key",
    ])
    .unwrap();
    assert_eq!(valid.state_root.unwrap(), PathBuf::from("state"));
    assert_eq!(valid.timeout, std::time::Duration::from_millis(10));
    assert_eq!(
        operation(
            &parse(["kit", "repo", "edit", PROJECT, "--idempotency-key=leaf-key"])
                .unwrap()
                .invocation
        ),
        "repo.edit"
    );
    for arguments in [
        vec!["kit", "--idempotency-key=root-key", "repo", "edit", PROJECT],
        vec![
            "kit",
            "repo",
            "--idempotency-key=group-key",
            "edit",
            PROJECT,
        ],
        vec![
            "kit",
            "project",
            "show",
            PROJECT,
            "--idempotency-key=read-only",
        ],
    ] {
        let error = parse(arguments).unwrap_err();
        assert!(
            error
                .message
                .contains("unexpected argument '--idempotency-key'")
        );
    }

    for (arguments, expected) in [
        (
            vec![
                "kit",
                "--state-root=a",
                "project",
                "show",
                PROJECT,
                "--state-root=b",
            ],
            "--state-root may only be specified once",
        ),
        (
            vec![
                "kit",
                "--timeout-ms=1",
                "run",
                "--timeout-ms=2",
                "show",
                RUN,
            ],
            "--timeout-ms may only be specified once",
        ),
        (
            vec!["kit", "--json", "project", "show", PROJECT, "--json"],
            "output format may only be specified once",
        ),
        (
            vec![
                "kit",
                "--format=json",
                "project",
                "show",
                PROJECT,
                "--format=json",
            ],
            "output format may only be specified once",
        ),
        (
            vec!["kit", "--json", "project", "show", PROJECT, "--format=json"],
            "--json, --jsonl, and --format conflict",
        ),
        (
            vec!["kit", "run", "--jsonl", "show", RUN, "--json"],
            "--json, --jsonl, and --format conflict",
        ),
    ] {
        let error = parse(arguments).unwrap_err();
        assert!(error.message.contains(expected), "{error}");
    }

    let structured = kit()
        .args([
            "--json",
            "project",
            "show",
            PROJECT,
            "--state-root=a",
            "--state-root=b",
        ])
        .output()
        .unwrap();
    assert_eq!(structured.status.code(), Some(2));
    let problem: Value = serde_json::from_slice(&structured.stderr).unwrap();
    assert_eq!(problem["code"], "invalid_request");
}

#[test]
fn auth_timeout_defaults_preserve_explicit_global_values() {
    for (arguments, expected_ms) in [
        (vec!["kit", "auth", "status", "openai"], 30_000),
        (vec!["kit", "auth", "logout", "openai"], 30_000),
        (vec!["kit", "auth", "login", "openai"], 300_000),
        (vec!["kit", "project", "create"], 5_000),
        (vec!["kit", "auth", "status", "openai", "--timeout-ms=1"], 1),
        (vec!["kit", "auth", "login", "openai", "--timeout-ms=1"], 1),
    ] {
        assert_eq!(
            parse(arguments).unwrap().timeout,
            std::time::Duration::from_millis(expected_ms)
        );
    }
}

#[test]
fn missing_discovery_is_unavailable_for_every_dispatch_class() {
    let root = unused_root("missing-discovery");
    let _ = fs::remove_dir_all(&root);
    for arguments in [
        vec!["project", "show", PROJECT],
        vec!["process", "list", PROJECT],
        vec!["repo", "status"],
    ] {
        let output = kit()
            .arg("--json")
            .arg("--state-root")
            .arg(&root)
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(6));
        let problem: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(problem["status"], 503);
        assert_eq!(problem["code"], "unavailable");
    }
}

#[cfg(unix)]
#[test]
fn non_utf_argv_is_a_native_clap_usage_error() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let output = kit()
        .arg("--json")
        .arg(OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(serde_json::from_slice::<Value>(&output.stderr).is_err());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("error: ")
    );
}

#[test]
fn prompt_forms_convert_directly_and_invalid_combinations_fail() {
    for arguments in [
        vec!["kit", "prompt", THREAD, "message"],
        vec!["kit", "prompt", THREAD, "--message=message"],
        vec!["kit", "prompt", "--thread", THREAD, "message"],
        vec!["kit", "prompt", "--thread", THREAD, "--message=message"],
        vec!["kit", "run", "start", THREAD, "message"],
        vec![
            "kit",
            "run",
            "start",
            "--thread",
            THREAD,
            "--message=message",
        ],
    ] {
        let Invocation::Client(request) = parse(arguments).unwrap().invocation else {
            panic!("expected prompt request")
        };
        let ClientRequest::Prompt(request) = *request else {
            panic!("expected prompt request")
        };
        assert_eq!(
            request.command.input,
            PromptInput::Message("message".to_owned())
        );
        assert!(!request.wait);
    }

    for arguments in [
        vec!["kit", "prompt", "--wait", THREAD, "message"],
        vec!["kit", "run", "start", THREAD, "message", "--wait"],
    ] {
        let Invocation::Client(request) = parse(arguments).unwrap().invocation else {
            panic!("expected prompt request")
        };
        let ClientRequest::Prompt(request) = *request else {
            panic!("expected prompt request")
        };
        assert!(request.wait);
    }

    for arguments in [
        vec!["kit", "prompt"],
        vec!["kit", "prompt", THREAD],
        vec!["kit", "prompt", "--thread", THREAD],
        vec![
            "kit",
            "prompt",
            "--thread",
            THREAD,
            "extra",
            "--message=message",
        ],
        vec!["kit", "prompt", THREAD, "extra", "--message=message"],
        vec![
            "kit",
            "prompt",
            THREAD,
            "--message=message",
            "--input",
            REFERENCE,
        ],
        vec![
            "kit",
            "repo",
            "edit",
            PROJECT,
            "--idempotency-key=first",
            "--idempotency-key=second",
        ],
        vec![
            "kit",
            "project",
            "create",
            "--idempotency-key=first",
            "--idempotency-key=second",
        ],
    ] {
        assert!(parse(arguments).is_err());
    }
}

#[test]
fn clap_frontend_reaches_every_semantic_operation() {
    let key = "--idempotency-key";
    let cases = [
        ("daemon", vec!["kit", "daemon"]),
        (
            "project.create",
            vec!["kit", "project", "create", "--id", PROJECT],
        ),
        ("project.get", vec!["kit", "project", "show", PROJECT]),
        (
            "project.get",
            vec!["kit", "project", "show", "--project", PROJECT],
        ),
        (
            "thread.create",
            vec![
                "kit",
                "thread",
                "create",
                "--project",
                PROJECT,
                "--id",
                THREAD,
            ],
        ),
        ("thread.list", vec!["kit", "thread", "list", PROJECT]),
        ("thread.get", vec!["kit", "thread", "show", THREAD]),
        (
            "thread.archive",
            vec!["kit", "thread", "archive", THREAD, "--version", "1"],
        ),
        (
            "thread.delete.initiate",
            vec![
                "kit",
                "thread",
                "delete",
                "--thread",
                THREAD,
                "--version",
                "1",
            ],
        ),
        (
            "thread.deletion.get",
            vec!["kit", "deletion", "show", "--deletion-job", "deletion-1"],
        ),
        (
            "run.start",
            vec!["kit", "prompt", "--thread", THREAD, "message"],
        ),
        (
            "run.start",
            vec!["kit", "run", "start", THREAD, "--message", "message"],
        ),
        (
            "run.start",
            vec!["kit", "prompt", THREAD, "--input", REFERENCE],
        ),
        ("run.list", vec!["kit", "run", "list", PROJECT]),
        ("run.get", vec!["kit", "run", "show", RUN]),
        ("run.cost", vec!["kit", "run", "cost", "--run", RUN]),
        ("run.prompts", vec!["kit", "run", "prompts", RUN]),
        ("run.transcript", vec!["kit", "run", "transcript", RUN]),
        (
            "run.cancel",
            vec!["kit", "run", "cancel", RUN, "--version", "1"],
        ),
        (
            "run.input",
            vec![
                "kit",
                "run",
                "input",
                RUN,
                "--input",
                REFERENCE,
                "--version",
                "1",
            ],
        ),
        (
            "run.timeline",
            vec!["kit", "events", "follow", "--run", RUN],
        ),
        (
            "thread.events",
            vec!["kit", "events", "follow", "--thread", THREAD],
        ),
        (
            "run.timeline",
            vec!["kit", "events", "--follow", "--run", RUN],
        ),
        (
            "event.cursor.status",
            vec![
                "kit",
                "events",
                "status",
                "--project",
                PROJECT,
                "--cursor",
                "cursor_0000000000000000",
            ],
        ),
        ("approval.pending", vec!["kit", "approval", "list", PROJECT]),
        (
            "approval.resolve",
            vec![
                "kit",
                "approval",
                "resolve",
                APPROVAL,
                "--decision",
                "approve",
                "--version",
                "1",
            ],
        ),
        ("auth.pending", vec!["kit", "auth", "list", PROJECT]),
        (
            "auth.resolve",
            vec![
                "kit",
                "auth",
                "resolve",
                RUN,
                "--granted",
                "deny",
                "--version",
                "1",
            ],
        ),
        ("service.status", vec!["kit", "status", PROJECT]),
        (
            "capability.list",
            vec!["kit", "capability", "list", PROJECT],
        ),
        (
            "artifact.metadata.register",
            vec![
                "kit",
                "artifact",
                "register",
                PROJECT,
                "--id",
                ARTIFACT,
                "--reference",
                REFERENCE,
                "--media-type",
                "application/octet-stream",
                "--size",
                "0",
            ],
        ),
        (
            "artifact.metadata.get",
            vec!["kit", "artifact", "show", ARTIFACT],
        ),
        (
            "project.retention.get",
            vec!["kit", "retention", "show", PROJECT],
        ),
        (
            "project.retention.set",
            vec![
                "kit",
                "retention",
                "set",
                PROJECT,
                "--version",
                "1",
                "--event",
                "forever",
                "--transcript",
                "1",
                "--terminal",
                "1",
                "--artifact",
                "1",
                "--experiment",
                "1",
                "--backup",
                "1",
            ],
        ),
        ("process.list", vec!["kit", "process", "list", PROJECT]),
        ("process.get", vec!["kit", "process", "show", PROCESS]),
        ("process.cancel", vec!["kit", "process", "cancel", PROCESS]),
        (
            "terminal.allocate",
            vec![
                "kit",
                "terminal",
                "allocate",
                PROCESS,
                "--columns",
                "80",
                "--rows",
                "24",
                "--max-output-bytes",
                "4096",
                "--max-output-age-ms",
                "60000",
            ],
        ),
        ("terminal.get", vec!["kit", "terminal", "show", TERMINAL]),
        (
            "terminal.viewer.attach",
            vec!["kit", "terminal", "attach", TERMINAL],
        ),
        (
            "terminal.writer.claim",
            vec![
                "kit",
                "terminal",
                "writer-claim",
                TERMINAL,
                "--lease-ms",
                "1000",
            ],
        ),
        (
            "terminal.attachment.get",
            vec![
                "kit",
                "terminal",
                "attachment-show",
                "--attachment",
                ATTACHMENT,
            ],
        ),
        (
            "terminal.writer.renew",
            vec![
                "kit",
                "terminal",
                "writer-renew",
                "--attachment",
                ATTACHMENT,
                "--lease-ms",
                "1000",
            ],
        ),
        (
            "terminal.writer.release",
            vec![
                "kit",
                "terminal",
                "writer-release",
                "--attachment",
                ATTACHMENT,
            ],
        ),
        (
            "terminal.input",
            vec![
                "kit",
                "terminal",
                "input",
                "--attachment",
                ATTACHMENT,
                key,
                "terminal-input-key",
            ],
        ),
        (
            "terminal.input.resolve",
            vec![
                "kit",
                "terminal",
                "input-resolve",
                "--attachment",
                ATTACHMENT,
                "--outcome",
                "not-applied",
                key,
                "terminal-resolve-key",
            ],
        ),
        (
            "terminal.resize",
            vec![
                "kit",
                "terminal",
                "resize",
                "--attachment",
                ATTACHMENT,
                "--columns",
                "100",
                "--rows",
                "40",
            ],
        ),
        (
            "terminal.output",
            vec!["kit", "terminal", "output", "--attachment", ATTACHMENT],
        ),
        (
            "terminal.resizes",
            vec!["kit", "terminal", "resizes", "--attachment", ATTACHMENT],
        ),
        (
            "terminal.detach",
            vec!["kit", "terminal", "detach", "--attachment", ATTACHMENT],
        ),
        (
            "executor.events",
            vec!["kit", "executor", "events", PROJECT],
        ),
        ("repo.status", vec!["kit", "repo", "status"]),
        ("repo.revision", vec!["kit", "repo", "revision", PROJECT]),
        (
            "repo.capabilities",
            vec!["kit", "repo", "capabilities", PROJECT],
        ),
        ("repo.discover", vec!["kit", "repo", "discover", PROJECT]),
        (
            "repo.search",
            vec!["kit", "repo", "search", "--project", PROJECT],
        ),
        ("repo.read", vec!["kit", "repo", "read", PROJECT]),
        (
            "repo.edit",
            vec!["kit", "repo", "edit", PROJECT, key, "repo-edit-key"],
        ),
        (
            "repo.run",
            vec!["kit", "repo", "run", PROJECT, key, "repo-run-key"],
        ),
        (
            "repo.check",
            vec!["kit", "repo", "check", PROJECT, key, "repo-check-key"],
        ),
        (
            "repo.result",
            vec!["kit", "repo", "result", "--result", "result-1"],
        ),
        (
            "repo.result.events",
            vec!["kit", "repo", "events", "--result", "result-1"],
        ),
        (
            "repo.result.approval",
            vec![
                "kit",
                "repo",
                "approval",
                "--result",
                "result-1",
                "--decision",
                "approved",
            ],
        ),
        (
            "repo.result.cancel",
            vec!["kit", "repo", "cancel", "--result", "result-1"],
        ),
        (
            "repo.artifact",
            vec![
                "kit",
                "repo",
                "artifact",
                "--artifact-ref",
                "artifact-ref-1",
            ],
        ),
    ];

    for (expected, arguments) in cases {
        let cli = parse(arguments).unwrap_or_else(|error| panic!("{expected}: {error}"));
        assert_eq!(operation(&cli.invocation), expected);
    }
}

#[test]
fn typed_parse_preserves_exact_values_and_defaults() {
    assert!(matches!(
        parse(["daemon"]).unwrap().invocation,
        Invocation::Daemon(_)
    ));
    assert!(matches!(parse(["ui"]).unwrap().invocation, Invocation::Ui));

    let cli = parse([
        "renamed-binary",
        "prompt",
        "--thread",
        THREAD,
        "--message=--json",
        "--state-root=--second",
        "--timeout-ms",
        "10",
        "--format=json",
    ])
    .unwrap();
    assert_eq!(cli.state_root.unwrap(), PathBuf::from("--second"));
    assert_eq!(cli.timeout, std::time::Duration::from_millis(10));
    assert_eq!(cli.format, kit::cli::core::OutputFormat::Json);
    let Invocation::Client(request) = cli.invocation else {
        panic!("expected prompt request")
    };
    let kit::cli::core::ClientRequest::Prompt(request) = *request else {
        panic!("expected prompt request")
    };
    assert_eq!(
        request.command.input,
        kit::api::service::PromptInput::Message("--json".to_owned())
    );

    for arguments in [
        vec!["kit", "prompt", "--thread", THREAD, "--", "--positional"],
        vec!["kit", "prompt", THREAD, "--message", "-named"],
    ] {
        let cli = parse(arguments).unwrap();
        let Invocation::Client(request) = cli.invocation else {
            panic!("expected prompt request")
        };
        let kit::cli::core::ClientRequest::Prompt(request) = *request else {
            panic!("expected prompt request")
        };
        assert!(matches!(
            request.command.input,
            kit::api::service::PromptInput::Message(_)
        ));
    }

    let defaults = parse(["kit", "terminal", "output", "--attachment", ATTACHMENT]).unwrap();
    let Invocation::Exec(defaults) = defaults.invocation else {
        panic!("expected terminal output request")
    };
    assert!(
        defaults
            .path
            .ends_with("/output?cursor=output_0000000000000001")
    );

    let descriptor = parse([
        "kit",
        "repo",
        "discover",
        PROJECT,
        "--input-file=--payload.json",
    ])
    .unwrap();
    let Invocation::Repo(descriptor) = descriptor.invocation else {
        panic!("expected repository request")
    };
    assert_eq!(descriptor.operation, "repo.discover");

    let globals = parse([
        "kit",
        "--state-root=state",
        "run",
        "--timeout-ms=10",
        "start",
        "--thread",
        THREAD,
        "--message=message",
        "--format=json",
    ])
    .unwrap();
    assert_eq!(globals.state_root.unwrap(), PathBuf::from("state"));
    assert_eq!(globals.timeout, std::time::Duration::from_millis(10));

    for arguments in [
        vec!["kit", "status", PROJECT, "--json", "--format=json"],
        vec!["kit", "status", PROJECT, "--state-root=a", "--state-root=b"],
        vec!["kit", "events", "follow", "--run", RUN, "--limit=0"],
        vec![
            "kit",
            "prompt",
            "--thread",
            THREAD,
            "--message=message",
            "--input",
            REFERENCE,
        ],
    ] {
        assert!(parse(arguments).is_err());
    }
    for arguments in [
        vec!["kit", "repo", "edit", PROJECT],
        vec!["kit", "repo", "run", PROJECT],
        vec!["kit", "repo", "check", PROJECT],
        vec!["kit", "terminal", "input", "--attachment", ATTACHMENT],
        vec![
            "kit",
            "terminal",
            "input-resolve",
            "--attachment",
            ATTACHMENT,
            "--outcome",
            "applied",
        ],
    ] {
        assert!(parse(arguments).is_err());
    }
}

fn operation(invocation: &Invocation) -> &str {
    match invocation {
        Invocation::Daemon(_) => "daemon",
        Invocation::Ui => "ui",
        Invocation::Provider(_) => "provider.local",
        Invocation::Auth(_) => "auth.local",
        Invocation::Client(request) => request.operation(),
        Invocation::Exec(request) => request.operation,
        Invocation::Repo(request) => request.operation,
    }
}
