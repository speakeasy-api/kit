# Unresolved Compatibility Pins

## BLK-14: Code Intelligence Pins

- Status: open
- Owner: repository intelligence owner
- Evidence: the Rust-only Tree-sitter cell is closed by exact runtime, grammar
  artifact, ABI and query pins in `build-manifest.yaml`. RFC section 32 also
  requires LSP position encoding and server versions and a SCIP schema/index
  version; those adapters remain unselected or unresolved.
- Action: record LSP server artifacts and position encoding when that adapter is
  selected, and select or explicitly disposition the SCIP index producer. Add
  another grammar cell only when its language becomes supported. Preserve the
  optional SCIP disposition rule from `IMPLEMENTATION_PLAN.md:487-497`.
- Verification: the pinned Rust grammar ABI/artifact/query invalidation
  conformance remains green; unresolved `lsp.*` and `scip.index` values are
  closed or explicitly dispositioned before gate `G05`.
- Blocks: complete code-intelligence pin closure at `G05`. Unit `1.09` remains
  honest and verifiable because each remaining absent value links here; this
  record does not claim LSP, SCIP, other languages or M005 completion.

## EXT-21: Immutable CI Build Image

- Status: open
- Owner: build infrastructure owner
- Evidence: GitHub's `ubuntu-24.04` hosted-runner label is versioned but mutable;
  it is not an OCI image digest or an immutable virtual-machine image identity.
- Action: provision a runner from an immutable image digest or image ID and
  record that identity in `build.runner_image`.
- Verification: two fresh runners created from the recorded identity report the
  same environment digest and pass `ci/lanes/reproducible-build.yaml`.
- Blocks: release-mode build provenance. Normal CI remains honest by retaining
  this explicit external blocker and never representing `ubuntu-latest` or a
  mutable hosted-runner label as a reproducible pin.

## EXT-23: ChatGPT Responses Backend

- Status: open
- Owner: compatibility owner
- Evidence: `openai-subscription` calls the exact internal endpoint
  `https://chatgpt.com/backend-api/codex/responses`. The service publishes no immutable protocol
  revision or release-stability contract, and accepted fields and behavior can change independently
  of Kit.
- Action: keep the adapter explicitly unstable, fail closed on unknown protocol transitions, and
  update fixtures whenever observed protocol behavior changes. Do not claim release stability
  unless OpenAI publishes an immutable supported contract for this endpoint.
- Verification: `scripts/verify_pins.sh` reports one unpinned mutable protocol while this blocker is
  open, and release pin verification remains blocked.
- Blocks: protocol pin closure and any claim that the ChatGPT subscription backend is release-stable.

## EXT-24: OpenAI Subscription Model Aliases

- Status: open
- Owner: compatibility owner
- Evidence: the `openai-subscription` allowlist contains provider-controlled aliases whose resolved
  model revisions and account entitlements can change without a Kit release.
- Action: retain the explicit allowlist, report the provider-observed model identity, and update the
  compatibility evidence whenever aliases or entitlements change.
- Verification: `scripts/verify_pins.sh` reports one unpinned mutable model while this blocker is
  open; local G00 pin evidence remains blocked rather than claiming zero mutable models.
- Blocks: immutable model pin closure and G00 evidence regeneration.
