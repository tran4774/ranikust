# Design documents

Design docs describe the **current intended design** of ranikust: what each
component does, the invariants it keeps, its algorithms, how it handles
failure, and what is still open. They are updated in the same pull request as
the code changes they describe. When code and design doc disagree, one of
them is a bug.

| | |
|---|---|
| **Why** a design is the way it is | [`docs/adr/`](../adr/), linked from each doc |
| **Evidence** behind numbers and limits | [`docs/research/`](../research/), frozen on 2026-10-29 |
| **Where** it lives in the code | [`ARCHITECTURE.md`](../../ARCHITECTURE.md) |

## Index

| Document | Covers | Main crates |
|---|---|---|
| [modes-and-detection.md](modes-and-detection.md) | Snapshot modes, `--mode`, the privilege probes and their order, refusal and guidance | `ranikust`, `rnk-sandbox`, `rnk-snapshot` |
| [overlay-snapshot.md](overlay-snapshot.md) | Overlay mode: state directory layout, mount options and procedure, `volatile` lifecycle, upperdir → OCI layer export | `rnk-snapshot`, `rnk-layer` |
| [kaniko-mode.md](kaniko-mode.md) | Walk and hash, ignore list, safe deletion, host checks, per-stage directories, the `--parallel-stages` path and fake `/proc` | `rnk-snapshot`, `rnk-rootfs`, `rnk-sandbox` |
| [scheduler.md](scheduler.md) | Stage DAG, `--jobs`, memory-aware limit, lazy export, cancellation, deterministic output | `rnk-sched`, `rnk-builder` |
| [rootfs.md](rootfs.md) | Confined path resolution (`RootPath`), `EAGAIN` and missing-syscall handling, metadata order, layer extraction and whiteout forms, `USER` and `--chown` lookup, fd-based deletion | `rnk-rootfs` |
| [sandbox.md](sandbox.md) | `RUN` isolation: namespaces, mount sequence, `pivot_root`, capabilities, stubs, users | `rnk-sandbox` |
| [cache-and-store.md](cache-and-store.md) | State directory, persistent store layout, locks, atomic publication, prune, cache keys | `rnk-store`, `rnk-cache`, `rnk-snapshot`, `rnk-registry` |
| [streaming.md](streaming.md) | What a build keeps and for how long: base layers extracted while downloading, built layers pushed as they are exported, bounded buffers | `rnk-builder`, `rnk-registry`, `rnk-snapshot`, `rnk-store` |
| [metrics.md](metrics.md) | Opt-in build metrics: span conventions, the spans every crate emits, the JSON report, comparison with kaniko's `metrics.json` | `rnk-metrics`, every crate |

## Conventions

- Present tense, describing the design as it is meant to work now. History
  belongs in ADRs and research reports.
- Each doc has: purpose, invariants, design, failure handling, open
  questions, references. Keep each section short; link instead of repeating.
- Numbers come from experiments and cite them. Behaviour not yet verified is
  labelled `[inferred]`.
- Hard-wrap prose at about 80 columns; do not wrap tables, headings, links or
  code.
