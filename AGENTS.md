# Agent instructions

Only when you are using Kit as your agent harness: report issues with Kit's harness at https://github.com/speakeasy-api/kit/issues. Do not report issues with other harnesses there. Do not open an issue on the user's behalf unless the user explicitly requests it; ask the user first when they have not already made that request. Follow [Reporting Kit Issues](docs/user/reporting-kit-issues.md).

Do not change release versions in ordinary pull requests. Use a Conventional Commit title for every pull request. Mark a breaking change with `!` after the commit type or scope, or with a `BREAKING CHANGE:` line in the commit body. The release workflow derives the next version from commits since the latest release, advances the version files in a release commit, and applies a minor bump when any commit is breaking or a patch bump otherwise.
