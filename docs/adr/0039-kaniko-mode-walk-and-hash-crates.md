---
status: proposed
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0039: The kaniko-mode walk uses rayon over `getdents64` and hashes with xxh3-128

## Context and Problem Statement

The kaniko-mode snapshot walks the root in parallel and hashes every
changed regular file
([kaniko-mode](../design/kaniko-mode.md#walk-hash-and-diff)). Survey 03 §3
left two picks open (Q-038): the content hash, `highway` (HighwayHash, as
kaniko) or `xxhash-rust` (xxh3), and the walker, `jwalk` or `rayon` with
our own `getdents64` loop. The maintainer asked for a benchmark before
choosing (Phase 2 task 0).

## Considered Options

- Hash: `highway` 1.3.0; `xxhash-rust` 0.8.19 (xxh3); `sha2` (already a
  dependency), as a reference.
- Walk: `jwalk` 0.9.0; `rayon` 1.12 with directories opened by `openat2`
  relative to their parent and read with `getdents64` through
  `rustix::fs::RawDir` (rustix is already a dependency).

`jwalk` 0.9.0 is marked unmaintained on crates.io ("Use `dua-core`
instead"), and it opens every directory by its full path
(`jwalk-0.9.0/src/lib.rs:453`, `fs::read_dir(path)`): it cannot open a
directory relative to its parent with `RESOLVE_BENEATH | NO_SYMLINKS |
NO_XDEV`, as the design's invariants require, and it fails on trees deeper
than `PATH_MAX`. It is measured only for comparison.

## Decision Outcome

Proposed: the walk is our own, a `rayon` scope with one task per directory
over `openat2` and `getdents64` (`rustix::fs::RawDir`), capped at 8
workers; the content hash is xxh3-128 from `xxhash-rust` (feature `xxh3`).

- The hash never leaves the process: it only tells whether a file changed
  between two walks of one build, so it needs no compatibility with
  kaniko's HighwayHash and no cryptographic strength. 128 bits keep
  accidental collisions out of the question over millions of files.
- 8 workers is the default cap: 16 and 32 were slower on every hash.
- New dependencies: `rayon` (MIT OR Apache-2.0) and `xxhash-rust`
  (BSL-1.0, already allowed in `deny.toml`), both pure Rust, so the musl
  and aarch64 builds need nothing new (`ADR-0009`, `ADR-0020`).

### Consequences

- Good, because the walk that meets the safety invariants is also the
  fastest one measured: 0.067 s for 265k entries at 8 workers without
  hashing, 0.140 s with xxh3, against jwalk's 0.073 s and 0.170 s.
- Good, because xxh3 is 2.5 times as fast as HighwayHash on one thread
  (29.8 against 11.7 GiB/s), which matters on CI runners with few CPUs:
  0.589 against 0.722 s for the whole root on one worker.
- Bad, because we own the walker's code (about 200 lines in the
  prototype), its error handling and its tests.
- Neutral, because at 8 workers the hash barely matters (0.140 s xxh3,
  0.153 s highway): the walk is bound by `statx`, `openat2` and `read`.

## Evidence

[`tests/bench/walk-hash/results/2026-10-30.md`](../../tests/bench/walk-hash/results/2026-10-30.md),
run by `tests/bench/walk-hash/run.sh`: experiment 09's node rootfs rebuilt
(265,270 entries, 2,544 MB), Ryzen 9 9950X3D, ext4 on NVMe, page cache
warm, 7 interleaved rounds, median (min–max), seconds per full walk:

| Workers | fd, no hash | fd, xxh3 | fd, highway | jwalk, no hash | jwalk, xxh3 |
|---:|---:|---:|---:|---:|---:|
| 1 | 0.247 | 0.589 | 0.722 | 0.306 | 0.699 |
| 4 | 0.086 | 0.204 | 0.222 | 0.092 | 0.204 |
| 8 | 0.067 | 0.140 | 0.153 | 0.073 | 0.170 |
| 16 | 0.072 | 0.192 | 0.196 | 0.070 | 0.198 |
| 32 | 0.074 | 0.193 | 0.203 | 0.076 | 0.205 |

Hash alone on in-memory files, GiB/s: xxh3 29.8 (1 thread), 54.4 (8);
highway 11.7, 48.7; sha256 2.6, 17.2.

- [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md):
  a rayon + xxh3 prototype walked 269k entries in 0.10–0.15 s with the stat
  cache; 32 workers were slower than 8.

Not measured [inferred]: a cold page cache and slow disks (Q-033), the
static musl binary, aarch64, and xattr reads; none favours one candidate
over another as far as known.

## More Information

Answers [Q-038](../open-questions.md#q-038-which-crates-fill-the-phase-2-dependency-picks)
once accepted, and completes the Phase 1 dependency picks of
[ADR-0030](0030-phase-1-dependency-picks.md). Design:
[kaniko-mode](../design/kaniko-mode.md#walk-hash-and-diff). Revisit the
worker cap with a cold-cache measurement (Q-033).
