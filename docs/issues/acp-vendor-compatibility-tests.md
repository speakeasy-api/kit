# ACP subagents lack vendor compatibility tests

Native fork coverage uses a synthetic ACP fixture that advertises `session/fork`, and Kit transcript cloning is tested independently. There are no compatibility tests or a maintained capability matrix for Claude, Codex, or Cursor ACP adapters, nor an explicit end-to-end generic-harness/no-fork case.

Add version-pinned smoke or contract tests where practical, and document externally tested adapter versions and capabilities. Tests should distinguish Kit defects from capabilities that a vendor adapter does not advertise.

Relevant coverage: `fixtures/mock-acp.py`, `src/acp_child.rs` tests, and `tests/subagent_sessions.rs`.
