use agentkit_core::{Item, ItemKind};

#[test]
fn completed_transcript_can_be_cloned_while_source_is_owned() {
    let directory = tempfile::tempdir().unwrap();
    let transcript = vec![
        Item::text(ItemKind::System, "system"),
        Item::text(ItemKind::User, "question"),
        Item::text(ItemKind::Assistant, "answer"),
    ];
    let source =
        kit::session::open(directory.path(), "source", false, false, transcript.clone()).unwrap();

    kit::session::clone_completed(directory.path(), "source", "branch").unwrap();

    assert_eq!(
        kit::session::load(directory.path(), "branch").unwrap(),
        source.transcript
    );
    drop(source);
}
