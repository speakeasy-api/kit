# Completed harness subagent could not be forked

While using Kit as the agent harness, a completed `subagent` result was passed directly to the hidden `fork` tool so documentation work could fan out from a discovery session. The fork failed before prompting with:

```text
could not read .kit/sessions/<session-id>.jsonl: No such file or directory
```

The completed subagent value contained a valid id, generation, output, and updates, but its transcript was not available under the parent runtime root. This creates friction because the model-visible contract presents completed subagent values as reusable by `prompt` and `fork`. Either sessions returned by the configured ACP harness need durable storage discoverable by those tools, or the tool should reject unsupported reuse earlier with guidance to start a fresh subagent.
