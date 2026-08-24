# ACP subagents lack vendor compatibility tests

Native fork coverage uses a synthetic ACP fixture that advertises `session/fork`, and Kit transcript cloning is tested independently. There are no compatibility tests or a maintained capability matrix for Claude, Codex, or Cursor ACP adapters, nor an explicit end-to-end generic-harness/no-fork case.

Add version-pinned smoke or contract tests where practical, and document externally tested adapter versions and capabilities. Tests should distinguish Kit defects from capabilities that a vendor adapter does not advertise.

Relevant coverage: `fixtures/mock-acp.py`, `src/acp_child.rs` tests, and `tests/subagent_sessions.rs`.

## Resolution

Kit now documents a version-pinned compatibility snapshot for Claude Agent ACP 0.63.0, Codex ACP 1.4.0, and Cursor CLI ACP 2026.08.04 in the README, including each adapter's observed `session/fork` capability and Kit behavior. The subagent manager also has an explicit process-backed `generic_harness_without_native_fork_returns_unsupported` test using `fixtures/mock-acp.py`. Runtime behavior remains capability-driven rather than vendor-name-driven, so CI does not need authenticated vendor binaries to distinguish unsupported adapter capabilities from Kit defects.
