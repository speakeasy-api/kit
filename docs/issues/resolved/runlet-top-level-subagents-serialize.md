# Independent top-level Runlet subagents execute sequentially

Five independent top-level `subagent` bindings executed one after another even though the Compose primer said independent calls without data dependencies run concurrently. The same calls inside a `for` ran concurrently after Runlet's intentional first-iteration warmup.

The subagent tools had default annotations, which `agentkit-tool-compose` mapped to Runlet `ExecutionPolicy::AtMostOnce`. Runlet treated every statement containing a non-`Pure` call as an implicit effect root and evaluated those statements in source order. Consequently, separate top-level subagent statements were serialized before the returned list was assembled; the dispatch limit never created parallelism that the evaluator did not expose.

Relevant implementation: Runlet effect-root analysis/evaluation, `agentkit-tool-compose` execution-policy mapping, and `src/tools/subagent.rs` tool annotations.

## Resolution

Resolved by Runlet 0.3.0 and agentkit-tool-compose 0.10.7. Independent implicit effect roots, including top-level tool and subagent calls, now run concurrently. Source order no longer provides implicit sequencing; callers use ordinary data dependencies when consuming a result or `after prerequisite { return tool(...) }` when control ordering is required without data flow. Kit 0.1.19 pins those releases and adds runtime and model-visible primer coverage for both behaviors.
