use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::Mutex,
};

use jsonschema::Validator;
use kit::{
    api::{
        http::{core::ROUTES, exec::EXEC_ROUTES},
        service::{
            ArtifactMetadataProjection, CommandReceipt, CursorStatusProjection, EventCursor,
            EventPage, ProjectProjection, Query, QueryProjection, RunProjection,
            RunPromptProjection, RunTranscriptProjection, StatusProjection, ThreadProjection,
            handlers,
        },
    },
    cli::core::{
        CLI_OPERATIONS, Client, ClientError, ClientResponse, Invocation, OutputFormat,
        execute_with_retry, operation_route, parity_table, parse, read_discovery, render_response,
    },
    domain::{
        config::EffectiveConfigReference,
        events::{ArtifactRef, RunState},
        ids::{ArtifactId, PrincipalId, ProjectId, RunId, ThreadId},
    },
};
use serde_json::{Value, json};

const PROJECT: &str = "project_00000000000000000000000001";
const THREAD: &str = "thread_00000000000000000000000001";
const RUN: &str = "run_00000000000000000000000001";
const APPROVAL: &str = "approval_00000000000000000000000001";
const ARTIFACT: &str = "artifact_00000000000000000000000001";
const INPUT: &str = "blake3:0000000000000000000000000000000000000000000000000000000000000000";

#[test]
fn cli_service_and_openapi_have_zero_uncovered_operations() {
    let parity = parity_table();
    for operation in kit::cli::repo::REPO_CLI_OPERATIONS {
        assert!(
            parity.contains(&format!(
                "{} | {} | {} | -",
                operation.command, operation.service_operation, operation.openapi_operation_id
            )),
            "{parity}"
        );
    }
    let operations = CLI_OPERATIONS
        .iter()
        .filter(|descriptor| descriptor.service_operation.is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        CLI_OPERATIONS.len(),
        5 + ROUTES.len() + EXEC_ROUTES.len(),
        "{}",
        parity_table()
    );
    assert_eq!(
        operations.len(),
        ROUTES.len() + EXEC_ROUTES.len(),
        "{}",
        parity_table()
    );
    assert!(operations.iter().all(|descriptor| {
        descriptor.openapi_operation_id.is_some() && descriptor.output_schema.is_some()
    }));

    let mut service = handlers()
        .iter()
        .map(|descriptor| descriptor.operation)
        .collect::<BTreeSet<_>>();
    service.extend(EXEC_ROUTES.iter().map(|route| route.operation));
    let cli_service = CLI_OPERATIONS
        .iter()
        .filter_map(|descriptor| descriptor.service_operation)
        .collect::<BTreeSet<_>>();
    let document = openapi();
    let mut api = operation_ids(&document);
    let exec_document: Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.exec.yaml")).unwrap();
    api.extend(operation_ids(&exec_document));
    let cli_api = CLI_OPERATIONS
        .iter()
        .filter_map(|descriptor| descriptor.openapi_operation_id)
        .collect::<BTreeSet<_>>();
    let uncovered_cli_service = cli_service.difference(&service).collect::<Vec<_>>();
    let uncovered_cli_api = cli_api.difference(&api).collect::<Vec<_>>();
    let routes = ROUTES.iter().chain(EXEC_ROUTES);
    let uncovered_routes = routes
        .clone()
        .map(|route| route.operation)
        .filter(|operation| !cli_service.contains(operation))
        .collect::<Vec<_>>();
    let routed = routes
        .clone()
        .map(|route| route.operation)
        .collect::<BTreeSet<_>>();

    assert!(
        uncovered_cli_service.is_empty(),
        "{}\nuncovered service operations: {uncovered_cli_service:?}",
        parity_table()
    );
    assert!(
        uncovered_cli_api.is_empty(),
        "{}\nuncovered OpenAPI operationIds: {uncovered_cli_api:?}",
        parity_table()
    );
    assert!(
        uncovered_routes.is_empty(),
        "{}\nuncovered public command/query routes: {uncovered_routes:?}",
        parity_table()
    );
    assert_eq!(routed, cli_service, "{}", parity_table());
    assert_eq!(
        ROUTES.len() + EXEC_ROUTES.len(),
        routed.len(),
        "duplicate HTTP service route"
    );
    assert_eq!(
        ROUTES.len() + EXEC_ROUTES.len(),
        routed.len(),
        "public CLI transport must map every route"
    );
    assert!(
        operations.iter().all(|descriptor| operation_route(
            descriptor.service_operation.expect("filtered operation")
        )
        .is_some()),
        "{}",
        parity_table()
    );
    assert_eq!(cli_api.len(), operations.len(), "duplicate OpenAPI mapping");

    for route in ROUTES.iter().chain(EXEC_ROUTES) {
        let source = if EXEC_ROUTES.contains(route) {
            &exec_document
        } else {
            &document
        };
        let operation = &source["paths"][route.path][route.method.to_ascii_lowercase()];
        let operation_id = operation["operationId"].as_str().unwrap();
        let descriptor = operations
            .iter()
            .find(|descriptor| descriptor.service_operation == Some(route.operation))
            .unwrap();
        assert_eq!(descriptor.openapi_operation_id, Some(operation_id));
    }
}

#[test]
fn representative_rfc_commands_parse_to_registered_operations() {
    let cases = [
        vec!["kit", "daemon"],
        vec!["kit", "project", "create", "--id", PROJECT],
        vec!["kit", "project", "show", PROJECT],
        vec![
            "kit",
            "thread",
            "create",
            "--project",
            PROJECT,
            "--id",
            THREAD,
        ],
        vec!["kit", "thread", "list", "--project", PROJECT],
        vec!["kit", "thread", "show", THREAD],
        vec!["kit", "thread", "archive", THREAD, "--version", "1"],
        vec!["kit", "thread", "delete", THREAD, "--version", "1"],
        vec![
            "kit",
            "deletion",
            "show",
            "--deletion-job",
            "deletion_00000000000000000000000000000001",
        ],
        vec!["kit", "prompt", "--thread", THREAD, "ship this change"],
        vec![
            "kit", "run", "start", "--thread", THREAD, "--id", RUN, "--input", INPUT,
        ],
        vec!["kit", "run", "show", RUN],
        vec!["kit", "run", "cost", RUN],
        vec!["kit", "run", "prompts", RUN],
        vec!["kit", "run", "transcript", RUN],
        vec!["kit", "run", "list", "--project", PROJECT],
        vec!["kit", "run", "cancel", RUN, "--version", "1"],
        vec![
            "kit",
            "run",
            "input",
            RUN,
            "--input",
            INPUT,
            "--version",
            "1",
        ],
        vec!["kit", "events", "follow", "--thread", THREAD],
        vec![
            "kit",
            "events",
            "status",
            "--project",
            PROJECT,
            "--cursor",
            "cursor_0000000000000000",
        ],
        vec!["kit", "approval", "list", "--project", PROJECT],
        vec![
            "kit",
            "events",
            "--follow",
            "--run",
            RUN,
            "--cursor",
            "kitc1_000000000000000000000000000000000000000000000000",
            "--jsonl",
        ],
        vec!["kit", "auth", "list", "--project", PROJECT],
        vec![
            "kit",
            "approval",
            "resolve",
            APPROVAL,
            "--decision",
            "approved",
            "--version",
            "1",
        ],
        vec![
            "kit",
            "auth",
            "resolve",
            RUN,
            "--granted",
            "true",
            "--version",
            "1",
        ],
        vec!["kit", "status", "--project", PROJECT, "--format", "json"],
        vec!["kit", "capability", "list", "--project", PROJECT],
        vec![
            "kit",
            "artifact",
            "register",
            "--id",
            ARTIFACT,
            "--project",
            PROJECT,
            "--reference",
            INPUT,
            "--media-type",
            "application/octet-stream",
            "--size",
            "0",
        ],
        vec!["kit", "artifact", "show", ARTIFACT],
        vec!["kit", "retention", "show", "--project", PROJECT],
        vec![
            "kit",
            "retention",
            "set",
            "--project",
            PROJECT,
            "--version",
            "1",
            "--event",
            "forever",
            "--transcript",
            "forever",
            "--terminal",
            "forever",
            "--artifact",
            "forever",
            "--experiment",
            "forever",
            "--backup",
            "forever",
        ],
    ];
    let registered = CLI_OPERATIONS
        .iter()
        .filter_map(|descriptor| descriptor.service_operation)
        .collect::<BTreeSet<_>>();
    for case in cases {
        let cli = parse(case).unwrap();
        if let Invocation::Client(request) = cli.invocation {
            assert!(
                registered.contains(request.operation()),
                "{}",
                parity_table()
            );
        }
    }
}

#[test]
fn follow_and_page_cursors_are_distinct_cli_contracts() {
    assert!(
        parse([
            "kit",
            "events",
            "follow",
            "--thread",
            THREAD,
            "--cursor",
            "cursor_0000000000000000",
        ])
        .is_err()
    );
    assert!(
        parse([
            "kit",
            "events",
            "status",
            "--project",
            PROJECT,
            "--cursor",
            "kitc1_000000000000000000000000000000000000000000000000",
        ])
        .is_err()
    );
}

#[test]
fn mutation_retry_retains_one_idempotency_key() {
    #[derive(Default)]
    struct RetryClient {
        keys: Vec<String>,
    }
    impl Client for RetryClient {
        fn execute(
            &mut self,
            request: &kit::cli::core::MutationRequest,
        ) -> Result<CommandReceipt, ClientError> {
            self.keys.push(request.idempotency_key().to_string());
            if self.keys.len() < 3 {
                Err(ClientError::unavailable("retry"))
            } else {
                Ok(CommandReceipt {
                    operation: request.operation,
                    commit_positions: Vec::new(),
                    replayed: false,
                })
            }
        }

        fn query(&mut self, _: Query) -> Result<QueryProjection, ClientError> {
            unreachable!()
        }
    }

    let cli = parse(["kit", "project", "create", "--id", PROJECT]).unwrap();
    let Invocation::Client(request) = cli.invocation else {
        panic!("expected client request")
    };
    let mut client = RetryClient::default();
    execute_with_retry(&mut client, &request, 3).unwrap();
    assert_eq!(client.keys.len(), 3);
    assert!(client.keys.windows(2).all(|keys| keys[0] == keys[1]));
}

#[test]
fn prompt_retry_retains_one_idempotency_key() {
    #[derive(Default)]
    struct RetryClient {
        keys: Vec<String>,
    }
    impl Client for RetryClient {
        fn execute(
            &mut self,
            _: &kit::cli::core::MutationRequest,
        ) -> Result<CommandReceipt, ClientError> {
            unreachable!()
        }

        fn prompt(
            &mut self,
            request: &kit::cli::core::PromptRequest,
        ) -> Result<kit::api::service::PromptReceipt, ClientError> {
            self.keys.push(request.idempotency_key().to_string());
            if self.keys.len() < 3 {
                Err(ClientError::unavailable("retry"))
            } else {
                Ok(kit::api::service::PromptReceipt {
                    run_id: RunId::parse(RUN).unwrap(),
                    receipt: CommandReceipt {
                        operation: "run.start",
                        commit_positions: Vec::new(),
                        replayed: false,
                    },
                })
            }
        }

        fn query(&mut self, _: Query) -> Result<QueryProjection, ClientError> {
            unreachable!()
        }
    }

    let cli = parse(["kit", "prompt", "--thread", THREAD, "retry me"]).unwrap();
    let Invocation::Client(request) = cli.invocation else {
        panic!("expected client request")
    };
    let mut client = RetryClient::default();
    execute_with_retry(&mut client, &request, 3).unwrap();
    assert_eq!(client.keys.len(), 3);
    assert!(client.keys.windows(2).all(|keys| keys[0] == keys[1]));
}

#[test]
fn json_output_uses_openapi_component_envelopes() {
    let document = openapi();
    let query = |projection| ClientResponse::Query(Box::new(projection));
    let responses = BTreeMap::from([
        (
            "ResourceReceipt",
            ClientResponse::Mutation {
                resource_id: PROJECT.to_owned(),
                receipt: CommandReceipt {
                    operation: "project.create",
                    commit_positions: Vec::new(),
                    replayed: false,
                },
            },
        ),
        (
            "Project",
            query(QueryProjection::Project(ProjectProjection {
                id: ProjectId::parse(PROJECT).unwrap(),
                principal_id: PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
                retention: None,
                version: 1,
            })),
        ),
        (
            "Thread",
            query(QueryProjection::Thread(ThreadProjection {
                id: ThreadId::parse(THREAD).unwrap(),
                project_id: ProjectId::parse(PROJECT).unwrap(),
                archived: false,
                deletion_requested: false,
                version: 1,
            })),
        ),
        (
            "Run",
            query(QueryProjection::Run(RunProjection {
                id: RunId::parse(RUN).unwrap(),
                thread_id: ThreadId::parse(THREAD).unwrap(),
                state: RunState::Queued,
                input: ArtifactRef::parse(INPUT).unwrap(),
                auth_granted: None,
                effective_config: EffectiveConfigReference {
                    digest: format!("sha256:{}", "0".repeat(64)),
                    experiment_identity: kit::domain::config::GRAMMAR_EDIT_EXPERIMENT_ID.to_owned(),
                    experiment_digest: format!("sha256:{}", "0".repeat(64)),
                    provenance: BTreeMap::new(),
                },
                owner: None,
                output: None,
                failure: None,
                version: 1,
            })),
        ),
        ("RunCost", query(QueryProjection::RunCost(Box::default()))),
        (
            "RunPrompts",
            query(QueryProjection::RunPrompts(RunPromptProjection::default())),
        ),
        (
            "RunTranscript",
            query(QueryProjection::RunTranscript(RunTranscriptProjection {
                run_id: RunId::parse(RUN).unwrap(),
                items: Vec::new(),
            })),
        ),
        (
            "ProjectStatus",
            query(QueryProjection::Status(StatusProjection {
                committed: EventCursor::START,
                ready: true,
            })),
        ),
        (
            "CursorStatus",
            query(QueryProjection::CursorStatus(CursorStatusProjection {
                requested: EventCursor::START,
                committed: EventCursor::START,
                caught_up: true,
            })),
        ),
        (
            "EventPage",
            query(QueryProjection::Events(EventPage {
                events: Vec::new(),
                next_cursor: EventCursor::START,
            })),
        ),
        ("ThreadList", query(QueryProjection::Threads(Vec::new()))),
        ("RunList", query(QueryProjection::Runs(Vec::new()))),
        (
            "ApprovalList",
            query(QueryProjection::Approvals(Vec::new())),
        ),
        (
            "AuthRequestList",
            query(QueryProjection::AuthRequests(Vec::new())),
        ),
        (
            "CapabilityList",
            query(QueryProjection::Capabilities(Vec::new())),
        ),
        (
            "ArtifactMetadata",
            query(QueryProjection::ArtifactMetadata(
                ArtifactMetadataProjection {
                    id: ArtifactId::parse(ARTIFACT).unwrap(),
                    project_id: ProjectId::parse(PROJECT).unwrap(),
                    reference: ArtifactRef::parse(INPUT).unwrap(),
                    media_type: "application/octet-stream".to_owned(),
                    size: 0,
                },
            )),
        ),
        ("ProjectRetention", query(QueryProjection::Retention(None))),
        (
            "DeletionJob",
            query(QueryProjection::DeletionJob(json!({
                "id": "deletion_00000000000000000000000000000001",
                "state": "requested",
                "blockers": [],
                "fence": 1,
                "requested_at_unix_micros": 0
            }))),
        ),
    ]);

    for descriptor in CLI_OPERATIONS {
        let Some(schema) = descriptor.output_schema else {
            continue;
        };
        if descriptor.stream {
            let value = json!({
                "cursor": "kitc1_000000000000000000000000000000000000000000000000",
                "project_id": PROJECT,
                "operation": "thread.create",
                "stream": THREAD,
                "payload": { "thread_id": THREAD, "project_id": PROJECT },
                "schema_version": 1,
            });
            schema_validator(&document, schema)
                .validate(&value)
                .unwrap_or_else(|error| panic!("stream output failed {schema}: {error}: {value}"));
            continue;
        }
        if descriptor
            .service_operation
            .is_some_and(|operation| EXEC_ROUTES.iter().any(|route| route.operation == operation))
        {
            continue;
        }
        let response = if schema == "ResourceReceipt" {
            responses["ResourceReceipt"].clone()
        } else {
            responses
                .get(schema)
                .unwrap_or_else(|| panic!("missing output fixture for {schema}"))
                .clone()
        };
        let output = render_response(response, OutputFormat::Json).unwrap();
        let value: Value = serde_json::from_str(output.stdout.trim()).unwrap();
        schema_validator(&document, schema)
            .validate(&value)
            .unwrap_or_else(|error| {
                panic!(
                    "{} output failed {schema}: {error}: {value}",
                    descriptor.command
                )
            });
    }
}

#[cfg(unix)]
#[test]
fn discovery_rejects_group_readable_credentials() {
    use std::os::unix::fs::PermissionsExt;

    static SERIAL: Mutex<()> = Mutex::new(());
    let _guard = SERIAL.lock().unwrap();
    let root = std::env::temp_dir().join(format!("kit-cli-discovery-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).unwrap();
    let path = root.join("daemon.json");
    fs::write(
        &path,
        br#"{"endpoint":"http://127.0.0.1:9137","credential":"secret"}"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_discovery(&root).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        read_discovery(&root).unwrap().endpoint,
        "http://127.0.0.1:9137"
    );
    fs::remove_dir_all(root).unwrap();
}

fn openapi() -> Value {
    serde_yaml::from_str(include_str!("../../docs/api/openapi.yaml")).unwrap()
}

fn operation_ids(value: &Value) -> BTreeSet<&str> {
    let mut ids = BTreeSet::new();
    collect_operation_ids(value, &mut ids);
    ids
}

fn collect_operation_ids<'a>(value: &'a Value, ids: &mut BTreeSet<&'a str>) {
    match value {
        Value::Object(object) => {
            if let Some(id) = object.get("operationId").and_then(Value::as_str) {
                ids.insert(id);
            }
            for value in object.values() {
                collect_operation_ids(value, ids);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_operation_ids(value, ids);
            }
        }
        _ => {}
    }
}

fn schema_validator(document: &Value, schema: &str) -> Validator {
    jsonschema::draft202012::options()
        .build(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "components": document["components"],
            "$ref": format!("#/components/schemas/{schema}"),
        }))
        .unwrap()
}
