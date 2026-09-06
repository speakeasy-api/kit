use super::*;

#[tokio::test]
async fn resume_recovers_empty_or_torn_first_migration_destination() {
    for empty in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().to_path_buf();
        let id = crate::session::new_id();
        let expected = vec![
            Item::text(ItemKind::System, "system"),
            Item::text(ItemKind::User, "legacy prompt"),
            Item::text(ItemKind::Assistant, "legacy answer"),
        ];
        let (legacy, scoped) = crate::session::test_support::interrupted_migration(
            root.path(),
            &id,
            expected.clone(),
            empty,
        );
        let before = [
            std::fs::read(&legacy).unwrap(),
            std::fs::read(&scoped).unwrap(),
        ];
        crate::session::branch::validate_resume(root.path(), &id).unwrap();
        assert_eq!(std::fs::read(&legacy).unwrap(), before[0]);
        assert_eq!(std::fs::read(&scoped).unwrap(), before[1]);
        assert!(!legacy.with_extension("lock").exists());
        assert!(!scoped.with_extension("lock").exists());

        let credentials = crate::credentials::CredentialStorage::Memory;
        crate::provider::store_openrouter_test_credentials(&credentials);
        let runtime = Runtime::new_with_provider_and_credentials(
            root.path(),
            "test-model",
            crate::ProviderKind::OpenRouter,
            credentials,
        )
        .unwrap();
        let (client_transport, agent_transport) = agent_client_protocol::Channel::duplex();
        let router = v2_router(runtime, SessionRegistry::new()).unwrap();
        let server = tokio::spawn(async move { router.connect_to(agent_transport).await });
        let updates = Arc::new(Mutex::new(Vec::<wire::UpdateSessionNotification>::new()));
        let received = updates.clone();
        let resumed_id = id.clone();
        let client = agent_client_protocol::Client
            .v2()
            .on_receive_notification(
                async move |update: wire::UpdateSessionNotification, _cx| {
                    received.lock().unwrap().push(update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(wire::InitializeRequest::new(
                    wire::ProtocolVersion::V2,
                    wire::Implementation::new("migration-resume-test", "0"),
                ))
                .block_task()
                .await?;
                cx.send_request(
                    wire::ResumeSessionRequest::new(resumed_id.clone(), workspace)
                        .replay_from(wire::ReplayFrom::Start(wire::ReplayFromStart::new())),
                )
                .block_task()
                .await?;
                // This second request requires the recovered real actor to be
                // live and settled, not merely a successful preflight response.
                let listed = cx
                    .send_request(ListPromptBranchesRequest {
                        session_id: wire::SessionId::new(resumed_id.clone()),
                    })
                    .block_task()
                    .await?;
                // M3 also offers the recovered safe assistant continuation.
                assert_eq!(listed.boundaries.len(), 2);
                cx.send_request(wire::CloseSessionRequest::new(resumed_id))
                    .block_task()
                    .await?;
                Ok(())
            });
        let result = timeout(Duration::from_secs(5), client).await;
        server.abort();
        let _ = server.await;
        result.expect("resume client timed out").unwrap();
        assert_eq!(crate::session::load(root.path(), &id).unwrap(), expected);
        assert!(std::fs::read(&scoped).unwrap().ends_with(b"\n"));
        let updates = updates.lock().unwrap();
        assert!(updates.iter().any(|update| matches!(&update.update,
            wire::SessionUpdate::UserMessage(message)
                if matches!(&message.content, MaybeUndefined::Value(content)
                    if content == &vec![wire::ContentBlock::Text(wire::TextContent::new("legacy prompt"))]))));
        assert!(!updates.iter().any(|update| matches!(
            update.update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Running(_))
        )));
    }
}
