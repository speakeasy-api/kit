use std::env;
use std::error::Error;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use agentkit_mcp::{
    McpConnection, McpServerConfig, McpTransportBinding, StreamableHttpTransportConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::sleep;

pub const FIXTURE_RESOURCE_URI: &str = "fixture://greeting";
pub const FIXTURE_PROMPT_NAME: &str = "greeting-prompt";
pub const FIXTURE_TOOL_NAME: &str = "echo";
pub const PROBE_TEXT: &str = "streamable-http-ok";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: String,
    pub jsonrpc_method: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceImplementation {
    RustSdkStatefulSse,
    RustSdkStatelessJson,
}

impl ReferenceImplementation {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "rust-stateful" | "stateful" => Some(Self::RustSdkStatefulSse),
            "rust-stateless" | "stateless" | "json" => Some(Self::RustSdkStatelessJson),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RustSdkStatefulSse => "rust-stateful",
            Self::RustSdkStatelessJson => "rust-stateless",
        }
    }

    fn server_mode_arg(&self) -> &'static str {
        match self {
            Self::RustSdkStatefulSse => "stateful",
            Self::RustSdkStatelessJson => "stateless-json",
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProbeResult {
    pub implementation: String,
    pub url: String,
    pub tool_names: Vec<String>,
    pub resource_ids: Vec<String>,
    pub prompt_ids: Vec<String>,
    pub tool_output: String,
    pub resource_text: String,
    pub prompt_text: String,
}

pub async fn probe_reference_implementation(
    implementation: ReferenceImplementation,
) -> Result<ProbeResult, Box<dyn Error>> {
    let launcher = ReferenceServerLauncher::discover()?;
    probe_reference_implementation_with_launcher(implementation, launcher).await
}

pub async fn probe_reference_implementation_with_binary(
    implementation: ReferenceImplementation,
    binary: impl Into<PathBuf>,
) -> Result<ProbeResult, Box<dyn Error>> {
    probe_reference_implementation_with_launcher(
        implementation,
        ReferenceServerLauncher::Binary(binary.into()),
    )
    .await
}

pub async fn capture_transport_exchange_with_binary(
    implementation: ReferenceImplementation,
    binary: impl Into<PathBuf>,
) -> Result<Vec<RecordedRequest>, Box<dyn Error>> {
    capture_transport_exchange_with_launcher(
        implementation,
        ReferenceServerLauncher::Binary(binary.into()),
    )
    .await
}

async fn probe_reference_implementation_with_launcher(
    implementation: ReferenceImplementation,
    launcher: ReferenceServerLauncher,
) -> Result<ProbeResult, Box<dyn Error>> {
    let mut server = spawn_reference_server(implementation, launcher).await?;
    let result = probe_server(implementation, &server.url).await;
    let shutdown_result = server.shutdown().await;

    match (result, shutdown_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn capture_transport_exchange_with_launcher(
    implementation: ReferenceImplementation,
    launcher: ReferenceServerLauncher,
) -> Result<Vec<RecordedRequest>, Box<dyn Error>> {
    let mut server = spawn_reference_server(implementation, launcher).await?;
    let result = capture_transport_exchange(&server).await;
    let shutdown_result = server.shutdown().await;

    match (result, shutdown_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn probe_server(
    implementation: ReferenceImplementation,
    url: &str,
) -> Result<ProbeResult, Box<dyn Error>> {
    let connection = McpConnection::connect(&McpServerConfig::new(
        "reference",
        McpTransportBinding::StreamableHttp(StreamableHttpTransportConfig::new(url)),
    ))
    .await?;

    let snapshot = connection.discover().await?;
    let tool_output = decode_tool_text(
        connection
            .call_tool(FIXTURE_TOOL_NAME, json!({ "text": PROBE_TEXT }))
            .await?,
    )?;
    let resource_text =
        decode_resource_text(connection.read_resource(FIXTURE_RESOURCE_URI).await?)?;
    let prompt = connection
        .get_prompt(FIXTURE_PROMPT_NAME, json!({ "name": "Ada" }))
        .await?;
    let prompt_text = collect_prompt_text(&prompt)?;
    connection.close().await?;

    Ok(ProbeResult {
        implementation: implementation.as_str().into(),
        url: url.into(),
        tool_names: snapshot
            .tools
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect(),
        resource_ids: snapshot
            .resources
            .into_iter()
            .map(|resource| resource.uri.clone())
            .collect(),
        prompt_ids: snapshot
            .prompts
            .into_iter()
            .map(|prompt| prompt.name)
            .collect(),
        tool_output,
        resource_text,
        prompt_text,
    })
}

fn decode_tool_text(result: agentkit_mcp::CallToolResult) -> Result<String, Box<dyn Error>> {
    let mut chunks = Vec::new();
    for content in result.content {
        if let Some(text) = content.raw.as_text() {
            chunks.push(text.text.clone());
        }
    }
    if chunks.is_empty() {
        return Err("tool result did not contain text content".into());
    }
    Ok(chunks.join("\n"))
}

fn decode_resource_text(
    result: agentkit_mcp::ReadResourceResult,
) -> Result<String, Box<dyn Error>> {
    for content in result.contents {
        if let agentkit_mcp::McpResourceContents::TextResourceContents { text, .. } = content {
            return Ok(text);
        }
    }
    Err("expected inline text resource, saw none".into())
}

fn collect_prompt_text(prompt: &agentkit_mcp::GetPromptResult) -> Result<String, Box<dyn Error>> {
    let mut text_parts = Vec::new();
    for message in &prompt.messages {
        if let agentkit_mcp::PromptMessageContent::Text { text } = &message.content {
            text_parts.push(text.clone());
        }
    }
    if text_parts.is_empty() {
        return Err("prompt result did not contain any textual content".into());
    }
    Ok(text_parts.join("\n"))
}

struct RunningReferenceServer {
    child: Child,
    inspect_url: String,
    url: String,
}

impl RunningReferenceServer {
    async fn fetch_recorded_requests(&self) -> Result<Vec<RecordedRequest>, Box<dyn Error>> {
        Ok(reqwest::get(&self.inspect_url)
            .await?
            .error_for_status()?
            .json::<Vec<RecordedRequest>>()
            .await?)
    }

    async fn shutdown(&mut self) -> Result<(), Box<dyn Error>> {
        match self.child.try_wait()? {
            Some(status) if status.success() => Ok(()),
            Some(status) => {
                Err(format!("reference server exited early with status {status}").into())
            }
            None => {
                self.child.kill().await?;
                let _ = self.child.wait().await?;
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug)]
enum ReferenceServerLauncher {
    Binary(PathBuf),
    CargoRun,
}

impl ReferenceServerLauncher {
    fn discover() -> Result<Self, Box<dyn Error>> {
        if let Some(path) = env::var_os("CARGO_BIN_EXE_mcp-reference-server") {
            return Ok(Self::Binary(PathBuf::from(path)));
        }

        for candidate in binary_candidates() {
            if candidate.exists() {
                return Ok(Self::Binary(candidate));
            }
        }

        Ok(Self::CargoRun)
    }

    fn command(
        &self,
        implementation: ReferenceImplementation,
        port: u16,
    ) -> Result<Command, Box<dyn Error>> {
        match self {
            Self::Binary(path) => {
                let mut command = Command::new(path);
                command
                    .arg(implementation.server_mode_arg())
                    .arg(port.to_string());
                Ok(command)
            }
            Self::CargoRun => {
                let mut command = Command::new("cargo");
                command
                    .args([
                        "run",
                        "--quiet",
                        "-p",
                        "mcp-reference-interop",
                        "--bin",
                        "mcp-reference-server",
                        "--",
                        implementation.server_mode_arg(),
                        &port.to_string(),
                    ])
                    .current_dir(workspace_root());
                Ok(command)
            }
        }
    }
}

async fn spawn_reference_server(
    implementation: ReferenceImplementation,
    launcher: ReferenceServerLauncher,
) -> Result<RunningReferenceServer, Box<dyn Error>> {
    let port = allocate_port()?;
    let base_url = format!("http://127.0.0.1:{port}");
    let url = format!("http://127.0.0.1:{port}/mcp");
    let mut command = launcher.command(implementation, port)?;
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    let mut child = command.spawn()?;

    wait_for_port_or_exit(port, &mut child, Duration::from_secs(20)).await?;
    Ok(RunningReferenceServer {
        child,
        inspect_url: format!("{base_url}/_inspect/requests"),
        url,
    })
}

async fn capture_transport_exchange(
    server: &RunningReferenceServer,
) -> Result<Vec<RecordedRequest>, Box<dyn Error>> {
    let connection = McpConnection::connect(&McpServerConfig::new(
        "reference",
        McpTransportBinding::StreamableHttp(StreamableHttpTransportConfig::new(&server.url)),
    ))
    .await?;
    let _ = connection.discover().await?;
    let _ = connection
        .call_tool(FIXTURE_TOOL_NAME, json!({ "text": PROBE_TEXT }))
        .await?;
    let _ = connection.read_resource(FIXTURE_RESOURCE_URI).await?;
    let _ = connection
        .get_prompt(FIXTURE_PROMPT_NAME, json!({ "name": "Ada" }))
        .await?;
    connection.close().await?;
    server.fetch_recorded_requests().await
}

fn binary_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(current) = env::current_exe()
        && let Some(dir) = current.parent()
    {
        candidates.push(dir.join("mcp-reference-server"));
        if let Some(parent) = dir.parent() {
            candidates.push(parent.join("mcp-reference-server"));
        }
    }
    candidates
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("example crate should live under the workspace root")
        .to_path_buf()
}

fn allocate_port() -> Result<u16, Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

async fn wait_for_port_or_exit(
    port: u16,
    child: &mut Child,
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
            drop(stream);
            return Ok(());
        }

        if let Some(status) = child.try_wait()? {
            return Err(format!("reference server exited before becoming ready: {status}").into());
        }

        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for reference server on port {port}").into());
        }

        sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_implementation_parse_supports_rust_modes() {
        assert_eq!(
            ReferenceImplementation::parse("stateful"),
            Some(ReferenceImplementation::RustSdkStatefulSse)
        );
        assert_eq!(
            ReferenceImplementation::parse("json"),
            Some(ReferenceImplementation::RustSdkStatelessJson)
        );
    }
}
