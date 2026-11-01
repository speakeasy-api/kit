//! Subprocess-boundary coverage: each peer records its actual PID and wire requests.
#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]
use super::*;
use serde_json::json;

const PEER: &str = r#"
import json, os, sys, time
mode = sys.argv[1]
def record(request):
    with open('wire.jsonl', 'a') as log:
        log.write(json.dumps({'pid': os.getpid(), 'request': request}) + '\n')
for line in sys.stdin:
    req = json.loads(line)
    record(req)
    if 'id' not in req:
        continue
    method = req['method']
    response = {'jsonrpc': '2.0', 'id': req['id']}
    if method == 'initialize':
        version = req['params']['protocolVersion']
        rejection = 'unsupported ACP protocol version 2; this endpoint only supports ACP protocol version 1'
        if mode == 'exit':
            sys.exit(17)
        if mode == 'timeout' or (mode == 'fallback-timeout' and version == 1):
            time.sleep(120)
        if mode == 'fallback-timeout' and version == 2:
            while not os.path.exists('release-v2'):
                time.sleep(0.01)
        if mode == 'auth':
            response['error'] = {'code': -32000, 'message': 'Authentication required', 'data': rejection}
        elif mode == 'invalid':
            response['error'] = {'code': -32600, 'message': 'Invalid request', 'data': 'unsupported option'}
        elif mode == 'malformed':
            response['result'] = {'protocolVersion': 'not-a-version'}
        elif mode == 'always-reject' or version == 2:
            response['error'] = {'code': -32600, 'message': 'Invalid request', 'data': rejection}
        elif mode == 'invalid-selection':
            response['result'] = {'protocolVersion': 2, 'info': {'name': 'peer', 'version': '1'}, 'capabilities': {}}
        else:
            assert 'info' not in req['params'] and 'capabilities' not in req['params']
            assert req['params']['clientInfo']['name'] == 'kit'
            response['result'] = {'protocolVersion': 1, 'agentCapabilities': {}}
    elif method == 'session/new':
        response['result'] = {'sessionId': 'v1-child'}
    else:
        response['error'] = {'code': -32601, 'message': 'Method not found'}
    print(json.dumps(response), flush=True)
"#;

fn config(root: &std::path::Path, mode: &str) -> ChildConfig {
    ChildConfig {
        root: root.into(),
        additional_directories: Vec::new(),
        model: "unused".into(),
        provider: Default::default(),
        reasoning_effort: None,
        openrouter_api_key: None,
        configured_mcp_config: None,
        configured_mcp_config_inherited: false,
        legacy_mcp_config: false,
        mcp_config: None,
        credential_storage: Default::default(),
        telemetry: Default::default(),
        harnesses: AcpHarnesses::new(BTreeMap::from([(
            "peer".into(),
            AcpHarnessProfile {
                command: "python3".into(),
                args: vec!["-c".into(), PEER.into(), mode.into()],
                permissions: AcpPermissionPolicy::Deny,
            },
        )]))
        .unwrap(),
        default_harness: "acp.peer".into(),
        parent_id: None,
        parent_name: None,
        tree_slots: None,
        prompt_cache_key: None,
    }
}

fn wire(root: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("wire.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn start(root: &std::path::Path, mode: &str) -> Result<ChildSession, ChildError> {
    ChildSession::start(
        config(root, mode),
        "acp.peer".into(),
        None,
        None,
        1,
        TurnCancellation::default(),
    )
    .await
}

#[tokio::test]
async fn explicit_rejection_restarts_once_with_v1_and_creates_session() {
    let root = tempfile::tempdir().unwrap();
    let child = start(root.path(), "strict-v1").await.unwrap();
    assert_eq!(child.session_id(), "v1-child");
    let requests = wire(root.path());
    assert_eq!(requests.len(), 3);
    assert_ne!(requests[0]["pid"], requests[1]["pid"]);
    assert_eq!(requests[1]["pid"], requests[2]["pid"]);
    assert_eq!(requests[0]["request"]["params"]["protocolVersion"], 2);
    assert_eq!(requests[1]["request"]["params"]["protocolVersion"], 1);
    assert_eq!(requests[2]["request"]["method"], "session/new");
    drop(child);
}

#[tokio::test]
async fn fallback_is_bounded_and_never_retries_other_startup_errors() {
    for (mode, attempts) in [
        ("always-reject", 2),
        ("invalid-selection", 2),
        ("auth", 1),
        ("invalid", 1),
        ("malformed", 1),
        ("exit", 1),
    ] {
        let root = tempfile::tempdir().unwrap();
        assert!(start(root.path(), mode).await.is_err(), "{mode}");
        let requests = wire(root.path());
        assert_eq!(requests.len(), attempts, "{mode}: {requests:?}");
        assert!(
            requests
                .iter()
                .all(|row| row["request"]["method"] == "initialize")
        );
    }
}

#[tokio::test]
async fn handshake_timeout_does_not_retry() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path(), "timeout");
    let task = tokio::spawn(ChildSession::start(
        config,
        "acp.peer".into(),
        None,
        None,
        1,
        TurnCancellation::default(),
    ));
    // Wait for the real peer to receive initialize, then advance the existing
    // production deadline rather than use wall-clock timing assertions.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.path().join("wire.jsonl").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(HANDSHAKE).await;
    let result = task.await.unwrap();
    tokio::time::resume();
    assert!(
        matches!(result, Err(ChildError::Failed(ref message)) if message.contains("handshake timeout"))
    );
    assert_eq!(wire(root.path()).len(), 1);
}

#[test]
fn version_rejection_does_not_mask_authentication_or_generic_invalid_requests() {
    let rejection = json!(
        "unsupported ACP protocol version 2; this endpoint only supports ACP protocol version 1"
    );
    assert!(protocol::requires_v1(
        &agent_client_protocol::Error::invalid_request().data(rejection.clone())
    ));
    assert!(!protocol::requires_v1(
        &agent_client_protocol::Error::auth_required().data(rejection)
    ));
    assert!(!protocol::requires_v1(
        &agent_client_protocol::Error::invalid_request()
    ));
}

#[tokio::test]
async fn fallback_shares_the_original_handshake_deadline() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path(), "fallback-timeout");
    let task = tokio::spawn(ChildSession::start(
        config,
        "acp.peer".into(),
        None,
        None,
        1,
        TurnCancellation::default(),
    ));
    wait_for_requests(root.path(), 1).await;
    tokio::time::pause();
    tokio::time::advance(HANDSHAKE / 2).await;
    tokio::time::resume();
    std::fs::write(root.path().join("release-v2"), "").unwrap();
    wait_for_requests(root.path(), 2).await;
    tokio::time::pause();
    tokio::time::advance(HANDSHAKE / 2).await;
    // Allow the actor's cleanup to settle, but not a fresh 30-second budget.
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("fallback reset the startup deadline")
        .unwrap();
    tokio::time::resume();
    assert!(
        matches!(result, Err(ChildError::Failed(ref message)) if message.contains("handshake timeout"))
    );
    assert_eq!(wire(root.path()).len(), 2);
}

async fn wait_for_requests(root: &std::path::Path, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let contents = std::fs::read_to_string(root.join("wire.jsonl")).unwrap_or_default();
            if contents.lines().count() >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
