# MCP `auth` hangs after refreshing stored credentials

While using Kit as the agent harness, calling `auth` for an MCP server that had previously authenticated hung instead of returning `authenticated` or an OAuth URL. The server had rejected its stored access token, and Kit successfully entered the refresh-and-reconnect path.

`McpRuntime::authorize` removed the saved OAuth manager from `oauth_managers` inside an `if let` chain. The temporary async mutex guard remained alive through the `if` body. After a successful refresh and reconnect, the body attempted to lock `oauth_managers` again to restore the manager, causing a self-deadlock.

## Resolution

Remove the OAuth manager in a separate statement and scope before awaiting refresh or reconnect work. This drops the mutex guard before the success path reacquires the lock to store the refreshed manager.
