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
ADR-0026 to ADR-0029 record decisions made while implementing Phase 1;
ADR-0030 and ADR-0031 answer open questions Q-012, Q-075 and Q-076.
ADR-0032 settles the remaining questions about observable behaviour by
following BuildKit. ADR-0049 puts security and performance before compatibility
with kaniko and BuildKit. ADR-0038 answers the questions that blocked Phase 2
(kaniko mode).

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
| [0029](0029-zstd-layers-as-independent-1-mib-frames.md) | zstd layers are written as independent 1 MiB frames | Accepted | 2026-10-30 | Exp 05 Q3, `rnk-layer` tests |
| [0030](0030-phase-1-dependency-picks.md) | Phase 1 dependencies: rustix, sha2, gzip by hand, reqwest, own config.json reader, clap | Accepted | 2026-10-30 | Q-012 (Phase 1 O5), Exp 05 |
| [0031](0031-run-host-name-and-hosts-file.md) | A RUN sees the fixed host name `ranikust` and the host's /etc/hosts, or loopback without network | Accepted | 2026-10-30 | Q-075, Q-076, kaniko source |
| [0032](0032-observable-behaviour-follows-buildkit.md) | Observable build behaviour follows BuildKit | Accepted | 2026-10-30 | 28 open questions, BuildKit source |
| [0033](0033-network-none-loopback-through-a-user-namespace.md) | `RUN --network=none` without `CAP_NET_ADMIN` is refused, unless `--allow network.none-userns` opts in to a user namespace under seccomp | Accepted | 2026-10-30 | Q-183, host-safety review, VM smoke suite |
| [0034](0034-unchanged-copy-ups-left-out-of-layers.md) | Unchanged copy-ups are left out of a layer, as BuildKit's overlay differ | Accepted | 2026-10-30 | Q-049, BuildKit source |
| [0035](0035-git-contexts-built-in-or-external-git.md) | Git contexts use a built-in git by default, an external `git` on request, with its flags configurable | Accepted | 2026-10-30 | Q-045, kaniko and BuildKit source |
| [0036](0036-kaniko-flags-in-phase-1.md) | Kaniko flags in Phase 1: accepted, ignored with a warning, or refused with the phase | Accepted | 2026-10-30 | Q-023, Q-020, Q-021, Q-031, kaniko source |
| [0037](0037-metrics-report-path-from-the-flag-only.md) | The metrics report path comes only from `--metrics-file` | Accepted | 2026-10-30 | maintainer, partly supersedes ADR-0025 |
| [0038](0038-kaniko-mode-settings-for-phase-2.md) | Kaniko-mode settings for Phase 2: workdir checks in `ranikust`, state directory, ignore list, lazy snapshots, fake `/proc`, `--parallel-stages` | Accepted | 2026-10-30 | Q-011, Q-019, Q-034, Q-035, Q-036, Q-037 |
| [0039](0039-kaniko-mode-walk-and-hash-crates.md) | The kaniko-mode walk uses rayon over `getdents64` and hashes with xxh3-128 | Accepted | 2026-10-30 | Q-038, `tests/bench/walk-hash` |
| [0040](0040-kaniko-mode-selection-container-check-and-flags.md) | Kaniko mode is picked when root may not mount, builds on `/` only in a container unless `--force`, and takes kaniko's snapshot flags | Accepted | 2026-10-31 | Q-260, kaniko `pkg/util/proc/proc.go` |
| [0041](0041-live-root-extraction-skips-device-nodes.md) | Extraction into a live root creates no device node or FIFO, as kaniko | Accepted | 2026-10-31 | Q-270, kaniko source |
| [0043](0043-direct-run-on-a-workdir-chroots-with-placed-files.md) | A `Direct` `RUN` on a workdir other than `/` enters it with `chroot`, with journaled placed files and the original `RLIMIT_NOFILE` | Accepted | 2026-10-31 | kaniko source, exp 12, `rnk-sandbox` tests |
| [0044](0044-kaniko-mode-walks-every-kept-stage.md) | Sequential kaniko mode walks every stage whose image is kept for a later `FROM` | Accepted | 2026-10-31 | Q-300 |
| [0045](0045-kaniko-mode-layer-holds-what-changed.md) | A kaniko-mode layer holds what changed, not the targets of changed symlinks | Accepted | 2026-10-31 | kaniko `pkg/filesystem/resolve.go`, follows BuildKit |
| [0046](0046-snapshotter-signature.md) | `Snapshotter` names its base-layer and diff types, so overlay and kaniko mode share one builder | Accepted | 2026-10-31 | Q-024 |
| [0047](0047-kaniko-workdir-chain-is-private.md) | A kaniko-mode host workdir must be out of other users' reach | Accepted | 2026-10-31 | Q-320 |
| [0048](0048-container-heuristic-guards-the-root.md) | Kaniko mode builds on `/` when the container heuristic says so, or with `--force` | Accepted | 2026-10-31 | Q-321 |
| [0049](0049-security-and-performance-over-kaniko-compatibility.md) | Security and performance come before kaniko compatibility; flags that change nothing are refused, kaniko's variables are not read | Accepted | 2026-10-31 | maintainer, partly supersedes ADR-0011, ADR-0032, ADR-0036; Q-008, Q-010, Q-047, Q-048, Q-260 |
| [0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md) | ranikust picks the build root itself: `/` in a container, else a directory of the state directory; no `--workdir`, no `--force` | Accepted | 2026-10-31 | maintainer, partly supersedes ADR-0011, ADR-0022, ADR-0038, ADR-0040, ADR-0047, ADR-0048; Q-110, Q-330 |
| [0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md) | Kaniko mode on `/` keeps its state and credentials out of `RUN`'s reach: anonymous files, a non-dumpable process, no ptrace for `RUN`, no surviving processes | Accepted | 2026-10-31 | maintainer, proof of concept |
| [0052](0052-push-input-aliases-and-issue-mining-fixes.md) | `push` takes an OCI layout, kaniko's old spellings are refused, and four fixes from the issue mining | Accepted | 2026-10-31 | maintainer, issue mining; Q-048, Q-162 |
| [0053](0053-local-context-is-a-plain-path-no-dir-scheme.md) | A local context is a plain path: the `dir://` scheme is dropped, and `--context-sub-path` applies only to fetched contexts | Accepted | 2026-10-31 | maintainer, partly supersedes ADR-0011, ADR-0036; Q-081 |
| [0054](0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md) | Kaniko mode on `/` is refused where root may mount or `/proc/sys` is writable; `dir://` and a local `--context-sub-path` are refused | Accepted | 2026-10-31 | maintainer, completes ADR-0053; Q-341, Q-081 |
| [0055](0055-stream-layers-keep-nothing-proportional-to-the-image.md) | Layers are streamed: base layers extracted while downloading, built layers pushed as soon as they are exported | Accepted | 2026-11-01 | Q-340 (memory), `design/streaming.md` |
