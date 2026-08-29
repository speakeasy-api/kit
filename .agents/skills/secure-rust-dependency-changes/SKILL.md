---
name: secure-rust-dependency-changes
description: Use whenever a Rust dependency is added or updated, including direct, transitive, build, development, target-specific, and optional dependencies.
---

# Secure Rust Dependency Changes

## Preconditions

1. Resolve the repository root, requested base, worktree, branch, and dependency change. Classify it as an addition or an update. Inspect repository instructions, release/versioning rules, and contribution guidance before changing files.
2. Require a clean target worktree unless the user explicitly authorizes mixing changes. If a fresh worktree was requested, fetch the base and create it first.
3. Locate every relevant manifest and lockfile entry. For updates, use `cargo tree -i <crate>@<version>` to identify direct and transitive users, exact pins, upper bounds, and active features.

## Security audit — every addition and update

4. Audit before accepting the dependency change. Keep the real manifest and lockfile unchanged during the audit when practical.
5. For every crate release that the change newly selects:
   - Check registry owners, publisher identity and history, yanked status, repository, license, maintenance activity, and MSRV. For updates, compare these with the locked release and inspect intermediate releases when versions are skipped.
   - Download the published archive and verify its SHA-256 value against registry metadata. For updates, verify the locked archive against the lockfile checksum too.
   - Match `.cargo_vcs_info.json` to the declared upstream commit or tag and compare packaged source with that revision. Record missing, lightweight, or unsigned tags and sole-owner packages as residual risks.
   - Inspect `Cargo.toml.orig`, dependencies, default and optional features, target-specific dependencies, build dependencies, and crate target types.
   - Inspect `build.rs`, proc macros, binaries, executable files, symlinks, unsafe code, process spawning, network/download behavior, generated-code paths, and opaque payloads.
6. Resolve the proposed change in a temporary snapshot and enumerate the complete lockfile package/version delta. Audit every newly introduced or upgraded transitive package to the same standard; packages already present at the same version do not need a new audit.
7. Never treat unchanged ownership or a matching checksum as sufficient by itself. Hold the change when provenance is inconsistent, source cannot be reconciled, executable behavior is unexplained, maintenance or licensing is unacceptable, or the transitive audit is incomplete.

## Update validation — updates only

8. Review changelogs and source diffs for API, validation, protocol, resource-limit, error-handling, feature-default, and compatibility changes that can affect the repository.
9. Simulate manifest edits in the temporary snapshot. Resolve with targeted `cargo update -p <crate>@<old> --precise <new>` commands and confirm the full lockfile delta is understood.
10. For transitive candidates, run an exact dry-run update. If an upstream dependency imposes an exact pin or upper bound, mark the update blocked and name the constraining package. Do not patch crates or broaden into unrelated upstream upgrades without explicit approval.
11. Produce an approve/hold decision for each requested update before modifying the real worktree. Include material behavior changes, required feature changes, resolution blockers, and residual provenance risks.

A newly added dependency does not need a separate functional-validation phase in this skill. Its API fit and behavior are validated by implementing and testing the feature that requires it. The security audit remains mandatory.

## Implement

12. For additions, add the audited version with only the features the feature implementation needs. For updates, apply only versions that passed both the security audit and update validation.
13. Preserve required behavior explicitly when defaults or optional features change; do not accept a silent validation or protocol regression.
14. Update the lockfile with the narrowest targeted Cargo command. Reject unrelated compatible upgrades introduced by an unscoped `cargo update` unless they were separately audited and requested.
15. Apply the repository's required release/version bump.

## Verify and report

16. Inspect the final manifest and lockfile diff, run `git diff --check`, and confirm that the changed package set matches the audited temporary resolution.
17. For updates, use `cargo tree -e features` for behavior-critical feature changes and run the smallest useful compatibility checks. For additions, rely on the feature's normal build and test work rather than creating redundant dependency-only validation.
18. Run `cargo audit` when available. Separate pre-existing or explicitly allowed warnings from findings introduced by the change; do not silently expand scope to fix unrelated advisories.
19. Report added and updated crates, blocked updates with exact constraints, transitive changes, audit decisions, residual provenance risks, version bump, and the feature or update checks that were run.
20. If publishing was requested, commit and push the cohesive change, then open or update a pull request. Keep the description reviewer-focused: summarize dependency scope, behavior impact, provenance conclusion, explicit feature decisions, and intentionally blocked pins. Read the pull request back to verify its title and body.
