# ACP subagent launch failures lack actionable diagnostics

During a worktree wave, the built-in `acp.kit` harness repeatedly failed with only:

```text
could not start ACP harness "acp.kit": No such file or directory (os error 2)
```

The parent was running a repository-built Kit executable, but the error did not report the resolved executable, argv, cwd, launch mode (built-in versus configured profile), or captured child stderr. A configured `acp.kit-dev` process separately reached the 30-second ACP handshake timeout, again without enough launch/stderr context to distinguish compilation delay, process failure, and protocol failure.

Subagent startup and handshake errors should include a safely rendered executable path, argv, cwd, profile source, process exit status when available, and a bounded tail of child stderr. Secrets must remain redacted. The error should also distinguish spawn failure, pre-handshake exit, and handshake timeout.

Relevant implementation: `src/acp_child.rs` process spawning, stderr forwarding, and handshake timeout handling.

## Progress

Startup diagnostics now distinguish spawn failure, protocol-handshake failure,
and the existing 30-second handshake timeout. They identify the harness and
whether it came from the built-in executable, an explicit `acp.kit` profile, or
a generic configured profile. The cwd is described only as `runtime root`; the
actual root path, executable, argv, and child stderr are deliberately omitted
because each can contain secrets. Errors after a session becomes ready retain
their existing behavior.

The readiness-channel failure path now preserves the actor's specific spawn or
protocol error instead of replacing it with a generic startup message. Focused
tests verify the phase/source annotations and that command, argument, executable,
and root-path values do not enter the diagnostics.

The observed `acp.kit-dev` timeout followed `cargo clean`, while Cargo was
rebuilding the entire codebase. It is expected startup pressure, not an
unexplained harness defect.

## Resolution

Startup now also races the ACP connection against the child process and reports a
distinct `pre-handshake exit` phase with the process exit status. A focused test
verifies the phase and status while ensuring the command, arguments, and runtime
root remain absent. The 30-second handshake bound remains fixed, and child stderr
remains deliberately excluded because it is untrusted and can contain secrets.
Together with the existing spawn, protocol-failure, and timeout phases, this makes
launch failures actionable without exposing launch credentials.
