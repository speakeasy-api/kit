use std::path::PathBuf;

use openrouter_mcp_tool::run_probe_with_command;

#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY and a live OpenRouter model"]
async fn root_agent_retrieves_secret_via_mcp_tool() {
    let secret = std::env::var("MCP_SECRET").unwrap_or_else(|_| "LANTERN-SECRET-93B7".into());
    let command = PathBuf::from(env!("CARGO_BIN_EXE_openrouter-mcp-tool"));
    let run = run_probe_with_command(&secret, None, command)
        .await
        .unwrap();

    assert!(
        run.tool_calls
            .iter()
            .any(|name| name == "mcp_mock_reveal_secret"),
        "expected the root agent to call the MCP tool, saw {:?}",
        run.tool_calls
    );
    assert!(
        run.output.contains(&secret),
        "expected root output to contain {secret}, got {:?}",
        run.output
    );
}
