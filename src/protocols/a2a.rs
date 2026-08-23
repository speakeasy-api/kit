use std::{convert::Infallible, future::Future, pin::Pin, sync::Arc, time::Duration};

use a2a_protocol_server::{
    AgentExecutor, Dispatcher, EventEmitter, JsonRpcDispatcher, RequestHandlerBuilder,
    request_context::RequestContext, streaming::EventQueueWriter,
};
use a2a_protocol_types::{
    agent_card::{AgentCapabilities, AgentCard, AgentInterface, AgentSkill},
    error::A2aResult,
    message::Part,
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
                    let rendered = error.to_string();
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

pub async fn start(
    runtime: Arc<Runtime>,
    address: String,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error>> {
    // Bind exactly once and keep this listener for the server. Selecting an
    // ephemeral port with one listener and rebinding it with another creates a
    // TOCTOU window in which another process can claim the advertised port.
    let listener = tokio::net::TcpListener::bind(&address).await?;
    let bound = listener.local_addr()?;
    let url = format!("http://{bound}");
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
    serve_bound(listener, JsonRpcDispatcher::new(handler)).await
}

async fn serve_bound(
    listener: tokio::net::TcpListener,
    dispatcher: impl Dispatcher,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error>> {
    let bound = listener.local_addr()?;
    let dispatcher = Arc::new(dispatcher);
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            let dispatcher = Arc::clone(&dispatcher);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let dispatcher = Arc::clone(&dispatcher);
                    async move { Ok::<_, Infallible>(dispatcher.dispatch(request).await) }
                });
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(io, service)
                .await;
            });
        }
    });
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::start;

    #[tokio::test]
    async fn port_zero_allocates_a_real_port() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = crate::Runtime::new(directory.path(), "gpt-5.4").unwrap();
        let bound = start(runtime, "127.0.0.1:0".into()).await.unwrap();
        assert_eq!(bound.ip(), std::net::Ipv4Addr::LOCALHOST);
        assert_ne!(bound.port(), 0);
        let rebound = tokio::net::TcpListener::bind(bound).await;
        assert!(
            matches!(rebound, Err(error) if error.kind() == std::io::ErrorKind::AddrInUse),
            "the server must retain the listener that selected the ephemeral port"
        );
    }
}
