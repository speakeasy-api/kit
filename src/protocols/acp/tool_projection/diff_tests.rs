use super::*;
use agentkit_core::{MetadataMap, SessionId, TurnId};
use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext, Tool};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn committed_diffs_preserve_text_and_omit_failures_and_oversized_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("file.txt");
    let tool = crate::tools::Observed::new(crate::tools::EditTool::new(root.path().into()))
        .with_root(root.path().into());
    let context = OwnedToolContext {
        session_id: SessionId::new("diff-cases"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };
    let large = "x".repeat(MAX_DIFF_TEXT + 1);
    for (original, input, ok, expected) in [
        (
            "a\r\nb\r\n",
            json!({"op":"edit", "hunks":[{"old":"a\nb", "new":"é\nc"}]}),
            true,
            Some(("modify", "a\r\nb\r\n", "é\r\nc\r\n")),
        ),
        (
            "a",
            json!({"op":"edit", "hunks":[{"old":"a", "new":""}]}),
            true,
            Some(("modify", "a", "")),
        ),
        ("", json!({"op":"delete"}), true, Some(("delete", "", ""))),
        (
            "gone\n",
            json!({"op":"delete"}),
            true,
            Some(("delete", "gone\n", "")),
        ),
        (
            "a",
            json!({"op":"edit", "hunks":[{"old":"a", "new":"b"}, {"old":"missing", "new":"c"}]}),
            false,
            None,
        ),
        ("a", json!({"op":"add", "content":"collision"}), false, None),
        (large.as_str(), json!({"op":"delete"}), true, None),
        (
            large.as_str(),
            json!({"op":"edit", "hunks":[{"old":large, "new":"small"}]}),
            true,
            None,
        ),
    ] {
        std::fs::write(&path, original).unwrap();
        let mut input = input;
        input["path"] = json!("file.txt");
        let request = super::tests::request("diff-cases", "edit", input);
        let mut receiver = bus().subscribe();
        assert_eq!(
            tool.invoke(request, &mut context.borrowed()).await.is_ok(),
            ok
        );
        let updates = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter(|update| update.session == "diff-cases")
            .collect::<Vec<_>>();
        let diffs = updates
            .iter()
            .filter(|update| update.value().get("content").is_some())
            .collect::<Vec<_>>();
        assert_eq!(updates.last().unwrap().ok, ok);
        if let Some((operation, old, new)) = expected {
            assert_eq!(diffs.len(), 1);
            let v1 = serde_json::to_value(diffs[0].v1().unwrap()).unwrap();
            let v2 = serde_json::to_value(diffs[0].v2().unwrap()).unwrap();
            assert_eq!(v1["content"][0]["oldText"], old);
            assert_eq!(v1["content"][0]["newText"], new);
            assert_eq!(v2["content"][0]["changes"][0]["operation"], operation);
            assert_eq!(v2["content"][0]["changes"][0]["path"], json!(path));
            if operation == "delete" {
                assert!(!path.exists());
            } else {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
            }
        } else {
            assert!(diffs.is_empty());
            if !ok {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            }
        }
    }
}

#[test]
fn diff_bounds_and_non_text_deletions_are_omitted() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("binary");
    let mut receiver = bus().subscribe();
    std::fs::write(&path, [0xff]).unwrap();
    assert!(deletion_text(&path).is_none());
    assert!(deletion_text(root.path()).is_none());
    let request = super::tests::request("diff-bounds", "edit", json!({}));
    diff(&request, Path::new("relative"), Some("old"), Some("new"));
    diff(&request, &path, Some(&"x".repeat(MAX_DIFF_TEXT)), Some("x"));
    assert!(
        !std::iter::from_fn(|| receiver.try_recv().ok())
            .any(|update| update.session == "diff-bounds")
    );
    diff(&request, &path, None, Some(&"é".repeat(MAX_DIFF_TEXT / 2)));
    let update = std::iter::from_fn(|| receiver.try_recv().ok())
        .find(|update| update.session == "diff-bounds")
        .unwrap();
    assert_eq!(
        update.value()["content"][0]["newText"]
            .as_str()
            .unwrap()
            .len(),
        MAX_DIFF_TEXT
    );
}
