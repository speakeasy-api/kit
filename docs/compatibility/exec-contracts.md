# Executor Contracts

`M003-W09` defines serialization and lifecycle boundaries for later mutation-overlay (`M004`)
and isolated-VM (`M012`) implementations. These interfaces and their conformance fakes are not
production isolation mechanisms.

## Compatibility

Both contracts use canonical, compact JSON and bind a BLAKE3 digest to the complete canonical
document. Object fields are emitted in schema declaration order and sets in lexical order.
Decoders reject non-canonical bytes, unknown fields, unsupported `schema_version`, and unsupported
`contract_version`; consumers must not guess compatibility. Version 1 is the only accepted schema
and contract version. A semantic or encoding change requires an explicit version change.

## Local execution profiles

Executor profile schema v1 derives requirements by execution label instead of applying full
container resource guarantees to every profile. The schema is unshipped; these corrections do not
inflate its version. `trusted_local` requires a native OS sandbox and reconstructible whole-process
tree control. macOS Seatbelt plus a process group does not provide that control because a descendant
can call `setsid`, so the local backend fails closed for trusted-local/reassignable execution on
macOS. Linux trusted-local execution also fails closed: its mount inputs are not descriptor or
lease pinned through bubblewrap setup, so path revalidation cannot exclude replacement races or
prove that every mounted tree remains free of sockets and special files. The container backend is
the production isolation path.

Linux path checks revalidate source/build/temp device and inode identities, and source traversal
rejects escaping links, non-regular special files, Unix sockets, hard-linked regular files,
cross-device directories, and nested mounts. These checks are defense in depth and do not authorize
trusted-local execution because they do not pin mount inputs against races. Local profiles accept
only the exact `/`, `/workspace`, `/build`, and `/tmp` targets represented by the backend. The
trusted-local profile still requires read-only source and system mounts, dedicated writable build
and temporary paths, denied network access, a scrubbed environment, repository code containment,
bounded captured output, and wall time; the local backend does not claim to satisfy that profile.
It also does not claim aggregate CPU, memory, PID, file, disk, or I/O limits. Adding any such
primitive makes the local backend return typed `NotAvailable`.
Restricted and hostile profiles retain those full requirements and never select the local backend
when they are absent.

Restricted Linux mutation-overlay execution extends the same M003 container/helper path rather
than adding a formatter-specific process runner. The helper receives a lease-pinned read-only base
source and a different writable overlay identity; runtime argv mounts the base only at
`/kit-stage-source` and the writable layer at `/workspace`. Root and base source are never writable.
The plan still persists its complete boundary before release, uses the process registry, enforces
finite CPU/memory/PID/file/disk/I/O/output/wall bounds, and accepts completion only with a matching
helper/runtime invocation record proving the boundary absent with zero survivors.

The pre-release `kit-container-v1` formatter request adds the formatter program and repeated
arguments plus requested executable and effective-config digests before canonical plan hashing.
The monitor record may omit formatter fields only for non-formatter runs. Formatter runs require
the complete `formatter_binary_digest`, `formatter_config_digest`, and
`formatter_artifact_digest` set. These helper-measured fields share the record's nonce,
`plan_digest`, and `invocation_digest`; partial or malformed sets are protocol errors. The
formatter executor separately requires the resolved image and artifact digest to equal the pinned
image constraint and the binary/config digests to equal their requested constraints. Missing or
mismatched helper measurements are rejected; failure to provision or start the external helper is
typed unavailable. This correction remains v1 because the contract has not shipped.

On Windows, `windows_job` is the native process-tree layer. It assigns each suspended child at
creation with `PROC_THREAD_ATTRIBUTE_JOB_LIST`, persists the owner/fence, root PID, root process
creation time, unguessable object identity, and exact Job limits, and only then resumes the primary
thread. The Job sets `KILL_ON_JOB_CLOSE` plus aggregate user-time, Job memory, and active-process
limits. This backend has no completion-port evidence that authoritatively identifies a terminating
CPU or memory limit. The synchronous path therefore reports the process exit generically and does
not infer a limit notification from `JobObjectLimitViolationInformation2`.
An active-process limit denies the extra process creation; it does not imply that the root was
terminated. Completion and cancellation query Job accounting and require `ActiveProcesses == 0`;
an unknown count is not quiescence.

Recovery first authenticates the persisted root PID by process creation time, then requires that
exact process to belong to the named Job and requires the configured kernel limits to match. If the
Job has either breakaway flag set, recovery rejects it even when every numeric limit still matches. If the
root is gone while descendants might remain, or any identity check fails, recovery records
`outcome_unknown` and never opens by name alone, terminates the candidate object, or reassigns the
workspace. A replacement same-name Job is not accepted.

Job Objects are not sandboxes. The backend advertises no filesystem, network, privilege,
container, or VM primitive. Windows `restricted` therefore still requires a separately probed
Windows container or Hyper-V boundary, and `hostile` still requires a VM tenant boundary. Selection
fails closed when that provider is absent; a Job-only probe is never labelled hostile isolation.
File-size, disk, and I/O resource limits are likewise not claimed by this layer.

Windows terminals use dynamically detected ConPTY APIs so Windows versions without ConPTY return
typed `platform_unavailable` instead of loading a pipe substitute. All console-side and host-side
pipe handles are non-inheritable; the child receives the `HPCON` attribute and no ambient handles.
Non-PTY launches use an explicit handle list containing only their stdin, stdout, and stderr pipe
ends; unrelated inheritable daemon handles are excluded. The driver supports binary writes and
resize and supplies its binding to the same creation-time Job launcher. The ConPTY ownership lock is
held through resize and process-creation FFI, so interruption cannot close a leased `HPCON`. Root exit closes input and
starts pseudoconsole close while the output capture reader drains, so interruption and Drop do not
wait on a full ConPTY output pipe. A persistent close reaper owns every asynchronously closed handle;
if that worker cannot be created, close falls back synchronously rather than losing the handle.

Windows owned launches start bounded stdout and stderr (or PTY) drains before waiting. Wall-time
expiry terminates the complete Job, proves zero active processes, and terminalizes process and
cancellation registries before returning. Captured PTY chunks use the same process-bound redaction
and persistence policy as other owned terminals, and retained output never exceeds `output_bytes`.

`host_compatibility` is a separate, explicit opt-in profile. It is weaker than trusted-local,
labelled `not isolation`, and cannot satisfy a request carrying an isolation label. Its canonical
effective policy records host read/write filesystem and source access, host network access,
unrestricted repository code, and only bounded captured output and wall time. It runs with an empty
ambient environment plus a fixed minimal environment. On Linux, a successfully probed bubblewrap
PID namespace supervises descendants while exposing the host filesystem and network; this remains
compatibility mode, not isolation. On macOS, selection reports this backend unavailable because the
process-group-only runner cannot enter owned execution or publish quiescence suitable for
reassignment; a `setsid` descendant could survive.

Linux explicit compatibility runs synchronously to completion through `OwnedProcess`, persist a
complete boundary before release, and clean the selected process boundary on completion or timeout.
They are not published through the process registry or public API and are therefore not
API-cancellable. Attempt ownership remains rejected because it requires the cancellation
coordinator; attempt-owned execution remains on the container backend. Local credential custody is
unavailable and fails closed.

## PTY availability

Linux and macOS have native PTY primitives. Primitive availability is not production executor availability:
no reachable production attempt-owned profile currently binds those primitives to an owned process
before spawn. Linux helper integration remains blocked by
[`EXT-22`](../operations/ext-register.md#prerequisites), and native-only tests do not close that
external prerequisite. PTY allocation therefore fails closed with a typed `platform_unavailable`
or `profile_unavailable` response when the selected platform or production profile cannot supply
an owned process PTY.

This availability boundary does not discard terminal history. Authorized replacement viewers can
continue reading retained output, resize events, and retention gaps from exited or interrupted
terminals through the API; input remains write-only and unretained.

## Mutation Overlay v1

An overlay contract binds one immutable `base_revision` and `base_digest` to one dedicated
`copy_on_write` writable layer, one mutation lock identity and monotonically fenced lease, and the
complete declared diff. The source view is always `read_only`; direct mutation of the source is not
representable by v1. Declared paths are canonical ASCII relative paths. Absolute paths, drive
prefixes, separators other than `/`, control characters, dot components, Unicode aliases, and
Windows-reserved or trailing-dot/space components are rejected. Only these validated paths enter
the canonical digest. Add/modify/delete entries carry the applicable base and result content
digests.

The lifecycle is `start -> promote|discard -> attest_quiescence`. Promotion and discard are a
single terminal choice and must succeed at most once. Every transition checks the current fence.
No layer may be reassigned before terminal disposition and attested quiescence, and a writable layer
identity must never be reused. Quiescence, not possession of a lease token, proves that escaped
processes cannot mutate a reassigned workspace.

## Isolated VM v1

A VM run contract binds a pinned `sha256` image to a unique run, instance, and rootfs writable-layer
identity. Rootfs storage is `copy_on_write`, network policy is `deny`, ambient/default grants must be
empty, and finite CPU, memory, disk, PID, and wall-time bounds are mandatory. Secrets can only be
named by opaque handles; secret material has no field in the schema.

The lifecycle is `start -> complete|kill -> attest_quiescence -> attest_outcome`. `complete` records
an exit code or signal; `kill` separately records a killed outcome. Every transition checks the
current fence. Instance and rootfs identities are single-use, including after successful teardown.
An outcome is accepted only after quiescence and only when attestation binds the run, instance,
contract digest, evidence digest, and the exact recorded completion or kill outcome. A production
VM backend must supply the isolation, termination, quiescence inspection, and attestation
mechanisms; the conformance fake only checks protocol state.
