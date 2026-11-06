---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0122: prune tells an entry in use by a shared lock and its last use by its marker's mtime

## Context and Problem Statement

[ADR-0032](0032-observable-behaviour-follows-buildkit.md) (Q-043) ages store
entries by last use with BuildKit's tiers, and `ranikust prune` must never
remove an entry a running build uses. BuildKit tracks both in its daemon's
database; ranikust has no daemon, and
[cache-and-store](../design/cache-and-store.md#prune) had marked the tracking
`[inferred]`. How does `prune` know an entry is in use, and when it was last
used (raised while planning Phase 5, see [the
roadmap](../../README.md#roadmap))? The store already takes `flock`s that the
kernel drops when a process dies ([locks](../design/cache-and-store.md#locks)).

## Considered Options

- (a) A shared `flock` on each entry a build uses, held for the build; `prune`
  takes it exclusive before removing; the last use is the entry's marker mtime,
  touched when the entry is taken. Crash-safe, one descriptor per entry.
- (b) A lease file per build listing its entries; `prune` skips entries a live
  lease lists: a new file format to keep consistent.
- (c) `prune` refuses while any build holds the store: simple, but it never runs
  on a busy CI host.

## Decision Outcome

A build holds a shared `flock` on each store entry it uses for the whole build,
`ranikust prune` takes it exclusive before removing the entry, and an entry's
last use is the mtime of its marker, touched when a build takes the entry.

The maintainer's answer (2026-11-06: "Bản sao context, như kaniko trên / (khuyến
nghị)", "A copy of the context, as kaniko does on / (recommended)") also covered
the questions of
[ADR-0121](0121-a-runs-bind-mounts-read-one-filtered-copy-of-the-context-copied-in-on-slash.md);
here it stood for option (a).

- **In use.** An entry a running build holds cannot be locked exclusive, so
  `prune` never removes it. The kernel drops a dead build's locks, so a crash
  leaves nothing to clean and no lease to expire.
- **Last use.** The marker's mtime, set when a build takes the entry, so age is
  counted from when an entry was last taken, not when its build ended.
- **Cost.** One descriptor per entry a build uses, held until the build ends.

Not settled here, and decided before Phase 5's task 17
([#157](https://github.com/tran4774/ranikust/issues/157)), which builds `prune`,
is built (a `question` issue if it needs the maintainer): which file the shared
lock is taken on (the per-digest lock files exist already and are never removed,
#1), and what the marker is for a blob, an extracted layer and a cache mount.
[#76](https://github.com/tran4774/ranikust/issues/76) (Phase 3) builds the
in-use mechanism that `prune --all` uses, and #157 uses the same one.

### Consequences

- Good, because it is crash-safe with no format to keep consistent: a dead
  build's locks are gone.
- Good, because `prune` can run on a busy CI host beside builds, which (c)
  forbids.
- Bad, because a build holds one descriptor per entry it uses, so a build with
  many layers needs a high descriptor limit **[inferred]**.
- Bad, because last use is when an entry was taken, so a long build's entries
  look older than its end.
- Neutral, because advisory locks on a network filesystem are weaker; NFS and
  FUSE state directories wait for Phase 6
  ([ADR-0100](0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md)).

## Evidence

- **[inferred]** BuildKit tracks use and age in its daemon's database, which
  ranikust does not have;
  [#137](https://github.com/tran4774/ranikust/issues/137) states this, and it
  was not re-read in BuildKit's source.
- **[code]** The store's locks are `flock`s the kernel releases on death, with a
  T0 test that kills with `SIGKILL`
  ([locks](../design/cache-and-store.md#locks)); per-digest lock files are never
  removed (the interim of #1, Q-015).

## More Information

Answers a Phase 5 question raised while planning, which has no issue number.
Related: [ADR-0006](0006-persistent-store.md) (digest locks and atomic rename),
[ADR-0021](0021-rnk-store-crate.md) (`prune` only calls `rnk-store`),
[ADR-0105](0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md)
(the minimal `prune --all`, which already promises never to remove what a
running build uses, "the mechanism being #76's").

[cache-and-store](../design/cache-and-store.md#prune) already points here.
Follow-ups, with Phase 5's task for `prune`
([#157](https://github.com/tran4774/ranikust/issues/157)) and not before it is
built: the `rnk-store` and `ranikust` cards, the book, the threat model (T13),
and the `## Changelog` entry.
