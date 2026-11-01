---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0066: Kaniko mode on `/` makes no names after a build in the container was cut short

## Context and Problem Statement

ADR-0056 keeps kaniko mode's state on disk on a container's `/`, out of a
`RUN`'s reach behind a fence that holds only this build's `RUN`
processes. When an earlier ranikust in the same container was killed
mid-`RUN` and was not PID 1, that `RUN`'s processes may live on outside
any fence and any sweep, and could open a name in the moment it exists.
ranikust sees the trace: its marker `.ranikust-building` (PID namespace
and boot id) left in the state directory, or leftover `.ranikust-anon-*`
names. The host-safety review of ADR-0056 asked what such a build does
(Q-360).

## Considered Options

- (a) Make no name for that build: state in `memfd`, with a warning saying
  why and how to avoid it (run ranikust as PID 1, or restart the
  container); the marker is kept for later builds; a `COPY --from` a kept
  stage or an image on `/`, which needs a named directory, fails the build
  with that advice.
- (b) Refuse the build.
- (c) Kill every process that is not an ancestor of ranikust: not safe,
  the container's other processes (a CI shell, a sidecar) cannot be told
  apart from the dead build's.
- (d) Make names only when ranikust is PID 1, `memfd` otherwise.

## Decision Outcome

(a) (maintainer, 2026-11-01: "4: ok chốt a"). This was already the
interim behaviour (`rnk_store::Store::open_anonymous`, merged with
ADR-0056) and stays as it is:

- A store opened on this container's marker, or on names an earlier store
  left, is *tainted*: it makes no name for the build (state in `memfd`),
  warns with the reason and the advice, and keeps the marker.
- `Store::named_window` fails in a tainted store, so a `COPY --from` a kept
  stage or an image unpacked by name fails the build with the same advice.
- A marker from another PID namespace or boot is a dead container's and is
  taken over; the store's exclusive `flock` on the state directory refuses
  a second build on it at the same time.

### Consequences

- Good, because the build still succeeds in the common case and no name
  exists for a surviving `RUN` process to open.
- Good, because nothing in the container is killed on a guess.
- Bad, because such builds use memory for their state until the container
  restarts or ranikust runs as PID 1, and a `COPY --from` a kept stage or
  an image fails there.
- Bad, because a `RUN` that deletes the marker on purpose is not covered;
  option (d) would cover it, at a memory cost under a CI shell.

## Evidence

- [code] `crates/rnk-store/src/store.rs` (`open_anonymous`, `CUT_SHORT`,
  `named_window`) and `crates/rnk-store/src/anon.rs` (`AnonFiles::tainted`),
  with their tests.
- The host-safety review of ADR-0056 (2026-11-01), which raised Q-360.

## More Information

Answers Q-360; completes ADR-0056 without rewriting it. Related: ADR-0051
(kaniko mode on `/` keeps state out of `RUN`'s reach), T21 in the threat
model.
