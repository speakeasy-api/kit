# Background subagents remain in `starting`

## Summary

Subagents launched from one background `compose` call remained in the `starting` state indefinitely. The same subagent workflow started and completed normally when launched alone in a foreground `compose` call.

## Environment

- Kit harness: 0.1.86
- Parent subagent depth: 1/2
- Three independent `subagent` calls were launched from one `compose` program with `background: 1`.

## Observed behavior

The three sessions remained in `starting` for more than two minutes. Closing completed inspection sessions did not unblock them. The parent had to close the three starting sessions and relaunch each task in a foreground call. This delayed implementation and prevented the intended parallel edit phase.

## Expected behavior

Background subagents should leave `starting`, or fail with an actionable startup error. If concurrent startup is unsupported, `compose` should queue the calls visibly instead of leaving them indefinitely in `starting`.

## Workaround

Close the stuck sessions and launch each subagent in a foreground `compose` call.
