//! Exercise public session and tool APIs against a real-disk backend whose
//! capacity is unavailable until a normal shell tool repairs the condition.
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use agentkit_core::{Item, ItemKind, MetadataMap, SessionId, Timestamp, ToolCallId, TurnId};
use agentkit_loop::{TranscriptEvent, TranscriptObserver};
use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext, Tool, ToolName, ToolRequest};
use kit::resilient_fs::{self, Fs};
use serde_json::json;

#[path = "support/capacity.rs"]
mod capacity;
use capacity::{Capacity, CapacityDisk};

#[test]
fn session_survives_outage_close_reopen_and_tool_driven_recovery() {
    const CHILD: &str = "KIT_RESILIENT_SESSION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session_survives_outage_close_reopen_and_tool_driven_recovery",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let root = home.join("project");
    fs::create_dir(&root).unwrap();
    let capacity = Arc::new(Capacity {
        exhausted: AtomicBool::new(false),
        exhaust_on_write: AtomicBool::new(false),
        repaired: home.join("repaired"),
    });
    assert!(
        resilient_fs::initialize_global(Fs::new(Arc::new(CapacityDisk(capacity.clone())))).is_ok()
    );
    let id = SessionId::new("resilience");
    let opened = kit::session::open(
        &root,
        &id.0,
        false,
        false,
        vec![Item::text(ItemKind::System, "system")],
    )
    .unwrap();
    capacity.exhausted.store(true, Ordering::SeqCst);
    opened.observer.on_transcript_event(TranscriptEvent {
        session_id: &id,
        item: &Item::text(ItemKind::User, "accepted during outage").with_created_at(Timestamp(123)),
    });
    assert!(resilient_fs::global().status().pending_operations > 0);
    drop(opened);
    // Reopening uses retained state and retained real mutation authority, not
    // a test-only SessionLock branch or a second independent memory transcript.
    let reopened = kit::session::open(&root, &id.0, true, false, vec![]).unwrap();
    assert_eq!(reopened.transcript.len(), 2);
    assert!(
        kit::session::open(&root, &id.0, true, true, vec![]).is_err(),
        "live observer must remain exclusive"
    );
    let virtual_path = root.join("memory-only.txt");
    resilient_fs::write(&virtual_path, b"original").unwrap();
    assert!(!virtual_path.exists());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let turn = TurnId::new("repair");
        let context = OwnedToolContext {
            session_id: id.clone(),
            turn_id: turn.clone(),
            metadata: MetadataMap::new(),
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };
        let request = |name: &str, input| {
            ToolRequest::new(
                ToolCallId::new(name),
                ToolName::new(name),
                input,
                id.clone(),
                turn.clone(),
            )
        };
        let edit = kit::tools::EditTool::new(root.clone());
        let result = edit.invoke(request("edit", json!({
            "op":"edit", "path":"memory-only.txt", "hunks":[{"old":"original","new":"changed"}]
        })), &mut context.borrowed()).await;
        assert!(
            result.is_err(),
            "explicit edit must fail on the real missing file, not mutate memfs"
        );
        assert_eq!(resilient_fs::read(&virtual_path).unwrap(), b"original");
        let shell = kit::tools::ShellTool::new(root.clone());
        shell
            .invoke(
                request("shell", json!({"command":"echo repaired > ../repaired"})),
                &mut context.borrowed(),
            )
            .await
            .unwrap();
    });
    assert!(
        capacity.repaired.exists(),
        "a real tool operation repaired the backend condition"
    );
    reopened.observer.on_transcript_event(TranscriptEvent {
        session_id: &id,
        item: &Item::text(ItemKind::User, "accepted after repair").with_created_at(Timestamp(124)),
    });
    drop(reopened);
    for _ in 0..32 {
        let report = resilient_fs::global().recover();
        assert!(report.blocked.is_none(), "{:?}", report.blocked);
        if report.remaining_operations == 0 {
            break;
        }
    }
    resilient_fs::global().require_disk(&home).unwrap();
    assert_eq!(resilient_fs::global().status().pending_operations, 0);
    assert_eq!(fs::read(&virtual_path).unwrap(), b"original");
    assert_eq!(kit::session::load(&root, &id.0).unwrap().len(), 3);
    // Inspect actual disk, not the facade, then replay again and prove exactness.
    let mut pending = vec![home.join(".kit/sessions")];
    let mut transcript = None;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else if entry.file_name() == "resilience.jsonl" {
                transcript = Some(entry.path());
            }
        }
    }
    let transcript = transcript.unwrap();
    let durable = fs::read_to_string(&transcript).unwrap();
    let records = durable
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3);
    assert_eq!(
        records
            .iter()
            .map(|record| record["generation"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(durable.matches("accepted during outage").count(), 1);
    assert_eq!(durable.matches("accepted after repair").count(), 1);
    assert_eq!(resilient_fs::global().recover().remaining_operations, 0);
    assert_eq!(fs::read_to_string(transcript).unwrap(), durable);
}

#[test]
fn legacy_migration_reopens_with_retained_parent_sync_during_outage() {
    const CHILD: &str = "KIT_RESILIENT_MIGRATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "legacy_migration_reopens_with_retained_parent_sync_during_outage",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let root = home.join("project");
    let legacy = root.join(".kit/sessions");
    fs::create_dir_all(&legacy).unwrap();
    let capacity = Arc::new(Capacity {
        exhausted: AtomicBool::new(false),
        exhaust_on_write: AtomicBool::new(false),
        repaired: home.join("repaired"),
    });
    resilient_fs::initialize_global(Fs::new(Arc::new(CapacityDisk(capacity.clone())))).unwrap();
    // Materialize the workspace's storage directory before the simulated outage.
    drop(
        kit::session::open(
            &root,
            "bootstrap",
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap(),
    );
    let record = json!({
        "schema_version": 2, "session_id": "legacy", "generation": 1,
        "item": Item::text(ItemKind::System, "legacy history"),
    });
    fs::write(legacy.join("legacy.jsonl"), format!("{record}\n")).unwrap();
    // Acquire real scoped and legacy leases before the first data write fails.
    capacity.exhaust_on_write.store(true, Ordering::SeqCst);
    let opened = kit::session::open(&root, "legacy", true, false, vec![]).unwrap();
    let transcript = opened.transcript.clone();
    assert!(resilient_fs::global().status().pending_operations > 0);
    drop(opened);
    let reopened = kit::session::open(&root, "legacy", true, false, vec![]).unwrap();
    assert_eq!(reopened.transcript, transcript);
    assert!(kit::session::open(&root, "legacy", true, true, vec![]).is_err());
    capacity.exhausted.store(false, Ordering::SeqCst);
    assert_eq!(resilient_fs::global().recover().remaining_operations, 0);
    drop(reopened);
    assert_eq!(
        kit::session::open(&root, "legacy", true, false, vec![])
            .unwrap()
            .transcript,
        transcript
    );
}
