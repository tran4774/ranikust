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
| [0026](0026-rnk-oci-owns-its-types.md) | `rnk-oci` owns its OCI and Docker image types instead of using `oci-spec` | Accepted | 2026-10-30 | `oci-spec` 0.10 source, `rnk-oci` tests; the string-form hand-over to `oci-client` refined by ADR-0070 |
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
| [0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md) | Kaniko mode on `/` keeps its state and credentials out of `RUN`'s reach: anonymous files, a non-dumpable process, no ptrace for `RUN`, no surviving processes | Accepted | 2026-10-31 | maintainer, proof of concept; ranikust's own capabilities and secret reads refined by ADR-0078; the state directory refined by ADR-0077 |
| [0052](0052-push-input-aliases-and-issue-mining-fixes.md) | `push` takes an OCI layout, kaniko's old spellings are refused, and four fixes from the issue mining | Accepted | 2026-10-31 | maintainer, issue mining; Q-048, Q-162 |
| [0053](0053-local-context-is-a-plain-path-no-dir-scheme.md) | A local context is a plain path: the `dir://` scheme is dropped, and `--context-sub-path` applies only to fetched contexts | Accepted | 2026-10-31 | maintainer, partly supersedes ADR-0011, ADR-0036; Q-081 |
| [0054](0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md) | Kaniko mode on `/` is refused where root may mount or `/proc/sys` is writable; `dir://` and a local `--context-sub-path` are refused | Accepted | 2026-10-31 | maintainer, completes ADR-0053; Q-341, Q-081 |
| [0055](0055-stream-layers-keep-nothing-proportional-to-the-image.md) | Layers are streamed: base layers extracted while downloading, built layers pushed as soon as they are exported | Accepted | 2026-11-01 | Q-340 (memory), `design/streaming.md`; the spool's lifetime on `/` refined by ADR-0081 |
| [0056](0056-kaniko-mode-state-on-disk-behind-a-run-fence.md) | Kaniko mode on `/` keeps its anonymous state on disk, named only while no `RUN` runs; kept stages are tars | Accepted | 2026-11-01 | Q-340, Q-350; VM PoC; its lock refined by ADR-0077 |
| [0057](0057-direct-run-heredoc-from-memfd-and-refusals.md) | A `Direct` `RUN` on `/` runs a `#!` heredoc from a sealed memfd, drops cache mounts, refuses secret files and `--network=none`; every `RUN` writes its streams to pipes of its own | Accepted | 2026-11-01 | maintainer, issue mining, host-safety; Q-380, Q-381, Q-382 |
| [0058](0058-a-step-that-changes-nothing-adds-no-layer.md) | A step that changes nothing adds no layer, only its history entry, as kaniko | Accepted | 2026-11-01 | maintainer, issue mining, partly supersedes ADR-0032 (Q-131); Q-385 |
| [0059](0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md) | Kaniko mode off `/` builds each stage in a directory of its own, scheduled as overlay mode's stages | Accepted | 2026-11-01 | maintainer, Phase 2 task 9; ADR-0014, ADR-0017 |
| [0060](0060-copy-leaves-special-files-out-where-run-has-no-nodev.md) | `COPY` leaves special files out where `RUN` has no `nodev`; `--parallel-stages` held back (Q-400) | Accepted | 2026-11-01 | maintainer, Phase 2 task 10, Q-400; ADR-0014, ADR-0041, ADR-0051 |
| [0061](0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md) | Per-stage directories go to their last reader and are released as soon as nobody reads them | Accepted | 2026-11-01 | maintainer, Q-410; ADR-0017, ADR-0041, ADR-0059; the reader's choice refined by ADR-0062 |
| [0062](0062-stage-directory-hand-over-decided-from-the-stage-graph.md) | A stage directory is handed over only to its only reader, decided from the stage graph | Accepted | 2026-11-01 | host-safety review of ADR-0061; refines ADR-0061 |
| [0063](0063-root-tests-that-mean-the-same-in-a-container-run-in-rootful-docker-on-the-test-machine.md) | Root tests that mean the same in a container run in rootful Docker on the test machine (tier T2c) | Accepted | 2026-11-01 | maintainer; VPS probe 2026-11-01; `docs/testing.md` |
| [0064](0064-materialize-refused-until-the-layer-cache.md) | `--materialize` is refused until the layer cache | Accepted | 2026-11-01 | maintainer; criterion-8 review |
| [0065](0065-single-snapshot-refused-for-good.md) | `--single-snapshot` is refused for good | Accepted | 2026-11-01 | maintainer, Q-420 measurement; ADR-0049 |
| [0066](0066-kaniko-mode-on-root-makes-no-names-after-a-build-cut-short.md) | Kaniko mode on `/` makes no names after a build in the container was cut short | Accepted | 2026-11-01 | maintainer, Q-360; host-safety review of ADR-0056 |
| [0067](0067-the-cli-crate-orchestrates-the-build.md) | The CLI crate orchestrates the build across the library crates | Accepted | 2026-11-02 | maintainer; refactor of `ranikust` |
| [0070](0070-registry-references-reach-oci-client-built-from-parts.md) | Registry references reach `oci-client` built from their parts, and a request fails if it would address another host | Accepted | 2026-11-02 | security hardening; refines ADR-0026 |
| [0074](0074-setuid-through-proc-root-of-a-full-sandbox-run-is-nosuid.md) | Through `/proc/<pid>/root` of a full-sandbox `RUN`, setuid files are `nosuid`; corrects ADR-0047 | Accepted | 2026-11-03 | source review; corrects a Consequence of ADR-0047; measured in the VM on 2026-11-03 |
| [0075](0075-layer-trusted-xattrs-are-never-set.md) | A layer's `trusted.*` xattrs are never set | Accepted | 2026-11-02 | BuildKit (containerd `archive.Apply`), follows ADR-0049 |
| [0076](0076-the-build-context-owner-is-a-lower-trust-principal.md) | The build context's owner is a principal of its own, trusted less than the operator | Accepted | 2026-11-02 | maintainer; source review of the operator paths |
| [0077](0077-kaniko-mode-on-root-never-uses-a-persistent-store.md) | Kaniko mode on `/` never uses a persistent store, shares its state directory with no other build, and leaves it empty | Accepted | 2026-11-02 | maintainer, hardening review; refines ADR-0051, ADR-0056; Q-441 |
| [0078](0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md) | Kaniko mode on `/` lends a `RUN` no capability, and ranikust reads its configuration and secrets once | Accepted | 2026-11-02 | maintainer, Phase 2 hardening; refines ADR-0051 |
| [0079](0079-resource-limits-with-a-limit-flag.md) | Registry and layer inputs are bounded by default, and `--limit NAME=VALUE` changes the bounds | Accepted | 2026-11-02 | maintainer, decision 4 of the hardening review; overrides Q-056; Q-448, Q-449 |
| [0080](0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md) | A full-sandbox `RUN` runs as PID 2 under an init that dies with ranikust | Accepted | 2026-11-03 | maintainer, red team of the hardening pass (T8) |
| [0081](0081-a-streamed-layer-on-slash-is-verified-once-per-build.md) | On `/`, a streamed base layer is downloaded and verified once per build, and kaniko mode decompresses a layer beside its extraction | Accepted | 2026-11-03 | maintainer, Phase 2 acceptance (criterion 3, W2); refines ADR-0055's amendment |
| [0082](0082-full-sandbox-binds-the-binary-read-only-over-itself.md) | The full sandbox binds ranikust's binary read-only over itself; `Direct` on `/` keeps today's barriers | Accepted | 2026-11-03 | maintainer, Q-422 (options (b) and (d)); ADR-0051, ADR-0080 |
| [0083](0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md) | A root `RUN`'s delay of its init's death is closed by a cgroup for the command where one is delegated, and by `--userns` in Phase 3 | Accepted | 2026-11-03 | maintainer, Q-452 (options (a) and (b)); partly supersedes ADR-0007; refines ADR-0080 |
| [0084](0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md) | Kaniko mode on `/` allows secret files; the directory that holds them is on the snapshot ignore list, and copying one into the image is the user's choice | Accepted | 2026-11-03 | maintainer, Q-445; partly supersedes ADR-0057 (Q-380); refines ADR-0078; refined by ADR-0085 |
| [0085](0085-a-secret-file-on-slash-goes-at-its-target-is-ignored-by-its-own-path-and-removed-after-its-run.md) | A secret file on `/` goes at its mount's target, only its own path is ignored, and the file placed for a `RUN` is removed after that `RUN` | Accepted | 2026-11-03 | maintainer, Q-453 (options (a) and (d)); refines ADR-0084 |
| [0086](0086-the-t2-vm-reaches-no-host-loopback-port-but-the-test-registries.md) | The T2 VM reaches no port of the host's loopback but the test registries, through passt and a restricted user network | Accepted | 2026-11-03 | maintainer, lead 28 of the Phase 1-2 security audit (option b) |
| [0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md) | Work flows through GitHub issues, pull requests and a Project, with agents running locally | Accepted | 2026-11-03 | maintainer, decisions D1, D2, D4 and D6 of the GitHub-flow proposal; D1 amended by ADR-0093; amended by ADR-0097; D6 refined by ADR-0111 |
| [0088](0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md) | Tracking lives on GitHub; knowledge tied to the code stays in the repository | Accepted | 2026-11-03 | maintainer, decision D3 (option A); ADR-0001 kept; ADR-0087 |
| [0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md) | Releases are a release pull request and a tag, built and attested by a workflow | Accepted | 2026-11-03 | maintainer; ADR-0009, ADR-0020, ADR-0087, ADR-0088 |
| [0090](0090-t2-also-runs-as-root-on-github-hosted-runners.md) | T2 also runs as root on GitHub-hosted runners, on every pull request | Accepted | 2026-11-03 | maintainer, GitHub flow (T2 on free runners, run directly on the runner); refines ADR-0063, amends its T2c marker "Where" for runners |
| [0091](0091-a-new-design-question-is-named-by-its-issue-number.md) | A new design question is named by its issue number; the migrated ones keep `Q-NNN` | Accepted | 2026-11-03 | maintainer, question on the numbering of new questions; refines ADR-0088 |
| [0092](0092-a-release-is-verified-by-workflow-and-tag-and-built-without-cache-or-unprotected-environment.md) | A release is verified by its workflow and tag, and built without a cache or an unprotected environment | Accepted | 2026-11-03 | maintainer; review of #42; refines ADR-0089 |
| [0093](0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md) | The agents' ask and deny rules are tightened guardrails, and agents may open `question` and `bug` issues | Accepted | 2026-11-03 | maintainer, questions on the ask/deny rules and on issues agents open; amends ADR-0087 (D1); amended by ADR-0111 |
| [0094](0094-the-release-ships-third-party-licence-notices-generated-by-cargo-about.md) | The release ships the third-party licence notices, generated by cargo-about | Accepted | 2026-11-03 | maintainer; ADR-0009, ADR-0089, ADR-0092 |
| [0095](0095-ci-runs-by-lanes-a-fast-lane-on-drafts-and-the-full-run-when-ready-behind-two-gate-checks.md) | CI runs by lanes: a fast lane on drafts, the full run when ready, behind two gate checks | Accepted | 2026-11-03 | maintainer, CI cost proposal (parts A to F, "mark ready" option); refines ADR-0087, ADR-0090, ADR-0009 (CI), ADR-0089 (dry run) |
| [0096](0096-agents-review-on-the-local-branch-before-every-push.md) | Agents review a change on the local branch before every push, and remove merged worktrees | Accepted | 2026-11-04 | maintainer; amends ADR-0087 (Flow); amended by ADR-0097 and ADR-0111 |
| [0097](0097-agents-work-in-fresh-bounded-contexts-instead-of-forks.md) | Agents work in fresh, bounded contexts: briefed subagents instead of forks, compaction at 300k, a model per task | Proposed | 2026-11-04 | maintainer; amends ADR-0087, ADR-0093, ADR-0096; refined by ADR-0111 |
| [0098](0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md) | A build in a user namespace refuses an image with an owner the namespace does not map, with one id or with ranges, and has no `ignore_chown_errors` flag | Accepted | 2026-11-04 | maintainer, Q-040 (#12, option (a)), how the refusal finds the owners (option (c'), "tất cả theo khuyến nghị"), #92 (option (d) here, (a) there), `--userns` owners outside the `ranikust` range ("Từ chối như ADR-0098 (khuyến nghị)") and user namespaces ranikust did not create ("Áp dụng cùng bản quét (khuyến nghị)", option (g) here, (1) as offered in the #92 comment); ADR-0004 |
| [0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md) | Rootless mode publishes extracted layers by rename, and runs every store operation on them inside the build's user namespace | Accepted | 2026-11-04 | maintainer, Q-152 (#5, option (c), not in the issue's list; "tất cả theo khuyến nghị"); ADR-0021, ADR-0050 |
| [0100](0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) | The probe is tested in today's harnesses plus runner, VM and k3s legs and Alma 9; older kernels, Docker rootless and NFS or FUSE wait for Phase 6 | Accepted | 2026-11-04 | maintainer, Q-039 (#11, option (b) plus row 9e of (c); "tất cả theo khuyến nghị"); ADR-0004, ADR-0063 and ADR-0090 stand |
| [0103](0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md) | A default seccomp profile applies wherever the full sandbox runs, `--seccomp=unconfined` turns it off, and the flag is refused in kaniko mode on `/` | Accepted | 2026-11-04 | maintainer, raised while planning Phase 3 (#41; option (a), "tất cả theo khuyến nghị"); refines ADR-0003 |
| [0104](0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md) | `--userns` runs the whole build in a user namespace with a range for root from a `ranikust` entry of `/etc/subuid`, extracted layers keyed by their id map, and rootless mode also leaves the init's uid unmapped | Accepted | 2026-11-04 | maintainer, raised while planning Phase 3 (#41; option (a), "tất cả theo khuyến nghị"), #90 (option (d) here, (a) there) and #91 (option (g) here, (a) there, the `ranikust` entry); refines ADR-0083 (b) and ADR-0003 |
| [0105](0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md) | A minimal `ranikust prune --all` in Phase 3 empties a rootless state directory inside the user namespace and refuses when root runs it | Accepted | 2026-11-04 | maintainer, raised while planning Phase 3 (#41; option (a), "tất cả theo khuyến nghị") and #93 (option (d) here, (a) there); ADR-0021 |
| [0106](0106-extraction-in-a-user-namespace-skips-device-nodes-and-counts-them.md) | Extraction in a user namespace skips device nodes and counts them as `nodes_skipped` | Accepted | 2026-11-04 | maintainer, raised while planning Phase 3 (#41; option (a), "tất cả theo khuyến nghị"); ADR-0041 |
| [0107](0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md) | Rootless Podman without a volume builds in kaniko mode on `/` with the volume hint, and a `hostUsers: false` pod with unmasked `/proc` stays refused until measured | Accepted | 2026-11-04 | maintainer, raised while planning Phase 3 (#41; options (a) and (c), "tất cả theo khuyến nghị"); refines ADR-0054 |
| [0108](0108-rootless-overlay-has-no-performance-target-and-its-time-is-recorded.md) | Rootless overlay has no performance target; its time per build is recorded against overlay as root and kaniko mode | Accepted | 2026-11-04 | maintainer, raised while planning Phase 3 (#41; option (a), "tất cả theo khuyến nghị") |
| [0111](0111-a-low-or-medium-security-finding-is-a-draft-advisory-that-does-not-block-a-release.md) | A Low or Medium security finding that is not a blocker is a draft security advisory and does not block a release | Accepted | 2026-11-05 | maintainer, workflow change and six follow-up answers ("Low/Medium, không phải blocker (khuyến nghị)", "Để agent làm xong", "Sau khi có bản release đã sửa (khuyến nghị)", "Lead mở vào GHSA nháp (khuyến nghị)", "Low/Medium qua PR thường (khuyến nghị)", "Lấy mức cao hơn (khuyến nghị)"); amends ADR-0093 (Issues) and ADR-0096 (fix loop); refines ADR-0087 (D6), ADR-0097 |
