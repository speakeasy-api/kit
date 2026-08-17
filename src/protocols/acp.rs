use std::sync::Arc;

use agentkit_acp::{
    AcpAgentFactory, AcpAgentFactoryContext, AcpHeadlessRuntime, AcpIntegration, AcpRuntimeError,
    AutoDenyResolver,
};
use async_trait::async_trait;

use crate::{provider::KitAdapter, runtime::Runtime};

#[derive(Clone)]
struct Factory(Arc<Runtime>);

#[async_trait]
impl AcpAgentFactory<KitAdapter> for Factory {
    async fn start(
        &self,
        context: AcpAgentFactoryContext,
    ) -> Result<
        agentkit_loop::LoopDriver<<KitAdapter as agentkit_loop::ModelAdapter>::Session>,
        AcpRuntimeError,
    > {
        self.0.start_acp_driver(context).await
    }
}

pub async fn serve(runtime: Arc<Runtime>) -> Result<(), AcpRuntimeError> {
    serve_transport(runtime, agent_client_protocol::Stdio::new()).await
}

async fn serve_transport(
    runtime: Arc<Runtime>,
    transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Agent> + 'static,
) -> Result<(), AcpRuntimeError> {
    let integration = AcpIntegration::builder()
        .name("kit")
        .approval_resolver(AutoDenyResolver)
        .build()?;
    AcpHeadlessRuntime::<KitAdapter>::builder()
        .integration(integration)
        .agent_factory(Factory(runtime))
        .serve_transport(transport)
        .await
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::{Channel, schema::ProtocolVersion};

    use super::*;

    #[tokio::test]
    async fn headless_dependency_does_not_advertise_or_handle_session_fork() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn(serve_transport(runtime, agent_transport));

        agent_client_protocol::Client
            .builder()
            .connect_with(client_transport, async move |connection| {
                let initialized = connection
                    .send_request(agentkit_acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(
                    initialized
                        .agent_capabilities
                        .session_capabilities
                        .fork
                        .is_none()
                );

                connection
                    .send_request(agentkit_acp::ForkSessionRequest::new(
                        "missing",
                        root.path().to_path_buf(),
                    ))
                    .block_task()
                    .await
                    .expect_err("the headless runtime has no session/fork route");
                Ok(())
            })
            .await
            .unwrap();

        server.abort();
        let _ = server.await;
    }
}
