# Kit ACP server cannot expose native session forks

Status: unresolved; blocked upstream.

Kit's ACP client supports `session/fork`, including capability detection,
concurrent sibling sessions, and `session/close`. Kit's ACP server cannot
currently advertise or handle the same method.

## Blocker

Kit pins `agentkit-acp` at 0.10.6. That dependency owns server routing through
`AcpHeadlessRuntime`. That runtime:

- constructs a fixed `AgentCapabilities` value without
  `SessionCapabilities::fork`;
- registers handlers for `initialize`, `session/new`, `session/prompt`,
  `session/cancel`, and `session/close`, but not `session/fork`;
- keeps its ACP-session-to-driver map private; and
- gives `AcpAgentFactory` only a `start` callback, with no fork callback or
  source-session context.

Kit therefore cannot add a correct fork route around the helper. Reimplementing
the whole private headless runtime locally would duplicate session routing,
cancellation, approval, notification, and close behavior and is not a safe
compatibility fix.

`src/protocols/acp.rs` has a protocol-level test that locks in this external
limitation: initialization omits the fork capability and a `session/fork`
request returns method-not-found.

## Upstream requirement

Upgrade when `agentkit-acp` exposes native fork support, or APIs that let Kit:

1. advertise `SessionForkCapabilities`;
2. receive the source ACP session ID and requested workspace;
3. create the destination driver from a stable completed-turn transcript
   snapshot;
4. bind the new ACP session to its own cancellation and notification route; and
5. close source and forked sessions independently.

After that upgrade, replace the blocker test with an end-to-end test that
creates a session, forks it, prompts both branches concurrently, and closes
both.
