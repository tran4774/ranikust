# Security audits

Each milestone security audit (the vendored `security-audit` skill, see
[`docs/agents/workflow.md`](../../agents/workflow.md)) keeps its deliverables
here, one directory per audit, named after the phases or the scope it covers.
The run itself writes outside the repository (agent prompts, per-agent
results, the coordinator's tools). Once its validators pass, only the
deliverables below move here, on a `docs/` branch. Every file carries a lead
only once it is fixed, rejected or accepted; until then the lead is a draft
security advisory ([Security findings](../../agents/workflow.md#security-findings)).

| File | Contents |
|---|---|
| `REPORT.md` | Scope, execution policy, posture, the prioritised leads, rejected candidates, coverage, maintainer decisions, hardening notes |
| `NEEDS-VALIDATION.md` | Each lead the run left unresolved, with its trace, evidence, blockers and the exact VM (T2) or T2c plan |
| `FINDINGS-DETAIL.md` | Reproduction detail for confirmed medium and higher findings |
| `findings.json` | Every final record (confirmed, needs_validation, rejected), valid against the skill's `report-schema.json` |
| `coverage-ledger.json` | Every coverage unit with its owner, reviewed paths, checks and hardening notes |
| `run-metadata.json` | Source ref, profile, scope, execution policy, run status |
| `architecture.md` | The architecture summary every agent of the run received |

A later run reads the previous `findings.json` and `coverage-ledger.json` as
prior-run input, as the skill describes.

## Audits

| Directory | Source ref | Profile | Result |
|---|---|---|---|
| [`phase-1-2/`](phase-1-2/REPORT.md) | `ed718fb` (end of Phase 2) | deep, whole workspace and `.claude/`, source-only | 0 confirmed, 28 needs_validation, 11 rejected; lead 1 is a possible Full-sandbox escape through inherited descriptors (runc CVE-2024-21626 pattern) |

Leads are unconfirmed until their VM or T2c plan has run. While a lead is
open it is a draft security advisory with an estimated severity, and its
details stay out of this directory
([Security findings](../../agents/workflow.md#security-findings)). The
phase-1-2 audit, public before that rule, stays as it is.

## Status of the phase-1-2 leads after the hardening pass (2026-11-03)

The report above is kept as written at `ed718fb`. This table records what
the hardening pass that followed did with each lead; the commits are on
`main`, each branch reviewed by `host-safety` (and `reviewer` where no
host-safety file changed) before its merge. Leads 1-3 and 28 were
reproduced in the VM on the unfixed code before they were fixed; lead 10
failed a T0 test first; the others are fixed against the traces in
NEEDS-VALIDATION.md.

| Lead | Status | Where |
|---|---|---|
| 1 | Fixed; reproduced first (8/8 cases). Residual `#!/proc/self/exe` with barriers pinned by tests | `fix/rnk-sandbox-run-child-lookups`; [Q-422](../../open-questions.md) |
| 2, 3 | Fixed; reproduced first | `fix/rnk-sandbox-run-child-lookups` |
| 4, 5 | Fixed: ranikust trims its own capabilities on `/`, secrets read once | `fix/direct-mode-authority`, ADR-0078 |
| 6, 11, 12, 13 | Fixed: the build context's owner is a principal of its own | `fix/context-owner-lower-trust`, ADR-0076 (Q-438 answered there) |
| 7, 8 | Fixed: kaniko mode on `/` never uses a persistent store; the operator rule stays load-bearing | `fix/kaniko-slash-no-persistent-store`, ADR-0077; Q-441 |
| 9 | Fixed: journal replay opens only regular files, bounded; one state-directory lock | `fix/state-dir-robustness`, `fix/kaniko-slash-no-persistent-store` |
| 10 | Fixed | `fix/rnk-registry-reference-host`, ADR-0070 |
| 14 | Fixed: a layer's `trusted.*` xattrs are never set; extracted layers moved to `layers/v3` | `fix/rnk-rootfs-extract-hardening`, ADR-0075 |
| 15, 24 | Fixed | `fix/state-dir-robustness` |
| 16, 17, 18, 19 | Fixed: `--limit` bounds | `fix/resource-limits`, ADR-0079; drip-feed Q-448, push side Q-449 |
| 20, 21, 22 | Fixed: memory and CPU linear in depth | `fix/rnk-rootfs-extract-hardening` |
| 23 | Fixed: untrusted names escaped on stderr | `fix/error-message-hygiene` |
| 25 | Accepted and documented: `--network=none` isolates the `RUN` only | threat model section 6, Q-447 |
| 26, 27 | Fixed; the allowlist rules are guardrails, not a boundary | `fix/tooling-permission-rules` |
| 28 | Fixed; reproduced first (root in the VM, a default and a `--privileged` container, a ranikust `RUN`). Egress through passt, IPv4 only, maps nothing to the host's loopback; a restricted second NIC carries SSH and, while `push` runs, the test registries; every VM run checks it, raw frames included | `fix/vm-network-no-host-loopback`, ADR-0086 |

A red-team pass over the merged result then found three more breaks, all
fixed before Phase 2 closed: a `RUN` outliving a killed ranikust (T8; a
stub PID 1, `fix/run-stub-init`, ADR-0080, residual Q-452), the kaniko
walk's and the copy's memory on deep trees, and registry text reaching
errors unbounded (`fix/red-team-findings`).
