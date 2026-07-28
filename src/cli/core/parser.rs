use std::{ffi::OsString, path::PathBuf, str::FromStr, time::Duration};

use clap::{ArgMatches, error::ErrorKind, parser::ValueSource};

use crate::{
    api::{
        http::core::decode_cursor,
        service::{
            Command, EventCursor, PromptCommand, PromptInput, Query, RetentionPeriod,
            RetentionPolicy,
        },
        stream::OpaqueStreamCursor,
    },
    domain::{
        events::{ApprovalDecision, SchemaVersion},
        ids::{ArtifactId, ProjectId, ThreadId},
    },
    store::sqlite::idempotency::IdempotencyKey,
};

use super::{ClientError, ClientErrorKind, ClientRequest, MutationRequest, PromptRequest};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
    Jsonl,
}

#[derive(Clone, Debug)]
pub struct Cli {
    pub format: OutputFormat,
    pub state_root: Option<PathBuf>,
    pub auto_start: bool,
    pub timeout: Duration,
    pub invocation: Invocation,
}

#[derive(Clone, Debug)]
pub enum Invocation {
    Daemon(DaemonCommand),
    Ui,
    Provider(crate::cli::provider::ProviderCommand),
    Client(Box<ClientRequest>),
    Exec(Box<crate::cli::exec::ExecRequest>),
    Repo(Box<crate::cli::repo::RepoRequest>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonCommand {
    pub daemonize: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationDescriptor {
    pub command: &'static str,
    pub service_operation: Option<&'static str>,
    pub openapi_operation_id: Option<&'static str>,
    pub output_schema: Option<&'static str>,
    pub mutation: bool,
    pub stream: bool,
}

pub const CLI_OPERATIONS: &[OperationDescriptor] = &[
    operation("daemon", None, None, None, false, false),
    operation("ui", None, None, None, false, false),
    operation("provider path", None, None, None, false, false),
    operation("provider list", None, None, None, false, false),
    operation("provider add", None, None, None, false, false),
    operation("provider use", None, None, None, false, false),
    operation(
        "project create",
        Some("project.create"),
        Some("createProject"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "project show",
        Some("project.get"),
        Some("getProject"),
        Some("Project"),
        false,
        false,
    ),
    operation(
        "thread create",
        Some("thread.create"),
        Some("createThread"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "thread list",
        Some("thread.list"),
        Some("listThreads"),
        Some("ThreadList"),
        false,
        false,
    ),
    operation(
        "thread show",
        Some("thread.get"),
        Some("getThread"),
        Some("Thread"),
        false,
        false,
    ),
    operation(
        "thread archive",
        Some("thread.archive"),
        Some("setThreadArchived"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "thread delete",
        Some("thread.delete.initiate"),
        Some("initiateThreadDeletion"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "deletion show",
        Some("thread.deletion.get"),
        Some("getDeletionJob"),
        Some("DeletionJob"),
        false,
        false,
    ),
    operation(
        "prompt",
        Some("run.start"),
        Some("startRun"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "run list",
        Some("run.list"),
        Some("listRuns"),
        Some("RunList"),
        false,
        false,
    ),
    operation(
        "run show",
        Some("run.get"),
        Some("getRun"),
        Some("Run"),
        false,
        false,
    ),
    operation(
        "run cost",
        Some("run.cost"),
        Some("getRunCost"),
        Some("RunCost"),
        false,
        false,
    ),
    operation(
        "run prompts",
        Some("run.prompts"),
        Some("getRunPrompts"),
        Some("RunPrompts"),
        false,
        false,
    ),
    operation(
        "run transcript",
        Some("run.transcript"),
        Some("getRunTranscript"),
        Some("RunTranscript"),
        false,
        false,
    ),
    operation(
        "run cancel",
        Some("run.cancel"),
        Some("cancelRun"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "run input",
        Some("run.input"),
        Some("provideRunInput"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "events follow --run",
        Some("run.timeline"),
        Some("getRunEvents"),
        Some("EventStreamFrame"),
        false,
        true,
    ),
    operation(
        "events follow --thread",
        Some("thread.events"),
        Some("getThreadEvents"),
        Some("EventStreamFrame"),
        false,
        true,
    ),
    operation(
        "events status",
        Some("event.cursor.status"),
        Some("getEventCursorStatus"),
        Some("CursorStatus"),
        false,
        false,
    ),
    operation(
        "approval list",
        Some("approval.pending"),
        Some("listPendingApprovals"),
        Some("ApprovalList"),
        false,
        false,
    ),
    operation(
        "approval resolve",
        Some("approval.resolve"),
        Some("resolveApproval"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "auth list",
        Some("auth.pending"),
        Some("listPendingAuthRequests"),
        Some("AuthRequestList"),
        false,
        false,
    ),
    operation(
        "auth resolve",
        Some("auth.resolve"),
        Some("resolveAuth"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "status",
        Some("service.status"),
        Some("getProjectStatus"),
        Some("ProjectStatus"),
        false,
        false,
    ),
    operation(
        "capability list",
        Some("capability.list"),
        Some("listCapabilities"),
        Some("CapabilityList"),
        false,
        false,
    ),
    operation(
        "artifact register",
        Some("artifact.metadata.register"),
        Some("registerArtifactMetadata"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "artifact show",
        Some("artifact.metadata.get"),
        Some("getArtifactMetadata"),
        Some("ArtifactMetadata"),
        false,
        false,
    ),
    operation(
        "retention show",
        Some("project.retention.get"),
        Some("getProjectRetention"),
        Some("ProjectRetention"),
        false,
        false,
    ),
    operation(
        "retention set",
        Some("project.retention.set"),
        Some("setProjectRetention"),
        Some("ResourceReceipt"),
        true,
        false,
    ),
    operation(
        "process list",
        Some("process.list"),
        Some("listProcesses"),
        Some("ProcessList"),
        false,
        false,
    ),
    operation(
        "process show",
        Some("process.get"),
        Some("getProcess"),
        Some("Process"),
        false,
        false,
    ),
    operation(
        "process cancel",
        Some("process.cancel"),
        Some("cancelProcess"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal allocate",
        Some("terminal.allocate"),
        Some("allocateTerminal"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal show",
        Some("terminal.get"),
        Some("getTerminal"),
        Some("Terminal"),
        false,
        false,
    ),
    operation(
        "terminal attach",
        Some("terminal.viewer.attach"),
        Some("attachTerminalViewer"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal writer-claim",
        Some("terminal.writer.claim"),
        Some("claimTerminalWriter"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal attachment-show",
        Some("terminal.attachment.get"),
        Some("getTerminalAttachment"),
        Some("Attachment"),
        false,
        false,
    ),
    operation(
        "terminal writer-renew",
        Some("terminal.writer.renew"),
        Some("renewTerminalWriter"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal writer-release",
        Some("terminal.writer.release"),
        Some("releaseTerminalWriter"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal input",
        Some("terminal.input"),
        Some("writeTerminalInput"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal input-resolve",
        Some("terminal.input.resolve"),
        Some("resolveTerminalInput"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal resize",
        Some("terminal.resize"),
        Some("resizeTerminal"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "terminal output",
        Some("terminal.output"),
        Some("readTerminalOutput"),
        Some("OutputPage"),
        false,
        false,
    ),
    operation(
        "terminal resizes",
        Some("terminal.resizes"),
        Some("readTerminalResizes"),
        Some("ResizePage"),
        false,
        false,
    ),
    operation(
        "terminal detach",
        Some("terminal.detach"),
        Some("detachTerminal"),
        Some("MutationReceipt"),
        true,
        false,
    ),
    operation(
        "executor events",
        Some("executor.events"),
        Some("listExecutorEvents"),
        Some("ExecutorEventPage"),
        false,
        false,
    ),
];

const fn operation(
    command: &'static str,
    service_operation: Option<&'static str>,
    openapi_operation_id: Option<&'static str>,
    output_schema: Option<&'static str>,
    mutation: bool,
    stream: bool,
) -> OperationDescriptor {
    OperationDescriptor {
        command,
        service_operation,
        openapi_operation_id,
        output_schema,
        mutation,
        stream,
    }
}

pub fn parse<I, S>(arguments: I) -> Result<Cli, ParseError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    let command = super::command_tree::command();
    if arguments.first().is_none_or(|value| {
        value.to_str().is_some_and(|value| {
            value.starts_with('-')
                || command
                    .get_subcommands()
                    .any(|subcommand| subcommand.get_name() == value)
        })
    }) {
        arguments.insert(0, OsString::from("kit"));
    }
    let matches = command
        .try_get_matches_from(arguments)
        .map_err(ParseError::lexical)?;
    let semantic_format = unambiguous_output_format(&matches).unwrap_or(OutputFormat::Human);
    let settings =
        settings(&matches).map_err(|message| ParseError::semantic(message, semantic_format))?;
    let idempotency_key = selected_leaf(&matches)
        .try_get_one::<String>("idempotency-key")
        .ok()
        .flatten()
        .map(|value| {
            IdempotencyKey::parse(value)
                .map_err(|_| format!("invalid value for --idempotency-key: {value}"))
        })
        .transpose()
        .map_err(|message| ParseError::semantic(message, settings.format))?;
    let invocation = invocation(&matches, &idempotency_key)
        .map_err(|message| ParseError::semantic(message, settings.format))?;
    let stream = matches!(&invocation, Invocation::Client(request) if request.is_stream());
    if settings.format == OutputFormat::Jsonl && !stream {
        return Err(ParseError::semantic(
            "--jsonl is only valid for event streams",
            settings.format,
        ));
    }
    Ok(Cli {
        format: settings.format,
        state_root: settings.state_root.map(PathBuf::from),
        auto_start: settings.auto_start,
        timeout: Duration::from_millis(settings.timeout_ms.unwrap_or(5_000)),
        invocation,
    })
}

fn invocation(
    matches: &ArgMatches,
    idempotency_key: &Option<IdempotencyKey>,
) -> Result<Invocation, String> {
    let (command, matches) = matches.subcommand().expect("Clap requires a command");
    match command {
        "daemon" => Ok(Invocation::Daemon(DaemonCommand {
            daemonize: matches.get_flag("daemonize"),
        })),
        "ui" => Ok(Invocation::Ui),
        "provider" => match subcommand(matches) {
            ("path", _) => Ok(Invocation::Provider(
                crate::cli::provider::ProviderCommand::Path,
            )),
            ("list", _) => Ok(Invocation::Provider(
                crate::cli::provider::ProviderCommand::List,
            )),
            ("use", matches) => Ok(Invocation::Provider(
                crate::cli::provider::ProviderCommand::Use {
                    name: string(matches, "name").to_owned(),
                },
            )),
            ("add", matches) => Ok(Invocation::Provider(
                crate::cli::provider::ProviderCommand::Add(Box::new(
                    crate::cli::provider::ProviderAdd {
                        name: string(matches, "name").to_owned(),
                        provider: crate::cli::provider::ProviderKind::parse(string(
                            matches, "provider",
                        )),
                        replace: matches.get_flag("replace"),
                        api_key_env: optional_string(matches, "api-key-env"),
                        auth_token_env: optional_string(matches, "auth-token-env"),
                        model: optional_string(matches, "model"),
                        base_url: optional_string(matches, "base-url"),
                        max_tokens: matches.get_one::<u32>("max-tokens").copied(),
                        version: optional_string(matches, "provider-version"),
                        beta: optional_string(matches, "beta"),
                        app_name: optional_string(matches, "app-name"),
                        site_url: optional_string(matches, "site-url"),
                        max_completion_tokens: matches
                            .get_one::<u32>("max-completion-tokens")
                            .copied(),
                        temperature: matches.get_one::<f32>("temperature").copied(),
                        reasoning_effort: optional_string(matches, "reasoning-effort"),
                    },
                )),
            )),
            _ => unreachable!(),
        },
        "project" => match subcommand(matches) {
            ("create", matches) => {
                let id =
                    optional::<ProjectId>(matches, "id", "--id")?.unwrap_or(generate_project()?);
                mutation(
                    Command::CreateProject {
                        schema_version: SchemaVersion::CURRENT,
                        project_id: id,
                    },
                    id,
                    idempotency_key,
                )
            }
            ("show", matches) => Ok(query(Query::GetProject {
                project_id: required_id(matches, "project", "project-id", "--project")?,
            })),
            _ => unreachable!(),
        },
        "thread" => match subcommand(matches) {
            ("create", matches) => {
                let project_id = required_id(matches, "project", "project-id", "--project")?;
                let thread_id =
                    optional::<ThreadId>(matches, "id", "--id")?.unwrap_or(generate_thread()?);
                mutation(
                    Command::CreateThread {
                        schema_version: SchemaVersion::CURRENT,
                        thread_id,
                        project_id,
                    },
                    thread_id,
                    idempotency_key,
                )
            }
            ("list", matches) => Ok(query(Query::ListThreads {
                project_id: required_id(matches, "project", "project-id", "--project")?,
            })),
            ("show", matches) => Ok(query(Query::GetThread {
                thread_id: required_id(matches, "thread", "thread-id", "--thread")?,
            })),
            ("archive", matches) => {
                let thread_id = required_id(matches, "thread", "thread-id", "--thread")?;
                mutation(
                    Command::SetThreadArchived {
                        schema_version: SchemaVersion::CURRENT,
                        thread_id,
                        archived: !matches.get_flag("undo"),
                        expected_version: number(matches, "version"),
                    },
                    thread_id,
                    idempotency_key,
                )
            }
            ("delete", matches) => {
                let thread_id = required_id(matches, "thread", "thread-id", "--thread")?;
                mutation(
                    Command::InitiateThreadDeletion {
                        schema_version: SchemaVersion::CURRENT,
                        thread_id,
                        expected_version: number(matches, "version"),
                    },
                    thread_id,
                    idempotency_key,
                )
            }
            _ => unreachable!(),
        },
        "deletion" => match subcommand(matches) {
            ("show", matches) => Ok(query(Query::GetDeletionJob {
                deletion_job_id: string(matches, "deletion-job").to_owned(),
            })),
            _ => unreachable!(),
        },
        "prompt" => prompt(matches, idempotency_key),
        "run" => match subcommand(matches) {
            ("start", matches) => prompt(matches, idempotency_key),
            ("list", matches) => Ok(query(Query::ListRuns {
                project_id: required_id(matches, "project", "project-id", "--project")?,
            })),
            ("show", matches) => Ok(query(Query::GetRun {
                run_id: required_id(matches, "run", "run-id", "--run")?,
            })),
            ("cost", matches) => Ok(query(Query::GetRunCost {
                run_id: required_id(matches, "run", "run-id", "--run")?,
            })),
            ("prompts", matches) => Ok(query(Query::GetRunPrompts {
                run_id: required_id(matches, "run", "run-id", "--run")?,
            })),
            ("transcript", matches) => Ok(query(Query::RunTranscript {
                run_id: required_id(matches, "run", "run-id", "--run")?,
            })),
            ("cancel", matches) => {
                let run_id = required_id(matches, "run", "run-id", "--run")?;
                mutation(
                    Command::CancelRun {
                        schema_version: SchemaVersion::CURRENT,
                        run_id,
                        expected_version: number(matches, "version"),
                    },
                    run_id,
                    idempotency_key,
                )
            }
            ("input", matches) => {
                let run_id = required_id(matches, "run", "run-id", "--run")?;
                mutation(
                    Command::ProvideRunInput {
                        schema_version: SchemaVersion::CURRENT,
                        run_id,
                        input: parsed(matches, "input", "--input")?,
                        expected_version: number(matches, "version"),
                    },
                    run_id,
                    idempotency_key,
                )
            }
            _ => unreachable!(),
        },
        "events" => match matches.subcommand() {
            Some(("follow", matches)) => events(matches),
            Some(("status", matches)) => Ok(query(Query::EventCursorStatus {
                project_id: required_id(matches, "project", "project-id", "--project")?,
                cursor: parse_page_cursor("--cursor", string(matches, "cursor"))?,
            })),
            None => events(matches),
            _ => unreachable!(),
        },
        "approval" => match subcommand(matches) {
            ("list", matches) => Ok(query(Query::PendingApprovals {
                project_id: required_id(matches, "project", "project-id", "--project")?,
            })),
            ("resolve", matches) => {
                let approval_id = required_id(matches, "approval", "approval-id", "--approval")?;
                let decision = match string(matches, "decision") {
                    "approved" | "approve" => ApprovalDecision::Approved,
                    "denied" | "deny" => ApprovalDecision::Denied,
                    _ => unreachable!("Clap validates approval decisions"),
                };
                mutation(
                    Command::ResolveApproval {
                        schema_version: SchemaVersion::CURRENT,
                        approval_id,
                        decision,
                        expected_version: number(matches, "version"),
                    },
                    approval_id,
                    idempotency_key,
                )
            }
            _ => unreachable!(),
        },
        "auth" => match subcommand(matches) {
            ("list", matches) => Ok(query(Query::PendingAuthRequests {
                project_id: required_id(matches, "project", "project-id", "--project")?,
            })),
            ("resolve", matches) => {
                let run_id = required_id(matches, "run", "run-id", "--run")?;
                let granted = matches!(string(matches, "granted"), "true" | "yes" | "grant");
                mutation(
                    Command::ResolveAuth {
                        schema_version: SchemaVersion::CURRENT,
                        run_id,
                        granted,
                        expected_version: number(matches, "version"),
                    },
                    run_id,
                    idempotency_key,
                )
            }
            _ => unreachable!(),
        },
        "status" => Ok(query(Query::Status {
            project_id: required_id(matches, "project", "project-id", "--project")?,
        })),
        "capability" => match subcommand(matches) {
            ("list", matches) => Ok(query(Query::ListCapabilities {
                project_id: required_id(matches, "project", "project-id", "--project")?,
            })),
            _ => unreachable!(),
        },
        "artifact" => match subcommand(matches) {
            ("register", matches) => {
                let artifact_id =
                    optional::<ArtifactId>(matches, "id", "--id")?.unwrap_or(generate_artifact()?);
                mutation(
                    Command::RegisterArtifactMetadata {
                        schema_version: SchemaVersion::CURRENT,
                        artifact_id,
                        project_id: required_id(matches, "project", "project-id", "--project")?,
                        reference: parsed(matches, "reference", "--reference")?,
                        media_type: string(matches, "media-type").to_owned(),
                        size: number(matches, "size"),
                    },
                    artifact_id,
                    idempotency_key,
                )
            }
            ("show", matches) => Ok(query(Query::GetArtifactMetadata {
                artifact_id: required_id(matches, "artifact", "artifact-id", "--artifact")?,
            })),
            _ => unreachable!(),
        },
        "retention" => match subcommand(matches) {
            ("show", matches) => Ok(query(Query::GetProjectRetention {
                project_id: required_id(matches, "project", "project-id", "--project")?,
            })),
            ("set", matches) => {
                let project_id = required_id(matches, "project", "project-id", "--project")?;
                mutation(
                    Command::SetProjectRetention {
                        schema_version: SchemaVersion::CURRENT,
                        project_id,
                        policy: RetentionPolicy {
                            event: retention(matches, "event")?,
                            transcript: retention(matches, "transcript")?,
                            terminal: retention(matches, "terminal")?,
                            artifact: retention(matches, "artifact")?,
                            experiment: retention(matches, "experiment")?,
                            backup: retention(matches, "backup")?,
                        },
                        expected_version: number(matches, "version"),
                    },
                    project_id,
                    idempotency_key,
                )
            }
            _ => unreachable!(),
        },
        "process" => match subcommand(matches) {
            ("list", matches) => Ok(Invocation::Exec(Box::new(
                crate::cli::exec::ExecRequest::list_processes(required_id(
                    matches,
                    "project",
                    "project-id",
                    "--project",
                )?),
            ))),
            ("show", matches) => Ok(Invocation::Exec(Box::new(
                crate::cli::exec::ExecRequest::get_process(required_id(
                    matches,
                    "process",
                    "process-id",
                    "--process",
                )?),
            ))),
            ("cancel", matches) => Ok(Invocation::Exec(Box::new(
                crate::cli::exec::ExecRequest::cancel_process(
                    required_id(matches, "process", "process-id", "--process")?,
                    request_key(idempotency_key)?,
                ),
            ))),
            _ => unreachable!(),
        },
        "terminal" => exec_terminal(subcommand(matches), idempotency_key),
        "executor" => match subcommand(matches) {
            ("events", matches) => Ok(Invocation::Exec(Box::new(
                crate::cli::exec::ExecRequest::events(
                    required_id(matches, "project", "project-id", "--project")?,
                    string(matches, "cursor"),
                ),
            ))),
            _ => unreachable!(),
        },
        "repo" => repo(subcommand(matches), idempotency_key),
        _ => unreachable!("Clap validates commands"),
    }
}

fn optional_string(matches: &ArgMatches, name: &str) -> Option<String> {
    matches.get_one::<String>(name).cloned()
}

fn exec_terminal(
    (action, matches): (&str, &ArgMatches),
    idempotency_key: &Option<IdempotencyKey>,
) -> Result<Invocation, String> {
    use crate::{api::http::exec::AllocateTerminalBody, cli::exec::ExecRequest};

    let request = match action {
        "allocate" => ExecRequest::allocate_terminal(
            required_id(matches, "process", "process-id", "--process")?,
            AllocateTerminalBody {
                columns: number(matches, "columns"),
                rows: number(matches, "rows"),
                max_output_bytes: usize::try_from(number::<u64>(matches, "max-output-bytes"))
                    .map_err(|_| "--max-output-bytes is too large".to_owned())?,
                max_output_age_millis: number(matches, "max-output-age-ms"),
            },
            request_key(idempotency_key)?,
        ),
        "show" => ExecRequest::get_terminal(required_id(
            matches,
            "terminal",
            "terminal-id",
            "--terminal",
        )?),
        "attach" => ExecRequest::attach_viewer(
            required_id(matches, "terminal", "terminal-id", "--terminal")?,
            request_key(idempotency_key)?,
        ),
        "writer-claim" => ExecRequest::claim_writer(
            required_id(matches, "terminal", "terminal-id", "--terminal")?,
            number(matches, "lease-ms"),
            request_key(idempotency_key)?,
        ),
        "attachment-show" => ExecRequest::get_attachment(string(matches, "attachment")),
        "writer-renew" => ExecRequest::renew_writer(
            string(matches, "attachment"),
            number(matches, "lease-ms"),
            request_key(idempotency_key)?,
        ),
        "writer-release" => ExecRequest::release_writer(
            string(matches, "attachment"),
            request_key(idempotency_key)?,
        ),
        "input" => ExecRequest::write_input_from(
            string(matches, "attachment"),
            input_source(string(matches, "input-file")),
            required_key(idempotency_key),
        ),
        "input-resolve" => ExecRequest::resolve_input(
            string(matches, "attachment"),
            match string(matches, "outcome") {
                "applied" => crate::api::http::exec::TerminalInputResolution::Applied,
                "not-applied" => crate::api::http::exec::TerminalInputResolution::NotApplied,
                _ => unreachable!("Clap validates terminal input outcomes"),
            },
            required_key(idempotency_key),
        ),
        "resize" => ExecRequest::resize(
            string(matches, "attachment"),
            number(matches, "columns"),
            number(matches, "rows"),
            request_key(idempotency_key)?,
        ),
        "output" => {
            ExecRequest::read_output(string(matches, "attachment"), string(matches, "cursor"))
        }
        "resizes" => {
            ExecRequest::read_resizes(string(matches, "attachment"), string(matches, "cursor"))
        }
        "detach" => {
            ExecRequest::detach(string(matches, "attachment"), request_key(idempotency_key)?)
        }
        _ => unreachable!(),
    };
    Ok(Invocation::Exec(Box::new(request)))
}

fn repo(
    (action, matches): (&str, &ArgMatches),
    idempotency_key: &Option<IdempotencyKey>,
) -> Result<Invocation, String> {
    use crate::{capabilities::native::NativeTool, cli::repo::RepoRequest};

    let request = match action {
        "status" => RepoRequest::status(),
        "revision" => {
            RepoRequest::revision(required_id(matches, "project", "project-id", "--project")?)
        }
        "capabilities" => {
            RepoRequest::capabilities(required_id(matches, "project", "project-id", "--project")?)
        }
        "result" => RepoRequest::result(string(matches, "result")),
        "events" => RepoRequest::events(string(matches, "result")),
        "artifact" => RepoRequest::artifact(string(matches, "artifact-ref")),
        "approval" => RepoRequest::approval(
            string(matches, "result"),
            string(matches, "decision") == "approved",
            request_key(idempotency_key)?,
        ),
        "cancel" => RepoRequest::cancel(string(matches, "result"), request_key(idempotency_key)?),
        action => {
            let tool = match action {
                "discover" => NativeTool::Discover,
                "search" => NativeTool::Search,
                "read" => NativeTool::Read,
                "edit" => NativeTool::Edit,
                "run" => NativeTool::Run,
                "check" => NativeTool::Check,
                _ => unreachable!(),
            };
            RepoRequest::invoke(
                required_id(matches, "project", "project-id", "--project")?,
                tool,
                repo_input_source(string(matches, "input-file")),
                matches!(tool, NativeTool::Edit | NativeTool::Run | NativeTool::Check)
                    .then(|| required_key(idempotency_key)),
            )
        }
    };
    Ok(Invocation::Repo(Box::new(request)))
}

fn events(matches: &ArgMatches) -> Result<Invocation, String> {
    let stream_cursor = matches
        .get_one::<String>("cursor")
        .cloned()
        .map(|value| parse_stream_cursor("--cursor", value))
        .transpose()?;
    let limit = usize::try_from(number::<u64>(matches, "limit"))
        .expect("Clap limits event page size to 1000");
    let query = if let Some(run) = matches.get_one::<String>("run") {
        Query::RunTimeline {
            run_id: parse_id("--run", run)?,
            after: EventCursor::START,
            limit,
        }
    } else {
        Query::ThreadEvents {
            thread_id: parse_id("--thread", string(matches, "thread"))?,
            after: EventCursor::START,
            limit,
        }
    };
    Ok(Invocation::Client(Box::new(ClientRequest::Query {
        operation: query.operation(),
        query,
        stream: true,
        stream_cursor,
    })))
}

fn prompt(
    matches: &ArgMatches,
    idempotency_key: &Option<IdempotencyKey>,
) -> Result<Invocation, String> {
    let positionals = matches
        .get_many::<String>("prompt-positionals")
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let named_thread = matches.get_one::<String>("thread");
    let named_input = matches.get_one::<String>("input");
    let named_message = matches.get_one::<String>("message");
    let (thread, positional_message) = if let Some(thread) = named_thread {
        if named_input.is_some() || named_message.is_some() {
            if !positionals.is_empty() {
                return Err("prompt accepts only one input".to_owned());
            }
            (thread.as_str(), None)
        } else {
            let [message] = positionals.as_slice() else {
                return Err("prompt requires a message or --input".to_owned());
            };
            (thread.as_str(), Some(*message))
        }
    } else {
        let Some((thread, remaining)) = positionals.split_first() else {
            return Err("prompt requires a thread identifier".to_owned());
        };
        if named_input.is_some() || named_message.is_some() {
            if !remaining.is_empty() {
                return Err("prompt accepts only one input".to_owned());
            }
            (*thread, None)
        } else {
            let [message] = remaining else {
                return Err("prompt requires a message or --input".to_owned());
            };
            (*thread, Some(*message))
        }
    };
    let input = if let Some(reference) = named_input {
        PromptInput::Artifact(parse_id("--input", reference)?)
    } else {
        PromptInput::Message(
            named_message
                .map(String::as_str)
                .or(positional_message)
                .expect("prompt input was checked")
                .to_owned(),
        )
    };
    Ok(Invocation::Client(Box::new(ClientRequest::Prompt(
        PromptRequest::new(
            PromptCommand {
                thread_id: parse_id("--thread", thread)?,
                run_id: optional(matches, "id", "--id")?,
                input,
                run_config: None,
                experiment_config: None,
            },
            request_key(idempotency_key)?,
        ),
    ))))
}

fn mutation(
    command: Command,
    resource_id: impl ToString,
    idempotency_key: &Option<IdempotencyKey>,
) -> Result<Invocation, String> {
    Ok(Invocation::Client(Box::new(ClientRequest::Mutation(
        MutationRequest::new(
            command,
            resource_id.to_string(),
            request_key(idempotency_key)?,
        ),
    ))))
}

fn query(query: Query) -> Invocation {
    Invocation::Client(Box::new(ClientRequest::Query {
        operation: query.operation(),
        query,
        stream: false,
        stream_cursor: None,
    }))
}

fn subcommand(matches: &ArgMatches) -> (&str, &ArgMatches) {
    matches.subcommand().expect("Clap requires a subcommand")
}

fn string<'a>(matches: &'a ArgMatches, id: &str) -> &'a str {
    matches
        .get_one::<String>(id)
        .expect("required/defaulted Clap value is present")
}

fn number<T>(matches: &ArgMatches, id: &str) -> T
where
    T: AnyNumber,
{
    matches
        .get_one::<T>(id)
        .copied()
        .expect("required/defaulted typed Clap value is present")
}

trait AnyNumber: Copy + Send + Sync + 'static {}
impl AnyNumber for u16 {}
impl AnyNumber for u64 {}

fn required_id<T>(
    matches: &ArgMatches,
    option: &str,
    positional: &str,
    label: &str,
) -> Result<T, String>
where
    T: FromStr,
{
    let value = matches
        .get_one::<String>(option)
        .or_else(|| matches.get_one::<String>(positional))
        .expect("Clap requires exactly one ID form");
    parse_id(label, value)
}

fn optional<T>(matches: &ArgMatches, id: &str, label: &str) -> Result<Option<T>, String>
where
    T: FromStr,
{
    matches
        .get_one::<String>(id)
        .map(|value| parse_id(label, value))
        .transpose()
}

fn parsed<T>(matches: &ArgMatches, id: &str, label: &str) -> Result<T, String>
where
    T: FromStr,
{
    parse_id(label, string(matches, id))
}

fn parse_id<T>(label: &str, value: &str) -> Result<T, String>
where
    T: FromStr,
{
    value
        .parse()
        .map_err(|_| format!("invalid value for {label}: {value}"))
}

fn retention(matches: &ArgMatches, id: &str) -> Result<RetentionPeriod, String> {
    let value = string(matches, id);
    if value == "forever" {
        return Ok(RetentionPeriod::Forever);
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .map(RetentionPeriod::ForMicros)
        .ok_or_else(|| format!("invalid value for --{id}: {value}"))
}

fn request_key(key: &Option<IdempotencyKey>) -> Result<IdempotencyKey, String> {
    key.clone().map_or_else(generate_idempotency_key, Ok)
}

fn required_key(key: &Option<IdempotencyKey>) -> IdempotencyKey {
    key.clone()
        .expect("Clap requires an idempotency key for this command")
}

fn input_source(path: &str) -> crate::cli::exec::InputSource {
    if path == "-" {
        crate::cli::exec::InputSource::Stdin
    } else {
        crate::cli::exec::InputSource::File(PathBuf::from(path))
    }
}

fn repo_input_source(path: &str) -> crate::cli::repo::InputSource {
    if path == "-" {
        crate::cli::repo::InputSource::Stdin
    } else {
        crate::cli::repo::InputSource::File(PathBuf::from(path))
    }
}

struct Settings {
    format: OutputFormat,
    state_root: Option<String>,
    auto_start: bool,
    timeout_ms: Option<u64>,
}

fn settings(matches: &ArgMatches) -> Result<Settings, String> {
    let json = global_values::<String>(matches, "json");
    let jsonl = global_values::<String>(matches, "jsonl");
    let format = global_values::<String>(matches, "format");
    if json.len() > 1 || jsonl.len() > 1 || format.len() > 1 {
        return Err("output format may only be specified once".to_owned());
    }
    if usize::from(!json.is_empty()) + usize::from(!jsonl.is_empty()) + format.len() > 1 {
        return Err("--json, --jsonl, and --format conflict".to_owned());
    }

    let state_root = singleton(matches, "state-root")?;
    let auto_start = !global_values::<bool>(matches, "auto-start").is_empty();
    let timeout_ms = singleton(matches, "timeout-ms")?;
    let format = if !json.is_empty() {
        OutputFormat::Json
    } else if !jsonl.is_empty() {
        OutputFormat::Jsonl
    } else if format.first().is_some_and(|format| format == "json") {
        OutputFormat::Json
    } else {
        OutputFormat::Human
    };
    Ok(Settings {
        format,
        state_root,
        auto_start,
        timeout_ms,
    })
}

fn unambiguous_output_format(matches: &ArgMatches) -> Option<OutputFormat> {
    let json = global_values::<String>(matches, "json");
    let jsonl = global_values::<String>(matches, "jsonl");
    let format = global_values::<String>(matches, "format");
    let selectors = usize::from(!json.is_empty())
        + usize::from(!jsonl.is_empty())
        + usize::from(!format.is_empty());
    match selectors {
        0 => Some(OutputFormat::Human),
        1 if !json.is_empty() => Some(OutputFormat::Json),
        1 if !jsonl.is_empty() => Some(OutputFormat::Jsonl),
        1 if format.iter().all(|value| value == "json") => Some(OutputFormat::Json),
        1 if format.iter().all(|value| value == "human") => Some(OutputFormat::Human),
        _ => None,
    }
}

fn selected_leaf(mut matches: &ArgMatches) -> &ArgMatches {
    while let Some((_, submatches)) = matches.subcommand() {
        matches = submatches;
    }
    matches
}

fn singleton<T>(matches: &ArgMatches, id: &str) -> Result<Option<T>, String>
where
    T: Clone + Send + Sync + 'static,
{
    let mut values = global_values(matches, id);
    if values.len() > 1 {
        return Err(format!("--{id} may only be specified once"));
    }
    Ok(values.pop())
}

fn global_values<T>(matches: &ArgMatches, id: &str) -> Vec<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn collect<T>(matches: &ArgMatches, id: &str, values: &mut Vec<T>)
    where
        T: Clone + Send + Sync + 'static,
    {
        for matched in matches.ids() {
            let matched = matched.as_str();
            if matches!(
                matched.strip_prefix("root-")
                    .or_else(|| matched.strip_prefix("group-"))
                    .or_else(|| matched.strip_prefix("leaf-")),
                Some(name) if name == id
            ) && matches.value_source(matched) == Some(ValueSource::CommandLine)
                && let Some(found) = matches.get_many::<T>(matched)
            {
                values.extend(found.cloned());
            }
        }
        if let Some((_, submatches)) = matches.subcommand() {
            collect(submatches, id, values);
        }
    }

    let mut values = Vec::new();
    collect(matches, id, &mut values);
    values
}

pub fn parity_table() -> String {
    let mut table = String::from("command | service operation | OpenAPI operationId | schema\n");
    for operation in CLI_OPERATIONS {
        table.push_str(&format!(
            "{} | {} | {} | {}\n",
            operation.command,
            operation.service_operation.unwrap_or("-"),
            operation.openapi_operation_id.unwrap_or("-"),
            operation.output_schema.unwrap_or("-"),
        ));
    }
    for operation in crate::cli::repo::REPO_CLI_OPERATIONS {
        table.push_str(&format!(
            "{} | {} | {} | -\n",
            operation.command, operation.service_operation, operation.openapi_operation_id,
        ));
    }
    table
}

fn parse_page_cursor(name: &str, value: &str) -> Result<EventCursor, String> {
    decode_cursor(value)
        .map(EventCursor::new)
        .ok_or_else(|| format!("invalid value for {name}: {value}"))
}

fn parse_stream_cursor(name: &str, value: String) -> Result<OpaqueStreamCursor, String> {
    OpaqueStreamCursor::parse(value.clone())
        .map_err(|_| format!("invalid value for {name}: {value}"))
}

fn generate_idempotency_key() -> Result<IdempotencyKey, String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| "secure randomness unavailable".to_owned())?;
    let value = random.iter().fold(String::from("cli-"), |mut value, byte| {
        value.push_str(&format!("{byte:02x}"));
        value
    });
    IdempotencyKey::parse(&value).map_err(|error| error.to_string())
}

fn generate_project() -> Result<ProjectId, String> {
    ProjectId::generate().map_err(|error| error.to_string())
}

fn generate_thread() -> Result<ThreadId, String> {
    ThreadId::generate().map_err(|error| error.to_string())
}

fn generate_artifact() -> Result<ArtifactId, String> {
    ArtifactId::generate().map_err(|error| error.to_string())
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    format: Option<OutputFormat>,
    error: clap::Error,
}

impl ParseError {
    fn lexical(error: clap::Error) -> Self {
        Self {
            message: error.to_string(),
            format: None,
            error,
        }
    }

    fn semantic(message: impl Into<String>, format: OutputFormat) -> Self {
        let message = message.into();
        Self {
            error: clap::Error::raw(ErrorKind::ValueValidation, &message),
            message,
            format: Some(format),
        }
    }

    pub fn output_format(&self) -> Option<OutputFormat> {
        self.format
    }

    pub fn into_clap(self) -> clap::Error {
        self.error
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for ClientError {
    fn from(error: ParseError) -> Self {
        ClientError::new(ClientErrorKind::Invalid, error.message)
    }
}
