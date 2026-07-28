# Core Evaluation Harness v1

The M004 core harness consumes immutable Phase-0 trial, task, and grader manifests. It verifies
source, toolchain, harness, hidden-test, acceptance, and gold-patch pins before invoking a
trial. Every invocation crosses the M003 `IsolatedTrialContract`; source conformance tests run the same
versioned grader protocol in the owned `kit-core-grader` subprocess only through the test-only
`source_semantics_fake` token. That route is source semantics, not platform evidence. On non-Linux
hosts `ConformanceCoreTrialExecutor::default()` returns typed unavailable for memory evidence. Production uses
`ProductionCoreTrialExecutor`, which delegates to `execute_production_trial`; an unavailable trusted
helper or image is an external-blocked result and never a host-process fallback.

The grader applies the accepted M004 unified-diff normalization engine to a fresh copy of the pinned source snapshot
and evaluates only declared content checks. It has no command-launching API. File, byte, check,
artifact, and log bounds are checked before grading. Linux production uses helper/cgroup enforcement;
unsupported local hosts return typed conformance-unavailable. Darwin source fixtures never claim hard
memory enforcement. Paths for hidden
tests, gold patches, acceptance rules, and harness configuration are protected, and those materials
are passed only to the grader side of the executor adapter.

Canonical report bytes bind all immutable inputs, normalized M003 route/profile/quiescence evidence,
actual artifact content digests, measured usage, public checks, hidden aggregate verdict/count/digest,
and outcome. Per-hidden-check details are retained only as an authenticated encrypted grader artifact.
Canaries come only from the digest-validated hidden manifest and cover raw, URL, base64, split, and
binary artifact forms. The ignored production probe is enabled only by a nonce-bound manifest
authenticated with the grader-only artifact key; normal production invocations pass neither the
manifest nor probe arguments. It independently targets the grader log, canonical report, combined
public/hidden check channel, final tree, and an extra artifact. Request IDs, process IDs,
and timing are retained in the separate volatile envelope and cannot perturb the report digest.
Calibration cases are constructed only from trusted base, pinned gold, empty, parser-invalid, and
protected-authority fixtures; callers cannot provide their bytes. Calibration binds task and invariant
harness/grader inputs, while each admitted roster entry is checked independently against the trial's
model, model-settings, config, and provider-capability digests.

Production usage receipts are minted and reverified transactionally by
`SqliteTrialUsageReceiptStore` from ordered provider/model/tool events, scheduler reconciliation,
attempt/fence ownership, provider request IDs, token/cost counters, and durable commit positions.
Missing or zero provider-call evidence cannot mint a receipt.

Source conformance:

```sh
cargo test --locked --test conformance harness_selfcheck
```

The trusted x86_64 and aarch64 cells run the exact ignored production-core test with
`KIT_CORE_EVIDENCE_ROOT` and upload artifacts plus an attestation. No external production PASS is
claimed until those cells actually complete and retain evidence.
