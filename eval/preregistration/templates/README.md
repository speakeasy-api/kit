# Preregistration Template

The template is a complete three-pair conformance example. A production plan must replace its IDs,
non-zero content pins, exact roster and dataset membership, seeds, arm order, estimands, prospective
power rationale, scientific margin ceiling, and policies before registration. Tooling must derive and
then verify the task-set, dataset-roster, and complete experiment-design commitments rather than accept
independent caller pins. The task-set and experiment digests commit every roster entry's task ID and
task-manifest digest. Model, model-settings, config, and provider-capability pins are selected from the
exact roster entry; harness, grader, helper, runtime, and image pins remain global commitments.

Plans never contain registration or measurement timestamps. `RegistrationAuthority::register`,
`admit_next`, and `record_trial` assign authoritative SQLite times and authenticate each append. The
caller cannot supply a timestamp, sequence, ledger position, genesis, receipt key, measured field, or
report trial slice. Admissions are signed and single-use; each fixed roster entry has one measured
attempt and incomplete pairs make the analysis incomplete.
`policies.error_imputation` must be replaced with study-specific conservative maxima; terminal errors
authenticate elapsed and durable usage evidence and use these values only when the corresponding
continuous evidence is unknown.
