# External Prerequisite Register - Unit 0.07

- Source: `.superworkflow/kit-rfc-complete/plan.md` section 10, rows `EXT-01` through `EXT-20`, plus contract-close blocker `EXT-22`
- Register opened: `2026-07-21`
- Resolves fact collection for `BLK-12` and kickoff registration for `BLK-13`

Role owners below are the owners assigned by the source plan. No human assignee
is identified in the repository, so every blocked row remains unprocured and
gate-blocking. "Registered" means the action, evidence, and gate were entered
on the date shown in that row; it does not mean infrastructure was obtained or
its verification passed.

## Prerequisites

| ID | Status / kickoff | Owner | Obtain action | Required evidence | Gate |
| --- | --- | --- | --- | --- | --- |
| `EXT-01` | Blocked pending an actual trusted run; Linux x86_64 CI cell and exact M004 production-core evidence command wired 2026-07-26 | Infra owner | Provision Linux x86_64 with cgroup v2 and Landlock | Record the cgroup2 mount and unified process cgroup; require non-empty controllers, writable `cgroup.procs`, creation/removal of a delegated child, and `cgroup.kill` in that child. The trusted cell must run the exact ignored production-core calibration test with `KIT_CORE_EVIDENCE_ROOT` and upload its artifacts and attestation. Any missing or mismatched datum fails preflight; source or local fixture results do not close this row. | `G03`, `G12` |
| `EXT-02` | Blocked; registered 2026-07-21, human unassigned | Infra owner | Install `runsc` on `EXT-01` | `runsc --version` returns a version; `docker run --runtime=runsc alpine true` exits 0 | `G12` (`14.01`) |
| `EXT-03` | Blocked; registered 2026-07-21, human unassigned | Infra owner | Provision bare metal or nested virtualization with Firecracker/KVM | `test -w /dev/kvm` exits 0; `firecracker --version` returns a version | `G12` (`14.02`) |
| `EXT-04` | Blocked pending an actual trusted run; Linux aarch64 CI cell and exact M004 production-core evidence command wired 2026-07-26 | Infra owner | Provision Linux aarch64 and repeat `EXT-01` checks | `uname -m` = `aarch64`; cgroup v2/Landlock checks and the exact ignored production-core calibration test pass with retained artifacts and attestation, or explicit `not_available` is recorded without passing the gate | `G03`, `G12` |
| `EXT-05` | Blocked; registered 2026-07-21, human unassigned | Infra owner | Provision Windows CI for CLI/API client surfaces | `cargo test --target x86_64-pc-windows-msvc --test conformance cli_parity` exits 0 with 0 failures | `G01` |
| `EXT-06` | Blocked; registered 2026-07-21, human unassigned | Infra owner | Provision PostgreSQL at the build-manifest major | `psql -c 'select version()'` matches the pin; `cargo test --test conformance store_pg_append` exits 0 with 0 failures | `G12` (`12.01`) |
| `EXT-07` | Blocked; registered 2026-07-21, human unassigned | Infra owner | Provision S3-compatible storage with KMS and per-tenant keys | `cargo test --test fault objectstore_crash` exits 0 with 0 failures; cross-tenant fetch successes = 0 | `G12` (`12.02`) |
| `EXT-08` | Blocked; registered 2026-07-21, human unassigned | Security owner | Provision an OIDC IdP and CA for mTLS issuance/revocation | `auth_remote` produces 7/7 denials; `remote_ingress` proves revocation within the declared bound | `G01` remote half, `G12` |
| `EXT-09` | Blocked; registered 2026-07-21, human unassigned | Infra owner | Provision at least three nodes plus partition/node-loss/clock-skew controls | `cargo test --test adversarial jepsen`: at least 100 randomized runs, 0 lost and 0 duplicated semantic effects | `G12` (`14.06`, `14.12`) |
| `EXT-10` | Blocked; registered 2026-07-21, human unassigned | Release owner | Provision a container registry and signing-key custody | Published-artifact signature verifies; every published image has an SBOM | `G12` (`14.10`) |
| `EXT-11` | Blocked; registered 2026-07-21, human unassigned | Infra owner | Provision x86_64 evaluation storage at or above the completed image inventory | Available bytes meet the final measured requirement; `M011-W02` completes with 0 `ENOSPC` | `G11` (`13.02`) |
| `EXT-12` | Blocked; register opened 2026-07-21, recruitment clock not started because human owner is unassigned | Program owner | Recruit canary users and name an external acceptance owner | Required delayed-outcome count is reached; promotion refuses a smaller count | `G11`, `G12` (`15.03`) |
| `EXT-13` | Blocked; registered 2026-07-21, human unassigned | Program owner | Source, license, and access-gate a private recent-task corpus | Unauthorized fetches are denied; corpus manifest digest is recorded per trial | `G11` (`13.04`) |
| `EXT-14` | Optional and unverified; registered 2026-07-21, human unassigned | Program owner | Provision GPU capacity only if the applicability policy selects self-hosted inference | Selected: accepted evaluation report; not selected: evidenced `not_selected` policy row | None when not selected |
| `EXT-15` | Direct blocker remains; source receipt/accounting closure completed 2026-07-27, credentials and spend approval are still absent, human unassigned | Program owner | Obtain provider API keys and approve a spend ceiling | Measured-default experiments use pinned provider model snapshots and receipts minted by `SqliteTrialUsageReceiptStore` from actual ordered provider/model/tool events plus scheduler reconciliation; deterministic/fake-provider data validates the protocol but is rejected as substitute measured evidence | `G05`, `G07`, `G08`, `G09`, `G11` |
| `EXT-16` | Blocked; registered 2026-07-21, human unassigned | Eval owner | Pin, license-check, and obtain all five named public corpora and harnesses | Build manifest contains five corpus digests and no missing set; harness self-check is 5/5 | `G11` (`13.04`) |
| `EXT-17` | Blocked; registered 2026-07-21, human unassigned | Protocol owner | Identify at least two independent ACP, A2A, and MCP implementations where available | `11.11` interop exits 0 with 0 failures per implementation; absence is `not_available`, not pass | `G10` |
| `EXT-18` | Blocked; register opened 2026-07-21, observation clock not started because no population/owner is assigned | Program owner | Schedule the non-compressible review/rework/rollback/defect window | Delayed human and defect outcomes exist for every policy requiring them | `G11`, `T5` |
| `EXT-19` | Blocked; atomic helper protocol source implementation added 2026-07-25, Windows runtime evidence, MSVC SDK, and human owner unavailable | Infra owner | Provision Windows with ConPTY, Job Objects, and an Authenticode-trusted `C:\Program Files\Kit\kit-windows-runtime.exe` implementing [`kit-windows-runtime-v1`](windows-runtime-protocol.md) over a Windows-container or Hyper-V boundary | On Windows: `cargo test --test conformance windows_job_limits` and `cargo test --test adversarial windows_job` exit 0; the helper atomically creates the runtime boundary and suspended root with creation-time Job assignment, returns duplicated handles, and attests the complete applied plan before registration and authenticated resume; ConPTY allocation, explicit handle-list isolation, binary input, resize, nonblocking teardown/reaper failure, duplicate allocation, and handle-count checks pass; durable cancellation retains the authenticated Job + runtime identities, root PID creation time, helper/runtime/isolation/plan/ownership/fence and registration precedes resume; restart recovery re-attests and controls both layers and proves zero survivors in each; CPU user-time and memory bounds enforce and return only generic non-success exit evidence without completion-port attribution, while the active-process limit denies the extra process; recovery rejects root PID reuse, same-name substitution, helper/runtime substitution, partial composites, and breakaway-flag mutation; breakaway successes and post-cancel/restart survivors are 0; restricted/hostile selection without the trusted helper and container/Hyper-V provider is typed unavailable. The 2026-07-25 Darwin source checks stop in native dependencies (`aws-lc-sys` and related native crates) before Kit compilation because the Windows MSVC SDK headers are absent (`stdlib.h`, `windows.h`, `VCINSTALLDIR=None`). Cross-compilation alone is not runtime evidence, so this row remains blocked until the Windows SDK/runtime lane passes. | `G03` (`4.08`, `4.14`), `G12` |
| `EXT-20` | Blocked; registered 2026-07-21, human unassigned | Infra owner | Provision macOS VM-per-run hostile-work capacity | `microvm_escape` blocks 100% of corpus and leaves 0 survivors; unavailable enforcement fails closed | `G03`, `G12` |
| `EXT-22` | Blocked pending actual trusted x86_64 and aarch64 evidence; exact production-core helper test wired 2026-07-26, human unassigned | Infra owner | Provision and integrate the trusted Linux `kit-container-v1` helper for production attempt-owned PTY launch, formatter execution, and production-core grading | Before helper release, the attempt cancellation claim is durable and the process boundary reports complete containment; with a child live, daemon `SIGKILL` and restart leave 0 child or descendant survivors in 100/100 runs. The production-core test must retain authenticated artifact channels, usage-receipt echo, and per-architecture attestation. Formatter requests bind command, config constraints, nonce, plan, and invocation; absent or mismatched measurements never produce production success. | `G03` (`4.07`, `4.11`, `4.12`) |

Row count: `21/21`. `EXT-22` verification is wired in both trusted Linux CI cells but has not
produced retained evidence, so it remains blocked. Formatter and source protocol fakes prove parsing and
fail-closed acceptance only; they do not close `EXT-22`. Native PTY primitive tests likewise do
not establish a reachable production attempt-owned PTY profile on Linux or macOS. Blocked
infrastructure remains blocked; no availability or gate completion is claimed.

## Disk And Image Inventory

Local remeasurement on 2026-07-21:

```
$ df -h /Users/danielkov/projects/kit
Filesystem      Size    Used   Avail Capacity iused ifree %iused  Mounted on
/dev/disk3s5   460Gi   370Gi    54Gi    88%    5.7M  569M    1%   /System/Volumes/Data
```

The current metadata-only rerun measured these published Docker image bytes:

| Set | Coverage | Measured bytes |
| --- | --- | ---: |
| SWE-bench Verified | 500/500 instances | 1,441,044,414,416 |
| SWE-bench Multilingual | 300/300 instances | 200,448,309,788 |
| SWE-bench Multimodal test | 508/510 instances; two repositories absent | 1,222,960,754,147 |
| **Measured lower bound** | 1308/1310 instances across three sets | **2,864,453,478,351** |

Sources were the Hugging Face Datasets Server row APIs and Docker Hub v2
`swebench` repository/tag `storage_size` or `full_size` fields, queried
2026-07-21. The rerun enumerated all 4501 `swebench` repositories in 46 pages,
matched normalized instance IDs, and used `tags/latest.full_size` for null
repository sizes. Requests were split across advertised rate-limit windows; no
image was downloaded. The two unmatched Multimodal IDs are
`quarto-dev__quarto-cli-6659` and `quarto-dev__quarto-cli-6902`. The measured
lower bound is about 2667.73 GiB (2.605 TiB), already far above the local 54 GiB
available.

The five-set total is not yet measurable and is not estimated:

- SWE-bench-Live currently exposes `test=1000`, `lite=300`, `verified=500`,
  and `full=1888`; no split/snapshot digest is pinned, so the intended image
  set is undefined.
- Authenticated GitHub package enumeration for `harbor-framework` returned
  only `harbor/officeqa-corpus` and `harbor/ubuntu-test`, neither a
  Terminal-Bench 2.1 task image. Repository commit
  `36d417f56c293b8271b306a0e4c566f58e98c153` contains 90 Dockerfiles, so task
  images require pinned builds before their byte total can be measured.

Consequently `BLK-12`, `EXT-11`, and `EXT-16` remain open. The accepted runner
size must use a completed five-set inventory, not this 2.605 TiB lower bound.
