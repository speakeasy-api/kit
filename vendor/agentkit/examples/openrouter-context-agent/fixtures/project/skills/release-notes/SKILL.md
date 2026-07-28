---
name: release-notes
description: Prepare release summaries for the lantern project. Use when the user asks about changelogs, release notes, version history, or how releases are formatted.
---

# Release Notes

## Format

Release notes for lantern follow this template:

```
## vX.Y.Z — YYYY-MM-DD

### Added
- Bullet per new feature.

### Changed
- Bullet per behavioral change.

### Fixed
- Bullet per bug fix.

### Migration
- Steps required to upgrade from the previous version.
```

## Rules

- Versions use semver: breaking changes bump major, new features bump minor, fixes bump patch.
- Each bullet must reference the relevant PR number in parentheses, e.g. `(#142)`.
- The Migration section is omitted when no migration steps are needed.
- Release notes are written in past tense ("Added support for…", not "Adds support for…").
- Maximum 5 bullets per section; group smaller changes under a single bullet when possible.
