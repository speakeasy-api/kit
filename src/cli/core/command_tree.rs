use clap::{Arg, ArgAction, ArgGroup, Command, ValueHint, builder::PossibleValuesParser};

const WIDTH: usize = 100;

pub fn command() -> Command {
    Command::new("kit")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Operate the Kit agent runtime and repository service")
        .after_help(
            "Examples:\n  kit project create\n  kit prompt --thread THREAD_ID \"Fix the failing test\"\n  kit repo read --project PROJECT_ID --input-file read.json\n  kit events follow --run RUN_ID --jsonl",
        )
        .disable_colored_help(true)
        .term_width(WIDTH)
        .subcommand_required(true)
        .arg_required_else_help(true)
        .args(global_args("root"))
        .subcommands([
            leaf("daemon", "Run the local Kit daemon").arg(flag(
                "daemonize",
                "daemonize",
                "Start the daemon in the background",
            )),
            project(),
            thread(),
            deletion(),
            prompt("prompt"),
            run(),
            events(),
            approval(),
            auth(),
            required_id(
                leaf("status", "Show project service status"),
                "project",
                "project-id",
                "PROJECT_ID",
                "Project identifier",
            ),
            capability(),
            artifact(),
            retention(),
            process(),
            terminal(),
            executor(),
            provider(),
            repo(),
        ])
}

fn global_args(scope: &'static str) -> [Arg; 6] {
    let ids = match scope {
        "root" => [
            "root-json",
            "root-jsonl",
            "root-format",
            "root-state-root",
            "root-auto-start",
            "root-timeout-ms",
        ],
        "group" => [
            "group-json",
            "group-jsonl",
            "group-format",
            "group-state-root",
            "group-auto-start",
            "group-timeout-ms",
        ],
        "leaf" => [
            "leaf-json",
            "leaf-jsonl",
            "leaf-format",
            "leaf-state-root",
            "leaf-auto-start",
            "leaf-timeout-ms",
        ],
        _ => unreachable!(),
    };
    [
        Arg::new(ids[0])
            .long("json")
            .help("Emit JSON output")
            .action(ArgAction::Append)
            .num_args(0)
            .default_missing_value("true")
            .conflicts_with_all([ids[1], ids[2]]),
        Arg::new(ids[1])
            .long("jsonl")
            .help("Emit JSON Lines output for event streams")
            .action(ArgAction::Append)
            .num_args(0)
            .default_missing_value("true")
            .conflicts_with_all([ids[0], ids[2]]),
        Arg::new(ids[2])
            .long("format")
            .value_name("FORMAT")
            .help("Output format")
            .value_parser(PossibleValuesParser::new(["human", "json"]))
            .action(ArgAction::Append)
            .conflicts_with_all([ids[0], ids[1]]),
        Arg::new(ids[3])
            .long("state-root")
            .value_name("PATH")
            .help("Kit state directory [default: .kit]")
            .value_hint(ValueHint::DirPath)
            .action(ArgAction::Append)
            .allow_hyphen_values(true),
        Arg::new(ids[4])
            .long("auto-start")
            .help("Start the daemon when it is unavailable")
            .action(ArgAction::SetTrue),
        Arg::new(ids[5])
            .long("timeout-ms")
            .value_name("MILLISECONDS")
            .help("Request timeout [default: 5000]")
            .value_parser(clap::value_parser!(u64).range(1..=300_000))
            .action(ArgAction::Append),
    ]
}

fn project() -> Command {
    group("project", "Create and inspect projects").subcommands([
        mutation_leaf(leaf("create", "Create a project").arg(value(
            "id",
            "id",
            "PROJECT_ID",
            "Project identifier to use instead of generating one",
        ))),
        required_id(
            leaf("show", "Show a project"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
    ])
}

fn thread() -> Command {
    group("thread", "Create and manage threads").subcommands([
        required_id(
            mutation_leaf(leaf("create", "Create a thread").arg(value(
                "id",
                "id",
                "THREAD_ID",
                "Thread identifier to use instead of generating one",
            ))),
            "project",
            "project-id",
            "PROJECT_ID",
            "Owning project identifier",
        ),
        required_id(
            leaf("list", "List project threads"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
        required_id(
            leaf("show", "Show a thread"),
            "thread",
            "thread-id",
            "THREAD_ID",
            "Thread identifier",
        ),
        required_id(
            mutation_leaf(
                leaf("archive", "Archive or unarchive a thread")
                    .arg(positive_u64(
                        "version",
                        "version",
                        "Expected thread version",
                    ))
                    .arg(flag("undo", "undo", "Unarchive the thread")),
            ),
            "thread",
            "thread-id",
            "THREAD_ID",
            "Thread identifier",
        ),
        required_id(
            mutation_leaf(leaf("delete", "Request thread deletion").arg(positive_u64(
                "version",
                "version",
                "Expected thread version",
            ))),
            "thread",
            "thread-id",
            "THREAD_ID",
            "Thread identifier",
        ),
    ])
}

fn deletion() -> Command {
    group("deletion", "Inspect deletion jobs").subcommand(leaf("show", "Show a deletion job").arg(
        required_value(
            "deletion-job",
            "deletion-job",
            "DELETION_JOB_ID",
            "Deletion job identifier",
        ),
    ))
}

fn prompt(name: &'static str) -> Command {
    let examples = if name == "prompt" {
        "Accepted forms:\n  kit prompt THREAD_ID MESSAGE\n  kit prompt THREAD_ID --message MESSAGE\n  kit prompt THREAD_ID --input ARTIFACT_REF\n  kit prompt --thread THREAD_ID MESSAGE\n  kit prompt --thread THREAD_ID --message MESSAGE\n  kit prompt --thread THREAD_ID --input ARTIFACT_REF\n\nWhen --thread is present, the single positional value is the prompt message."
    } else {
        "Accepted forms:\n  kit run start THREAD_ID MESSAGE\n  kit run start THREAD_ID --message MESSAGE\n  kit run start THREAD_ID --input ARTIFACT_REF\n  kit run start --thread THREAD_ID MESSAGE\n  kit run start --thread THREAD_ID --message MESSAGE\n  kit run start --thread THREAD_ID --input ARTIFACT_REF\n\nWhen --thread is present, the single positional value is the prompt message."
    };
    leaf(name, "Start a run with a prompt")
        .after_help(examples)
        .arg(value(
            "id",
            "id",
            "RUN_ID",
            "Run identifier to use instead of generating one",
        ))
        .arg(value("thread", "thread", "THREAD_ID", "Thread identifier"))
        .arg(value(
            "input",
            "input",
            "ARTIFACT_REF",
            "Use an artifact as prompt input",
        ))
        .arg(value("message", "message", "MESSAGE", "Prompt message"))
        .arg(
            Arg::new("prompt-positionals")
                .value_names(["THREAD_ID", "MESSAGE"])
                .index(1)
                .num_args(1..=2)
                .help("Without --thread: THREAD_ID then MESSAGE; with --thread: MESSAGE only"),
        )
        .group(
            ArgGroup::new("named-prompt-input")
                .args(["input", "message"])
                .multiple(false),
        )
        .arg(idempotency_key(false))
}

fn run() -> Command {
    group("run", "Start and manage runs").subcommands([
        prompt("start"),
        required_id(
            leaf("list", "List project runs"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
        run_id(leaf("show", "Show a run")),
        run_id(leaf("cost", "Show run cost and usage")),
        run_id(leaf("prompts", "Show prompts used by a run")),
        run_id(leaf("transcript", "Show a run transcript")),
        run_id(mutation_leaf(leaf("cancel", "Cancel a run").arg(
            positive_u64("version", "version", "Expected run version"),
        ))),
        run_id(mutation_leaf(
            leaf("input", "Provide artifact input to a run")
                .arg(required_value(
                    "input",
                    "input",
                    "ARTIFACT_REF",
                    "Input artifact reference",
                ))
                .arg(positive_u64("version", "version", "Expected run version")),
        )),
    ])
}

fn events() -> Command {
    event_target(
        Command::new("events")
            .about("Follow events or inspect an event cursor")
            .disable_colored_help(true)
            .term_width(WIDTH)
            .arg_required_else_help(true)
            .subcommand_negates_reqs(true)
            .args(global_args("group"))
            .arg(flag("follow", "follow", "Follow events continuously").required(true))
            .subcommands([
                event_target(leaf("follow", "Follow run or thread events")),
                required_id(
                    leaf("status", "Show event cursor status").arg(required_value(
                        "cursor",
                        "cursor",
                        "CURSOR",
                        "Page cursor",
                    )),
                    "project",
                    "project-id",
                    "PROJECT_ID",
                    "Project identifier",
                ),
            ]),
    )
}

fn event_target(command: Command) -> Command {
    command
        .arg(value("run", "run", "RUN_ID", "Follow one run"))
        .arg(value("thread", "thread", "THREAD_ID", "Follow one thread"))
        .arg(value(
            "cursor",
            "cursor",
            "CURSOR",
            "Resume after an opaque stream cursor",
        ))
        .arg(
            value("limit", "limit", "COUNT", "Events requested per page")
                .default_value("100")
                .value_parser(clap::value_parser!(u64).range(1..=1_000)),
        )
        .group(
            ArgGroup::new("event-target")
                .args(["run", "thread"])
                .required(true)
                .multiple(false),
        )
}

fn approval() -> Command {
    group("approval", "List and resolve approvals").subcommands([
        required_id(
            leaf("list", "List pending approvals"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
        required_id(
            mutation_leaf(
                leaf("resolve", "Resolve an approval")
                    .arg(
                        required_value("decision", "decision", "DECISION", "Approval decision")
                            .value_parser(PossibleValuesParser::new([
                                "approved", "approve", "denied", "deny",
                            ])),
                    )
                    .arg(positive_u64(
                        "version",
                        "version",
                        "Expected approval version",
                    )),
            ),
            "approval",
            "approval-id",
            "APPROVAL_ID",
            "Approval identifier",
        ),
    ])
}

fn auth() -> Command {
    group("auth", "List and resolve authorization requests").subcommands([
        required_id(
            leaf("list", "List pending authorization requests"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
        required_id(
            mutation_leaf(
                leaf("resolve", "Resolve an authorization request")
                    .arg(
                        required_value(
                            "granted",
                            "granted",
                            "DECISION",
                            "Whether authorization is granted",
                        )
                        .value_parser(PossibleValuesParser::new([
                            "true", "yes", "grant", "false", "no", "deny",
                        ])),
                    )
                    .arg(positive_u64("version", "version", "Expected run version")),
            ),
            "run",
            "run-id",
            "RUN_ID",
            "Run identifier",
        ),
    ])
}

fn capability() -> Command {
    group("capability", "Inspect project capabilities").subcommand(required_id(
        leaf("list", "List project capabilities"),
        "project",
        "project-id",
        "PROJECT_ID",
        "Project identifier",
    ))
}

fn artifact() -> Command {
    group("artifact", "Register and inspect artifacts").subcommands([
        required_id(
            mutation_leaf(
                leaf("register", "Register artifact metadata")
                    .arg(value(
                        "id",
                        "id",
                        "ARTIFACT_ID",
                        "Artifact identifier to use instead of generating one",
                    ))
                    .arg(required_value(
                        "reference",
                        "reference",
                        "ARTIFACT_REF",
                        "Content-addressed artifact reference",
                    ))
                    .arg(required_value(
                        "media-type",
                        "media-type",
                        "MEDIA_TYPE",
                        "Artifact media type",
                    ))
                    .arg(
                        required_value("size", "size", "BYTES", "Artifact size in bytes")
                            .value_parser(clap::value_parser!(u64)),
                    ),
            ),
            "project",
            "project-id",
            "PROJECT_ID",
            "Owning project identifier",
        ),
        required_id(
            leaf("show", "Show artifact metadata"),
            "artifact",
            "artifact-id",
            "ARTIFACT_ID",
            "Artifact identifier",
        ),
    ])
}

fn retention() -> Command {
    let set = [
        ("event", "Event retention"),
        ("transcript", "Transcript retention"),
        ("terminal", "Terminal retention"),
        ("artifact", "Artifact retention"),
        ("experiment", "Experiment retention"),
        ("backup", "Backup retention"),
    ]
    .into_iter()
    .fold(
        mutation_leaf(
            leaf("set", "Set the project retention policy").arg(positive_u64(
                "version",
                "version",
                "Expected project version",
            )),
        ),
        |command, (name, help)| {
            command.arg(required_value(name, name, "MICROSECONDS|forever", help))
        },
    );
    group("retention", "Inspect and set retention policy").subcommands([
        required_id(
            leaf("show", "Show the project retention policy"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
        required_id(
            set,
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
    ])
}

fn process() -> Command {
    group("process", "Inspect and cancel executor processes").subcommands([
        required_id(
            leaf("list", "List project processes"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
        process_id(leaf("show", "Show a process")),
        process_id(mutation_leaf(leaf("cancel", "Cancel a process"))),
    ])
}

fn terminal() -> Command {
    group("terminal", "Allocate and operate process terminals").subcommands([
        process_id(mutation_leaf(
            leaf("allocate", "Allocate a process terminal")
                .arg(nonzero_u16("columns", "columns", "Terminal columns"))
                .arg(nonzero_u16("rows", "rows", "Terminal rows"))
                .arg(positive_usize(
                    "max-output-bytes",
                    "max-output-bytes",
                    "Maximum retained output bytes",
                ))
                .arg(positive_u64(
                    "max-output-age-ms",
                    "max-output-age-ms",
                    "Maximum retained output age",
                )),
        )),
        terminal_id(leaf("show", "Show a terminal")),
        terminal_id(mutation_leaf(leaf("attach", "Attach a terminal viewer"))),
        terminal_id(mutation_leaf(
            leaf("writer-claim", "Claim the terminal writer lease").arg(positive_u64(
                "lease-ms",
                "lease-ms",
                "Writer lease duration",
            )),
        )),
        attachment(leaf("attachment-show", "Show a terminal attachment")),
        attachment(mutation_leaf(
            leaf("writer-renew", "Renew a terminal writer lease").arg(positive_u64(
                "lease-ms",
                "lease-ms",
                "Writer lease duration",
            )),
        )),
        attachment(mutation_leaf(leaf(
            "writer-release",
            "Release a terminal writer lease",
        ))),
        attachment(recovery_leaf(
            leaf("input", "Write terminal input").arg(
                value(
                    "input-file",
                    "input-file",
                    "PATH",
                    "Read input from PATH or - for stdin",
                )
                .default_value("-")
                .value_hint(ValueHint::FilePath),
            ),
        )),
        attachment(recovery_leaf(
            leaf(
                "input-resolve",
                "Resolve an uncertain terminal input outcome",
            )
            .arg(
                required_value("outcome", "outcome", "OUTCOME", "Observed input outcome")
                    .value_parser(PossibleValuesParser::new(["applied", "not-applied"])),
            ),
        )),
        attachment(mutation_leaf(
            leaf("resize", "Resize a terminal")
                .arg(nonzero_u16("columns", "columns", "Terminal columns"))
                .arg(nonzero_u16("rows", "rows", "Terminal rows")),
        )),
        attachment(
            leaf("output", "Read terminal output").arg(
                value("cursor", "cursor", "CURSOR", "Output cursor")
                    .default_value("output_0000000000000001"),
            ),
        ),
        attachment(
            leaf("resizes", "Read terminal resize events").arg(
                value("cursor", "cursor", "CURSOR", "Resize cursor")
                    .default_value("resize_0000000000000001"),
            ),
        ),
        attachment(mutation_leaf(leaf(
            "detach",
            "Detach a terminal attachment",
        ))),
    ])
}

fn executor() -> Command {
    group("executor", "Inspect executor activity").subcommand(required_id(
        leaf("events", "List project executor events").arg(
            value("cursor", "cursor", "CURSOR", "Executor event cursor")
                .default_value("exec_0000000000000000"),
        ),
        "project",
        "project-id",
        "PROJECT_ID",
        "Project identifier",
    ))
}

fn repo() -> Command {
    let invoke = |name, about| {
        let command = required_id(
            leaf(name, about).arg(
                value(
                    "input-file",
                    "input-file",
                    "PATH",
                    "Read the JSON request from PATH or - for stdin",
                )
                .default_value("-")
                .value_hint(ValueHint::FilePath),
            ),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        );
        if matches!(name, "edit" | "run" | "check") {
            recovery_leaf(command)
        } else {
            command
        }
    };
    group("repo", "Inspect and operate the repository").subcommands([
        leaf("status", "Show repository service status"),
        required_id(
            leaf("revision", "Show the current repository revision"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
        required_id(
            leaf("capabilities", "List repository capabilities"),
            "project",
            "project-id",
            "PROJECT_ID",
            "Project identifier",
        ),
        invoke("discover", "Discover repository paths"),
        invoke("search", "Search repository content"),
        invoke("read", "Read repository content"),
        invoke("edit", "Apply a structured repository edit"),
        invoke("run", "Run an allowed repository command"),
        invoke("check", "Run repository checks"),
        leaf("result", "Show a repository operation result").arg(required_value(
            "result",
            "result",
            "RESULT_ID",
            "Repository result identifier",
        )),
        leaf("events", "Show repository result events").arg(required_value(
            "result",
            "result",
            "RESULT_ID",
            "Repository result identifier",
        )),
        mutation_leaf(
            leaf("approval", "Resolve a repository operation approval")
                .arg(required_value(
                    "result",
                    "result",
                    "RESULT_ID",
                    "Repository result identifier",
                ))
                .arg(
                    required_value("decision", "decision", "DECISION", "Approval decision")
                        .value_parser(PossibleValuesParser::new(["approved", "denied"])),
                ),
        ),
        mutation_leaf(
            leaf("cancel", "Cancel a repository operation").arg(required_value(
                "result",
                "result",
                "RESULT_ID",
                "Repository result identifier",
            )),
        ),
        leaf("artifact", "Fetch a repository artifact").arg(required_value(
            "artifact-ref",
            "artifact-ref",
            "ARTIFACT_REF",
            "Repository artifact reference",
        )),
    ])
}

fn provider() -> Command {
    group("provider", "Manage persistent model provider profiles").subcommands([
        leaf("path", "Print the persistent provider configuration path"),
        leaf("list", "List persistent provider profiles"),
        leaf("add", "Add or replace a persistent provider profile")
            .arg(
                Arg::new("name")
                    .value_name("NAME")
                    .index(1)
                    .required(true)
                    .help("Profile name (1-64 ASCII letters, digits, '.', '_', or '-')"),
            )
            .arg(
                required_value("provider", "provider", "PROVIDER", "Provider type").value_parser(
                    PossibleValuesParser::new(["openai", "anthropic", "openrouter", "ollama"]),
                ),
            )
            .arg(flag(
                "replace",
                "replace",
                "Replace an existing profile with the same name",
            ))
            .arg(value(
                "api-key-env",
                "api-key-env",
                "ENV",
                "Override API key environment variable [defaults: OPENAI_API_KEY, OPENROUTER_API_KEY, or ANTHROPIC_API_KEY fallback]",
            ))
            .arg(value(
                "auth-token-env",
                "auth-token-env",
                "ENV",
                "Override Anthropic auth token environment variable [default: ANTHROPIC_AUTH_TOKEN when present]",
            ))
            .arg(value("model", "model", "MODEL", "Provider model name"))
            .arg(value(
                "base-url",
                "base-url",
                "URL",
                "Complete provider request endpoint",
            ))
            .arg(
                value(
                    "max-tokens",
                    "max-tokens",
                    "POSITIVE_INTEGER",
                    "Anthropic maximum output tokens",
                )
                .value_parser(clap::value_parser!(u32).range(1..)),
            )
            .arg(value(
                "provider-version",
                "version",
                "VERSION",
                "Anthropic API version",
            ))
            .arg(value(
                "beta",
                "beta",
                "FLAGS",
                "Comma-separated Anthropic beta flags",
            ))
            .arg(value(
                "app-name",
                "app-name",
                "NAME",
                "OpenRouter application name",
            ))
            .arg(value("site-url", "site-url", "URL", "OpenRouter site URL"))
            .arg(
                value(
                    "max-completion-tokens",
                    "max-completion-tokens",
                    "POSITIVE_INTEGER",
                    "OpenRouter maximum completion tokens",
                )
                .value_parser(clap::value_parser!(u32).range(1..)),
            )
            .arg(
                value(
                    "temperature",
                    "temperature",
                    "NUMBER",
                    "OpenRouter sampling temperature",
                )
                .value_parser(clap::value_parser!(f32)),
            )
            .arg(value(
                "reasoning-effort",
                "reasoning-effort",
                "EFFORT",
                "OpenRouter reasoning effort",
            )),
        leaf("use", "Select a persistent provider profile").arg(
            Arg::new("name")
                .value_name("NAME")
                .index(1)
                .required(true)
                .help("Profile name"),
        ),
    ])
}

fn group(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .disable_colored_help(true)
        .term_width(WIDTH)
        .subcommand_required(true)
        .arg_required_else_help(true)
        .args(global_args("group"))
}

fn leaf(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .disable_colored_help(true)
        .term_width(WIDTH)
        .args(global_args("leaf"))
}

fn mutation_leaf(command: Command) -> Command {
    command.arg(idempotency_key(false))
}

fn recovery_leaf(command: Command) -> Command {
    command.arg(idempotency_key(true))
}

fn idempotency_key(required: bool) -> Arg {
    value(
        "idempotency-key",
        "idempotency-key",
        "KEY",
        "Stable mutation key used for retries and outcome recovery",
    )
    .required(required)
}

fn required_id(
    command: Command,
    option: &'static str,
    positional: &'static str,
    value_name: &'static str,
    help: &'static str,
) -> Command {
    command
        .arg(value(option, option, value_name, help).required_unless_present(positional))
        .arg(
            Arg::new(positional)
                .value_name(value_name)
                .index(1)
                .help(help)
                .required_unless_present(option),
        )
        .group(
            ArgGroup::new("required-id")
                .args([option, positional])
                .required(true)
                .multiple(false),
        )
}

fn run_id(command: Command) -> Command {
    required_id(command, "run", "run-id", "RUN_ID", "Run identifier")
}

fn process_id(command: Command) -> Command {
    required_id(
        command,
        "process",
        "process-id",
        "PROCESS_ID",
        "Process identifier",
    )
}

fn terminal_id(command: Command) -> Command {
    required_id(
        command,
        "terminal",
        "terminal-id",
        "TERMINAL_ID",
        "Terminal identifier",
    )
}

fn attachment(command: Command) -> Command {
    command.arg(required_value(
        "attachment",
        "attachment",
        "ATTACHMENT_ID",
        "Terminal attachment identifier",
    ))
}

fn value(
    id: &'static str,
    long: &'static str,
    value_name: &'static str,
    help: &'static str,
) -> Arg {
    Arg::new(id)
        .long(long)
        .value_name(value_name)
        .help(help)
        .action(ArgAction::Set)
        .allow_hyphen_values(true)
}

fn required_value(
    id: &'static str,
    long: &'static str,
    value_name: &'static str,
    help: &'static str,
) -> Arg {
    value(id, long, value_name, help).required(true)
}

fn flag(id: &'static str, long: &'static str, help: &'static str) -> Arg {
    Arg::new(id)
        .long(long)
        .help(help)
        .action(ArgAction::SetTrue)
}

fn positive_u64(id: &'static str, long: &'static str, help: &'static str) -> Arg {
    required_value(id, long, "POSITIVE_INTEGER", help)
        .value_parser(clap::value_parser!(u64).range(1..))
}

fn positive_usize(id: &'static str, long: &'static str, help: &'static str) -> Arg {
    required_value(id, long, "POSITIVE_INTEGER", help)
        .value_parser(clap::value_parser!(u64).range(1..))
}

fn nonzero_u16(id: &'static str, long: &'static str, help: &'static str) -> Arg {
    required_value(id, long, "POSITIVE_INTEGER", help)
        .value_parser(clap::value_parser!(u16).range(1..))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::cli::{core::CLI_OPERATIONS, exec::EXEC_CLI_OPERATIONS, repo::REPO_CLI_OPERATIONS};

    use super::*;

    const EXPLICIT_ALIASES: &[&str] = &["events", "run start"];

    #[test]
    fn tree_and_catalog_have_exactly_the_same_public_leaves() {
        let command = command();
        let paths = paths(&command);
        let registered = CLI_OPERATIONS
            .iter()
            .map(|operation| operation.command.split(" --").next().unwrap())
            .chain(
                EXEC_CLI_OPERATIONS
                    .iter()
                    .map(|operation| operation.command),
            )
            .chain(
                REPO_CLI_OPERATIONS
                    .iter()
                    .map(|operation| operation.command),
            )
            .chain(EXPLICIT_ALIASES.iter().copied())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        assert_eq!(paths, registered);

        let keyed = keyed_paths(&command, false);
        let expected = CLI_OPERATIONS
            .iter()
            .filter(|operation| operation.mutation)
            .map(|operation| operation.command.split(" --").next().unwrap())
            .chain(
                EXEC_CLI_OPERATIONS
                    .iter()
                    .filter(|operation| operation.mutation)
                    .map(|operation| operation.command),
            )
            .chain(
                REPO_CLI_OPERATIONS
                    .iter()
                    .filter(|operation| operation.mutation)
                    .map(|operation| operation.command),
            )
            .chain(["run start"])
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        assert_eq!(keyed, expected);
        assert_eq!(
            keyed_paths(&command, true),
            [
                "repo check",
                "repo edit",
                "repo run",
                "terminal input",
                "terminal input-resolve",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
    }

    fn paths(command: &Command) -> BTreeSet<String> {
        fn visit(command: &Command, prefix: &str, paths: &mut BTreeSet<String>) {
            for subcommand in command.get_subcommands() {
                let path = if prefix.is_empty() {
                    subcommand.get_name().to_owned()
                } else {
                    format!("{prefix} {}", subcommand.get_name())
                };
                if !subcommand.has_subcommands() || EXPLICIT_ALIASES.contains(&path.as_str()) {
                    paths.insert(path.clone());
                }
                if subcommand.has_subcommands() {
                    visit(subcommand, &path, paths);
                }
            }
        }

        let mut paths = BTreeSet::new();
        visit(command, "", &mut paths);
        paths
    }

    fn keyed_paths(command: &Command, required: bool) -> BTreeSet<String> {
        fn visit(command: &Command, prefix: &str, required: bool, paths: &mut BTreeSet<String>) {
            for subcommand in command.get_subcommands() {
                let path = if prefix.is_empty() {
                    subcommand.get_name().to_owned()
                } else {
                    format!("{prefix} {}", subcommand.get_name())
                };
                if subcommand.get_arguments().any(|argument| {
                    argument.get_long() == Some("idempotency-key")
                        && (!required || argument.is_required_set())
                }) {
                    paths.insert(path.clone());
                }
                visit(subcommand, &path, required, paths);
            }
        }

        let mut paths = BTreeSet::new();
        visit(command, "", required, &mut paths);
        paths
    }
}
