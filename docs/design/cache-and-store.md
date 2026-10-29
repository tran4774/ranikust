# Cache and store

How ranikust keeps downloaded and extracted content between builds, shares it
between concurrent builds, and keys its layer cache.

Decisions: [ADR-0005](../adr/0005-own-versioned-cache-key.md),
[ADR-0006](../adr/0006-persistent-store.md),
[ADR-0007](../adr/0007-run-anywhere-shared-store.md),
[ADR-0012](../adr/0012-accept-divergence-between-modes.md),
[ADR-0021](../adr/0021-rnk-store-crate.md),
[ADR-0022](../adr/0022-common-state-dir.md).

## Invariants

- Store entries are content-addressed and immutable once published. A
  published entry is complete: it appears only through an atomic rename.
- Two builds never download or extract the same digest at the same time.
- A crash leaves only temporary entries and build directories, both removed
  on the next start.
- Cache keys depend only on build inputs, never on the snapshot mode, the
  host, the time or the thread count.

## Store layout

```
<state-dir>/
├── .ranikust-root
├── blobs/sha256/<hex>          compressed blobs (base images, cache layers)
├── layers/sha256/<hex>/        extracted layers by diff ID, immutable [open: Q-015]
├── l/<short> -> ../layers/<id> overlay mode, mount(2) fallback only
├── caches/<sha256(id)>/        RUN --mount=type=cache, kept between builds
├── locks/{blobs,layers}/sha256/<hex>   per-digest lock files [open: Q-015]
├── tmp/<build-id>/<n>          entries being written by one build, renamed into place
├── sandbox/runs/<id>.journal   stub journal of a running RUN, and <id>/ its /etc copies
├── builds/<build-id>.lock      held (flock) while the build runs
└── builds/<build-id>/          one per running build, deleted afterwards
```

The store lives in the state directory, `--state-dir`, in every mode
([ADR-0022](../adr/0022-common-state-dir.md)); the flag replaces the
default in either mode.

- **Overlay mode**: default `<workdir>/.ranikust`. Stage roots are overlay
  mounts in `builds/<id>/stages/<n>/rootfs` ([overlay-snapshot](overlay-snapshot.md)),
  so the state directory must be on a filesystem that can hold an upper
  directory [inferred].
- **kaniko mode**: default `/ranikust/state`, the counterpart of kaniko's
  `/kaniko`. The workdir is the root filesystem being walked (`/` in a
  container), so the state directory is on the kaniko-mode ignore list and is
  never walked, snapshotted or deleted ([kaniko-mode](kaniko-mode.md#ignore-list)).
  kaniko mode also keeps per-build `deps/` (files for `COPY --from`) and
  stage images there.

Extracted layers use overlay's whiteout form (`mknod c 0 0`,
`user.overlay.opaque=y`) so that overlay mode can stack them directly.
kaniko mode cannot use them as they are: it extracts the blobs into the
stage root, or copies a flattened base kept in the store
([ADR-0017](../adr/0017-stage-rootfs-by-copy-not-hardlinks.md)) [inferred: the
form of a flattened base in the store is not designed yet].

## Locks

- **Per digest**: before downloading a blob or extracting a layer, a build
  takes the lock for that digest. A build that waited finds the entry
  published and uses it.
- **Per build**: each build owns `builds/<build-id>/` and
  `tmp/<build-id>/`; nothing else writes there. It holds the lock on
  `builds/<build-id>.lock` while it runs, taken before either directory
  exists; a build whose lock is free is dead, and the next start removes
  its directories.
- **kaniko-mode workdir**: `flock(LOCK_EX | LOCK_NB)` on the workdir; a
  second build on the same workdir is refused. Only kaniko mode takes this
  lock, because there the workdir is the root being built. Overlay-mode
  builds that share a workdir share its store through the per-digest locks.
- Locks are `flock` on lock files in the store, released by the kernel when
  the process dies (T0 test with `SIGKILL`). Per-digest lock files are never
  removed, so no build ever locks a file another has just unlinked.

## Atomic publication

1. Under the per-digest lock, return the entry if it is published.
2. Write the new entry under `tmp/<build-id>/<n>` on the same filesystem.
3. Verify it (blob digest; layer extracted completely): the caller does,
   while filling it.
4. `fsync` a blob [open: Q-100], then `rename` it to its final name.

Start-up removes the `tmp/<id>` and `builds/<id>` directories whose build
is no longer running. A state directory that is not empty and has no
regular `.ranikust-root` file is refused, so this clean-up never runs in a
directory the user pointed at by mistake. In overlay mode an upper directory written under
`volatile` is never trusted after a crash.

## Prune

`ranikust prune` calls `rnk-store`, which removes entries by total store size
and by age. It takes the same locks as a build and never removes an entry a
running build uses [inferred: the in-use tracking is not designed yet].
Defaults and the age measure (last use or creation) are [open: Q-043].

## Cache keys

Each instruction's key is a hash over a version field and the inputs the
instruction depends on, chained to the previous key:

```
key(k) = H(version, key(k-1), instruction, inputs(k))
```

- **version**: bumped whenever the algorithm changes; old entries are simply
  never matched again.
- **chain root**: the resolved base image digest for the target platform
  [inferred].
- **instruction**: the instruction after variable expansion, with the build
  arguments and environment in effect [inferred].
- **inputs**: for `COPY`/`ADD`, the digests of the files read from the
  context (after `.dockerignore`) or from the source stage; for
  `RUN --mount`, the mount specifications [inferred].
- **never**: the snapshot mode, the host, timestamps, thread counts.

The exact field list is fixed in phase 4, with the cold, warm and partial
cache scenarios of workload W2.

Cached layers are stored in a registry repository (`--cache-repo`) or an OCI
layout. A cache hit adds the layer to the lower stack (overlay mode) or
extracts it into the root (kaniko mode). One compressed blob serves both the
image and the cache, and cache pushes run asynchronously (kaniko optimization
notes, docs/19, external, not in this repository).

Because keys ignore the mode, a layer cached by one mode can be used by the
other and carry that mode's accepted differences
([ADR-0012](../adr/0012-accept-divergence-between-modes.md)).

## Ownership

- **`rnk-store`** ([ADR-0021](../adr/0021-rnk-store-crate.md)): the layout of
  `blobs/`, `layers/`, `caches/` and `tmp/`, the per-digest locks, atomic
  publication, start-up clean-up, prune, and `BuildPaths`.
- **Filling an entry** is the caller's job; `rnk-store` publishes it:
  - a blob: `rnk-registry` streams it into a temporary entry and verifies
    its digest;
  - a layer: `rnk-snapshot` chains `rnk-layer`'s decoder (gzip or zstd to a
    plain tar stream) into `rnk-rootfs::extract`, which writes the overlay
    whiteout form. `rnk-rootfs` does not depend on `rnk-layer`.
- Using `layers/` as lower directories, and the `l/<short>` names of the
  `mount(2)` fallback: `rnk-snapshot`.
- `caches/<id>` for `RUN --mount=type=cache`: the directory comes from
  `rnk-store`, the bind mount from `rnk-sandbox`.
- Cache keys, lookup and push: `rnk-cache`; encoding: `rnk-layer`.
- `ranikust prune`: the binary, calling only `rnk-store`.

## Failure handling

- A digest mismatch on download discards the temporary file and fails the
  pull; nothing is published.
- A lock that cannot be taken because the store is on a filesystem without
  `flock` support is a start-up error that asks for a local `--state-dir`.
- A filler that fails leaves nothing: its temporary entry is removed and
  the lock released; a build killed while filling leaves `tmp/<id>`, which
  the next start removes.

## Open questions

- [Q-036](../open-questions.md#q-036-what-is-the-state-directory-default-for-kaniko-mode-on-a-host) What is the state directory default for kaniko mode on a host?
- [Q-018](../open-questions.md#q-018-where-does-the-probe-mount-its-test-overlay) Where does the probe mount its test overlay?
- [Q-016](../open-questions.md#q-016-how-is-the-store-lock-protocol-proven-under-crashes-and-races) How is the store lock protocol proven under crashes and races?
- [Q-043](../open-questions.md#q-043-what-are-the-prune-defaults) What are the prune defaults?
- [Q-042](../open-questions.md#q-042-how-do-cache-lookahead-and-cached-stage-skipping-work) How do cache lookahead and cached-stage skipping work?

## References

- [Survey 05](../research/survey/05-decisions.md) Q3–Q5;
  [survey 06](../research/survey/06-overlay-and-parallelism.md) §2, §6;
  [survey 02](../research/survey/02-kaniko-mode-requirements.md) §2 (state
  directory), §4 (workdir lock).
- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md) §5:
  per-layer parallel extraction of the `node` base in 0.37 s against 0.88 s
  in kaniko.
- [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md) §4:
  state files inside the walked root leaked into the diff.
