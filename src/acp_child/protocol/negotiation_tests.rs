use super::{Client, Version, initialize, request};
use agent_client_protocol::{
    Builder, Channel,
    schema::{ProtocolVersion, v2},
};
use agentkit_acp as v1;
use std::{path::PathBuf, time::Duration};

#[tokio::test]
async fn sdk_v2_negotiation_and_session_dispatch() {
    let (client_wire, agent_wire) = Channel::duplex();
    let server = tokio::spawn(async move {
        agent_client_protocol::Agent
            .v2()
            .on_receive_request(
                async |req: v2::InitializeRequest, responder, _cx| {
                    assert_eq!(req.protocol_version, ProtocolVersion::V2);
                    assert_eq!(req.info.name, "kit");
                    responder.respond(
                        v2::InitializeResponse::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("sdk-v2", "test"),
                        )
                        .capabilities(
                            v2::AgentCapabilities::new().session(v2::SessionCapabilities::new()),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |req: v2::NewSessionRequest, responder, _cx| {
                    assert_eq!(req.cwd, v2::AbsolutePath::new("/workspace"));
                    assert!(req.mcp_servers.is_empty());
                    responder.respond(v2::NewSessionResponse::new(v2::SessionId::new(
                        "v2-session",
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(agent_wire)
            .await
    });
    let client = Builder::new(Client).connect_with(client_wire, async |connection| {
        let (version, _, _, _) = initialize(&connection, Version::V2).await?;
        assert_eq!(version, Version::V2);
        let response = request(
            connection,
            version,
            v1::NewSessionRequest::new("/workspace"),
        )
        .await?;
        assert_eq!(response.session_id.to_string(), "v2-session");
        Ok(())
    });
    let result = tokio::time::timeout(Duration::from_secs(5), client).await;
    server.abort();
    let _ = server.await;
    result.expect("SDK v2 endpoint timed out").unwrap();
}

async fn sdk_v1_negotiation(offer: Version) {
    let (client_wire, agent_wire) = Channel::duplex();
    let server = tokio::spawn(async move {
        agent_client_protocol::Agent
            .builder()
            .on_receive_request(
                async move |req: v1::InitializeRequest, responder, _cx| {
                    // The SDK selects v1 before dispatching to its typed handler.
                    assert_eq!(req.protocol_version, ProtocolVersion::V1);
                    assert_eq!(req.client_info.unwrap().name, "kit");
                    responder.respond(v1::InitializeResponse::new(ProtocolVersion::V1))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |req: v1::NewSessionRequest, responder, _cx| {
                    assert_eq!(req.cwd, PathBuf::from("/workspace"));
                    assert!(req.mcp_servers.is_empty());
                    responder.respond(v1::NewSessionResponse::new(v1::SessionId::new(
                        "v1-session",
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(agent_wire)
            .await
    });
    let client = Builder::new(Client).connect_with(client_wire, async move |connection| {
        let (version, _, _, _) = initialize(&connection, offer).await?;
        assert_eq!(version, Version::V1);
        let response = request(
            connection,
            version,
            v1::NewSessionRequest::new("/workspace"),
        )
        .await?;
        assert_eq!(response.session_id.to_string(), "v1-session");
        Ok(())
    });
    let result = tokio::time::timeout(Duration::from_secs(5), client).await;
    server.abort();
    let _ = server.await;
    result.expect("SDK v1 endpoint timed out").unwrap();
}

#[tokio::test]
async fn sdk_v1_selection_from_v2_offer_and_session_dispatch() {
    sdk_v1_negotiation(Version::V2).await;
}

#[tokio::test]
async fn sdk_direct_v1_offer_and_session_dispatch() {
    sdk_v1_negotiation(Version::V1).await;
}
