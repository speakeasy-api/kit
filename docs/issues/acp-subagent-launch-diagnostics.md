# ACP subagent launch failures lack actionable diagnostics

During a worktree wave, the built-in `acp.kit` harness repeatedly failed with only:

```text
could not start ACP harness "acp.kit": No such file or directory (os error 2)
```

The parent was running a repository-built Kit executable, but the error did not report the resolved executable, argv, cwd, launch mode (built-in versus configured profile), or captured child stderr. A configured `acp.kit-dev` process separately reached the 30-second ACP handshake timeout, again without enough launch/stderr context to distinguish compilation delay, process failure, and protocol failure.

Subagent startup and handshake errors should include a safely rendered executable path, argv, cwd, profile source, process exit status when available, and a bounded tail of child stderr. Secrets must remain redacted. The error should also distinguish spawn failure, pre-handshake exit, and handshake timeout.

Relevant implementation: `src/acp_child.rs` process spawning, stderr forwarding, and handshake timeout handling.
