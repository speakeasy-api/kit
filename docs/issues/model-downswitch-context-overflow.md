# Model downswitch can exceed the target context window

Kit can switch the main ACP session to a model with a smaller context window without first checking whether the existing transcript fits that model. The next turn keeps the live transcript and creates the selected provider session, so the target provider can reject the request for exceeding its context limit.

Automatic compaction does not currently prevent this. Its trigger uses the latest provider-reported usage and context-window metadata in the transcript, which still describes the previously active model. After the switch, manual or automatic compaction also follows the new model and may fail because its summarization request contains the already oversized transcript.

Until this is fixed, compact with the larger model before switching:

```text
/compact
/model <lower-context-model>
```

A safe downswitch should be transactional and serialized through the session actor. Kit should retain context-window information in the model catalog, compare the transcript against the target window, compact with the currently active model before publishing the new selection, persist the replacement transcript, and then apply the switch. If the target window is unknown or compaction cannot make the transcript fit, Kit should leave the current selection unchanged and return an actionable error.

Relevant implementation: `src/provider/adapter.rs` (`SelectableSession::begin_turn`), `src/protocols/acp.rs` (`Command::SetModel` and `set_model`), and `src/compaction.rs` (`automatic`, `compaction_reason`, and `KitCompactionBackend`).
