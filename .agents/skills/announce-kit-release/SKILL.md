---
name: announce-kit-release
description: Use when asked to summarize the current Kit release against the previous release and publish a customer-facing announcement to Slack, including requests such as "announce the latest release", "post release notes", or "share what changed since the last version".
---

# Announce a Kit Release

1. Confirm the current checkout, latest release tag, and immediately preceding release tag. Fail if the current revision is not the release being announced or if either comparison tag is missing.
2. Inspect the commit subjects, changed files, user documentation, and release-range diff between the two tags. Use these only as evidence; do not put raw commits, hashes, file lists, diff statistics, or implementation details in the announcement.
3. Translate the changes into concise customer outcomes. Group related work into a short list, lead with the release version, and mention compatibility or performance fixes only when they affect users. Do not use emojis.
4. Format the payload as Slack `mrkdwn`, not GitHub Markdown. Use single asterisks for bold titles (`*What's new*`), not `**bold**` or `#` headings. If using Block Kit sections, set their text type to `mrkdwn`, not `plain_text`. Include mise installation instructions in plain triple-backtick code blocks with no language label: Slack displays labels such as `shell` as literal code rather than syntax highlighting. The opening fence, command text, and closing fence must be on separate lines. Never put a command on the opening-fence line or put the closing fence after a command. Use this exact payload shape:

   ````text
   *Install or upgrade with mise*
   ```
   mise use -g github:speakeasy-api/kit
   kit --version
   ```

   *To pin this version:*

   ```
   mise use -g github:speakeasy-api/kit@<version-without-v-prefix>
   ```
   ````

5. Use the Slack channel ID from `SLACK_CHANNEL_ID` in the environment, or an explicit channel ID supplied in the task. Pass this ID directly to Slack tools; do not search, list, or resolve channels by name. The configured ID remains valid if the channel is renamed. If no channel ID is supplied, stop and ask for one rather than guessing a destination.
6. Before sending or drafting, inspect the exact Slack payload. The release title and section titles must use Slack's single-asterisk bold syntax. Every opening and closing triple-backtick fence must be on its own line with no language label; the first line inside each code block must be an installation or verification command, not `shell` or `bash`. Prose or links must appear only after a closing fence. Reject language-tagged fences, commands on fence lines, and unclosed blocks.
7. If the user explicitly asked to post, send the announcement directly. Otherwise, create a draft for review. Post only to the supplied channel ID; fail if it is invalid or inaccessible, and never substitute another destination.
8. Read the sent message back from Slack and verify that the install commands did not absorb later prose or links. For a restricted bot explicitly known to lack history access, instead inspect the actual message returned in Slack's successful send response: verify the channel matches the supplied channel ID, a message timestamp is present, and the returned text preserves the bold title markers and unlabeled, correctly delimited code blocks. This verifies the send response, not a later history read. A bare success acknowledgement or the original outgoing payload is not sufficient; fail if the response lacks the posted message. If the message is malformed, replace or delete it when Slack tools allow that; otherwise post one corrected copy and report that the malformed message requires manual deletion.
9. For a sent announcement, retrieve its permalink through Slack tools using the verified channel and message timestamp. Return the Slack message or draft link.

## Announcement shape

- Heading: `*Kit <version> is available*`
- One sentence describing the overall customer benefit.
- `*What's new*` with three to six outcome-focused bullets.
- `*Install or upgrade with mise*` with the install and verification commands in an unlabeled code block, then `*To pin this version:*` with the pinned-version command in a separate unlabeled code block.
- No emojis, raw commits, hashes, diff statistics, or internal implementation notes.
