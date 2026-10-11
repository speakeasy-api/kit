mod a2a;
pub mod ahp;
mod artifact;
mod docs;
mod edit;
pub(crate) mod mcp;
mod observed;
mod read_file;
mod shell;
pub(crate) mod subagent;

pub use crate::credentials::CredentialStorage;
pub use a2a::A2aTool;
pub use artifact::ArtifactTool;
pub use docs::DocsTool;
pub use edit::EditTool;
pub use mcp::{AuthTool, McpTool, ToolSchema, ToolSearch};
pub use observed::Observed;
pub(crate) use observed::shared as observe_shared;
pub use read_file::ReadFileTool;
pub use shell::ShellTool;
pub use subagent::{CloseTool, ForkTool, PromptTool, SubagentTool, Subagents, SubagentsTool};

mod image_gen;
pub use image_gen::ImageGenTool;

mod eval;
pub(crate) use eval::EvalTool;

/// Bind SDK admission to the same decoders used by native tool invocation.
/// Dynamic MCP names and schema-only tools do not have a Rust argument type.
fn native_argument_contracts() -> std::collections::BTreeMap<String, ahp::ToolArgumentContract> {
    [
        ("shell", shell::argument_contract()),
        ("edit", edit::argument_contract()),
        ("artifact", artifact::argument_contract()),
        ("read_file", read_file::argument_contract()),
        ("docs", docs::argument_contract()),
        ("image_gen", image_gen::argument_contract()),
        ("a2a", a2a::argument_contract()),
    ]
    .into_iter()
    .chain(subagent::argument_contracts())
    .chain(mcp::argument_contracts())
    .map(|(name, contract)| (name.to_owned(), contract))
    .collect()
}
