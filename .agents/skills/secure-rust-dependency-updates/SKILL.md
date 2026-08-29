---
name: secure-rust-dependency-updates
description: Use when asked to update Rust crate dependencies only after checking release provenance and supply-chain risk, including requests such as "safely update these crates", "audit dependency changes before upgrading", or "update as many dependencies as feasible without trusting new releases blindly".
---

# Secure Rust Dependency Updates

## Preconditions

1. Resolve the repository root, requested base, worktree, branch, and exact old-to-new crate list. Inspect repository instructions, release/versioning rules, and contribution guidance before changing files.
2. Require a clean target worktree unless the user explicitly authorizes mixing changes. If a fresh worktree was requested, fetch the base and create it before auditing.
3. Locate every relevant manifest and lockfile entry. Classify each crate as direct or transitive, and use `cargo tree -i <crate>@<version>` to identify exact pins, upper bounds, features, and dependents.

## Audit before editing

4. Keep the repository unchanged while auditing. Parallelize independent crate families when useful, but require evidence for every recommendation.
5. For every old and candidate release, including intermediate releases when versions are skipped:
   - Check registry owner and publisher continuity, yanked status, repository, license, and MSRV.
   - Download the published archives and verify their SHA-256 values against registry metadata. Verify the locked release against the lockfile checksum.
   - Match `.cargo_vcs_info.json` to the declared upstream commit or tag and compare packaged source with that revision. Record missing, lightweight, or unsigned tags and sole-owner packages as residual risks.
   - Diff `Cargo.toml.orig`, dependencies, default and optional features, target-specific dependencies, build dependencies, MSRV, and crate target types.
   - Inspect added or changed `build.rs`, proc macros, binaries, executable files, symlinks, unsafe code, process spawning, network/download behavior, generated-code paths, and opaque payloads.
   - Review changelogs and source diffs for API, validation, protocol, resource-limit, error-handling, and compatibility changes that can affect the repository.
6. Never treat unchanged ownership or a matching checksum as sufficient by itself. Hold a release when provenance is inconsistent, source cannot be reconciled, new executable behavior is unexplained, or behavior compatibility is unclear.
7. Simulate the requested manifest edits in a temporary snapshot created from the current revision. Resolve with targeted `cargo update -p <crate>@<old> --precise <new>` commands, then enumerate the complete lockfile package/version delta.
8. Audit every newly added or upgraded transitive package from the simulated resolution to the same standard. Explain parallel semver lines instead of assuming a newly added version replaces the old one.
9. For transitive candidates, run an exact dry-run update. If an upstream dependency imposes an exact pin or upper bound, mark the update blocked and name the constraining package. Do not patch crates or broaden into unrelated upstream upgrades without explicit approval.
10. Produce an approve/hold decision for each requested crate before modifying the real worktree. Include material behavior changes and unresolved provenance caveats.

## Implement

11. Apply only approved, resolvable updates. Preserve required behavior explicitly when defaults or optional features change; enable the necessary feature rather than accepting a silent validation or protocol regression.
12. Update the lockfile with the narrowest targeted Cargo command. Reject unrelated compatible upgrades introduced by an unscoped `cargo update` unless they were separately audited and requested.
13. Apply the repository's required release/version bump for dependency changes.

## Validate

14. Inspect the final manifest and lockfile diff, run `git diff --check`, and confirm that the changed package set matches the audited simulation.
15. Use `cargo tree -e features` for behavior-critical feature changes. Run the smallest useful repository checks, normally `cargo check --all-targets` and relevant tests.
16. Run `cargo audit` when available. Separate pre-existing or explicitly allowed warnings from findings introduced by the update; do not silently expand scope to fix unrelated advisories.
17. Report updated crates, blocked crates with exact constraints, transitive changes, residual provenance risks, version bump, and check results.
18. If publishing was requested, commit and push the cohesive change, then open or update a pull request. Keep the description reviewer-focused: summarize dependency scope, behavior impact, provenance conclusion, explicit feature decisions, and intentionally blocked pins. Read the pull request back to verify its title and body.
