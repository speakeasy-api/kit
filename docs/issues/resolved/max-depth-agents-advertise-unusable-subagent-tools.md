# Maximum-depth agents advertise unusable subagent tools

Agents already running at Kit's maximum subagent depth still received `subagent` and `fork` in the hidden compose catalog. During a nested research run, maximum-depth agents attempted these advertised calls repeatedly; every attempt failed immediately with `subagent depth limit (2) reached`, and retry loops amplified the noise.

## Resolution

Kit now omits `subagent` and `fork` from the compose registry when the current depth is at the configured maximum. The runtime depth checks remain as defense in depth, while non-depth-increasing session tools remain available.

Relevant implementation: `src/runtime.rs` (`Runtime::compose_with_jobs`) and `src/tools/subagent.rs` (`Subagents::check_depth`).
