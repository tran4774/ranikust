# Architecture Decision Records

This directory records the decisions that shape ranikust: what was decided,
which options were weighed, what it costs, and the evidence behind it. Each
record is short (about one page) and covers one decision.

| | |
|---|---|
| **Format** | [MADR 4.0](https://adr.github.io/madr/), short variant, plus an **Evidence** section. See [`template.md`](template.md). |
| **File names** | `NNNN-title-with-dashes.md`, numbered in order of creation. Numbers are never reused. |
| **Language** | English. |

## How ADRs relate to the other documents

| Document | Answers | Changes |
|---|---|---|
| `docs/adr/` (here) | *Why* is it like this? | Never rewritten; superseded by a newer ADR. |
| [`docs/design/`](../design/) | *How* does it work now? | Updated in the same PR as the code. |
| [`ARCHITECTURE.md`](../../ARCHITECTURE.md) | *Where* is it in the code? | Updated when crate responsibilities move. |
| [`docs/research/`](../research/) | *What was measured* before any code was written? | Frozen on 2026-10-29. |

ADR-0003 to ADR-0014 carry over decisions Q1 to Q12 of the pre-code survey
([survey 05](../research/survey/05-decisions.md)); ADR-0015 to ADR-0020 come
from the experiments. All twenty were accepted by the maintainer on
2026-10-29. ADR-0021 to ADR-0024 answer the open questions O1 to O4 that
blocked Phase 1 ([phase-1](../agents/phase-1.md)); they were accepted on
2026-10-30. ADR-0025 records how builds are measured (`rnk-metrics`);
ADR-0026 to ADR-0028 record decisions made while implementing Phase 1.

## Status lifecycle

```
proposed ──► accepted ──► deprecated
    │            │
    │            └──────► superseded by ADR-NNNN
    └──► rejected
```

- **proposed**: open for review in a pull request.
- **accepted**: in force. The body of an accepted ADR is not edited. Only the
  `status` and `date` front matter change, when the ADR is deprecated or
  superseded.
- **rejected**: considered and declined; kept so the question is not reopened
  without new evidence.
- **deprecated**: no longer applies, and nothing replaces it.
- **superseded by ADR-NNNN**: replaced by a newer ADR, which says
  "Supersedes ADR-MMMM" in its More Information section.

A correction or a change of mind is always a new ADR. Reopen a decision only
when new measurements contradict its evidence.

## Adding an ADR

1. In Claude Code, run `/adr <title>`. The skill picks the next number, copies
   the template, and adds a row to the index below. Without the skill, copy
   [`template.md`](template.md) to `NNNN-title-with-dashes.md` by hand.
2. Fill in every section. State the decision in one sentence at the top of
   Decision Outcome. Link evidence under `docs/research/` or a new experiment
   report; label unmeasured claims `[inferred]`.
3. If the ADR replaces another one, set the old ADR's status to
   `superseded by ADR-NNNN` in the same pull request.
4. Update the design docs, `ARCHITECTURE.md` and the user book where the
   decision changes what they describe.
5. Open a pull request with status `proposed`. On approval, set `accepted`
   and the date of acceptance.

## Index

| ADR | Title | Status | Date | Origin |
|---|---|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted | 2026-10-29 | — |
| [0002](0002-two-snapshot-modes.md) | Two snapshot modes: overlay when permitted, kaniko-style walk otherwise | Accepted | 2026-10-29 | Survey 00, 06 |
| [0003](0003-dockerfile-trust-and-default-isolation.md) | Dockerfile trust is the user's call; default isolation like `docker build` | Accepted | 2026-10-29 | Q1 |
| [0004](0004-support-every-privilege-level.md) | Support every privilege level: detect, fall back, or refuse | Accepted | 2026-10-29 | Q2 |
| [0005](0005-own-versioned-cache-key.md) | Own versioned cache key, no kaniko cache compatibility | Accepted | 2026-10-29 | Q3 |
| [0006](0006-persistent-store.md) | Persistent store with prune, digest locks and atomic rename | Accepted | 2026-10-29 | Q4 |
| [0007](0007-run-anywhere-shared-store.md) | Run anywhere; concurrent builds share one store | Accepted | 2026-10-29 | Q5 |
| [0008](0008-out-of-scope-bake-warmer-cloud-contexts.md) | Out of scope: bake, warmer, cloud build contexts | Accepted | 2026-10-29 | Q6 |
| [0009](0009-x86-64-first-aarch64-ready.md) | x86_64 releases first, aarch64 kept buildable | Accepted | 2026-10-29 | Q7 |
| [0010](0010-run-network-host-or-none.md) | `RUN` shares the host network; `--network=none` | Accepted | 2026-10-29 | Q8 |
| [0011](0011-accept-common-kaniko-flags.md) | Accept the common kaniko flags | Accepted | 2026-10-29 | Q9 |
| [0012](0012-accept-divergence-between-modes.md) | Accept content and digest divergence between modes | Accepted | 2026-10-29 | Q10 |
| [0013](0013-jobs-default-available-parallelism.md) | `--jobs` defaults to `available_parallelism()` | Accepted | 2026-10-29 | Q11 |
| [0014](0014-kaniko-mode-sequential-by-default.md) | kaniko mode is sequential by default; `--parallel-stages` opts in | Accepted | 2026-10-29 | Q12 |
| [0015](0015-hand-written-dockerfile-parser.md) | Hand-written Dockerfile parser and `.dockerignore` matcher | Accepted | 2026-10-29 | Exp 04 |
| [0016](0016-overlay-mount-options.md) | Overlay mount options; `EXDEV` on lower directory renames is accepted | Accepted | 2026-10-29 | Exp 01, 06, 06a, 10, 11, 13 |
| [0017](0017-stage-rootfs-by-copy-not-hardlinks.md) | Per-stage rootfs by copy or reflink, never hardlink farms | Accepted | 2026-10-29 | Exp 12 |
| [0018](0018-no-proot-in-1-0.md) | No proot in 1.0 | Accepted | 2026-10-29 | Exp 12, 13 |
| [0019](0019-java-tool-options-on-parallel-path.md) | Inject `JAVA_TOOL_OPTIONS` on the parallel path under cgroup limits | Accepted | 2026-10-29 | Exp 15, 16 |
| [0020](0020-static-musl-via-cargo-zigbuild.md) | Static musl binary; cargo-zigbuild for cross builds | Accepted | 2026-10-29 | Exp 05 |
| [0021](0021-rnk-store-crate.md) | A new crate, `rnk-store`, owns the persistent store | Accepted | 2026-10-30 | Phase 1 O1 |
| [0022](0022-common-state-dir.md) | One state directory for every mode | Accepted | 2026-10-30 | Phase 1 O3 |
| [0023](0023-mode-flag.md) | `--mode` selects the snapshot mode; `--snapshot-mode` keeps kaniko's meaning | Accepted | 2026-10-30 | Phase 1 O2 |
| [0024](0024-tls-rustls-ring.md) | rustls with the `ring` provider; trust host CAs and embedded roots together | Accepted | 2026-10-30 | Phase 1 O4, Exp 05 |
| [0025](0025-build-metrics-from-tracing-spans.md) | Build metrics from `tracing` spans, aggregated by an opt-in `rnk-metrics` crate | Accepted | 2026-10-30 | kaniko fork metrics, Exp 07, 08, 12 |
| [0026](0026-rnk-oci-owns-its-types.md) | `rnk-oci` owns its OCI and Docker image types instead of using `oci-spec` | Accepted | 2026-10-30 | `oci-spec` 0.10 source, `rnk-oci` tests |
| [0027](0027-context-filtering-follows-buildkit-walk.md) | Build-context filtering follows BuildKit's walk, not `MatchesOrParentMatches` | Accepted | 2026-10-30 | Q-050, `rnk-ignore` goldens |
| [0028](0028-minimum-kernel-linux-5-8.md) | Require Linux 5.8 or later and check it at start-up | Accepted | 2026-10-30 | Q-054, `rnk-rootfs` escape suite |
