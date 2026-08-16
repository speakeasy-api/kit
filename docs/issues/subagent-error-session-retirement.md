# Dispatched subagent errors retire the reusable session

After a follow-up prompt is dispatched, any unsuccessful outcome removes that logical subagent from Kit's registry. Kit does this because the remote or durable transcript may have changed, so accepting the prior generation could continue from an unknown state.

The conservative rule prevents unsafe reuse but makes transient transport and protocol failures unrecoverable. A recovery design would need a way to determine whether the turn committed, reconcile generations, or restart from a known completed snapshot.

Relevant implementation: `src/tools/subagent.rs` (`Subagents::prompt`). A concrete trigger is tracked separately in `docs/issues/subagent-keepalive.md`.
