# Security audits

Each milestone security audit (the vendored `security-audit` skill, see
[`docs/agents/workflow.md`](../../agents/workflow.md)) keeps its deliverables
here, one directory per audit, named after the phases or the scope it covers.
The run itself writes outside the repository (agent prompts, per-agent
results, the coordinator's tools). Once its validators pass, only the
deliverables below move here, on a `docs/` branch.

| File | Contents |
|---|---|
| `REPORT.md` | Scope, execution policy, posture, the prioritised leads, rejected candidates, coverage, maintainer decisions, hardening notes |
| `NEEDS-VALIDATION.md` | Every unresolved lead with its trace, evidence, blockers and the exact VM (T2) or T2c plan |
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
open, treat its details as an unfixed vulnerability report: SECURITY.md
asks for private disclosure.
