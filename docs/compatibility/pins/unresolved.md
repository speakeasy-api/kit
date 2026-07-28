# Unresolved Compatibility Pins

## BLK-14: Code Intelligence Pins

- Status: open
- Owner: repository intelligence owner
- Evidence: RFC section 32 requires Tree-sitter grammar/query versions, LSP
  position encoding and server versions, and a SCIP schema/index version. The
  RFC and preflight records select no supported language set, grammar runtime,
  grammar/query revisions, LSP servers or position encoding, or generated SCIP
  index.
- Action: select the supported language cells, then record exact grammar and
  query artifact digests, LSP server artifacts and position encoding, and the
  SCIP index producer/schema identity. Preserve the optional SCIP disposition
  rule from `IMPLEMENTATION_PLAN.md:487-497`.
- Verification: every corresponding empty `grammar.*`, `lsp.*`, and
  `scip.index` value in `build-manifest.yaml` is replaced by an immutable value;
  grammar ABI/query hash invalidation, LSP version/position fencing, and SCIP
  schema/index freshness conformance pass at gate `G05`.
- Blocks: complete code-intelligence pin closure at `G05`. Unit `1.09` remains
  honest and verifiable because each absent value links here; this record does
  not claim the code-intelligence adapters are implemented or selected.

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
