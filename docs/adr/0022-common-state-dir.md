---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0022: One state directory for every mode

## Context and Problem Statement

The persistent store ([ADR-0006](0006-persistent-store.md)) had two homes. In
overlay mode the workdir was the store. In kaniko mode the workdir is the
root filesystem being walked and emptied, so the store lived in a separate
state directory (`--state-dir`, kaniko's `--kaniko-dir`), on a host by
default `<workdir>/../state`. Two layouts meant two sets of "never write
outside" checks and two answers to where `rnk-store`
([ADR-0021](0021-rnk-store-crate.md)) finds its files (Phase 1 open question
O3). kaniko keeps `/kaniko` inside the root and on its ignore list. Where
does the store live?

## Considered Options

- Keep both homes: the workdir in overlay mode, `--state-dir` in kaniko
  mode.
- Always `--state-dir`, always outside the root. In a container, kaniko
  mode's root is `/`, so the state directory would need a volume: kaniko's
  plain `docker run` usage would no longer work as is.
- Always `--state-dir`, with a default per mode: inside the workdir in
  overlay mode, and a kaniko-style directory on the ignore list in kaniko
  mode.

## Decision Outcome

ranikust keeps its store in one state directory, set by `--state-dir`, in
every mode: by default `<workdir>/.ranikust` in overlay mode and
`/ranikust/state` in kaniko mode.

- The state directory holds the store layout of
  [cache-and-store](../design/cache-and-store.md): `blobs/`, `layers/`,
  `caches/`, `tmp/` and the lock files, plus the per-build `builds/<id>`
  areas and the stub journal.
- In kaniko mode `/ranikust/state` plays the role of kaniko's `/kaniko`. It
  is on the kaniko-mode ignore list and is never walked, snapshotted or
  deleted.
- `--state-dir` replaces the default in either mode. It corresponds to
  kaniko's `--kaniko-dir` ([ADR-0011](0011-accept-common-kaniko-flags.md)).
- Concurrent builds share one store through per-digest locks
  ([ADR-0007](0007-run-anywhere-shared-store.md)). Only kaniko mode takes an
  exclusive lock on the workdir, because there the workdir is the root being
  built (`/` in a container). Overlay-mode builds do not lock the workdir.

### Consequences

- Good, because there is one layout, one set of path checks and one
  argument for `rnk-store`, whatever the mode.
- Good, because a kaniko pipeline needs no new volume: `/ranikust/state`
  behaves like `/kaniko`.
- Good, because several overlay-mode builds can share a workdir and its
  store.
- Bad, because in a container the store sits inside the root that `RUN`
  sees. kaniko-mode `RUN` runs directly on `/`, so it can read and change
  the store, and a Dockerfile that writes under `/ranikust` collides with it,
  as with `/kaniko` ([survey 02](../research/survey/02-kaniko-mode-requirements.md)
  §2).
- Bad, because in overlay mode the per-build upper directories live in the
  state directory, which therefore needs a filesystem that can hold an
  overlay upper directory [inferred].
- Neutral, because the kaniko-mode ignore list grows by one path, as
  kaniko's does.

## Evidence

- [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md)
  finding 4: a state directory inside the walked root leaked into the diff,
  and putting it on the ignore list fixed it [measured].
- [Survey 02](../research/survey/02-kaniko-mode-requirements.md) §2: kaniko
  keeps `/kaniko` inside the root on its ignore list; the drawbacks (`RUN`
  can see it, `COPY --from` or `WORKDIR` may collide); inside a container it
  "can be kept for compatibility".
- kaniko: `kaniko@44aa7ddc0 pkg/config/init.go:41` (`KanikoDir` from
  `KANIKO_DIR`, else the executable's directory), `pkg/util/fs_util.go:72-89`
  (the default ignore list starts with `KanikoDir`),
  `cmd/executor/cmd/root.go:345` (`--kaniko-dir`) [code].
- [Survey 02](../research/survey/02-kaniko-mode-requirements.md) §4: one
  build per kaniko-mode workdir, enforced by `flock`.

## More Information

Refines [ADR-0006](0006-persistent-store.md) (where the store lives) and
[ADR-0011](0011-accept-common-kaniko-flags.md) (`--state-dir` defaults).
Layout and locks: [cache-and-store](../design/cache-and-store.md). Open:
the default when kaniko mode runs outside a container, and where the
probe's test overlay goes before the mode, and with it the default, is known.
