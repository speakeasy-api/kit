use agentkit_core::{ItemKind, Part};

use super::load_initial_transcript;
use agentkit_tools_core::ToolSource;

use super::Runtime;

#[tokio::test]
async fn loads_all_agents_md_files_outermost_first() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(parent.path().join("AGENTS.md"), "outer guidance").unwrap();
    std::fs::write(root.join("AGENTS.md"), "inner guidance").unwrap();

    let transcript = load_initial_transcript(&root, "system".into())
        .await
        .unwrap();

    assert_eq!(
        transcript.iter().map(|item| item.kind).collect::<Vec<_>>(),
        [ItemKind::System, ItemKind::Context, ItemKind::Context]
    );
    let text = transcript[1..]
        .iter()
        .map(|item| match &item.parts[0] {
            Part::Text(text) => text.text.as_str(),
            other => panic!("expected text context, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(text[0].contains("outer guidance"));
    assert!(text[1].contains("inner guidance"));
}

#[test]
fn compose_is_the_only_visible_tool_and_documents_mcp_meta_tools() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let specs = runtime.compose(0).specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name.0, "compose");
    assert!(specs[0].description.contains("`tool_search`"));
    assert!(specs[0].description.contains("`auth`"));
    assert!(specs[0].description.contains("`tool`"));
    assert!(!specs[0].description.contains("mcp_filesystem_read_file"));
}
