# Subagent session can stay in `starting` indefinitely

While implementing the macOS desktop app with Kit 0.1.86, a final review subagent remained in `starting` and never accepted work. Closing completed sibling sessions did not unblock it. Cancelling that session and creating a fresh subagent produced the same behavior.

This creates friction because `subagents` provides no failure reason or timeout for the stuck startup, so the parent cannot distinguish queueing from a failed harness launch. The parent must abandon the review or repeatedly cancel and retry.

Expected behavior: a subagent either starts within a bounded interval or transitions to a failed state with a diagnostic that explains the capacity or harness problem.
