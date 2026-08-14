use std::sync::Arc;

use agentkit_acp::{
    AcpAgentFactory, AcpAgentFactoryContext, AcpHeadlessRuntime, AcpIntegration, AcpRuntimeError,
    AutoDenyResolver,
};
use async_trait::async_trait;

use crate::{provider::OpenAiSubscriptionAdapter, runtime::Runtime};

#[derive(Clone)]
struct Factory(Arc<Runtime>);

#[async_trait]
impl AcpAgentFactory<OpenAiSubscriptionAdapter> for Factory {
    async fn start(
        &self,
        context: AcpAgentFactoryContext,
    ) -> Result<
        agentkit_loop::LoopDriver<
            <OpenAiSubscriptionAdapter as agentkit_loop::ModelAdapter>::Session,
        >,
        AcpRuntimeError,
    > {
        self.0.start_acp_driver(context).await
    }
}

pub async fn serve(runtime: Arc<Runtime>) -> Result<(), AcpRuntimeError> {
    let integration = AcpIntegration::builder()
        .name("kit")
        .approval_resolver(AutoDenyResolver)
        .build()?;
    AcpHeadlessRuntime::<OpenAiSubscriptionAdapter>::builder()
        .integration(integration)
        .agent_factory(Factory(runtime))
        .serve_stdio()
        .await
}
