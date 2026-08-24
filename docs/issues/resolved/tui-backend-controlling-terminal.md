# Nested agent prompt could take over the TUI terminal

While using Kit as the agent harness, a subagent launched by another subagent ran a command that opened an interactive passphrase prompt through `/dev/tty`. Although ACP and tool processes use piped standard streams, they still inherited the TUI's controlling terminal. The prompt drew over Kit's interface and could leave the headless agent blocked waiting for user input.

## Resolution

The TUI now starts its headless `kit serve` backend in a new Unix session. The backend and all nested agents and tools therefore cannot open the TUI's controlling terminal. Interactive commands fail normally through their captured tool output instead of taking over the interface.
