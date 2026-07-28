---
name: deploy-checks
description: Validate staging deployments for the lantern project. Use when the user asks about deploy readiness, pre-flight checks, staging validation, or deployment checklists.
---

# Deploy Checks

## Pre-deploy checklist

Before promoting a staging build to production, verify all of the following:

1. `cargo test --all-features` passes with zero failures.
2. `cargo clippy --all-features -- -D warnings` produces no diagnostics.
3. The `CHANGELOG.md` entry for the release version exists and is non-empty.
4. Environment variable `LANTERN_ENV` is set to `staging` on the staging host.
5. The health endpoint (`GET /healthz`) returns HTTP 200 within 5 seconds.

## Rollback procedure

If any check fails after deploy:

1. Revert to the previous container image tag.
2. Run the health endpoint check again.
3. File an incident in the `#lantern-ops` Slack channel with the failing check name.

## Staging-specific notes

- Staging uses the database `lantern_staging` on host `db-staging.internal`.
- Secrets are loaded from AWS SSM parameter store under `/lantern/staging/`.
- Rate limiting is disabled in staging to simplify load testing.
