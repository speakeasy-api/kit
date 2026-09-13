# Runtime side-channel authority

Runtime events are private, best-effort stderr observations, not a snapshot,
replay log, or proof of current remote state. This protocol does not change ACP.

## Wire contract

New publishers use two marked JSON event shapes:

- `runtime_boundary`: `source` (a random, worker-lifetime identity), `epoch`
  (a monotonically increasing unsigned integer), and `state`: `open`,
  `heartbeat`, or `lost`.
- `runtime_scoped`: `source`, `epoch`, and `payload` (another runtime event).

A worker initially writes `open` at epoch 0. All its authoritative payloads,
including session attachment markers and forwarded events, name its epoch.
Forwarding wraps rather than replaces the original event: the complete chain of
containing publishers remains a prerequisite for descendant authority. Source
identities are not authentication credentials; stderr is owned by the child.

An accepted `heartbeat` or scoped payload renews an already-live epoch's
five-second receive lease. It never opens an epoch. After explicit loss or local
expiry, same-epoch heartbeats, repeated opens, and payloads are rejected. Recovery
requires an explicit strictly newer `open`, or a newly owned replacement stream.
An ancestor heartbeat cannot restore a descendant. Ancestor loss also
invalidates its dependent descendants; each needs its own newer open.

The writer attempts a healthy epoch rotation every two seconds, even without
queue overflow. Healthy continuous rotation preserves observations. It does not
clear or reconstruct state. This gives receivers which expired locally a way to
recover without a reverse channel when IO and publication claims make progress.
A different source ID is not, by itself, an exclusive replacement operation:
actual stream replacement must replace the receiver owner and its tracker.

## Publication ordering and failure

The queue remains **1024 frames**. Serialization writes into a bounded 16 KiB
buffer; provenance is limited to 16 wrappers. Authoritative publication does not
wait for IO, queue capacity, or a reset acknowledgement. Diagnostics are
best-effort and do not renew or repair authoritative state.

Producer tokens are captured before encoding. Queue overflow or invalid encoding
closes exactly that token's admission generation using compare-and-exchange.
A delayed failure cannot close a newer generation. Queued frames carrying an
invalidated token are discarded, including a retained token enqueued after
reopening. A frame already selected by the single writer may finish, but its
write is strictly before `lost` and the replacement `open`. New-generation
admission opens only after the complete replacement boundary write succeeds.

For healthy periodic rotation, producers hold a nonblocking shared publication
claim. The writer tries an exclusive claim, drains previously admitted frames
before the new open, and then advances the epoch. The drain is bounded by queue
capacity. Publication attempted while that claim is held records loss
separately, so a dropped observation cannot disappear in the epoch commit.
That loss is reported by the writer (possibly after the just-written open if it
raced the boundary). Progress requires eventual acquisition of the exclusive
claim; no scheduling fairness bound is claimed.

Only the writer performs IO. IO error, partial-write error, unwind, epoch
exhaustion, and sender disconnect retire that worker; they cannot restart it
under a reused identity. The worker's terminal guard runs on all exit paths.
The publication-claim lock contains no mutable snapshot. Producers never block
on it, and poison is not recovered. No execution future joins the writer;
cancellation and dropping publishers cannot wait for a blocked stderr sink.

## Recovery and compatibility limits

Loss clears retained observations conservatively. Subsequent accepted events
can describe future activity, but an open does not reconstruct lost agent,
child, compaction, storage, or progress snapshots. Existing progress tombstones
and session identity filtering still apply. Loss of one authority may clear
more UI state than necessary; it does not authorize another authority's backlog.
A persistent incomplete-status warning distinguishes accepted future observations
from complete status: another publisher's healthy open cannot hide a descendant's
ongoing loss. A fresh visible session clears this historical warning, not the
owned stream's epoch tombstones. If an attachment marker was itself lost, the
receiver requires a fresh accepted marker; it does not guess session ownership.

Legacy boolean status lacks provenance and epochs. Its loss/expiry is latched
for its owned stream: an unscoped `true` is not a recovery certificate. Mixed
legacy/scoped input is treated conservatively, not promoted to scoped authority.
Old readers do not recognize new wrappers and therefore cannot render their
payloads; the protocol intentionally fails closed rather than dual-emitting
unscoped copies. Existing legacy event shapes remain parseable. The side channel
is ephemeral, so no persistent artifact rewrite or migration is performed.

**Freshness is an ordering contract, not an absolute wall-clock guarantee.** A
receiver rejects backlog from epochs it has invalidated. A newer explicit open
may itself have spent time in an OS pipe, forwarding queue, or receiver mailbox.
One-way buffered data cannot prove that this boundary was generated after the
receiver's local expiry, nor that the remote process is still alive at display
time. Buffered newer boundaries and their observations can consequently be
accepted until the next loss/lease expiry. No timestamp or heartbeat can remove
that physical-buffer limitation without an acknowledged challenge or an owned
stream replacement. Eventual recovery assumes IO drains, the runtime continues
executing, and fresh boundaries and observations can traverse every dependency.

Receiver authority registries retain at most 4096 source identities per owned
stream. They never evict epoch tombstones to admit an unseen identity: at the
limit, new identities fail closed until the stream is replaced. This bounds
registry memory/work but is a deliberate long-lived-stream liveness limit.
The existing stderr line readers still assemble a full line before the parser's
64 KiB limit; arbitrary child output without a newline is not bounded by this
publisher protocol. This change does not claim a bounded physical receive path.
