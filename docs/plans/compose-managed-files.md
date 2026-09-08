# Compose-managed files and multimodal tools

## Goal and non-negotiable constraints

Keep **compose as the only model-exposed tool**. Build composable file references for image reads, image transformations, multimodal subagents, and consistent TUI rendering. Do not add a directly exposed read-image tool.

Implement as four stacked PRs, each owned by a distinct subagent in a distinct worktree created using `wt`. Phase 1 carries this plan. Start each later phase only after its predecessor passes the review gate. Never merge, enable auto-merge, enqueue, deploy, or change release versions.

Base: current main (`origin/main` at setup: `0a9291d7d3ccdd6837903d0521ab2b040ab54044`). Phase-1 worktree was created using `wt switch --create feat/compose-files-phase-1 --base main --no-hooks --format json`, then fast-forwarded to fetched origin/main. The user's original dirty worktree must remain untouched.

## Architecture

### Managed File value

Tools exchange bounded ordinary JSON descriptors, not base64 strings or raw filesystem paths. Illustrative descriptor (final schema requires design review):

```json
{
  "$kit": "file",
  "version": 1,
  "id": "file_opaque_id",
  "name": "my_image.png",
  "mime_type": "image/png",
  "size_bytes": 184230,
  "image": { "width": 1024, "height": 768 }
}
```

Bytes are immutable snapshots in Kit-managed storage. Descriptors survive session resume independently of the original path. Resolution requires session access; metadata is validated against the stored object. A marker is not authorization. Forged IDs, cross-session references, and traversal must fail safely. Future fork/subagent grants must be explicit. Use the existing artifact/resilient filesystem infrastructure where appropriate, but do not stretch the UTF-8 artifact reader into a binary contract. Define version handling, retention, cleanup, restart behavior, and storage-failure semantics.

### Compose delivery boundary

```text
read_file / image transform / subagent -> managed bytes + JSON File descriptor
  -> Runlet ordinary JSON -> final returned JSON
  -> Kit reference validation and resolution
  -> ToolOutput::Parts(text/structured result + typed image parts)
  -> canonical tool result -> provider adapter -> model
                          -> ACP -> TUI
```

Only references reachable from the final returned value deliver content to the parent model. Intermediate images remain private to the operations consuming them. Resolve nested references with bounded depth/count, deterministic order, identity deduplication, and labels identifying return-value positions. Invalid/inaccessible/unsupported selected references must never appear as successful text-only image delivery.

Integrate resolution with compose result finalization in `BackgroundableCompose`, sharing behavior across invoke, invoke_outcome, foreground, and background completion. JSON markers should permit the first implementation without changing Runlet or agentkit-tool-compose. Re-check current main before relying on earlier traced line numbers.

Extract references before text spilling. Apply the 8 KiB spill policy only to the text/JSON portion; preserve selected typed images and enforce separate image budgets. Define interruption, retry/replay, cancellation, and ownership lifetime.

### Provider contract

The canonical transcript retains typed tool-result images. **Phase 1 MUST include the non-native fallback**, not defer it to a later phase. Use native multimodal tool output on verified routes; otherwise project text/metadata tool results with a pointer to an immediately following user-role image message through the existing user attachment encoder. Place that message after the full parallel tool-result batch, never between unanswered results. This is provider-request-only: never persist fake user turns, rerun compose, duplicate delivery, or stringify pixels. Preserve original result text, labels, diagnostics, call/result pairing, normalization budgets, background completion, and replay/continuations. Lack of native image tool-output support is not a fatal gate when ordinary image input transport is available; model vision capability remains a provider/model constraint, not an arbitrary Kit allowlist. Verify actual private Responses and Completions/OpenRouter request encoders.

The acceptance criterion is an actual image block in the next outgoing provider request, not a base64 string or marker. Replay must use snapshotted content after the original file changes or disappears.

### Hidden tool suite

Initial read_file imports local content and returns a managed File. Image operations consume references and create new immutable references. Use flat callable names compatible with current Runlet. Export is explicit, not an implicit overwrite. Define format sniffing, bounded read/decode/allocation, cancellation, decoded-pixel/output budgets, animation policy, orientation, and metadata handling. No HTTP/SVG/PDF/OCR support is needed for the initial reader.

Proposed end-state example (new APIs, not existing syntax contracts):

```text
image = read_file({ path: "my_image.png" })
rotated = image_rotate({ image, degrees: 90 })
cropped = image_crop({
  image: rotated,
  aspect_ratio: { width: 1, height: 1 },
  anchor: "center"
})
stickered = subagent({
  model: "<multi-modal input and output model>",
  prompt: "Add a Hello Kitty sticker to the attached image.",
  attachments: [cropped],
  output_schema: {
    type: "object",
    properties: { result: { "$ref": "kit://schemas/file/v1", "x-kit-image-index": 0 } },
    required: ["result"],
    additionalProperties: false
  }
})
return stickered.output.result
```

Explicit attachment arguments, not concatenating a reference into prompt text, determine image input. A model capable of images behind a text-only harness is still unsupported.

### Phase 2 implemented contract

The chosen explicit export name is `export_file({ file, path })`; its path/status receipt has no File marker. Hidden `image_rotate({ image, degrees })`, `image_crop({ image, aspect_ratio, anchor })`, and `image_resize({ image, width, height, fit })` use the unchanged version-1 session-authorized File descriptor and immutable disk publication. Geometry, all nine anchors, contain/cover/stretch rounding, format/orientation/metadata policy, per-stage allocation limits, and disk-only no-clobber export semantics are specified in [the user guide](../user/compose-and-local-tools.md#transform-images-in-one-compose-program).

The pipeline remains one compose invocation with final-return-only delivery, using the existing foreground/background/replay finalizer and native/user-role provider transport. Export uses OS process permissions rather than inventing a filesystem sandbox; its explicit create-new/partial-output cancellation contract does not overwrite or roll back user paths. No dependency or persistent envelope schema change is intended. Phase 3 subagent contracts and phase 4 presentation remain separate milestones.

### Multimodal subagents

Extend the currently text-oriented ACP child prompt/output path to retain typed attachments and assistant media. Import native generated media into managed storage. Bind actual emitted media to a file-aware output contract; models must not invent file IDs. Specify single-image binding and reject ambiguous multiple output. Validate shape AND reference existence/access, with explicit contract failure rather than silent string fallback for the new file-aware contract. Parent outputs must survive child close; grants and promotion must preserve session isolation.

### Phase 3 implemented contract

The subagent tool layer accepts optional typed `attachments` on `subagent`, `prompt`, and `fork`, resolving authority from `ToolRequest.session_id`. Native ACP image inputs and generated outputs use managed storage rather than model-created identities. Attachment grants cross working-directory stores explicitly; generated-image parent publication and final access validation complete before the existing success transition. Errors use the existing create cleanup, continuation retry-handle, and fork cleanup paths without adding shared-state instrumentation.

File-aware `output_schema` uses the locally resolved `kit://schemas/file/v1` reference. The supported binding is exactly one root File or one required fixed nested object-property path. By default, Kit requires exactly one distinct native assistant image; an optional caller-fixed `x-kit-image-index` integer 0–7 beside the exact File `$ref` instead selects a distinct image in first-emission order. Kit rejects attempted model binding, validates surrounding JSON and the completed schema, and only constructs omitted surrounding objects when the complete result is valid. Arrays, unions, conditionals, indirect references and multiple bindings are unsupported. Every native image occurrence is independently validated and charged against occurrence, encoded/decoded byte, and aggregate pixel budgets before deduplication. Only equal MIME types and byte-identical image payloads from the current output collapse; visually identical images with different bytes remain distinct and require an explicit index to select one. Every occurrence, including unselected images, must validate and satisfy budgets before selection. Out-of-range indices fail without fallback, and only the selected image is imported/published. Kit does not strip signed metadata or deduplicate perceptually. No model identity, input image, or previous turn participates in that comparison. Ordinary text-only schema fallback is preserved. Native images without a file-aware schema have the explicit `output: { value, files }` surface, not base64 diagnostic updates. The [user guide](../user/compose-and-local-tools.md#attach-files-to-subagents-and-return-native-images) specifies the contract and final-return-only behavior.

The complete read/rotate/crop → built-in ACP subagent → managed output → child close → `return output.result` pipeline has been verified against the canonical OpenRouter `google/gemini-3-pro-image` route. The caller explicitly selected distinct native output index 0; this does not claim that the backend emits only one image. The selected bytes were visually verified to contain the requested Hello Kitty sticker. Eligibility comes from exact-model and concrete-endpoint capability discovery, not a model allowlist. Other harness/provider routes require their own native-output support. Shared TUI presentation remains Phase 4 work.

### Shared TUI presentation

Generalize user-image rendering into reusable media presentation for user attachments, tool results, assistant-generated media, and Markdown image nodes. Share decode/cache/terminal protocols and budgets, and align live updates with history replay. Keep media out of text-only search/previews/logs.

Markdown image nodes such as `![Edited image](kit-file://file_opaque_id)` resolve through the managed file resolver. Local paths follow filesystem permission policy. Remote images require explicit network policy, asynchronous bounded fetch/decode, redirect/address validation, and no implicit credentials. Do not fetch arbitrary model-supplied URLs without the applicable authorization. Parse real image nodes, not regexes scanning code fences. Retain alt text, source links, placeholders, and terminal fallback. Ordinary links remain links unless explicitly previewed. TUI display never automatically attaches pixels to model context.

### Phase 4 presentation contract

The TUI retains one presentation-only message/image representation for native user, assistant, and tool media. CommonMark image nodes use source-aware layout without splitting Markdown documents. Managed references resolve under current-session authority; external sources are temporary snapshots and are re-resolved after cache reset or replay. Duplicate typed/Markdown viewports require independently authorized, exact resolved content identity within the same message; repeated Markdown nodes remain visible. No display path imports files, grants access, or adds model-context attachments.

Remote loading is default-denied and requires the startup `KIT_TUI_IMAGE_ORIGINS` exact-origin HTTPS policy. Redirects are rejected, every resolved address must be public, and DNS results are pinned while TLS hostname checks remain active. The dedicated client has no ambient proxy or credential configuration. Acquisition and the common decode/protocol renderer have separate bounded worker admission; blocking DNS/file/decode operations retain their permits even after cancellation. The [TUI guide](../user/tui-and-sessions.md#images-in-the-transcript) specifies fallback, cache, replay, and local-file semantics.

## Milestones and PR stack

| Phase | Branch / PR base | Owner | Scope and completion milestone |
| --- | --- | --- | --- |
| 1 | `feat/compose-files-phase-1` -> `main` | Dedicated phase-1 subagent | Versioned managed storage/reference contract; hidden read_file; compose finalizer; media-aware spill; native provider delivery plus mandatory user-image transport fallback; basic tool-result TUI rendering. `return read_file({ path: "screenshot.png" })` delivers real pixels to model and TUI. |
| 2 | `feat/compose-files-phase-2` -> phase-1 branch | New dedicated phase-2 subagent | Rotate/crop/resize/export, immutable references, bounded transforms, docs/tests. A pipeline works in one compose invocation; only returned files reach parent model. |
| 3 | `feat/compose-files-phase-3` -> phase-2 branch | New dedicated phase-3 subagent | Typed subagent attachments/output, file-aware schema/binding, capability checks, parent/child grants and output promotion. Sticker-editing pipeline is supported end to end. |
| 4 | `feat/compose-files-phase-4` -> phase-3 branch | New dedicated phase-4 subagent | Unified user/tool/assistant/Markdown rendering, resolver policy, live/replay consistency, caching and terminal fallback. All image origins render safely. |

Each successor worktree is created with `wt` from its predecessor's CURRENT review-clean tip, invoked from the phase-1 worktree; do not fork all phases independently from main. Verify `git merge-base --is-ancestor <parent> HEAD`, and set PR base to the parent branch. The plan is inherited through the stack. Changes to a published parent require pausing descendants and asking for restack/history-rewrite approval.

## Per-phase owner instructions and review gate

1. Read this plan, current repository instructions, and relevant skills. Reinspect code on current main/parent; the original investigation was on a different dirty worktree.
2. Get independent design consensus before implementation; retain an explicit reviewer session ID in the orchestration ledger, not committed docs. Resolve material disagreements with the same reviewer.
3. Load shared-state skill for lock/shared-state changes, updating-artifact-schema for persistent schema changes, secure-rust-dependency-changes for dependency surface changes, and pr for PR creation. Apply ship-issue/shepherd with stack/no-merge overrides; this is not a Linear task.
4. Implement only your phase, preserving later extensibility. Keep production free of test-only instrumentation. Run smallest useful checks during iteration, then repository-required checks. No release version changes.
5. Self-review for reuse, quality, efficiency; obtain independent current-head review. Open a Conventional Commit PR with correct base, plan link, testing and limitations.
6. KEEP GOING through CI/reviewer findings. Fix valid findings, rerun tests, push follow-up commits, respond/resolve threads, and obtain fresh review evidence for the latest head. Never self-approve or misrepresent absent approval.
7. Stop successfully ONLY when local checks pass, CI passes with no pending/unknown required check, reviewers have approved the current head with no unresolved threads, and GitHub reports conflict-free/mergeable against its current base. A skipped stacked-base workflow is not a pass without repository-approved equivalent current-head evidence.
8. Do not merge, enable auto-merge, enqueue, deploy, or launch a successor yourself. Return the gate evidence to the parent orchestrator. If external approval/auth/infra genuinely blocks progress, report the blocker honestly; parent does not launch next phase.

The parent launches each successor only after verifying the previous gate. Keep an uncommitted ledger outside tracked plan content with phase, worktree, branch/base/tip, agent/reviewer session IDs, PR URL, CI/check evidence, approval SHA, unresolved threads, mergeability, and blockers.

## Acceptance test matrix

- Real provider request contains image input after compose return, natively or in the mandatory request-only user-image fallback after the full tool-result batch. No descriptor/base64 text substituted for pixels.
- Nested returns, multiple references, deduplication, deterministic labels, and intermediate non-delivery.
- Text spill does not hide or corrupt image delivery; bounded traversal and independent byte/pixel budgets.
- Malformed/forged/stale/cross-session references, unsupported versions, missing files, corrupt formats, directories, permissions, oversized/decompression-bomb input.
- Foreground/background completion, interruption/resume, cancellation, replay, storage errors.
- Session resume/fork and child close preserve authorized output independently of original path.
- Transform geometry, format/animation/metadata policies, immutable source, explicit export.
- Subagent capability/contract failures, native output import, ambiguous binding, grant isolation.
- Live and replayed TUI tool/assistant/user images, Markdown policy, async placeholders/fallback, no credentialed arbitrary fetches.

## Initial status

Plan persisted before implementation. Phase 1 is next; phases 2–4 are blocked on predecessor review gates. Operational status and agent IDs live in the uncommitted orchestration ledger.
