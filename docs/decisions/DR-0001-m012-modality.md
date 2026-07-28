# DR-0001: M012 Program Modality

Status: accepted for this implementation program; RFC amendment queued.

## Conflict

`RFC.md:301` says a PostgreSQL/object-store implementation **MAY** support
clustered or multi-tenant deployment. `RFC.md:196` separately says Kit **MAY**
split control-plane and executor services later. In contrast,
`IMPLEMENTATION_PLAN.md:10`, `IMPLEMENTATION_PLAN.md:26`,
`IMPLEMENTATION_PLAN.md:740-780`, and `RFC.md:1454` require M012 clustered and
hostile multi-tenant behavior for the initial complete product.

The RFC modality remains `MAY` in the normative registry record sourced from
`RFC.md:301`. This decision does not rewrite that source modality. For this
program, the locked completion goal raises M012 clustered and hostile behavior
to a release-blocking program obligation at `G12`. Service-process splitting
remains optional because the required behavior can be implemented without
making deployment topology part of public semantics.

## Decision

The initial complete product cannot pass `G12` without PostgreSQL/object-store
storage, remote executors, hostile isolation, tenant separation, distributed
fairness, remote identity, disaster recovery, rolling upgrades, and the M012
adversarial/failover evidence. A local-only release is not RFC completion for
this program.

## Queued RFC Amendment

Amendment ID: `RFC-0001-AMEND-001`.

Target: replace the second sentence of `RFC.md:301`:

> A PostgreSQL/object-store implementation MAY support clustered or
> multi-tenant deployment.

with:

> The initial complete product MUST include a PostgreSQL/object-store
> implementation supporting clustered and hostile multi-tenant deployment.
> A local-only distribution MAY omit those service dependencies when it does
> not claim the initial-complete-product conformance profile.

Keep `RFC.md:196` unchanged: splitting control-plane and executor services is
an implementation topology choice, not a waiver of clustered semantics.

The amendment is queued for RFC-owner review under
`IMPLEMENTATION_PLAN.md:811`. This unit does not edit `RFC.md`. Until the
amendment is accepted, this decision is the explicit program-level override;
the source requirement continues to report modality `MAY` and links here.

## Verification

`G12` must report every M012 mandatory work package passing on one release
candidate. Reopen this decision if the RFC amendment is rejected or if the
completion goal no longer includes clustered and hostile deployment.
