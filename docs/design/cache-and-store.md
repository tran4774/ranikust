# Cache and store

**TL;DR:** how ranikust keeps downloaded and extracted content between
builds, shares it between concurrent builds, and keys its layer cache.

Decisions: [ADR-0005](../adr/0005-own-versioned-cache-key.md),
[ADR-0006](../adr/0006-persistent-store.md),
[ADR-0007](../adr/0007-run-anywhere-shared-store.md),
[ADR-0012](../adr/0012-accept-divergence-between-modes.md),
[ADR-0021](../adr/0021-rnk-store-crate.md),
[ADR-0077](../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md),
[ADR-0113](../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md),
[ADR-0114](../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md),
[ADR-0116](../adr/0116-the-cache-lives-in-a-registry-or-an-oci-layout-of-one-layout-per-key.md).

## Invariants

- Store entries are content-addressed and immutable once published. A
  published entry is complete: it appears only through an atomic rename.
- Two builds never download or extract the same digest at the same time.
- A crash leaves only temporary entries and build directories, both removed
  on the next start.
- The layer cache (Phase 4), against threats T13 and T25 to T29 of the
  [threat model](../security/threat-model.md#5-threats-and-mitigations),
  whose rows say what is built:
  - Keys and tags name build inputs only, never a secret's value or a
    credential, and never depend on the snapshot mode, the host, the
    build's time or a thread count ([cache keys](#cache-keys), T27).
  - Only the operator names the cache repository or layout; it is trusted
    as the destination is until cache entries are signed (T25).
  - A hit is hostile image content, checked and extracted as a base layer
    is, and every cache document read is bounded by `--limit`. A bad
    entry, or a lookup or push that fails, is a miss or a warning: the
    exit status and the image do not change (T26).
  - A layer is pushed under a key only if its step read what the key names
    (T28).
  - The cache never changes the output: with `--reproducible`, a cold, a
    warm and a partial build in one mode give one digest at any `--jobs`
    (T29).
  - The store holds no cache key, so builds sharing a state directory
    share only content-addressed data; kaniko mode on `/` keeps no local
    cache (T13).

## Store layout

```
<state-dir>/
├── .ranikust-state
├── blobs/sha256/<hex>          compressed blobs (base images, cache layers)
├── layers/v3/sha256/<hex>/     extracted layers by chain ID, immutable [open: #1]
├── caches/<sha256(id)>/        RUN --mount=type=cache, kept between builds
├── locks/{blobs,layers/v3}/sha256/<hex>   per-digest lock files [open: #1]
├── tmp/<build-id>/<n>          entries being written by one build, renamed into place
├── sandbox/runs/<id>.journal   stub journal of a running RUN, and <id>/ its /etc copies
├── builds/<build-id>.lock      held (flock) while the build runs
└── builds/<build-id>/          one per running build, deleted afterwards;
                                stage roots are stages/<n>/rootfs (ADR-0059)
```

The store lives in the state directory: `/var/lib/ranikust` in every mode
unless `--state-dir` names another
([ADR-0050](../adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md)).
It is the private directory of every build root: owned by the user running
ranikust, `0700`, below directories only root may write
([kaniko-mode](kaniko-mode.md#host-safety-checks)). Kaniko mode on `/` keeps
no persistent store, only an anonymous one that leaves the directory empty
([ADR-0077](../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md)).

Planned, Phase 3 (rootless mode and `--userns`, nothing built): an extracted
layer's key includes the digest of the build's id map, so one state directory
holds one copy per map
([ADR-0104](../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md);
how [#1](https://github.com/tran4774/ranikust/issues/1) names a layer must
include the map).

- **Overlay mode:** stage roots are overlay mounts in
  `builds/<id>/stages/<n>/rootfs` ([overlay-snapshot](overlay-snapshot.md)),
  so the state directory must be on a filesystem that can hold an upper
  directory [inferred]; one on overlayfs is refused.
- **kaniko mode on `/`:** the state directory lies inside the root, like
  kaniko's `/kaniko`, so it is on the ignore list and never walked,
  snapshotted or deleted ([kaniko-mode](kaniko-mode.md#ignore-list)).
- **kaniko mode elsewhere:** every stage has a root of its own,
  `builds/<build-id>/stages/<n>/rootfs`, as in overlay mode
  ([ADR-0059](../adr/0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md)).

Extracted layers use overlay's whiteout form (`mknod c 0 0`,
`user.overlay.opaque=y`) so that overlay mode can stack them directly. kaniko
mode cannot: it extracts the blobs into the stage root, or copies a flattened
base kept in the store
([ADR-0017](../adr/0017-stage-rootfs-by-copy-not-hardlinks.md)) [inferred:
the form of a flattened base in the store is not designed yet].

## Locks

All are `flock` on files in the store, released by the kernel when the
process dies (a T0 test kills with `SIGKILL`).

- **Per digest:** taken before downloading a blob or extracting a layer. A
  build that waited finds the entry published and uses it. These lock files
  are never removed, so no build locks a file another has just unlinked.
- **Per build:** each build owns `builds/<build-id>/` and `tmp/<build-id>/`
  and holds `builds/<build-id>.lock` while it runs, taken before either
  directory exists. A build whose lock is free is dead; the next start
  removes its directories.
- **Per state directory**
  ([ADR-0077](../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md)):
  every store locks the state directory itself for its life, before anything
  is written.
  - A persistent store holds it shared, so persistent builds share a state
    directory. It refuses a directory holding `.ranikust-building`
    (`Error::UsedOnSlash`).
  - An anonymous store (kaniko mode on `/`) holds it exclusive: it never runs
    beside another build there (`Error::Busy`), because a `Direct` `RUN`
    writes the state directory as root. It refuses a persistent store
    (`Error::StateNotEmpty`), makes `.ranikust-building` its own again after
    every `RUN` and at its end, and empties the directory at its end, files
    closed first and lock released last. A close that leaves anything is
    `Error::NotEmptied`, which fails the build.
- **kaniko mode on `/`:** `flock(LOCK_EX | LOCK_NB)` on a descriptor of `/`
  itself: a second build on the container's `/` is refused, whatever its
  state directory.

## Atomic publication

1. Under the per-digest lock, return the entry if it is published.
2. Write the new entry under `tmp/<build-id>/<n>` on the same filesystem.
3. Verify it (blob digest; layer extracted completely): the caller does,
   while filling it.
4. `fsync` a blob, then `rename` it to its final name. Extracted layers are
   not synced, as containerd's defaults under BuildKit
   ([ADR-0032](../adr/0032-observable-behaviour-follows-buildkit.md)).

Planned, Phase 3 (rootless mode, nothing built): the same steps, with every
operation on extracted layers run inside the build's user namespace, since the
user cannot rename, `chmod` or unlink entries of subordinate ids from outside it
([ADR-0099](../adr/0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)).
Every directory the store creates has a mapped uid and gid, since an unmapped
gid alone stops the rename of a `0555` directory.

Start-up removes the `tmp/<id>` and `builds/<id>` directories whose build is
no longer running. A state directory that is not empty and has no regular
`.ranikust-state` file is refused, so this clean-up never runs in a directory
the user pointed at by mistake. In overlay mode an upper directory written
under `volatile` is never trusted after a crash.

## Prune

Not built. Phase 3 plans a minimal `ranikust prune --all` that empties a
rootless state directory inside the user namespace and refuses when root runs
it ([ADR-0105](../adr/0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md));
the rest is Phase 5: `ranikust prune` will call `rnk-store`, which removes
entries by total size and by age. Both forms open the store as a build does
(state-directory checks, the `.ranikust-state` marker), so they refuse a
directory ranikust did not make; they take the same locks as a build and never
remove an entry a running build uses (the mechanism is
[#76](https://github.com/tran4774/ranikust/issues/76)'s, in Phase 3; decided by
[ADR-0122](../adr/0122-prune-tells-an-entry-in-use-by-a-shared-lock-and-its-last-use-by-its-markers-mtime.md),
not built yet: a shared `flock` on each entry a build uses, and the entry's
marker mtime as its last use). The defaults are BuildKit's tiers, aging by last
use ([ADR-0032](../adr/0032-observable-behaviour-follows-buildkit.md)): cache
mounts and contexts after 48 h unused beyond 512 MB, anything after 60 days
unused, and size caps of 10% reserved, 20% minimum free, 80% maximum used.

## Cache keys

The keys are hashed in `rnk-cache`; `rnk-builder`'s key plan computes every
step's key before any stage runs
([ADR-0114](../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)),
but no build looks them up yet (Phase 4). Each step's key is a SHA-256
over a version field and the inputs the step depends on, chained to the
previous key (ADR-0005; the fields are
[ADR-0113](../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)'s,
BuildKit's):

```
key(k) = H(version, key(k-1), instruction, inputs(k))
```

- **version:** `KEY_VERSION`, bumped whenever the algorithm changes; old
  entries are simply never matched again.
- **chain root:** the base image's platform manifest digest (none for
  `FROM scratch`) and the platform; for `FROM <stage>`, that stage's final
  key. Every step is looked up even after a miss, so the chain alone does
  not stop reuse: the caller treats a miss in the base stage as a miss for
  every step built on it (`StageKeys::base_stage`, ADR-0114's stop rule: no
  cached layer on a lower it was not made on).
- **instruction:** the instruction after variable expansion, with its flags.
- **inputs:**
  - every declared `ARG` and the environment in effect; `USER`, `WORKDIR`
    and `SHELL`; an owner name as written, after expansion, never the ids
    it resolves to (the `USER` that owns what a `WORKDIR` creates, a
    `--chown`): it resolves in the stage's root, which under the stop rule
    the previous key fixes, and which does not exist before the build
    ([ADR-0124](../adr/0124-a-chown-owner-is-keyed-as-written-and-the-chain-fixes-the-root-it-resolves-in.md));
  - for a `RUN`, its network as BuildKit keys it (its `--network`, or
    `none` for one without it under the build's `--network=none`), each
    secret mount's id, target, mode and owner, given or not (below), and
    each cache mount's id and sharing, marked as dropped where
    [ADR-0057](../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md)
    drops it;
  - for `COPY` and `ADD` from the build context, each file read (after
    `.dockerignore`): its path relative to the context, content, mode, owner
    (`0:0`, the `--chown` being in the instruction, ADR-0124), type, link
    target,
    `security.capability` and, under `--reproducible`, its mtime clamped to
    the epoch, so only the mtime of a file older than the epoch counts;
  - for `COPY --from=<image>`, the image's platform manifest digest; for
    `COPY --from=<stage>`, the source stage's final key (a forced miss when
    that stage ran in this build,
    [ADR-0114](../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)).

  The instruction holds no heredoc body: each one (of a `RUN`, `#!` or
  not, a `COPY` or an `ADD`) is an input of its own, after the expansion
  BuildKit does. An `ADD` of a local archive is keyed as one file, the
  archive, as BuildKit keys it, though the build refuses it until Phase 5
  ([ADR-0125](../adr/0125-cache-copy-layers-covers-copy-and-a-local-add.md)):
  no build pushes a layer under that key before then, and a change to how
  an archive is extracted is keyed or bumps `KEY_VERSION`.
- **never:** the snapshot mode, the host, its users or id maps, the time,
  an mtime outside `--reproducible`, thread counts, the order of an
  unordered collection.

`ADD <url>` and `RUN --mount=type=bind` get their keys with those Phase 5
features; until then, as for any step the plan cannot key as the build runs
it, that step and every step after it have no key: a miss. How the fields
are written into the hash:
[the `rnk-cache` card](../agents/crates/rnk-cache.md#byte-form).

A secret mount is keyed whether or not the build is given the secret, as
BuildKit, which builds the mount from the Dockerfile alone and hashes the
whole exec op (ADR-0032;
`buildkit@cd8ee244 frontend/dockerfile/dockerfile2llb/convert_secrets.go:14-90`,
`solver/llbsolver/ops/exec.go:114-179`).

A cached layer is looked up and pushed under a tag, `rnk-v<version>-<hex>`,
whose `<hex>` hashes the key with the layer's encoding: the compression, its
level (none for a plain tar), and whether file times are kept or clamped to
an epoch, with the epoch. A hit was then encoded with this build's settings,
so under `--reproducible` a warm build gives a cold build's digests when the
layers it reuses were cached by a build in the same mode (below); builds
with another encoding share no entries
([ADR-0113](../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)).
The cold, warm and partial cache scenarios of workload W2 test the keys.

Cached layers live in a registry repository (`--cache-repo`) or an OCI
layout. A cache hit adds the layer to the lower stack (overlay mode) or
extracts it into the root (kaniko mode). One compressed blob serves both the
image and the cache, and cache pushes run asynchronously.

A bad cache entry costs a miss and a restart, not a failed build
([ADR-0123](../adr/0123-a-cache-hit-that-fails-makes-its-stage-start-again-once-with-no-cache.md),
decided and planned in [#130](https://github.com/tran4774/ranikust/issues/130)
and [#132](https://github.com/tran4774/ranikust/issues/132), not built yet).
What a restart does when a layer of its chain is held by neither the store nor a
registry is not decided [open: #171], nor are five more cases of the restart
[open: #172].

- In kaniko mode with a directory per stage a hit streams into the stage root; on
  `/` it is spooled and verified before extraction. A hit found bad before its
  extraction began (the source refused it; on `/`, the spool pass, which will also
  check the tar headers and `--limit`,
  [#174](https://github.com/tran4774/ranikust/issues/174)) is a plain miss. One
  found bad after extraction began (on `/`, only a failure while writing) makes
  the stage start again from its base
  ([ADR-0113](../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)),
  at most once and with no cache at all: every step runs, and its layers are
  pushed under their keys as a cold build pushes them, only the steps the cache
  flags cache.
- A restart always rebuilds its base by extracting the layers of its chain by
  digest, from the store, else from the registry that holds them, whether or not
  the parent's directory is still there.
- A connection dropped after the first byte of a hit is re-requested from the
  offset with a Range request, about 3 attempts without progress, and the answer
  must be a 206 starting at the offset; then the entry is a miss and the stage
  starts again. On `/`, a resume that still fails during the spool is a plain
  miss. That count does not bound a connection that drops every few bytes; the
  overall pull deadline is [#28](https://github.com/tran4774/ranikust/issues/28).
- A bad blob found while pushing the layers of a skipped, fully cached stage
  makes it stop being skipped: it is built with no cache, its layers pushed as
  above. A blob mounted across repositories
  ([ADR-0055](../adr/0055-stream-layers-keep-nothing-proportional-to-the-image.md))
  is not read: it is trusted by its digest and its entry's diff ID.

Because keys ignore the mode, a layer cached by one mode can be used by the
other and carry that mode's accepted differences
([ADR-0012](../adr/0012-accept-divergence-between-modes.md)).

## Ownership

- **`rnk-store`** ([ADR-0021](../adr/0021-rnk-store-crate.md)): the layout of
  `blobs/`, `layers/`, `caches/` and `tmp/`, per-digest locks, atomic
  publication, start-up clean-up, prune, `BuildPaths`.
- **Filling an entry** is the caller's job; `rnk-store` publishes it. A blob:
  `rnk-registry` streams it into a temporary entry and verifies its digest. A
  layer: `rnk-snapshot` chains `rnk-layer`'s decoder (gzip or zstd to a plain
  tar stream) into `rnk-rootfs::extract`, which writes the overlay whiteout
  form; `rnk-rootfs` does not depend on `rnk-layer`.
- `rnk-snapshot` uses `layers/` as lower directories; the `mount(2)`
  fallback's links live in the stage directory
  ([overlay-snapshot](overlay-snapshot.md#mounting)).
- `caches/<id>`: the directory comes from `rnk-store`, the bind mount from
  `rnk-sandbox`.
- Cache keys, lookup and push: `rnk-cache`; encoding: `rnk-layer`;
  `ranikust prune`: the binary, calling only `rnk-store`.

## Failure handling

- A digest mismatch on download discards the temporary file and fails the
  pull; nothing is published.
- A store on a filesystem without `flock` support is a start-up error that
  asks for a local `--state-dir`.
- Anything but the store's own entry under an entry's name (a symlink, the
  wrong type, a mount) is an error, never followed or replaced. Clean-up
  refuses to empty a mount point or a directory that changed between its
  `statx` and its opening.
- A filler that fails leaves nothing: its temporary entry is removed and the
  lock released. A build killed while filling leaves `tmp/<id>`, which the
  next start removes.

## Open questions

- [Q-015](https://github.com/tran4774/ranikust/issues/1) Which digest names an extracted layer, and where do locks live?
- [Q-016](https://github.com/tran4774/ranikust/issues/2) How is the store lock protocol proven under crashes and races?
- [Q-018](https://github.com/tran4774/ranikust/issues/4) Where does the probe mount its test overlay?

## References

- [Survey 05](../research/survey/05-decisions.md) Q3-Q5;
  [survey 06](../research/survey/06-overlay-and-parallelism.md) §2, §6;
  [survey 02](../research/survey/02-kaniko-mode-requirements.md) §2 (state
  directory).
- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md) §5:
  per-layer parallel extraction of the `node` base in 0.37 s against 0.88 s
  in kaniko.
- [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md) §4:
  state files inside the walked root leaked into the diff.
