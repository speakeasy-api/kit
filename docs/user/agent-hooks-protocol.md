# Agent Hooks Protocol

Kit delivers [Agent Hooks Protocol](https://github.com/agenthooksprotocol/rust-sdk)
events to hook backends registered in `~/.agents/hooks.json` (global) and
`<root>/.agents/hooks.json` (project). When neither file exists, hooks are off.

## Configure hooks

Each file holds an AHP registration:

```json
{
  "protocolVersion": "draft",
  "hooks": [
    {
      "id": "org.example.policy",
      "transport": {
        "type": "stdio",
        "command": "/usr/local/bin/policy-hook",
        "lifecycle": "persistent"
      },
      "subscriptions": [
        {
          "events": ["tool.before"],
          "mode": "intercept",
          "timeoutMs": 3000,
          "failurePolicy": "fail-closed",
          "content": {"default": "body"}
        }
      ]
    }
  ]
}
```

Kit reads the global file, then the project file. A project hook replaces the
global hook with the same `id`; other hooks from both files stay active. Kit
skips an unreadable or malformed file with a warning. A subscription
selector that matches no event Kit emits disables hooks with a warning.

Hook backends run local processes or receive session content. Review both files
before you run Kit in a project you do not trust.

## Events

| Event | Where Kit emits it | Hook authority |
| --- | --- | --- |
| `session.start` | A session's model connection opens | Observe |
| `session.end` | A prompt run finishes or an ACP session closes | Observe |
| `user.message.inbound` | A new turn admits a user prompt | Deny, replace the prompt |
| `model.request.before` | Before each model call, with the transcript | Deny |
| `model.response.after` | A model call finishes, with its output | Observe |
| `model.error` | A model call fails | Observe |
| `tool.before` | Before a tool runs | Allow, deny, return a result, replace or merge input |
| `tool.after` | A tool finishes, with its output | Observe |
| `context.compact.before` | Before compaction summarizes the transcript | Return a summary, replace summarizer instructions |
| `context.compact.after` | Before the summary is installed | Replace the summary |

A denied prompt is removed and the turn ends without a model call. A denied
model request fails the turn. A denied tool call returns a permission error to
the model.

Kit checks edited tool input against the tool's input schema before the tool
runs; an edit that does not match counts as a hook failure. An edit that the
hook does not also allow goes through Kit's permission policy again.

Tool events cover the tools that compose scripts call, such as `shell`, `edit`,
`read_file` and the MCP `tool` dispatcher. Kit projects text, structured data,
tool calls and tool results into message parts. It omits media, files and
reasoning.

Kit does not emit `turn.*`, `model.switch.*`, `config.change.*`, `file.changed`
or the tool permission and progress events yet.
