# Built-in Kit subagent spawn can lose the current executable

While running Kit through the Kit harness, a `subagent` compose child failed before
the ACP session started:

```text
ACP harness spawn failure: No such file or directory (os error 2)
(harness="acp.kit", source=built-in current executable, cwd=runtime root)
```

The parent session was healthy and `shell` children continued to work. The built-in
`acp.kit` launcher reported that it intended to reuse the current executable, but
the executable path was unavailable when spawning from the runtime root. This
prevents delegated review and other subagent work.

The launcher should retain an absolute, verified path to the current executable
(or surface the attempted path) rather than relying on a relative path or argv
value that may not resolve from the runtime root.
