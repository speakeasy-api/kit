mod client;
mod command_tree;
mod daemon;
mod http;
mod output;
mod parser;

pub use client::{
    Client, ClientError, ClientErrorKind, ClientRequest, ClientResponse, EmbeddedClient,
    MutationRequest, PromptRequest, execute_with_retry, wait_for_terminal_run,
};
pub use command_tree::command as command_tree;
pub use daemon::{
    AutoStart, DISCOVERY_FILE, DaemonConnection, DaemonDiscovery, DiscoveryError, connect_daemon,
    default_state_root, read_discovery,
};
pub use http::{HttpClient, operation_route};
pub use output::{
    EXIT_CONFLICT, EXIT_INTERNAL, EXIT_NOT_FOUND, EXIT_OK, EXIT_RUN_FAILED, EXIT_TRANSPORT,
    EXIT_USAGE, Output, render_error, render_exec_response, render_response,
};
pub use parser::{
    CLI_OPERATIONS, Cli, DaemonCommand, Invocation, OperationDescriptor, OutputFormat, ParseError,
    parity_table, parse,
};
