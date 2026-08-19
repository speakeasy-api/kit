# Kit ACP server cannot expose native session forks

Status: unresolved; intentionally out of scope for the Kit-owned ACP server.

Kit's ACP client supports `session/fork`, including capability detection,
concurrent sibling sessions, and `session/close`. Kit's ACP server cannot
currently advertise or handle the same method.

## Current boundary

Kit owns its ACP server and per-session actor in `src/protocols/acp.rs`, using
`agent-client-protocol` for transport and the public `AcpIntegration` API for
conversion, observer routing, and session binding. The server deliberately
advertises and handles `initialize`, `session/new`, `session/prompt`,
`session/cancel`, and `session/close`, but does not advertise or register
`session/fork`.

`src/protocols/acp.rs` has a protocol-level test that locks in this boundary:
initialization omits the fork capability and a `session/fork` request returns
method-not-found. Native server-side fork remains a separate design decision; it
must not be inferred from Kit's client-side support for forking nested ACP
harnesses.
