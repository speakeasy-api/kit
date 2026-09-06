// Production policy is deliberately non-overridable. Test-only scopes below
// retain assertions and unwrap ergonomics; placeholder lints remain denied.
#![cfg_attr(
    not(test),
    forbid(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::disallowed_methods,
        clippy::disallowed_macros
    )
)]

mod acp_child;
mod artifacts;
pub mod compaction;
mod compose_output;
#[doc(hidden)]
pub mod config_files;
mod credentials;
pub mod docs;
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
        finish_best_effort_recovery, finish_recovery, request_shutdown, shutdown_token,
        start_recovery_worker,
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
