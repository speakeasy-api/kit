use agentkit_core::{ItemKind, Part};

use super::load_initial_transcript;
use agentkit_tools_core::ToolSource;

use super::{Runtime, SessionRequest, SessionSelection};

#[test]
fn configured_session_is_consumed_only_after_successful_start() {
    let request = SessionRequest {
        id: "selected".into(),
        resume: false,
        force: false,
    };
    let mut selection = SessionSelection {
        configured: Some(request),
        claimed: false,
    };

    let (first, configured) = selection.claim();
    assert_eq!(first.id, "selected");
    assert!(configured);
    selection.finish(configured, false, true);
    let (retry, configured) = selection.claim();
    assert_eq!(retry.id, "selected");
    assert!(
        retry.resume,
        "a transcript opened before failure is resumed"
    );
    selection.finish(configured, true, false);
    assert!(selection.configured.is_none());
}

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

#[test]
fn system_prompt_explains_skill_activation() {
    let root = tempfile::tempdir().unwrap();
    let skill = root.path().join(".agents/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: Review code.\n---\nReview it.\n",
    )
    .unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let prompt = runtime.system_prompt(0);
    assert!(prompt.contains("Available agent skills are listed"));
    assert!(
        prompt.contains("return `activate_skill({ name: \"<skill-name>\" })` before proceeding")
    );
}

#[test]
fn system_prompt_points_to_docs_for_kit_guidance() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let prompt = runtime.system_prompt(0);
    assert!(prompt.contains(
        "Use `docs({ query: \"<your query here>\" })` to troubleshoot issues in Kit and find user guidance."
    ));
}
