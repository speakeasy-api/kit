mod acp_child;
mod artifacts;
pub mod compaction;
mod compose_output;
#[doc(hidden)]
pub mod config_files;
mod credentials;
pub mod docs;
mod effects;
pub mod events;
mod fatal;
mod file_search;
#[path = "resilient_fs/mod.rs"]
mod filesystem;
pub mod plugins;
pub(crate) mod process_tree;
pub mod protocols;
pub mod provider;
pub mod runtime;
/// Shared internal filesystem and process-lifetime recovery controls.
pub mod resilient_fs {
    pub use crate::filesystem::*;
    pub use crate::storage_runtime::{
        finish_recovery, request_shutdown, shutdown_token, start_recovery_worker,
    };
}
pub mod session;
mod storage_runtime;
pub mod telemetry;
pub mod tools;
pub mod transcript;
pub mod tui;

pub use acp_child::{
    AcpHarnessProfile, AcpHarnesses, AcpPermissionPolicy, BUILTIN_HARNESS, SubagentHarnessPolicy,
};
pub use provider::{ProviderKind, ReasoningEffort};
pub use runtime::Runtime;
