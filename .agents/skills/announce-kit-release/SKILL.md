---
name: announce-kit-release
description: Use when asked to summarize the current Kit release against the previous release and publish a customer-facing announcement to Slack, including requests such as "announce the latest release", "post release notes", or "share what changed since the last version".
---

# Announce a Kit Release

1. Confirm the current checkout, latest release tag, and immediately preceding release tag. Fail if the current revision is not the release being announced or if either comparison tag is missing.
2. Inspect the commit subjects, changed files, user documentation, and release-range diff between the two tags. Use these only as evidence; do not put raw commits, hashes, file lists, diff statistics, or implementation details in the announcement.
3. Translate the changes into concise customer outcomes. Group related work into a short list, lead with the release version, and mention compatibility or performance fixes only when they affect users. Do not use emojis.
4. Include mise installation instructions in fenced `shell` code blocks. The opening fence, command text, and closing fence must be on separate lines. Never put a command on the opening-fence line or put the closing fence after a command. Use this exact payload shape:

   ````markdown
   ```shell
   mise use -g github:speakeasy-api/kit
   kit --version
   ```

   To pin this version:

   ```shell
   mise use -g github:speakeasy-api/kit@<version-without-v-prefix>
   ```
   ````

5. Search Slack for the requested channel by name and resolve its current channel ID. Never hard-code a Slack channel ID.
6. Before sending or drafting, inspect the exact Slack payload. Every opening ` ```shell` fence must end immediately with a newline, every closing ` ``` ` fence must be on its own line, and prose or links must appear only after a closing fence. Reject payloads such as ` ```shell command` or `command``` `.
7. If the user explicitly asked to post, send the announcement directly. Otherwise, create a draft for review. Do not post to an approximate channel name; fail and ask for clarification if the exact channel cannot be found.
8. Read the sent message back from Slack and verify that the install commands did not absorb later prose or links. If it is malformed, replace or delete it when Slack tools allow that; otherwise post one corrected copy and report that the malformed message requires manual deletion.
9. Return the Slack message or draft link.

## Announcement shape

- Heading: `Kit <version> is available`
- One sentence describing the overall customer benefit.
- `What's new` with three to six outcome-focused bullets.
- `Install or upgrade with mise` with the install, verification, and pinned-version commands.
- No emojis, raw commits, hashes, diff statistics, or internal implementation notes.
