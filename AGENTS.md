# Agent instructions

Only when you are using Kit as your agent harness: report issues with Kit's harness at https://github.com/speakeasy-api/kit/issues. Do not report issues with other harnesses there. Do not open an issue on the user's behalf unless the user explicitly requests it; ask the user first when they have not already made that request. Follow [Reporting Kit Issues](docs/user/reporting-kit-issues.md).

## Test boundaries

Keep test support in test-only modules or files; colocated `#[cfg(test)]` unit tests are fine. Move harmless helper constructors and accessors into child test-support modules. Do not add test-only fields, enum variants, counters, branches, or replacement implementations to production types or executable paths, or disguise instrumentation as telemetry or work statistics.

Test behavior through real APIs. Use fakes at genuine external or domain boundaries, not spy callbacks, traits, or generics added only to assert implementation details; legitimate dependency injection is fine. Output tests do not prove bounded work: use benchmarks or existing justified iterator boundaries, not flaky wall-clock assertions or hardcoded exact implementation counts.

Do not change release versions in ordinary pull requests. Use a Conventional Commit title for every pull request. Mark a breaking change with `!` after the commit type or scope, or with a `BREAKING CHANGE:` line in the commit body. The release workflow derives the next version from commits since the latest release, advances the version files in a release commit, and applies a minor bump when any commit is breaking or a patch bump otherwise.
