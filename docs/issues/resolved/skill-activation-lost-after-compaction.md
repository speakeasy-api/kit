# Skill activation state outlived compacted skill instructions

Kit deduplicated `activate_skill` calls by session in process-local state. Transcript compaction independently replaced historical tool calls and results with a generated checkpoint. If that checkpoint did not preserve a previously activated skill's full instructions, a later activation in the same session returned `Skill already read.` even though those instructions were no longer present in model context.

This occurred in session `s-1787332399269-74710-1`: `ship-issue` was activated at generation 5, compaction replaced the transcript at generation 375 without retaining the skill or its instructions, and activation at generations 378–379 returned `Skill already read.` The same failure occurred for `simplify` at generations 656–657 after later compactions.

## Resolution

Each agent driver now owns a `SkillRegistry` shared only by its generated `activate_skill` tool and compactor. After compaction removes an activated skill result and any durable replacement succeeds, the compactor calls `reset_activations`. Failed and no-op compactions, marker-only manual compactions, and replacements that retain the skill result do not change activation state. Skills whose instructions disappeared from model-facing history can therefore be activated again without affecting unrelated sessions.

Regression tests verify removal detection and prove that resetting one session registry restores its full skill body while another session remains deduplicated.

Relevant implementation: `src/compaction.rs` (`AutomaticCompactor::mutate`) and `src/runtime.rs` (`build_skill_tools` and `Runtime::compose_with_jobs`).
