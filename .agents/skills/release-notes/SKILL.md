---
name: release-notes
description: Use when asked to summarize changes since the last release tag into a customer-facing release note, such as "summarize the changes since the last release", "write the release notes", or "what changed since the last version". Only for drafting the note; for publishing to Slack use announce-kit-release instead.
---

# Release Notes from Git History

1. Last release tag: `git tag --sort=-creatordate | head -1`.
2. Version number: read from `Cargo.toml` at HEAD. Never infer from tag arithmetic — merges bump the patch independently of tags.
3. Collect evidence:
   - Verify the range first: resolve both endpoints with `git rev-parse HEAD <tag>^{commit}`, then count it with `git rev-list <tag>..HEAD --count`. If the count is implausibly large for one release, the range is wrong (e.g. the `..HEAD` suffix was lost) — stop and re-resolve instead of summarizing.
   - Commits with full messages: `git log <tag>..HEAD --format='%h %s%n%b'`
   - Per-commit scope: `git show --stat <sha>`
   - Read the actual diff of behavior-affecting changes to state real user impact, not restated commit subjects.
4. Contributor handles: `gh api repos/<owner>/<repo>/commits/<sha> --jq .author.login` — GitHub maps commit email to account. Never resolve handles by searching usernames or guessing from names; name search returns same-name strangers.
5. Write the note:
   - Title: a one-line theme naming the release's dominant customer benefit.
   - Headline features get a short expanded section with benefit bullets; everything else stays one compact bullet each. If nothing needs expansion, just list compact bullets.
   - Attribute every change to `@handle`.
   - Close with a contributors list and a compatibility statement.

## Style rules (hard)

- Customer-facing: describe what the user experiences, not what the code does.
- No internal identifiers: file paths, class/function names, line counts, PR or issue numbers, commit hashes. If the user wanted git output they would use git.
- GitHub handles, never full names.
- No emojis.
- Version comes from the manifest, never from tag arithmetic.
