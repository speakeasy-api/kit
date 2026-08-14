use std::{future::Future, pin::Pin, sync::Arc};

use a2a_protocol_server::{
    AgentExecutor, EventEmitter, JsonRpcDispatcher, RequestHandlerBuilder,
    request_context::RequestContext, serve_with_addr, streaming::EventQueueWriter,
};
use a2a_protocol_types::{
    agent_card::{AgentCapabilities, AgentCard, AgentInterface, AgentSkill},
    error::A2aResult,
    message::Part,
    task::TaskState,
};

use crate::runtime::Runtime;

struct KitAgent(Arc<Runtime>);

impl AgentExecutor for KitAgent {
    fn execute<'a>(
        &'a self,
        context: &'a RequestContext,
        queue: &'a dyn EventQueueWriter,
    ) -> Pin<Box<dyn Future<Output = A2aResult<()>> + Send + 'a>> {
        Box::pin(async move {
            let emit = EventEmitter::new(context, queue);
            emit.status(TaskState::Working).await?;
            let prompt = context
                .message
                .parts
                .iter()
                .filter_map(Part::text_content)
                .collect::<Vec<_>>()
                .join("\n");
            if prompt.trim().is_empty() {
                emit.artifact(
                    "error",
                    vec![Part::text("A2A request must contain a text part")],
                    None,
                    Some(true),
                )
                .await?;
                emit.status(TaskState::Failed).await?;
                return Ok(());
            }
            match self
                .0
                .run_cancelled(prompt, 0, Some(context.cancellation_token.clone()))
                .await
            {
                Ok(output) => {
                    emit.artifact("result", vec![Part::text(output)], None, Some(true))
                        .await?;
                    emit.status(TaskState::Completed).await?;
                }
                Err(error) => {
                    emit.artifact(
                        "error",
                        vec![Part::text(error.to_string())],
                        None,
                        Some(true),
                    )
                    .await?;
                    emit.status(TaskState::Failed).await?;
                }
            }
            Ok(())
        })
    }
}

pub async fn start(
    runtime: Arc<Runtime>,
    address: String,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error>> {
    let url = format!("http://{address}");
    let card = AgentCard {
        url: None,
        name: "Kit".into(),
        description: "Directory-rooted coding agent".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        supported_interfaces: vec![AgentInterface {
            url,
            protocol_binding: "JSONRPC".into(),
            protocol_version: "1.0".into(),
            tenant: None,
        }],
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: "coding".into(),
            name: "Coding".into(),
            description: "Inspect, edit, and run code in the configured directory".into(),
            tags: vec!["coding".into()],
            examples: None,
            input_modes: None,
            output_modes: None,
            security_requirements: None,
        }],
        capabilities: AgentCapabilities::none(),
        provider: None,
        icon_url: None,
        documentation_url: None,
        security_schemes: None,
        security_requirements: None,
        signatures: None,
    };
    let handler = Arc::new(
        RequestHandlerBuilder::new(KitAgent(runtime))
            .with_agent_card(card)
            .build()?,
    );
    Ok(serve_with_addr(&address, JsonRpcDispatcher::new(handler)).await?)
}
