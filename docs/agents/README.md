# Agent docs: start here

**TL;DR:** A task-oriented map of ranikust for coding agents (and humans in a
hurry). Pick the row that matches your task and open only what it lists. Every
file here summarises an authoritative source and links to it; load a long
document only when a card sends you there.

## Find what to read

| Your task | Read (in order) |
|---|---|
| First time in the repo | [overview.md](overview.md), [rules.md](rules.md) |
| Work on a crate | [rules.md](rules.md), [crates/\<crate\>.md](crates/), then the design doc the card links |
| Pick the next task | The [Project](https://github.com/users/tran4774/projects/1) and the phase milestones (`ADR-0088`): Phase 3 is tracking issue [#41](https://github.com/tran4774/ranikust/issues/41), its plan not yet written. Phases 1 and 2 are done: [phase-1.md](phase-1.md), [phase-2.md](phase-2.md) (frozen) |
| Match kaniko behaviour, port a kaniko feature or flag | [kaniko-map.md](kaniko-map.md), then `upstream-oracle` / the `kaniko-parity` skill; for BuildKit, `buildkit-parity` |
| Build, test, commit, open a PR | [workflow.md](workflow.md) |
| Unknown term (whiteout, copy-up, lazy export, W3…) | [glossary.md](glossary.md) |
| Need a spec, upstream source or crate doc | [refs.md](refs.md) |
| Need the reason behind a decision | the ADR on the crate card; index in [`../adr/README.md`](../adr/README.md) |
| Need measured evidence | the experiment on the crate card; index in [`../research/README.md`](../research/README.md) |
| Instrument a crate, read or compare a metrics report | the card's `## Metrics`, then [`../design/metrics.md`](../design/metrics.md#spans-per-crate) |
| Hit a question no ADR or design doc answers | the [`question` issues](https://github.com/tran4774/ranikust/issues?q=is%3Aissue+label%3Aquestion) and the answered archive [`../open-questions.md`](../open-questions.md); if it is in neither, open an issue with the Design question form ([rules.md](rules.md#open-questions)) |

## Files

| File | What it holds |
|---|---|
| [overview.md](overview.md) | What ranikust is; where the pipeline, modes, crate graph and state are described |
| [rules.md](rules.md) | The must-follow list: host safety, code traps, determinism, writing |
| [workflow.md](workflow.md) | Dev loop, `just` recipes, test tiers, adding a crate, dependency or ADR, commits, keep-in-sync matrix |
| [phase-1.md](phase-1.md), [phase-2.md](phase-2.md) | Frozen records of Phase 1 (MVP) and Phase 2 (kaniko mode): scope, task order, acceptance criteria |
| [kaniko-map.md](kaniko-map.md) | kaniko packages and flags to ranikust crates and flags, with source references; behaviour to match |
| [glossary.md](glossary.md) | Terms used across code and docs |
| [refs.md](refs.md) | Specs, upstream code, chosen crates and their docs |
| [crates/](crates/) | One card per crate (template: [crates/_template.md](crates/_template.md)) |

The files carry no line counts: they change with every edit. Use `wc -l` to
size a file before reading it.

### Crate cards

| Crate | Card | One line |
|---|---|---|
| `ranikust` | [crates/ranikust.md](crates/ranikust.md) | The binary: CLI, process set-up |
| `rnk-dockerfile` | [crates/rnk-dockerfile.md](crates/rnk-dockerfile.md) | Dockerfile to typed instructions and stage graph |
| `rnk-ignore` | [crates/rnk-ignore.md](crates/rnk-ignore.md) | `.dockerignore` matching (moby `patternmatcher`) |
| `rnk-context` | [crates/rnk-context.md](crates/rnk-context.md) | Build context sources |
| `rnk-oci` | [crates/rnk-oci.md](crates/rnk-oci.md) | OCI and Docker image types |
| `rnk-registry` | [crates/rnk-registry.md](crates/rnk-registry.md) | Registry client: auth, pull, push |
| `rnk-rootfs` | [crates/rnk-rootfs.md](crates/rnk-rootfs.md) | Confined filesystem access (`RootPath`) |
| `rnk-store` | [crates/rnk-store.md](crates/rnk-store.md) | Persistent store in the state directory: blobs, layers, cache mounts, locks, prune, `BuildPaths` |
| `rnk-sandbox` | [crates/rnk-sandbox.md](crates/rnk-sandbox.md) | `RUN` isolation and its privilege probes |
| `rnk-snapshot` | [crates/rnk-snapshot.md](crates/rnk-snapshot.md) | Stage roots and diffs, overlay and kaniko modes, overlay probes |
| `rnk-sched` | [crates/rnk-sched.md](crates/rnk-sched.md) | Stage DAG scheduler |
| `rnk-layer` | [crates/rnk-layer.md](crates/rnk-layer.md) | Deterministic tar, parallel compression, digests |
| `rnk-cache` | [crates/rnk-cache.md](crates/rnk-cache.md) | Cache keys, layer cache |
| `rnk-builder` | [crates/rnk-builder.md](crates/rnk-builder.md) | Builds one stage, assembles the image |
| `rnk-metrics` | [crates/rnk-metrics.md](crates/rnk-metrics.md) | Log subscriber and opt-in build metrics: spans to a JSON report |

## Conventions in these files

- **Status** on every card: `planned` (no code), `skeleton` (crate exists,
  docs only), `in progress`, `done`. Trust the code over the card; fix the
  card in the same PR when they disagree.
- **Labels:** `[decided]` (backed by an ADR), `[design]` (current design, may
  change), `[open: #N]` (undecided; issue #N, labelled `question`: do not
  invent an answer).
- **References** are short and greppable:
  - `ADR-0016`: `../adr/0016-overlay-mount-options.md`;
  - `design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer`: a design doc
    section;
  - `exp-07`: `../research/experiments/07-kaniko-vs-overlay.md`;
  - `kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:123`: kaniko source
    (osscontainertools/kaniko `34ba9eadf` plus local commits, see
    [`../research/README.md`](../research/README.md#source-snapshot));
  - external links in full.
- Every file opens with a **TL;DR** of 1-3 lines.
- Headings are fixed per file type, so `rg '^## Pitfalls' docs/agents` and
  similar queries work across cards.

## Keeping it current

- Change a crate's API, status or boundaries: update its card in the same PR.
- A task's pull request closes its issue (`Closes #N`). Phases 1 and 2 are
  done and their files frozen; later phases live in issues (`ADR-0088`).
- A card or file grows past about 140 lines: move detail into `docs/design/`
  and link it.
- Never copy long passages from ADRs, design docs or research: summarise in a
  line and link.
