# Agent docs: start here

A compact, task-oriented map of ranikust for coding agents (and humans in a
hurry). Read this file, pick the row that matches your task, and open only
what it lists. Everything here is a summary that points to the authoritative
source; do not load the long documents unless a card sends you there.

## Find what to read

| Your task | Read (in order) |
|---|---|
| First time in the repo | [overview.md](overview.md), [rules.md](rules.md) |
| Work on a crate | [rules.md](rules.md), [crates/\<crate\>.md](crates/), then the design doc the card links |
| Pick the next Phase 1 task | [phase-1.md](phase-1.md), then the crate card |
| Match kaniko behaviour, port a kaniko feature or flag | [kaniko-map.md](kaniko-map.md), then `kaniko-oracle` / the `kaniko-parity` skill |
| Build, test, commit, open a PR | [workflow.md](workflow.md) |
| Unknown term (whiteout, copy-up, lazy export, W3…) | [glossary.md](glossary.md) |
| Need a spec, upstream source or crate doc | [refs.md](refs.md) |
| Need the reason behind a decision | the ADR listed on the crate card; index in [`../adr/README.md`](../adr/README.md) |
| Need measured evidence | the experiment listed on the crate card; index in [`../research/README.md`](../research/README.md) |
| Hit a question no ADR or design doc answers | [`../open-questions.md`](../open-questions.md): check it is not there, then add it (rules: [rules.md](rules.md#open-questions)) |

## Files

| File | What it holds | Size |
|---|---|---|
| [overview.md](overview.md) | What ranikust is, the build pipeline, the two modes, the crate graph, where state lives | 110 lines |
| [rules.md](rules.md) | Host safety, code traps, determinism, writing rules: the must-follow list | 112 lines |
| [workflow.md](workflow.md) | Dev loop, `just` recipes, test tiers, adding a crate/dependency/ADR, commits, keep-in-sync matrix | 126 lines |
| [phase-1.md](phase-1.md) | Phase 1 (MVP) scope, task order, owners, acceptance criteria, what is deferred | 128 lines |
| [kaniko-map.md](kaniko-map.md) | kaniko packages and flags → ranikust crates, with source references | 140 lines |
| [glossary.md](glossary.md) | Terms used across code and docs | 69 lines |
| [refs.md](refs.md) | Specs, upstream code, chosen crates and their docs | 93 lines |
| [crates/](crates/) | One card per crate (template: [crates/_template.md](crates/_template.md)) | 94–120 lines each |

### Crate cards

| Crate | Card | One line |
|---|---|---|
| `ranikust` | [crates/ranikust.md](crates/ranikust.md) | The binary: CLI, process setup |
| `rnk-dockerfile` | [crates/rnk-dockerfile.md](crates/rnk-dockerfile.md) | Dockerfile → typed instructions and stage graph |
| `rnk-ignore` | [crates/rnk-ignore.md](crates/rnk-ignore.md) | `.dockerignore` matching (moby `patternmatcher`) |
| `rnk-context` | [crates/rnk-context.md](crates/rnk-context.md) | Build context sources |
| `rnk-oci` | [crates/rnk-oci.md](crates/rnk-oci.md) | OCI/Docker image types |
| `rnk-registry` | [crates/rnk-registry.md](crates/rnk-registry.md) | Registry client: auth, pull, push |
| `rnk-rootfs` | [crates/rnk-rootfs.md](crates/rnk-rootfs.md) | Confined filesystem access (`RootPath`) |
| `rnk-store` | [crates/rnk-store.md](crates/rnk-store.md) | Persistent store in the state directory: blobs, layers, cache mounts, locks, prune, `BuildPaths` |
| `rnk-sandbox` | [crates/rnk-sandbox.md](crates/rnk-sandbox.md) | `RUN` isolation, its privilege probes |
| `rnk-snapshot` | [crates/rnk-snapshot.md](crates/rnk-snapshot.md) | Stage roots and diffs, overlay and kaniko modes, overlay probes |
| `rnk-sched` | [crates/rnk-sched.md](crates/rnk-sched.md) | Stage DAG scheduler |
| `rnk-layer` | [crates/rnk-layer.md](crates/rnk-layer.md) | Deterministic tar, parallel compression, digests |
| `rnk-cache` | [crates/rnk-cache.md](crates/rnk-cache.md) | Cache keys, layer cache |
| `rnk-builder` | [crates/rnk-builder.md](crates/rnk-builder.md) | Builds one stage, assembles the image |

## Conventions in these files

- **Status** on every card: `planned` (no code), `skeleton` (crate exists,
  docs only), `in progress`, `done`. Trust the code over the card; fix the
  card in the same PR when they disagree.
- **Labels:** `[decided]` (backed by an ADR), `[design]` (current design,
  may change), `[open: Q-NNN]` (undecided; registered in
  [`../open-questions.md`](../open-questions.md); do not invent an answer).
- **References** are short and greppable:
  - `ADR-0016`: `../adr/0016-overlay-mount-options.md`;
  - `design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer`: a design doc section;
  - `exp-07`: `../research/experiments/07-kaniko-vs-overlay.md`;
  - `kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:123`: kaniko source
    (osscontainertools/kaniko `34ba9eadf` plus local commits, see
    [`../research/README.md`](../research/README.md#source-snapshot));
  - external links in full.
- Every file opens with a **TL;DR** of 1–3 lines, so reading the first few
  lines is often enough.
- Headings are fixed per file type, so `rg '^## Pitfalls' docs/agents` and
  similar queries work across cards.

## Keeping it current

These files are cheap to read only if they stay accurate and short.

- Change a crate's API, status or boundaries → update its card in the same PR.
- Finish a Phase 1 task → tick it in [phase-1.md](phase-1.md).
- A card or file grows past ~140 lines → move detail into `docs/design/` and
  link it.
- Never copy long passages from ADRs, design docs or research; summarize in a
  line and link.
