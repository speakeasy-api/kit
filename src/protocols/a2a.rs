use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use a2a_protocol_server::{
    AgentExecutor, EventEmitter, JsonRpcDispatcher, RequestHandlerBuilder,
    request_context::RequestContext, streaming::EventQueueWriter,
};
use a2a_protocol_types::{
    agent_card::{AgentCapabilities, AgentCard, AgentInterface, AgentSkill},
    error::A2aResult,
    message::Part,
    security::{HttpAuthSecurityScheme, SecurityRequirement, SecurityScheme, StringList},
    task::TaskState,
};

use sha2::{Digest as _, Sha256};

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
                    let session_id = a2a_session_id(context);
                    let rendered = crate::fatal::render_loop_error(&error);
                    let rendered = match crate::fatal::record_loop_error(
                        &session_id,
                        crate::fatal::Surface::A2a,
                        &error,
                    ) {
                        Ok(Some(path)) => {
                            eprintln!(
                                "stored fatal error log for {session_id}: {}",
                                path.display()
                            );
                            rendered
                        }
                        Ok(None) => rendered,
                        Err(log_error) => {
                            eprintln!(
                                "could not store fatal error log for {session_id}: {log_error}"
                            );
                            rendered
                        }
                    };
                    emit.artifact("error", vec![Part::text(rendered)], None, Some(true))
                        .await?;
                    emit.status(TaskState::Failed).await?;
                }
            }
            Ok(())
        })
    }
}

fn a2a_session_id(context: &RequestContext) -> String {
    let mut digest = Sha256::new();
    digest.update(context.task_id.0.as_bytes());
    digest.update([0]);
    digest.update(context.context_id.as_bytes());
    let digest = digest.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("a2a-{encoded}")
}

pub(crate) fn dispatcher(
    runtime: Arc<Runtime>,
    bound: std::net::SocketAddr,
    authenticated: bool,
) -> Result<JsonRpcDispatcher, Box<dyn std::error::Error>> {
    let url = format!("http://{bound}");
    let (security_schemes, security_requirements) = if authenticated {
        let name = "bearer".to_string();
        (
            Some(HashMap::from([(
                name.clone(),
                SecurityScheme::Http(HttpAuthSecurityScheme {
                    scheme: "bearer".into(),
                    bearer_format: None,
                    description: Some("Bearer token loaded by the Kit server".into()),
                }),
            )])),
            Some(vec![SecurityRequirement {
                schemes: HashMap::from([(name, StringList { list: Vec::new() })]),
            }]),
        )
    } else {
        (None, None)
    };
    let card = AgentCard {
        url: None,
        name: "Kit".into(),
        description: "Coding agent runtime".into(),
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
        security_schemes,
        security_requirements,
        signatures: None,
    };
    let handler = Arc::new(
        RequestHandlerBuilder::new(KitAgent(runtime))
            .with_agent_card(card)
            .build()?,
    );
    Ok(JsonRpcDispatcher::new(handler))
}
