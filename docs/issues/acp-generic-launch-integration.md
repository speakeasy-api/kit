# Generic ACP harnesses lack Kit runtime integration

Generic ACP harness profiles are launched from their literal configured command and arguments with the runtime root as cwd. Unlike `acp.kit`, they do not receive Kit-specific model, persistent-session/resume, MCP, credential-store, or inherited-depth arguments.

Some asymmetry is intentional because arbitrary ACP programs do not share Kit's CLI. The remaining gap is the absence of a documented, portable way to convey equivalent runtime configuration where ACP supports it, plus a compatibility/configuration guide for common Claude, Codex, and Cursor adapters.

Relevant implementation: `src/acp_child.rs` (`AcpHarnesses::spawn`) and the ACP profile section in `README.md`.
