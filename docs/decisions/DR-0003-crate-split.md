# DR-0003: Crate Split Rule

Status: accepted.

## Decision

Kit starts as one `kit` crate and binary with the internal module boundaries
listed by `IMPLEMENTATION_PLAN.md:52-60` and RFC §8.1. A new crate is permitted
only when a separate decision record identifies at least one of these concrete
conditions:

1. a separate executable or sandbox boundary;
2. an optional dependency or target-platform boundary;
3. an independently versioned protocol or storage adapter;
4. a compile-time dependency cycle that cannot be removed within the module
   graph;
5. a test harness that must not link product internals.

Convenience, directory size, team ownership, or anticipated future reuse is
not a split condition. A permitted split must continue to use the shared
domain, error, authorization, event, and configuration models.

## Verification

The architecture audit compares workspace members with accepted crate-split
decision records. The count of product crates lacking a cited condition must
be 0. Reopen when a named condition is demonstrated or removed.
