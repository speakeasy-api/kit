#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Read},
    os::unix::fs::PermissionsExt,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use kit::{
    api::service::{EventCursor, Query, QueryProjection},
    cli::core::{Client, ClientErrorKind, HttpClient, read_discovery},
    domain::ids::{RunId, ThreadId},
    runtime::daemon::TELEMETRY_FILE,
    telemetry::otel::DurableLocalExporter,
};
use serde_json::{Value, json};

const THREAD: &str = "thread_00000000000000000000000001";
const OTHER_THREAD: &str = "thread_00000000000000000000000002";
const AUTO_PROJECT: &str = "project_00000000000000000000000001";

#[test]
fn parse_errors_preserve_authoritative_json_format() {
    let output = kit_command()
        .args(["--json", "project", "show", "bad-id"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let problem: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(problem["type"], "/problems/invalid_request");
    assert_eq!(problem["status"], 400);
    assert_eq!(problem["code"], "invalid_request");

    let malformed = kit_command()
        .args(["repo", "edti", "--json"])
        .output()
        .unwrap();
    assert!(serde_json::from_slice::<Value>(&malformed.stderr).is_err());
    assert!(String::from_utf8_lossy(&malformed.stderr).starts_with("error: "));
}

#[test]
fn daemon_cli_rejects_raw_mcp_secret_configuration_before_startup() {
    let root = TestRoot::new("mcp-config-contract");
    let config = root.0.join("config.json");
    fs::create_dir_all(&root.0).unwrap();
    fs::write(
        &config,
        br#"{"current":"local","providers":{"local":{"provider":"ollama","model":"test"}},"mcp_servers":[{"id":"docs","transport":{"kind":"http","endpoint":"https://example.com/mcp"},"owner":{"principal_id":"principal_00000000000000000000000001","project_id":"project_00000000000000000000000001"},"source":"mcp.docs","trust_domain":"example.com","namespace":"docs","version":"1","credential":"raw-secret","egress":{"scheme":"https","host":"example.com","port":443},"descriptors":[]}]}"#,
    )
    .unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let output = kit_command()
        .args(["daemon", "--state-root"])
        .arg(&root.0)
        .env("KIT_CONFIG_FILE", &config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("persistent MCP configuration: invalid provider config")
    );
    assert!(!root.0.join("daemon.json").exists());
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        Self(
            std::env::temp_dir().join(format!("kit-cli-{label}-{}-{sequence}", std::process::id())),
        )
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        terminate_daemons(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cli_uses_daemon_http_and_persists_across_restart() {
    let root = TestRoot::new("transport");
    let mut daemon = start_daemon(&root.0);
    wait_for_discovery(&root.0, &mut daemon);
    let identity: Value = serde_json::from_slice(
        &fs::read(root.0.join("daemon-identity.json")).expect("read daemon identity"),
    )
    .expect("parse daemon identity");
    let project = identity["project_id"].as_str().expect("project id");

    let created = cli(&root.0, &["--json", "project", "create", "--id", project]);
    assert_success(&created);
    assert_eq!(json_output(&created)["resource"]["id"], project);

    let shown = cli(&root.0, &["--json", "project", "show", project]);
    assert_success(&shown);
    assert_eq!(json_output(&shown)["id"], project);

    let thread = cli(
        &root.0,
        &[
            "--json",
            "thread",
            "create",
            "--project",
            project,
            "--id",
            THREAD,
        ],
    );
    assert_success(&thread);
    assert_success(&cli(
        &root.0,
        &[
            "--json",
            "thread",
            "create",
            "--project",
            project,
            "--id",
            OTHER_THREAD,
        ],
    ));

    let mut follow = kit_command()
        .args([
            "--jsonl",
            "events",
            "follow",
            "--thread",
            THREAD,
            "--timeout-ms",
            "5000",
            "--state-root",
        ])
        .arg(&root.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn event follower");
    let stdout = follow.stdout.take().expect("event follower stdout");
    let (lines, received_lines) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let heartbeat = match received_lines.recv_timeout(Duration::from_secs(3)) {
        Ok(line) => line.expect("read event follower heartbeat"),
        Err(error) => {
            let _ = follow.wait();
            let mut stderr = String::new();
            follow
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("event follower did not connect: {error}: {stderr}");
        }
    };
    assert_eq!(
        serde_json::from_str::<Value>(&heartbeat).unwrap()["event"],
        "heartbeat"
    );

    assert_success(&cli(
        &root.0,
        &[
            "--json",
            "thread",
            "archive",
            OTHER_THREAD,
            "--version",
            "1",
        ],
    ));
    let archived = cli(
        &root.0,
        &["--json", "thread", "archive", THREAD, "--version", "1"],
    );
    assert_success(&archived);
    assert_eq!(
        json_output(&archived)["receipt"]["operation"],
        "thread.archive"
    );
    let event = loop {
        let line = received_lines
            .recv_timeout(Duration::from_secs(3))
            .expect("event follower did not receive committed mutation")
            .expect("read event follower frame");
        let event: Value = serde_json::from_str(&line).unwrap();
        if event.get("operation").is_some() {
            break event;
        }
    };
    assert_eq!(event["operation"], "thread.archive");
    assert_eq!(event["stream"], THREAD);
    let cursor = event["cursor"].as_str().unwrap().to_owned();
    follow.kill().expect("stop event follower");
    follow.wait().expect("wait event follower");
    reader.join().expect("join event follower reader");

    let mut resumed = kit_command()
        .args([
            "--jsonl",
            "events",
            "follow",
            "--thread",
            THREAD,
            "--cursor",
            &cursor,
            "--timeout-ms",
            "5000",
            "--state-root",
        ])
        .arg(&root.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn resumed event follower");
    let mut resumed_lines = BufReader::new(resumed.stdout.take().unwrap()).lines();
    let heartbeat = resumed_lines.next().unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&heartbeat).unwrap()["event"],
        "heartbeat"
    );
    assert_success(&cli(
        &root.0,
        &[
            "--json",
            "thread",
            "archive",
            THREAD,
            "--version",
            "2",
            "--undo",
        ],
    ));
    let resumed_event = loop {
        let event: Value = serde_json::from_str(&resumed_lines.next().unwrap().unwrap()).unwrap();
        if event.get("operation").is_some() {
            break event;
        }
    };
    assert_eq!(resumed_event["operation"], "thread.archive");
    assert_eq!(resumed_event["stream"], THREAD);
    assert_ne!(resumed_event["cursor"], cursor);
    resumed.kill().expect("stop resumed event follower");
    resumed.wait().expect("wait resumed event follower");

    let retained = cli(
        &root.0,
        &[
            "--json",
            "retention",
            "set",
            "--project",
            project,
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
    );
    assert_success(&retained);
    assert_eq!(
        json_output(&retained)["receipt"]["operation"],
        "project.retention.set"
    );

    let deleted = cli(
        &root.0,
        &["--json", "thread", "delete", THREAD, "--version", "3"],
    );
    assert_success(&deleted);
    assert_eq!(
        json_output(&deleted)["receipt"]["operation"],
        "thread.delete.initiate"
    );
    let deleted_thread = cli(&root.0, &["--json", "thread", "show", THREAD]);
    assert_success(&deleted_thread);
    assert_eq!(json_output(&deleted_thread)["deletion_requested"], true);

    let status = cli(&root.0, &["--json", "status", "--project", project]);
    assert_success(&status);
    assert_eq!(json_output(&status)["ready"], true);

    let discovery = read_discovery(&root.0).expect("valid discovery");
    write_discovery(
        &root.0,
        &json!({ "endpoint": discovery.endpoint, "credential": "wrong" }),
    );
    let unauthorized = cli(&root.0, &["project", "show", project]);
    assert_eq!(
        unauthorized.status.code(),
        Some(4),
        "{}",
        diagnostic(&unauthorized)
    );

    write_discovery(
        &root.0,
        &json!({ "endpoint": "http://example.com:80", "credential": "wrong" }),
    );
    let invalid = cli(&root.0, &["project", "show", project]);
    assert_eq!(invalid.status.code(), Some(2), "{}", diagnostic(&invalid));

    stop(&mut daemon);
    let mut restarted = start_daemon(&root.0);
    wait_until_success(
        &root.0,
        &mut restarted,
        &["--json", "project", "show", project],
    );
    let persisted = cli(&root.0, &["--json", "thread", "show", THREAD]);
    assert_success(&persisted);
    assert_eq!(json_output(&persisted)["project_id"], project);
    stop(&mut restarted);
}

#[test]
fn cli_auto_start_is_opt_in_and_bounded() {
    let root = TestRoot::new("autostart");
    let disabled = cli(&root.0, &["project", "show", AUTO_PROJECT]);
    assert_eq!(disabled.status.code(), Some(6), "{}", diagnostic(&disabled));

    let started = cli(
        &root.0,
        &[
            "--auto-start",
            "--json",
            "project",
            "create",
            "--id",
            AUTO_PROJECT,
        ],
    );
    assert_success(&started);
    assert!(read_discovery(&root.0).is_ok());
    let database =
        rusqlite::Connection::open(root.0.join("state.sqlite3")).expect("open auto-start database");
    let creates: u64 = database
        .query_row(
            "SELECT count(*) FROM events WHERE event_type = 'project.create'",
            [],
            |row| row.get(0),
        )
        .expect("read auto-start side effect");
    assert_eq!(creates, 1);
    let (payload, causation_id): (Vec<u8>, String) = database
        .query_row(
            "SELECT payload, causation_id FROM events WHERE event_type = 'project.create'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read persisted command event");
    let payload: Value = serde_json::from_slice(&payload).expect("parse persisted command event");
    assert_eq!(payload["command"]["schema_version"], 1);
    assert!(causation_id.starts_with("cmd_"));
    drop(database);
    terminate_daemons(&root.0);

    let daemonized_root = TestRoot::new("daemonized");
    let daemonized = cli(&daemonized_root.0, &["daemon", "--daemonize"]);
    assert_success(&daemonized);
    assert!(read_discovery(&daemonized_root.0).is_ok());
    terminate_daemons(&daemonized_root.0);
}

#[test]
fn newly_created_project_can_create_thread() {
    let root = TestRoot::new("project-thread-route");
    fs::create_dir_all(&root.0).unwrap();
    let state_root = fs::canonicalize(&root.0).unwrap();
    let mut daemon = OwnedDaemon::start(&state_root);
    wait_for_discovery(&state_root, &mut daemon.child);

    let created = cli(&state_root, &["--json", "project", "create"]);
    assert_success(&created);
    let project = json_output(&created)["resource"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let thread = cli(
        &state_root,
        &["--json", "thread", "create", "--project", &project],
    );
    assert_success(&thread);
}

#[test]
fn concurrent_auto_start_replaces_sigkill_stale_discovery_once() {
    let root = TestRoot::new("stale-autostart");
    let mut daemon = start_daemon(&root.0);
    wait_for_discovery(&root.0, &mut daemon);
    let identity: Value = serde_json::from_slice(
        &fs::read(root.0.join("daemon-identity.json")).expect("read daemon identity"),
    )
    .expect("parse daemon identity");
    let project = identity["project_id"]
        .as_str()
        .expect("project id")
        .to_owned();
    assert_success(&cli(
        &root.0,
        &["--json", "project", "create", "--id", &project],
    ));

    assert!(
        Command::new("kill")
            .args(["-KILL", &daemon.id().to_string()])
            .status()
            .expect("SIGKILL stale daemon")
            .success()
    );
    daemon.wait().expect("reap stale daemon");
    assert!(
        read_discovery(&root.0).is_ok(),
        "SIGKILL must leave discovery stale"
    );

    let clients = (0..8)
        .map(|_| {
            let mut command = kit_command();
            command
                .args(["--auto-start", "--json", "project", "show", &project])
                .arg("--state-root")
                .arg(&root.0)
                .env("KIT_PROJECT_ROOT", root.0.join("unconfigured-project"))
                .args(["--timeout-ms", "5000"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn concurrent auto-start client")
        })
        .collect::<Vec<_>>();
    for client in clients {
        assert_success(
            &client
                .wait_with_output()
                .expect("collect concurrent auto-start client"),
        );
    }

    assert_eq!(daemon_processes(&root.0).len(), 1);
}

#[test]
fn auto_start_replaces_daemon_from_a_previous_build() {
    let root = TestRoot::new("stale-binary");
    let mut daemon = start_daemon(&root.0);
    wait_for_discovery(&root.0, &mut daemon);
    let old_pid = daemon.id();
    let recorded: Value =
        serde_json::from_slice(&fs::read(root.0.join("daemon.json")).expect("read discovery"))
            .expect("parse discovery");
    assert_eq!(recorded["pid"].as_u64(), Some(u64::from(old_pid)));
    let identity = recorded["executable"]
        .as_object()
        .expect("daemon must record its executable identity");
    let mut stale = recorded.clone();
    stale["executable"]["modified_unix_micros"] = json!(
        identity["modified_unix_micros"]
            .as_u64()
            .expect("modified micros")
            + 1
    );
    write_discovery(&root.0, &stale);

    let daemon_identity: Value = serde_json::from_slice(
        &fs::read(root.0.join("daemon-identity.json")).expect("read daemon identity"),
    )
    .expect("parse daemon identity");
    let project = daemon_identity["project_id"].as_str().expect("project id");
    // The CLI's exit probe (kill 0) must observe the SIGTERMed daemon
    // disappearing, so reap the child promptly instead of leaving a zombie.
    let reaper = thread::spawn(move || wait_for_exit(&mut daemon, Duration::from_secs(30), "stale daemon"));
    let replaced = cli(
        &root.0,
        &["--auto-start", "--json", "project", "create", "--id", project],
    );
    assert_success(&replaced);

    reaper.join().expect("reap stale daemon");
    let processes = daemon_processes(&root.0);
    assert_eq!(
        processes.len(),
        1,
        "exactly one daemon survives stale replacement"
    );
    assert_ne!(processes[0].0, old_pid, "stale daemon must be replaced");
    let refreshed: Value = serde_json::from_slice(
        &fs::read(root.0.join("daemon.json")).expect("read refreshed discovery"),
    )
    .expect("parse refreshed discovery");
    assert_eq!(
        refreshed["executable"], recorded["executable"],
        "replacement daemon records the live binary identity"
    );
    terminate_daemons(&root.0);
}

#[test]
fn daemonized_process_starts_a_detached_unix_session() {
    let root = TestRoot::new("detached-session");
    assert_success(&cli(&root.0, &["daemon", "--daemonize"]));

    let processes = daemon_processes(&root.0);
    assert_eq!(processes.len(), 1);
    let (pid, parent, group, session) = processes[0];
    assert_eq!(group, pid, "daemon must lead its own process group");
    assert_eq!(session, pid, "daemon must lead its own session");
    assert_eq!(parent, 1, "daemonized process must be reparented");
}

#[test]
fn prompt_runs_to_completion_through_daemon_and_cli() {
    let root = TestRoot::new("prompt");
    let mut daemon = start_daemon(&root.0);
    wait_for_discovery(&root.0, &mut daemon);
    initialize_thread(&root.0);

    let prompt = cli(&root.0, &["--json", "prompt", "hello", "--thread", THREAD]);
    assert_success(&prompt);
    let receipt = json_output(&prompt);
    assert_eq!(receipt["receipt"]["operation"], "run.start");
    let run_id = receipt["resource"]["id"].as_str().unwrap().to_owned();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let shown = cli(&root.0, &["--json", "run", "show", "--run", &run_id]);
        assert_success(&shown);
        let state = json_output(&shown)["state"].as_str().unwrap().to_owned();
        if state == "completed" {
            break;
        }
        assert!(!matches!(state.as_str(), "failed" | "cancelled"));
        assert!(Instant::now() < deadline, "prompt run timed out in {state}");
        thread::sleep(Duration::from_millis(20));
    }
    let shown = cli(&root.0, &["--json", "run", "show", "--run", &run_id]);
    assert_eq!(
        json_output(&shown)["output"]["preview"],
        "hello from kit's deterministic development provider"
    );
    let cost = cli(&root.0, &["--json", "run", "cost", "--run", &run_id]);
    assert_success(&cost);
    assert_eq!(
        json_output(&cost)["usage"]["categories"]["uncached_input"]["billed_tokens"],
        4
    );
    assert_eq!(json_output(&cost)["cost"]["effective"]["micros"], 6);
    let prompts = cli(&root.0, &["--json", "run", "prompts", "--run", &run_id]);
    assert_success(&prompts);
    let prompts = json_output(&prompts);
    assert!(
        prompts["first_dynamic_byte"].as_u64().unwrap()
            < prompts["context_bytes"].as_u64().unwrap()
    );
    assert!(
        prompts["estimated_tokens"].as_u64().unwrap() <= prompts["token_budget"].as_u64().unwrap()
    );
    let transcript = cli(&root.0, &["--json", "run", "transcript", "--run", &run_id]);
    assert_success(&transcript);
    let transcript = String::from_utf8(transcript.stdout.clone()).unwrap();
    assert!(transcript.contains("hello from kit's deterministic development provider"));
    assert!(!transcript.contains("SECRET_CHAIN_OF_THOUGHT"));

    let discovery = read_discovery(&root.0).unwrap();
    let mut client = HttpClient::connect(&discovery, Duration::from_secs(5)).unwrap();
    let QueryProjection::Events(events) = client
        .query(Query::RunTimeline {
            run_id: RunId::parse(&run_id).unwrap(),
            after: EventCursor::START,
            limit: 100,
            opaque_cursor: None,
        })
        .unwrap()
    else {
        panic!("unexpected run timeline projection")
    };
    let public_events = serde_json::to_string(&events.events).unwrap();
    assert!(public_events.contains("run.transition"));

    let database = rusqlite::Connection::open(root.0.join("state.sqlite3")).unwrap();
    let mut statement = database
        .prepare("SELECT event_type, payload FROM events ORDER BY commit_position")
        .unwrap();
    let semantic_events = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let semantic_events = semantic_events
        .into_iter()
        .map(|(event_type, payload)| format!("{event_type}:{}", String::from_utf8_lossy(&payload)))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(semantic_events.contains("model_call.intent"));
    assert!(semantic_events.contains("model_call.outcome"));
    assert!(semantic_events.contains("hello from kit's deterministic development provider"));
    assert!(semantic_events.contains("input_tokens"));
    assert!(semantic_events.contains("0.000006"));
    assert!(!semantic_events.contains("SECRET_CHAIN_OF_THOUGHT"));
    drop(statement);
    drop(database);

    let health = reqwest::blocking::get(format!("{}/health/ready", discovery.endpoint))
        .unwrap()
        .bytes()
        .unwrap();
    let health: Value = serde_json::from_slice(&health).unwrap();
    assert_eq!(health["ready"], true);
    assert_eq!(health["executor"]["running"], true);
    assert_eq!(health["executor"]["accepting"], true);

    stop(&mut daemon);
    let identity: Value =
        serde_json::from_slice(&fs::read(root.0.join("daemon-identity.json")).unwrap()).unwrap();
    let key: [u8; 32] = identity["cursor_key"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_u64().unwrap() as u8)
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let telemetry_path = root.0.join(TELEMETRY_FILE);
    let raw = fs::read(&telemetry_path).unwrap();
    assert!(
        !raw.windows(b"hello from kit".len())
            .any(|value| value == b"hello from kit")
    );
    let batches = DurableLocalExporter::open(telemetry_path, &key, 16 * 1024 * 1024)
        .unwrap()
        .read_batches()
        .unwrap();
    let envelopes = batches
        .iter()
        .flat_map(|batch| &batch.run_envelopes)
        .filter(|envelope| envelope.canonical.is_some())
        .collect::<Vec<_>>();
    assert_eq!(envelopes.len(), 1);
    assert!(
        envelopes[0]
            .canonical
            .as_ref()
            .unwrap()
            .prompt
            .first_dynamic_byte
            > 0
    );
    let mut restarted = start_daemon(&root.0);
    wait_until_success(
        &root.0,
        &mut restarted,
        &["--json", "run", "show", "--run", &run_id],
    );
    let shown = cli(&root.0, &["--json", "run", "show", "--run", &run_id]);
    assert_eq!(json_output(&shown)["state"], "completed");
    assert_eq!(
        json_output(&shown)["output"]["preview"],
        "hello from kit's deterministic development provider"
    );
    let restarted_cost = cli(&root.0, &["--json", "run", "cost", "--run", &run_id]);
    assert_eq!(
        json_output(&restarted_cost)["cost"]["effective"]["micros"],
        6
    );

    let database = rusqlite::Connection::open(root.0.join("state.sqlite3")).unwrap();
    let outcomes: u64 = database
        .query_row(
            "SELECT count(*) FROM events WHERE event_type = 'model_call.outcome'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outcomes, 1);
    stop(&mut restarted);
}

#[test]
fn follow_ignores_request_deadline_while_heartbeats_continue() {
    let root = TestRoot::new("follow-request-timeout");
    let mut daemon = start_daemon(&root.0);
    wait_for_discovery(&root.0, &mut daemon);
    initialize_thread(&root.0);

    let mut follow = kit_command()
        .args([
            "--jsonl",
            "events",
            "follow",
            "--thread",
            THREAD,
            "--timeout-ms",
            "200",
            "--state-root",
        ])
        .arg(&root.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn event follower");
    let stdout = follow.stdout.take().unwrap();
    let (line, received_line) = mpsc::channel();
    let reader = thread::spawn(move || {
        let heartbeat = BufReader::new(stdout).lines().next();
        let _ = line.send(heartbeat);
    });
    let heartbeat = received_line
        .recv_timeout(Duration::from_secs(3))
        .expect("event follower did not produce a heartbeat")
        .expect("event follower closed before heartbeat")
        .expect("read event follower heartbeat");
    assert_eq!(
        serde_json::from_str::<Value>(&heartbeat).unwrap()["event"],
        "heartbeat"
    );

    follow.kill().expect("stop event follower");
    follow.wait().expect("wait event follower");
    reader.join().expect("join event follower reader");
    stop(&mut daemon);
}

#[test]
fn explicit_follow_inactivity_timeout_is_enforced() {
    let root = TestRoot::new("follow-inactivity");
    let mut daemon = start_daemon(&root.0);
    wait_for_discovery(&root.0, &mut daemon);
    initialize_thread(&root.0);
    let discovery = read_discovery(&root.0).unwrap();
    let mut client = HttpClient::connect_with_follow_inactivity_timeout(
        &discovery,
        Duration::from_secs(5),
        Duration::from_millis(100),
    )
    .unwrap();

    let started = Instant::now();
    let error = client
        .follow(
            &Query::ThreadEvents {
                thread_id: THREAD.parse::<ThreadId>().unwrap(),
                after: EventCursor::START,
                limit: 100,
                opaque_cursor: None,
            },
            None,
            |_| Ok(()),
        )
        .unwrap_err();
    assert_eq!(error.kind, ClientErrorKind::Timeout);
    assert!(error.message.contains("inactivity"));
    assert!(started.elapsed() < Duration::from_secs(2));
    stop(&mut daemon);
}

#[test]
fn sigterm_with_active_follower_exits_promptly() {
    let root = TestRoot::new("follow-shutdown");
    let mut daemon = start_daemon(&root.0);
    wait_for_discovery(&root.0, &mut daemon);
    initialize_thread(&root.0);
    let mut follow = kit_command()
        .args([
            "--jsonl",
            "events",
            "follow",
            "--thread",
            THREAD,
            "--timeout-ms",
            "5000",
            "--state-root",
        ])
        .arg(&root.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn event follower");
    let stdout = follow.stdout.take().unwrap();
    let (lines, received_lines) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    received_lines
        .recv_timeout(Duration::from_secs(3))
        .expect("event follower did not connect")
        .expect("read event follower heartbeat");

    assert!(
        Command::new("kill")
            .args(["-TERM", &daemon.id().to_string()])
            .status()
            .expect("send SIGTERM")
            .success()
    );
    assert!(
        wait_for_exit(&mut daemon, Duration::from_secs(5), "daemon").success(),
        "daemon did not shut down cleanly"
    );
    let _ = wait_for_exit(&mut follow, Duration::from_secs(5), "event follower");
    reader.join().expect("join event follower reader");
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_kit")
}

fn kit_command() -> Command {
    let mut command = Command::new(binary());
    command
        .env("KIT_PROVIDER", "deterministic-test")
        .env("KIT_FAKE_PROVIDER", "openai");
    command
}

fn start_daemon(root: &Path) -> Child {
    prepare_project_root(root);
    kit_command()
        .args(["daemon", "--state-root"])
        .arg(root)
        .env("KIT_PROJECT_ROOT", root.join("unconfigured-project"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start foreground daemon")
}

struct OwnedDaemon {
    child: Child,
}

impl OwnedDaemon {
    fn start(root: &Path) -> Self {
        prepare_project_root(root);
        let mut command = kit_command();
        command
            .args(["daemon", "--state-root"])
            .arg(root)
            .env("KIT_PROJECT_ROOT", root.join("unconfigured-project"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        Self {
            child: command.spawn().expect("start owned foreground daemon"),
        }
    }
}

impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: start() places this owned child in a process group led by its PID.
            unsafe {
                libc::killpg(self.child.id() as libc::pid_t, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn prepare_project_root(root: &Path) {
    let project = root.join("unconfigured-project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("README.md"),
        "deterministic native workspace\n",
    )
    .unwrap();
    if project.join(".git").exists() {
        return;
    }
    for arguments in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Kit Test",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(arguments)
                .current_dir(&project)
                .status()
                .unwrap()
                .success()
        );
    }
}

fn initialize_thread(root: &Path) {
    let identity: Value = serde_json::from_slice(
        &fs::read(root.join("daemon-identity.json")).expect("read daemon identity"),
    )
    .expect("parse daemon identity");
    let project = identity["project_id"].as_str().expect("project id");
    assert_success(&cli(
        root,
        &["--json", "project", "create", "--id", project],
    ));
    assert_success(&cli(
        root,
        &[
            "--json",
            "thread",
            "create",
            "--project",
            project,
            "--id",
            THREAD,
        ],
    ));
}

fn wait_for_discovery(root: &Path, daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if read_discovery(root).is_ok() {
            return;
        }
        if let Some(status) = daemon.try_wait().expect("check daemon") {
            panic!("daemon exited before discovery: {status}");
        }
        assert!(Instant::now() < deadline, "daemon readiness timed out");
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_until_success(root: &Path, daemon: &mut Child, arguments: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = cli(root, arguments);
        if output.status.success() {
            return;
        }
        if let Some(status) = daemon.try_wait().expect("check restarted daemon") {
            panic!("restarted daemon exited: {status}: {}", diagnostic(&output));
        }
        assert!(
            Instant::now() < deadline,
            "restarted daemon readiness timed out"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn cli(root: &Path, arguments: &[&str]) -> Output {
    prepare_project_root(root);
    let mut command = kit_command();
    command
        .args(arguments)
        .arg("--state-root")
        .arg(root)
        .env("KIT_PROJECT_ROOT", root.join("unconfigured-project"))
        .arg("--timeout-ms")
        .arg("5000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn CLI");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().expect("poll CLI").is_some() {
            return child.wait_with_output().expect("collect CLI output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect timed out CLI");
            panic!("CLI subprocess timed out: {}", diagnostic(&output));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn stop(child: &mut Child) {
    if child.try_wait().expect("poll daemon").is_none() {
        child.kill().expect("kill daemon");
    }
    child.wait().expect("wait daemon");
}

fn wait_for_exit(child: &mut Child, timeout: Duration, label: &str) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll subprocess") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait().expect("wait timed out subprocess");
            panic!("{label} did not exit before timeout");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn write_discovery(root: &Path, value: &Value) {
    let path = root.join("daemon.json");
    fs::write(
        &path,
        serde_json::to_vec(value).expect("serialize discovery"),
    )
    .expect("write discovery");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure discovery");
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("invalid JSON output: {error}: {}", diagnostic(output)))
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", diagnostic(output));
}

fn diagnostic(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn terminate_daemons(root: &Path) {
    let root = root.to_string_lossy();
    for signal in ["-TERM", "-KILL"] {
        let processes = Command::new("ps").args(["-axo", "pid=,command="]).output();
        let Ok(processes) = processes else {
            return;
        };
        for line in String::from_utf8_lossy(&processes.stdout).lines() {
            if line.contains(binary())
                && line.contains(root.as_ref())
                && line.contains("daemon")
                && let Some(pid) = line.split_whitespace().next()
            {
                let _ = Command::new("kill").args([signal, pid]).status();
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn daemon_processes(root: &Path) -> Vec<(u32, u32, u32, u32)> {
    let root = root.to_string_lossy();
    let processes = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,command="])
        .output()
        .expect("list daemon processes");
    String::from_utf8_lossy(&processes.stdout)
        .lines()
        .filter(|line| {
            line.contains(binary()) && line.contains(root.as_ref()) && line.contains("daemon")
        })
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            // SAFETY: getsid only reads process metadata for the parsed live PID.
            let session = unsafe { libc::getsid(pid as libc::pid_t) };
            if session < 0 {
                return None;
            }
            Some((
                pid,
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
                session as u32,
            ))
        })
        .collect()
}
