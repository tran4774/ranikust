# Cache and store

**TL;DR:** how ranikust keeps downloaded and extracted content between
builds, shares it between concurrent builds, and keys and stores its layer
cache.

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
    exit status and the image do not change, apart from ADR-0123's open
    cases [open: #171] [open: #172] (T26).
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
├── .ranikust-state             marker: a non-empty directory without it is refused
├── .ranikust-pruning           the marker renamed while prune --all empties it
├── blobs/sha256/<hex>          compressed blobs (base images, cache layers)
├── layers/v3/sha256/<hex>/     extracted layers by chain ID (and id map), immutable [open: #1]
├── caches/<sha256(id)>/        RUN --mount=type=cache, kept between builds
├── locks/{blobs,layers/v3}/sha256/<hex>   per-digest lock files [open: #1]
├── tmp/<build-id>/<n>          entries being written by one build, renamed into place
├── sandbox/runs/<id>.journal   stub journal of a running RUN, and <id>/ its /etc copies
├── .ranikust-anon-userns-probe-*/  rootless overlay mode's probe, held (flock) while in use
├── builds/<build-id>.lock      held (flock) while the build runs
└── builds/<build-id>/          one per running build, deleted afterwards;
                                stage roots are stages/<n>/rootfs (ADR-0059)
```

The store lives in the state directory: `/var/lib/ranikust` as root in every
mode, `~/.local/share/ranikust` for another user, unless `--state-dir` names
another
([ADR-0050](../adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md)).
It is the private directory of every build root: owned by the user running
ranikust, `0700`, below directories only root or that user may write
([kaniko-mode](kaniko-mode.md#host-safety-checks)). Kaniko mode on `/` keeps
no persistent store, only an anonymous one that leaves the directory empty
([ADR-0077](../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md)).
Rootless overlay mode opens the persistent store inside the build's user
namespace, with `--allow proc.rbind` too, since its `RUN`s reach no other
process's files through `/proc`
([ADR-0130](../adr/0130-with-allow-proc-rbind-the-residual-is-the-measured-one-the-peer-scan-stays-and-the-build-uses-the-persistent-store.md)).
Root's `--userns` does too, in a state directory of its own: owned by the
first ids of root's `ranikust` range, which the build runs as on the host,
never root's own, whose layers are extracted with real ids
([sandbox](sandbox.md#--userns)). Its layers are kept apart from root's by
that directory, and within it by their map, as below.

In a user namespace other than the initial one (a rootless or `--userns`
build, or ranikust started in a rootless container), an extracted layer's key
is the `sha256` of a tag of its own, the layer's chain ID and the namespace's
uid and gid maps as `/proc/self` shows them, sorted: one state directory holds
one copy per map
([ADR-0104](../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md),
#77; two `--userns` maps with the same first ids, T2). The tag puts the keys in a space of their own (`KEY_DOMAIN` in
[`userns.rs`](../../crates/rnk-snapshot/src/userns.rs)). The maps are relative to
the parent namespace, so nested namespaces with the same maps under parents
with different ones share a key, and a build in one sees the owners of a
layer the other extracted through its own maps: the overflow id for ids it
does not map, other owners where the ranges overlap. This happens, for
example, to a namespace ranikust makes in a rootless Podman container whose
state directory is on a volume, after the user's ranges in `/etc/subuid`
change: the moved range of the enclosing container is not seen. A canary per
state directory and map will refuse such layers
([#183](https://github.com/tran4774/ranikust/issues/183),
[ADR-0128](../adr/0128-a-canary-guards-nested-namespaces-a-budget-bounds-device-names-and-a-device-left-out-keeps-the-lower-entry.md)).
`rnk-snapshot` makes the key and the store takes it as it takes a chain ID,
an opaque digest; how
[#1](https://github.com/tran4774/ranikust/issues/1) names a layer must keep
the map.

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
    (`Error::StateNotEmpty`), unless the store holds no data: that one it
    empties first, under its lock, as `Store::abandon` does
    ([ADR-0136](../adr/0136-root-auto-mode-falls-back-to-kaniko-mode-from-a-set-up-that-fails-in-the-builds-user-namespace.md)). It makes `.ranikust-building` its own again after
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

In a build's user namespace (rootless mode, #77), the same steps, with every
operation on extracted layers run inside it as its mapped root, since the store
is opened there and the user cannot rename, `chmod` or unlink entries of
subordinate ids from outside it
([ADR-0099](../adr/0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)):
a killed extraction leaves its entry in `tmp/`, which the next build removes.
Every directory the store creates has a mapped uid and gid, since an unmapped
gid alone stops the rename of a `0555` directory and the `chown` of what is
written below it: `Store::open` gives its own directories the process's gid
and no setgid bit, whatever a setgid state directory or a `grpid` mount gave
them. Outside a user namespace no map needs the gid, so a `chown` the
filesystem refuses (`EPERM`) is a warning and the directory keeps its group
and mode; in one, or where the namespace cannot be told, it is an error, as
is any other failure, a refused `chmod` included (the `chown` before it
needs the same ownership).

Start-up removes the `tmp/<id>` and `builds/<id>` directories whose build is
no longer running. A state directory that is not empty and has no regular
`.ranikust-state` file is refused, so this clean-up never runs in a directory
the user pointed at by mistake. Only the directories of ranikust's own
user-namespace probe (a rootless build cut short before its store opened)
do not count, each a real directory on the state directory's mount, told
without following a link: such a directory is a new store and gets the
marker, and the store removes each whose lock it takes, through
descriptors (a probe holds its own while it uses it). So does every
emptying of the directory (an anonymous store's start and end, a store with
no data emptied, `prune --all`): one a probe holds stays, and keeps a store
with no data as it is. `check_state_dir`,
which rootless overlay mode runs before its probe, keeps the state
directory's shared lock until the store opens. In overlay mode an upper
directory written under `volatile` is never trusted after a crash.

A rootless set-up that fails before its build, where root's `--mode=auto`
falls back to kaniko mode on `/`, ends with `Store::abandon`
([ADR-0136](../adr/0136-root-auto-mode-falls-back-to-kaniko-mode-from-a-set-up-that-fails-in-the-builds-user-namespace.md)):
the store closes, and the directory is emptied where the store holds no
data, whoever made it: nothing in `blobs/`, `layers/` or `caches/`, only
names `prune --all` takes, checked under the lock turned exclusive
without waiting, so never while another build holds it. It is emptied as
`prune --all` empties it, but with `.ranikust-state` removed last in place
of a `.ranikust-pruning`: with no data, nothing a later build reuses can
be left half-deleted, and a set-up killed before or during it leaves a
store with no data, which kaniko mode on `/` empties the same way before
it builds (`Store::open_anonymous`), as the next failed set-up does. An
open that fails once it took the directory empties it the same way
(`Store::open_or_abandon`). A store with data is kept, and kaniko mode
refuses it.

## Prune

`ranikust prune --all` (Phase 3,
[ADR-0105](../adr/0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md))
empties the state directory of a user other than root, files of
subordinate ids included, so the user can then remove it. In order: root
is refused (euid 0 in any user namespace, a container's root included),
naming Phase 5's `prune`; then a path ending in `..`, before anything is
opened; then, outside any namespace, a build's checks of
the state directory, which neither make it nor change its mode, and one
that does not exist, or is empty, is left so; then `Store::check_prune`,
the same read-only check `Store::prune_all` runs, so `prune --all` refuses
there whatever `prune_all` would refuse in the directory, in this order:
this process's own `/`, `.ranikust-building` in any form, a
`.ranikust-pruning` that is not a regular file, a non-empty directory
without a regular marker (`.ranikust-state`, or the `.ranikust-pruning` a
prune that did not finish leaves), and a top-level name neither the store
nor a build makes there (the markers, its directories, `sandbox/`, an
anonymous store's names); then the build's user namespace (where the
user gets none, the refusal gives commands that remove the user's own
files, and root's `prune --all` for those of its subordinate ids); in
it, `Store::prune_all`. That takes the state directory's `flock` exclusive
without waiting, as an anonymous store does, and holds it to the end:
every build holds it shared for its life, so `prune --all` removes
nothing while any build runs there (it says so and exits 1), and a build
that starts meanwhile is refused. Under the lock it runs the check again,
since a build may have changed the directory meanwhile; it creates
nothing in an empty one. Before deleting it syncs the directory with
`.ranikust-pruning` in it, whether it renamed `.ranikust-state` itself or
found one an earlier prune left: entries are emptied in place, under
their published names, so a prune killed or failed part-way leaves a
layer a build would reuse with files missing, and `Store::open` refuses
any directory holding that name until a `prune --all` finishes (a
ranikust that predates it finds the directory unmarked, and refuses it
too); such a build's refusal gives the next step: `ranikust prune --all`
for a user, root's commands for root. The rename makes no inode, which a
full disk or quota, the usual reason to prune, would refuse. A failed
sync renames back only a marker this prune renamed: one an earlier prune
left stays, since that prune may have removed entries. It deletes by
descriptor (no symlink followed, no mount crossed), names what it could
not remove, then syncs the filesystem and removes `.ranikust-pruning`
last, so the marker's removal never reaches the disk without the
deletions; a failed sync keeps it. Where it cannot finish (a mount below,
files of ids no user namespace maps), its refusal names the ways out:
unmount, set up the user's subordinate ids, move the directory aside and
remove it in `podman unshare`, or have root run `ranikust prune --all`,
whose refusal gives root's commands to empty it in place: root's
commands are printed only by root's own process.

The commands a refusal prints (those ways out, root's, and a user's
without a user namespace) act only on the directory ranikust checked:
the process that prints them opens the path as given, made absolute,
with no symlink followed on it, runs `Store::check_prune` on it, then
the build's own rule with the directory's owner as the user, and
refuses one of the system directories (`statedir::check_for_commands`):
commands only for a directory a build would accept
([ADR-0133](../adr/0133-roots-prune-commands-delete-only-the-owners-ids-and-only-in-a-directory-a-build-would-accept.md)),
none for one another user may write, or below one, the refusal then
saying to fix the owner or mode or to remove the directory by hand. It
takes the directory's device, inode and owner, and the mode the rule
judged. Each command first refuses a `PATH` with a relative or empty
entry and unsets the loader's variables that name libraries or modules
by path (`LD_LIBRARY_PATH`, `LD_PRELOAD`, `LD_AUDIT`, `GCONV_PATH`),
stopping where one cannot be unset, since it runs its tools from inside
a directory another user may own (in `eval '…'`, so that no interactive
shell's history expansion reads the pattern's `!`). It then checks, right after its one `cd -P`, that
the directory it acts on has that identity (`stat -c %d:%i:%u:%a`
piped to `grep -qxF`, a hardening: no quoted command substitution for
an interactive shell's history expansion or quoting to touch): `.` for
the emptying, `./name` in the parent it entered for `rmdir` and the
move aside. So a path changed after the message was printed, a parent
swapped for a symlink included, makes it fail before it changes
anything. A path that ends in `..` is refused before anything is
opened, whatever it leads to, by root's refusal (no commands) and a
user's `prune --all` alike, saying to give the directory's own path.
Where root's open of the path refuses it for a symbolic link on it, no
commands are printed either, and the refusal says to give the path
with none and, for a user's directory, to run `ranikust prune --all`
as that user; where the check refuses the directory for any other
reason, the refusal gives that reason instead (a user's own
`prune --all` is refused before, with the check's reason).

Root gets no `rmdir` for an empty directory: only one that holds the
marker is a state directory. Both emptying scripts first check that
`find --version` names GNU findutils and lists `FTS(FTS_CWDFD)`, the
walk they rely on, and otherwise stop before changing anything, saying
so (Alpine: `apk add findutils`); the behaviour below is that `find`'s.
Root's emptying tests for a regular marker, then removes it, and stops
before changing anything, with a message for each, where there is none
or it cannot be removed; each script counts a listing that fails as
entries left.
Root's emptying of a directory another user owns deletes only that
user's entries and those of its subordinate ids
([ADR-0133](../adr/0133-roots-prune-commands-delete-only-the-owners-ids-and-only-in-a-directory-a-build-would-accept.md)):
its uid and its first `/etc/subuid` range (`subids::lookup`), the only
uids its builds' user namespace maps, or its uid alone where it has no
range in `/etc/subuid` and `/etc/subgid` or they cannot be read (the
guidance names the error). Before the marker goes, it looks for an entry of any other uid
(a range changed since the build included) and stops there, with
nothing changed, saying so, also where that search fails; after the
deletion, it checks that nothing is left, and stops before the
`rmdir`, naming the uids it left. A directory root owns has every
entry deleted: root's store holds image ids that may match host
accounts.
In the directory, the emptying names only `.`, so the lock, the
marker's check and the deletion act on it whatever is put at its path
meanwhile; it takes the same exclusive `flock` (`flock -xn .`), so it
fails while a build runs there, takes a marker only as a regular file,
and enters no other filesystem (`find -xdev`); a bind mount of the same
filesystem is not told apart, its contents deleted with the rest, so
the commands say to run only while nothing is mounted below
(ranikust's own deletion compares mount IDs instead).

The rest is Phase 5: `ranikust prune` will call `rnk-store`, which removes
entries by total size and by age, as a build opens the store, taking the
same locks, and never removing an entry a running build uses: decided by
[ADR-0122](../adr/0122-prune-tells-an-entry-in-use-by-a-shared-lock-and-its-last-use-by-its-markers-mtime.md),
not built yet (a shared `flock` on each entry a build uses, and the entry's
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

- **version:** `KEY_VERSION`, bumped whenever the algorithm changes, or
  the [entry form](#cache-entries) a lookup checks; old entries are simply
  never matched again (without the bump, each would be refused with a
  warning).
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

Because keys ignore the mode, a layer cached by one mode can be used by the
other and carry that mode's accepted differences
([ADR-0012](../adr/0012-accept-divergence-between-modes.md)).

## Cache entries

Cached layers live in a registry repository (`--cache-repo`) or an OCI
layout. The registry cache is built (`rnk-cache`'s `RegistryCache`), but no
build uses it yet (Phase 4 tasks 7 and 8); the layout cache is planned
(task 11). A cache hit adds the layer to the lower stack (overlay mode) or
extracts it into the root (kaniko mode). One compressed blob serves both the
image and the cache, and cache pushes run asynchronously.

An entry, under its tag
([ADR-0113](../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)):

- an OCI image manifest, with no `artifactType` or `subject`, holding one
  layer of the OCI media type of the entry's compression;
- three annotations: `io.github.tran4774.ranikust.cache.key` (the key's
  SHA-256 in lower-case hex), `io.github.tran4774.ranikust.cache.version`
  (`KEY_VERSION`, decimal) and `io.github.tran4774.ranikust.cache.encoding`
  (`gzip;level=6;times=kept`, `zstd;level=3;times=clamped;epoch=0`,
  `uncompressed;times=kept`);
- an OCI config holding the push time as `created` (RFC 3339, UTC), the
  layer's diff ID, and the `architecture` and `os` of the platform the
  stage is built for, with its `variant` when it has one. OCI requires the
  first two, which kaniko's entries leave empty
  (`kaniko@44aa7ddc0 pkg/executor/push.go:447`, `pushLayerToCache`), and
  the platform is no secret. A lookup does not check them: the key, whose
  root holds the platform, keeps an entry to one.

A lookup reads the manifest under the tag and then the config, each bounded
by its `--limit` limit (ADR-0079):

- a 404 is a miss, never retried, and needs no warning;
- any other failure (a 5xx after the client's retries, a stall past
  `registry-idle`, bad credentials, a document over its limit) is a miss
  with a warning;
- an entry not of the form above, or naming another key, version or
  encoding, is a miss with a warning: a hostile repository costs misses,
  never a failed build;
- an entry whose `created` plus `--cache-ttl` is past is a miss without a
  warning; a hit does not refresh `created`, and a `created` ahead of the
  clock never expires, as kaniko compares it.

A hit returns the layer's descriptor and diff ID, which prove nothing: the
caller checks the blob against both as it checks a base layer's, and
extracts it under the same rules; in kaniko mode,
[a cache hit](kaniko-mode.md#a-cache-hit) says when a blob found bad leaves
the root broken.

A blob found bad costs a miss with a warning, as a refused entry does
([ADR-0113](../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)),
and, once its extraction began, a restart; not a failed build
([ADR-0123](../adr/0123-a-cache-hit-that-fails-makes-its-stage-start-again-once-with-no-cache.md),
decided and planned in [#130](https://github.com/tran4774/ranikust/issues/130)
and [#132](https://github.com/tran4774/ranikust/issues/132), not built yet).
What a restart does when a layer of its chain is held by neither the store nor a
registry is not decided [open: #171], nor are five more cases of the restart
[open: #172].

- In kaniko mode with a directory per stage a hit streams into the stage root; on
  `/` it is spooled and verified before extraction. A hit found bad before its
  extraction began (the source refused it; on `/`, the spool pass, which also
  checks the tar headers and `--limit`) is a miss and starts nothing again. One
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
  must be a 206 starting at the offset; then the entry is a miss with a
  warning and the stage starts again. On `/`, a resume that still fails during
  the spool is a miss with a warning before `/` changes, and starts nothing
  again. That count does not bound a connection that drops every few bytes;
  the overall pull deadline is
  [#28](https://github.com/tran4774/ranikust/issues/28).
- A bad blob found while pushing the layers of a skipped, fully cached stage
  makes it stop being skipped: it is built with no cache, its layers pushed as
  above. A blob mounted across repositories
  ([ADR-0055](../adr/0055-stream-layers-keep-nothing-proportional-to-the-image.md))
  is not read: it is trusted by its digest and its entry's diff ID.

A push refuses a layer of another compression than its entry's, then sends
the layer blob (skipped when the repository holds it, mounted from a
destination of the same registry, uploaded otherwise), the config, and the
manifest last, so a tag never names an entry whose blobs are missing. Its
failure is a warning; the build's exit status and image do not change.

The cache repository is treated as a destination: the build's registry
client, with its trust and limits; credentials over plain HTTP only to a
registry the operator named, and only over verified TLS in kaniko mode on
`/` (ADR-0051,
[ADR-0077](../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md)).
Only the operator names it, never the Dockerfile.

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
