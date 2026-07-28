# agentkit-acp

Agent Client Protocol integration for agentkit hosts.

This crate re-exports upstream ACP wire types from `agent-client-protocol` and
adds only agentkit-specific glue: session binding, observer routing, prompt
conversion, cancellation handles, and approval resolver abstractions.

Hosts can wire `AcpIntegration` into their own `AgentBuilder` as a
`LoopObserver`, bind ACP session ids to agentkit session ids, and drain
`AcpClientMessage`s into their ACP connection.

For standalone agents, `AcpHeadlessRuntime` serves ACP over stdio or an
upstream SDK transport. It handles initialize, session lifecycle, prompt
conversion, streaming updates, cancellation, and ACP permission requests for
agentkit approval interrupts.

Run the in-memory end-to-end example with:

```sh
cargo run -p openrouter-acp-trio
```
