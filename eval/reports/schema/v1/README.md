# Core Statistical Report v1

`RegistrationAuthority` owns a single contiguous SQLite ledger for registrations, admissions,
scheduler admission consumptions, executions, experiment freezes, and reports. Every row carries one
global position, its predecessor digest, and an attempt ordinal; SQLite triggers reject updates and
deletes. Before external CAS, the authority durably records the exact pending descendant bytes and
digest. Recovery either completes CAS from the old head or finalizes an already-installed new head;
forks and multiple pending children fail closed. Production opening is unavailable without a
transparency or secure `LedgerAnchor`. The conformance anchor is excluded from release builds and its
receipts are never production evidence.

`ProductionEvaluationService` is the production execution entrypoint and accepts only
`ProductionCoreTrialExecutor`. It retains immutable global harness, grader-manifest, agent image,
grader image, helper, and runtime pins. Each roster entry supplies its exact task-manifest pin; the
selected harness and its task-specific calibration token are checked before dispatch, and both
terminal boundary attestations are checked before recording. Generic executors are available only
through the debug-only `ConformanceEvaluationService`.

The durable scheduler consumes
each signed, nonce-bearing `TrialAdmission` exactly once into a chained pending-anchor record and does
not make the run executable until the authority has anchored that consumption. The scheduler run ID,
token, nonce, positions, and consumption digest appear in the executor request, provider/tool/events
evidence, CoreHarness report, and sealed receipt. The execution ledger accepts only the resulting
harness-authenticated `MeasuredTrialReceipt`, checks its event high-water mark, and never accepts a
caller-created `TrialEvidence` object. Every roster entry permits exactly one measured attempt; missing,
incomplete, failed, and preregistered-excluded entries remain explicit and no latest-win retry exists.
The coordinator captures an event watermark before dispatch and transactionally stores the terminal
watermark with terminal scheduler state after all effects are durable. Evidence is exactly the
run-bound interval `(start, terminal]`; earlier events are excluded and same-run later events reject.

Before creating an error receipt, the coordinator durably enters `terminalizing` with the canonical
bounded reason, elapsed time, durable scheduler usage totals, and exact imputation provenance. Replay
reuses that record through authority receipt, scheduler completion, and final phase. Plans preregister
strictly positive conservative maximum cost and latency imputations. Error receipts are therefore
never free or zero-duration, and an imputed confirmatory cost or latency metric is reported as
`confirmatory_metric_unavailable` with no non-inferiority decision.

The sole confirmatory metric uses a two-sided 95 percent finite-sample paired t interval for continuous
means. Binary matched risk differences use unconditional simultaneous Clopper-Pearson intervals for
the full-sample multinomial probabilities `p10` and `p01`: each component receives a Bonferroni alpha
split and the difference is `[L10-U01, U10-L01]`, clamped to `[-1,1]`. One-sided non-inferiority uses
the corresponding simultaneous one-sided bounds. This construction is deliberately conservative,
including at zero discordance, in exchange for coverage of at least 95 percent and type-I error at most
5 percent without conditioning on the observed discordant margin. Exhaustive finite multinomial-grid
tests cover all outcome cells, zero margins, and zero discordance. Exploratory metrics remain point
estimates only.

After every fixed-roster attempt is terminal and no consumption remains pending, the authority
appends and anchors `ExperimentFrozen` with an immutable ledger cutoff. Admission and execution then
reject. `build_report` rejects before freeze, emits exactly one canonical final report, and persists an authority-signed
`StatisticalReportReceipt` binding its digest, registration, authority, exact ledger position, previous
digest, and resulting ledger head. `verify_report` resolves the registration, every execution receipt,
the underlying harness/event evidence, freeze cutoff, recomputed report bytes, ledger chain, and
external anchor; caller rehashing is not proof.

All plan, registration, and report schemas reference the shared
`eval/preregistration/schema/v1/components.schema.json` definitions.

```sh
check-jsonschema --check-metaschema eval/preregistration/schema/v1/components.schema.json
cargo test --locked --test conformance eval_stats_report
```
