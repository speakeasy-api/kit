# DR-0007: In-place upgrade of built-in capability extensions

- Status: proposed
- Problem: any rebuild that changes a built-in extension's schema or
  implementation digest makes every existing state root refuse to boot
  (`extension contract conflicts with kit.native-provider@1.0.0`, exit 70).
  The only recovery is wiping the state root. That is acceptable for no one:
  developers hit it on every change to a pinned source file, and users would
  hit it on every kit upgrade. This makes kit unshippable as a product.

## Decision

Registration of a **built-in, daemon-bootstrap-trusted** extension whose
`(scope, reference)` already exists with a different contract **supersedes**
the stored entry instead of conflicting. Everything else keeps today's
refusal: external/MCP extensions, untrusted registrations, entries whose
`kind` or `trust` classification changed, and version downgrades of the same
reference.

## Mechanism

1. `CapabilityExtensionRegistry::insert` (src/capabilities/extensions/mod.rs)
   gains a supersede arm: when `existing != entry`, the incoming registration
   carries the daemon-bootstrap trusted token, and kind/trust/reference are
   unchanged, build a candidate with the entry replaced and persist it as a
   normal snapshot revision (the existing optimistic-CAS
   `persist_extension_registry_snapshot` path — concurrency already handled).
2. `commit_candidate` already revokes lifecycles for entries that stop being
   active; a superseded entry revokes the old lifecycle the same way, so any
   in-flight guard on the old contract is cancelled, never mixed.
3. Emit one audit event per supersede: `capability.extension.upgraded`
   `{reference, old_schema_digest, new_schema_digest, old_implementation_digest,
   new_implementation_digest}`. Historical evidence stays valid because events
   already embed the digests they were minted under; nothing rewrites history.
4. Runs parked on a durable wait whose tool bindings were minted from the
   superseded contract fail that invocation at resume with a typed
   `descriptor_superseded` error (same shape as `snapshot_unavailable`): the
   model re-plans against the refreshed catalog. No binding migration in v1.
5. No store schema change: supersede is an ordinary snapshot revision bump;
   prior revisions remain in the snapshot history for audit.

## Not in scope

- Upgrading external/MCP extension contracts (still a hard conflict).
- Cross-version migration of persisted capability state.
- Downgrade protection beyond refusing a lower `version` for the same
  reference.

## Blast radius

- Code: extensions/mod.rs insert/commit path, one new event type, resume-time
  binding validation in the executor.
- RFC: the normative "contract conflict" text must gain the supersede clause →
  registry re-anchoring migration (known two-commit recipe).
- Tests: conformance ext-m00x suites gain upgrade/supersede cases; the
  dev-workflow "wipe the state root" note dies.
