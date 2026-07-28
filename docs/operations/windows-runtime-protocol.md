# Windows runtime protocol

`kit-windows-runtime-v1` is the trust boundary for restricted and hostile Windows execution. The installed helper must pass Authenticode verification and is addressed by its SHA-256 identity.

## Spawn

The daemon sends one JSON request on the helper's owned stdin, bounded to 1 MiB. The request contains:

- the complete canonical `ExecutorProfile`, including tier, normalized mount targets/access, source-write mode, egress, credential handles and injection modes, repository policy, requirements, platform/architecture, and all eight resource limits;
- explicit mount source/target/access descriptors, pinned image and trial storage identities when applicable;
- program, argv, clear-and-set environment, current directory, and environment scrub policy;
- daemon ownership, attempt/fence identity, and a fresh nonce;
- `pipes` by default, or the caller PID and exact ConPTY attribute-handle list. The helper may duplicate only those listed handles.
- for a credential profile, a secret-channel descriptor bound to the profile digest, ownership/fence, nonce, destinations, and final plan digest. Authorization and broker resolution complete before the helper is spawned. Secret bytes travel only over a dedicated owned pipe handle passed to the helper; they are never present in the JSON/stdin request, argv, environment, output, logs, or durable boundary record. A missing broker or dedicated-channel capability is `CredentialBrokerUnavailable` before boundary creation. Profiles without credentials use the original stdin-only request path.

The helper atomically creates the Windows container or Hyper-V boundary, creates and limits the Job, creates the root suspended inside the runtime boundary with the Job assigned at creation, and returns handles duplicated into the daemon. It must attest the request digest, nonce, helper/runtime/isolation identities, root PID and creation time, Job name/token/root-and-limit identity, creation/assignment/suspension state, terminal binding, and every returned handle. Any missing, extra, empty, duplicate, or mismatched field fails closed. The daemon persists and registers the complete composite before an authenticated `resume` operation.

Immediately after a `boundary_created=true` attestation is authenticated, the daemon arms an abort guard. Every later validation, handle conversion, Job/root check, composite build, persistence, cancellation registration, registry preparation, or resume error sends authenticated `abort` and accepts cleanup only when the helper re-attests the same identity, an absent boundary, a reaped root, and zero survivors. The guard is disarmed only after durable registration, successful resume, and ownership transfer. Unproved cleanup is `outcome_unknown`; the complete recovery identity remains available for quarantine/reconciliation.

## Recovery and control

The durable composite stores both complete nested identities. Windows runtime identity `v4` and composite identity `v2` use canonical byte-length-prefixed fields with lowercase BLAKE3 binding. Components are capped at 4 KiB, aggregate identities at 8 KiB, and persisted records at 32 KiB; oversized, truncated, modified, or noncanonical records are rejected before recovery allocation. Every recover, inspect, kill, reap, and resume request sends the plan/helper/runtime/isolation/generation identity plus root PID, root creation time, Job locator, Job token, and Job root/limit identity. Every response must re-attest the same root-to-Job-to-runtime binding (`root_bound=true`, `job_bound=true`) before control is accepted.

## Probe

Capability names are not availability evidence. Probe succeeds only when the trusted helper reports successful Job operations and atomic suspended spawn; ConPTY must also be exercised when requested. The daemon additionally validates duplicated Job limits, root creation time, and Job membership before resume.

The Windows runtime and conformance lane remain tracked by `EXT-19`; host-only protocol tests do not replace Windows runtime evidence.
