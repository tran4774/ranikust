# kaniko mode

How ranikust builds when overlay is not available: on `/` in a container
one root filesystem, emptied safely between stages; anywhere else a
directory per stage in the state directory (ADR-0050, ADR-0059). Each
layer is found by walking and hashing the root. Also covers the opt-in
`--parallel-stages` path with its fake `/proc`.

Decisions: [ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md),
[ADR-0017](../adr/0017-stage-rootfs-by-copy-not-hardlinks.md),
[ADR-0018](../adr/0018-no-proot-in-1-0.md),
[ADR-0019](../adr/0019-java-tool-options-on-parallel-path.md),
[ADR-0022](../adr/0022-common-state-dir.md),
[ADR-0023](../adr/0023-mode-flag.md),
[ADR-0038](../adr/0038-kaniko-mode-settings-for-phase-2.md),
[ADR-0059](../adr/0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md).

## Variants

| Variant | When | Stage root | `RUN` | Stages |
|---|---|---|---|---|
| **Sequential** (default in a container) | root in a container, cannot mount (Docker default, ordinary pods) | `/` | directly (`rnk-sandbox`'s `Isolation::Direct`), with the container's real `/proc`, `/dev` and network files | one at a time, `--jobs` ignored |
| **Per-stage, sandboxed** | root outside a container (it must be able to mount: a host where root may not mount is refused, Q-330, `ADR-0050`, so the `chroot` path of `ADR-0043` is not used) | `<state>/builds/<id>/stages/<n>/rootfs`, one per stage (ADR-0059) | full sandbox ([sandbox](sandbox.md)) | DAG, `--jobs` |
| **Per-stage, bare chroot** | as Sequential, plus `--parallel-stages` | one directory per stage | bare `chroot`, no `/proc` | DAG, `--jobs` |

All variants share the walk, the diff, the exporter and safe deletion. They
implement the `StageRootfs` trait of `rnk-snapshot`: `/` (`Kaniko`, shared,
the scheduler runs one stage at a time) or a private directory per stage
(`KanikoStages`, scheduled like overlay mode).

## Invariants

- Every path inside the root is reached through `rnk-rootfs`'s `RootPath`
  (`openat2` with `RESOLVE_IN_ROOT`); symlinks and `..` cannot escape.
- The walk and the deletion never follow symlinks and never cross a mount
  point.
- The state directory is never walked, snapshotted or deleted; otherwise its
  files leak into layers. With the root `/` (in a container) it is
  `/var/lib/ranikust` (or `--state-dir`), inside the root and on the ignore
  list, as kaniko's `/kaniko`; off `/` every stage root lies inside it,
  `<state>/builds/<id>/stages/<n>/rootfs` (ADR-0050, ADR-0059).
- On `/`, a `RUN` reaches nothing of ranikust's ([ADR-0051](../adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)):
  - the store keeps nothing with a path: `Store::open_anonymous`; pulled
    blobs and encoded layers are anonymous files on disk, held by
    descriptor and named `/proc/self/fd/<n>` to ranikust's own code
    ([ADR-0056](../adr/0056-kaniko-mode-state-on-disk-behind-a-run-fence.md)):
    `O_TMPFILE` in the state directory, else (a container's overlayfs
    root, kernels 6.8 and 5.15, measured) a file made by a random name and
    unlinked at once, only while no `RUN` process exists: the store's
    fence keeps names and `RUN`s apart (`Store::enter_run` and
    `Store::leave_run` around each `RUN`, `Store::named_window` for a
    name); a `RUN` that ended with an error once its command may have run
    poisons it, and a start on the traces of a build cut short in this
    container (its marker `.ranikust-building`, holding the PID namespace
    and boot id, or names left with none) makes no name, so a `COPY
    --from` a kept stage or an image fails [open: Q-360]; `memfd` only
    then, or when no file can be made on disk. After each `RUN` the state
    directory's path must still lead, with no link, to the directory held. The state directory holds no name while a `RUN` runs;
    what must outlive the emptying of the root (a kept stage) is a tar in
    an anonymous file (`Store::scratch_file`);
  - ranikust is not dumpable: `/proc/<ranikust>/{fd,environ,mem,maps,root}`
    need `CAP_SYS_PTRACE` (a root `RUN` still lists descriptor numbers);
  - the `RUN` child drops every capability outside Docker's defaults from
    its bounding set (`CAP_SETPCAP`); without it, any capability outside
    them stops the `RUN`; the ambient, inheritable, permitted and
    effective sets go to the defaults in every case; ranikust's own soft
    `RLIMIT_CORE` is 0;
  - registry credentials only over verified TLS: a `RUN` may edit
    `/etc/hosts` and `/etc/resolv.conf`;
  - kaniko mode on `/` where root may mount, or where `/proc/sys` is not
    a read-only mount, is refused ([ADR-0054](../adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md)); devices passed with
    `--device` stay the operator's choice;
  - the `RUN` gets ranikust's original soft `RLIMIT_CORE` back, as it gets
    `RLIMIT_NOFILE`;
  - ranikust is a child subreaper (`Sandbox::start_direct`); after each
    `RUN` it kills, through a pidfd after checking the parent and start
    time again, everything that became its child during the `RUN` and
    their descendants, and reaps them by pid without blocking, never
    `waitpid(-1)`; where seccomp refuses `pidfd_open`, a plain `kill` for
    its own children only (a pid stays theirs until reaped), deeper
    processes on the next rounds, once reparented to ranikust;
  - warnings for what it cannot close: a `--build-arg NAME=value` whose
    name looks like a secret (`/proc/<pid>/cmdline`), another process
    holding `DOCKER_AUTH_CONFIG` in its environment.
- Anything ranikust places in a root (stubs, `hosts`, `resolv.conf`, `/dev`
  nodes, fake `/proc`) is removed, and its parent directory's metadata
  restored, before the diff.

## Sequential flow

Per stage, as in kaniko: extract the base into the root, take the baseline
walk, then for each instruction run it, walk and diff, and record the layer.
At the end of the stage, keep what later stages need (files for
`COPY --from`, the stage image when another stage builds `FROM` it) in the
state directory, then empty the root. Sequential mode keeps kaniko's stage
squashing, which folds a parent stage into the child that builds on it
[inferred: survey 06 §4.1 drops squashing only for overlay mode]:
`rnk_dockerfile::squash_for_shared_root`, from `kaniko@44aa7ddc0
pkg/dockerfile/dockerfile.go:297-415`. A base folds into its only child
when no `COPY --from` or `RUN --mount from=` reads it and it is not the
result; chains fold whole. Each unit lists what later units read from it
(`readers`: `COPY --from` sources, `RUN --mount from=` roots) and whether
its image is kept for a later `FROM` (`save_image`); both are kept before
the root is reused (below). `rnk-sched` runs the units one at a time, in
plan order (`Placement::Shared`).

## Stage roots on the shared root

`rnk_snapshot::Kaniko` is the kaniko-mode `Snapshotter`, `KanikoRoot` its
`StageRootfs`, on the traits as [ADR-0046](../adr/0046-snapshotter-signature.md)
settles them (Q-024):

- **One root, one holder.** `Kaniko` holds the root as a `RootPath` opened
  once by `ranikust`'s set-up: `/` in a container, else the directory it
  made in the state directory; never a path opened again. At most one `KanikoRoot` holds
  it at a time; `prepare(Base::Stage(parent))` takes it over from `parent`
  when nothing emptied the root since (a squashed chain), and fails
  otherwise.
- **Base layers are blobs.** `base_layer` records the blob in the store and
  extracts nothing: the root is the only copy. `prepare` decodes each base
  layer into the root with `rnk_rootfs::extract_live`, checking its diff
  ID as overlay mode does.
- **When the root is emptied** follows kaniko on `/` (`kaniko@44aa7ddc0
  pkg/executor/build.go`): the first base goes over what is there
  (`:632-641`; kaniko deletes first only with `--pre-cleanup`,
  `:1944-1947`), every later base starts from an emptied root (kaniko's
  `DeleteFilesystem` between stages, `:2179`), and the end of the build
  leaves `/` as it is (kaniko empties it only with `--cleanup`,
  `:1955-1957`; ranikust refuses `--cleanup`). A root in the state
  directory, made for the build, is emptied before the first base too, and
  at the end of a build in which a stage changed it (`Kaniko::finish`),
  then removed with the build directory; a build that fails before its
  first base touches neither.
- **A build on `/` never inherits an earlier one** (ADR-0052, D1). At the
  first base on `/` (under the hold and the lock of `/`, ADR-0050), before
  anything is extracted, `new_base` looks for `/ranikust/.built`: found,
  the first base empties `/` (the ignore list kept) as every later base
  does, credential files outside `/ranikust`, mounts and the ignore list
  included (read at start-up, so that build still has them; a third build
  does not), so a second build in the same CI container ships nothing the
  first left; missing, the first base goes over `/` as kaniko's, so the
  container's own tools stay. Either way the marker is made then: a build
  that stops before its first base (a typo, a failed pull) leaves none,
  one killed during the extraction leaves it. `/ranikust` is made, or
  set, `0700` (a non-root process of the container loses access to it);
  it must be a directory on `/`'s own mount, so a symlink or
  a volume there (which could outlive the container and empty a later
  one's `/`) is refused. It is opened once and checked against its name,
  and the marker is looked up and made (`O_CREAT|O_EXCL|O_NOFOLLOW`)
  relative to that descriptor without leaving its mount; anything under
  the marker's name, also one made in between, counts as a marker. It is not ADR-0056's
  `.ranikust-building` in the state directory, which marks a build in
  progress for the store and is removed by a clean close: this one stays
  for the container's life. Builds in one container trust one another: a
  `RUN` runs as root on `/` and can delete the marker, as it can replace
  anything under `/ranikust`.
- **The ignore list** is built once per build from `/proc/self/mountinfo`,
  `--ignore-path` and the state directory
  (`rnk_snapshot::kaniko::ignore_list`), with physical paths: `/` or the
  root the set-up made, and the state directory's canonical path, opened
  without following a symlink; the state directory is on it only when it
  lies inside the root (`/`).
  It is used by extraction, the walk and deletion alike. A state directory
  outside the root by path that is a directory of the root reached through
  another mount (a bind mount of a directory inside it, found by the
  mounts' device and `fs_root` in mountinfo), or one that holds the root
  that way, is refused, since the list knows it by path only.
- **The operator's paths stay out of the root.** The context, the outputs
  (`--tar-path`, `--oci-layout-path`, the digest files), `--metrics-file`
  and file secrets
  are refused before the root changes when they lie in it, unless the
  ignore list keeps them (a volume, `/ranikust`, `--ignore-path`).
- **A root off `/` is private** (ADR-0047's rule, moved to the state
  directory by ADR-0050): an image's setuid files land in it, so it lies
  below the state directory, owned by the user running ranikust, mode
  `0700`, every parent root's and not writable by others (root's sticky
  directories excepted), walked by descriptor before anything is created.
  Each emptying gives the root its own owner and mode back. The sandbox is
  started with the root's descriptor and refuses a `RUN` on any other path
  or directory. (The binary builds off `/` in per-stage directories since
  ADR-0059: each `0700` below the same checked chain, the sandbox started
  as for overlay mode's mount points.) Kaniko mode on `/` takes `flock` on
  a descriptor of `/` itself: one build per container, whatever the state
  directories.
- **Layers.** A stage whose layers are needed (`set_layers_needed`, from the
  lazy-snapshot rule) takes a baseline walk after its base, then
  `end_layer` walks and diffs, and the diff, held with the root, is the
  layer source; `rnk-builder` encodes it before the next instruction, into
  a temporary blob that is dropped on error (`LayerExporter::export`). An
  unchanged `RUN` gives an empty tar, which the builder drops: the step
  adds only its history entry (ADR-0058). A stage whose layers are not needed
  is never walked: `end_layer` returns an empty diff that is never encoded.
  `begin_layer` and `view` do nothing but hand out the root.
- **Release** gives the root up; emptying (`rnk_rootfs::delete_contents`
  with the ignore list, so the state directory and mounts stay, then
  `Walker::forget`) happens at the next base or at the
  end, as above.
- **A vanished or swapped entry fails the walk** (`Error::Changed`),
  directories and files alike, as kaniko's walk fails on a callback or
  hash error (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1688-1691`,
  `pkg/snapshot/layered_map.go:160-163`): nothing may change the root while
  it is walked.
- **`RUN`** gets the full sandbox on a root in the state directory (a host
  where root may not mount is refused, Q-330), and `Isolation::Direct` on
  `/` in a container (ADR-0043); `RLIMIT_NOFILE` as ranikust
  started (`NofileLimit::current()` before the raise,
  `Sandbox::set_nofile_limit`). The sandbox reopens the root by path for
  each `RUN` and refuses it when it is no longer the directory
  `Sandbox::start_with_root` checked (device, inode, mount id). On `/` a
  `#!` heredoc runs from a sealed memfd (`RunSpec::script`), a secret as
  a variable works, a cache mount is left out with a warning (the `RUN`
  runs without it, and what it writes at the target lands in its layer), and a secret as a file and `--network=none` are
  refused before any secret is read
  ([ADR-0057](../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md)).
- **What later units read is kept** before the root is reused, in the
  build's keep area `<state>/builds/<id>/kept`, beside the root and never
  inside it (on `/` it lies in the ignored state directory). At the end of
  a unit that later units read, `ranikust` asks for it
  (`rnk_builder::SourceRoot::keep`), and `KanikoRoot::keep` copies it out
  (`rnk_rootfs::copy_filtered`, owners, modes, times, xattrs and
  hardlinks kept, the ignore list's paths left out, and a path to keep on
  another mount refused) and gives the root up; each copy is a new
  directory of the keep area, opened and checked to be the one just made
  (device, inode, mount) before anything is written or deleted in it;
  from then `view` reads the copy:
  - a unit only read by `COPY --from` keeps the entries its sources name,
    with every symlink on the way to them, and the copy must resolve each
    source to the same entries; otherwise, and for a source naming the
    root (`/`, `.`, a link to `/`), a source with a variable, a
    `RUN --mount from=` or an `ONBUILD` trigger, the whole root is kept.
    `Keep::Paths` takes physical paths only: a path with a symlink before
    its last name, or `..`, is refused, since the ignore list judges the
    path as given;
  - a unit a later `FROM` builds on (`save_image`) is kept whole, and
    `prepare(Base::Stage(kept))` copies it back into the emptied root,
    whose baseline walk then starts from it;
  - an image read by `COPY --from=<image>` is unpacked into the keep area
    (`Snapshotter::prepare_source`), never into the shared root.

  A kept copy is deleted when its stage is released at the end of the
  build, with the build directory otherwise. On `/` (ADR-0056) a kept
  stage is a plain tar in an anonymous file, written by a metadata-only
  walk and the walk's exporter, and a `COPY --from=<image>` stays its
  blobs (a streamed one spooled and verified on first read, before any
  directory is made, ADR-0055); a reader's view is unpacked into a directory of the state
  directory named by the store, inside a fence window, and deleted at the
  end of the instruction that read it (`end_layer`, also `prepare`,
  `keep`, `release`, the end of the build); `FROM` a kept tar extracts it into
  the emptied root, device nodes and FIFOs left out (ADR-0041). Kept tars
  are extracted with names taken literally (`Whiteouts::Literal`); a
  stage holding a `.wh.*` file cannot be kept (Q-053); device nodes and
  FIFOs made in a view are acceptable only because no `RUN` runs while it
  exists. With `--parallel-stages` (task 10), a view would keep the fence
  closed to other stages' `RUN`s, which wait at most a minute: to design
  there.
- **Refused:** kaniko mode without root, and `--parallel-stages`
  (ADR-0060, [open: Q-400]), each with its reason. Off `/` the stages do not share a root: see
  [Per-stage directories](#per-stage-directories).

`Kaniko` also takes a root other than `/` (a directory, its keep area
beside it, emptied at the end of the build): the binary no longer uses it
that way since ADR-0059, its tests do, and `--parallel-stages` may.

## Walk, hash and diff

- **Parallel walk**: one task per directory on a `rayon` pool. A directory is
  opened relative to its parent's file descriptor (`openat2` with
  `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS` and
  `RESOLVE_NO_XDEV`), never by a full path: trees deeper than `PATH_MAX`
  otherwise fail with `ENAMETOOLONG`. Each entry gets
  `statx(dirfd, name, AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT)`.
- **Mount guard**: an entry whose `stx_mnt_id` differs from the root's is
  skipped, a second line of defence after the ignore list.
- **Hash**: content hash (xxh3 or HighwayHash; the choice barely matters)
  plus a hash of the xattrs, excluding `*.overlay.*`.
- **Stat cache**: key `{dev, ino, size, mtime, ctime, mode, uid, gid}`. An
  entry is reused only if `max(mtime, ctime)` is more than 2 s older than the
  start of the scan (racy window). The cache is the previous walk's record
  at the same path in the path arena, so it costs no memory of its own.
- **Diff rule**: added or modified when any of kind, mode, uid, gid, size,
  mtime, rdev, symlink target, content hash or xattr hash changed. A deletion
  becomes one whiteout at the topmost deleted path.
- Compute the diff during the walk (mark entries seen; unseen old entries are
  deletions) rather than comparing two maps afterwards; store entries
  compactly (path arena), since a 269k-entry map took about 200 MB.
- The walk is a `rayon` scope over `openat2` and `getdents64`, hashing with
  xxh3-128 ([ADR-0039](../adr/0039-kaniko-mode-walk-and-hash-crates.md)), on
  8 workers by default: 16 and 32 were slower than 8 on the benchmark
  machine. `--snapshot-workers N` changes the count; it never changes a
  layer.
- **`--snapshot-mode`** picks what is compared, as in kaniko
  ([ADR-0023](../adr/0023-mode-flag.md)): `full` (default) everything above,
  content included; `redo` mode, mtime, size, uid and gid, without reading
  content; `time` mtime only. Neither reads a file, so the stat cache does
  not apply to them. In every mode a change of the entry's type is a
  change, which kaniko's `time` mode misses when the mtime matches.
- xattrs are hashed on regular files and directories only, through the
  descriptor the walk already holds.
- A regular file is opened only after its `O_PATH` pin matches its `statx`
  (type, inode, device, mount), then through `/proc/self/fd`, so nothing
  swapped in is ever opened; hashing reads at most `size + 1` bytes. A
  subdirectory is opened in its own task, from its parent's shared
  descriptor, so waiting directories hold no descriptor of their own.

## Ignore list

Mount points below the root, read from `/proc/self/mountinfo` (excluding the
root itself), plus `--ignore-path`, plus the state directory when it lies
inside the root, plus, as kaniko (`ADR-0038`), all of `/ranikust` (an image
may keep the binary there, as kaniko keeps `/kaniko/executor`),
`/etc/mtab` and `/tmp/apt-key-gpghome`. `/proc`, `/sys` and `/dev` are
always ignored, and so are the runtime's markers `/.dockerenv` and
`/run/.containerenv`, which are not image content: emptying `/` keeps
them, so a later build still detects its container. Unlike kaniko, `/var/run` is not on the list: it is ignored
only where it, or something below it, is a mount point, as every mount
point is, so an image's `/var/run -> /run` symlink (alpine 3.12 and later)
is extracted and kept (ADR-0052, D4); kaniko's `--ignore-var-run` and
`--whitelist-var-run` would change nothing and are refused (ADR-0049).
Where the container's `/var/run` is itself a link (`../run`), a base
layer with `var/run/.wh..wh..opq` but no `var/run/` entry of its own now
fails the build: the opaque marker's directory is a symlink, which live
extraction never follows, so nothing under `/run` is emptied (fail closed;
with the layer's own `var/run/` the link is replaced by its directory).
Before D4 the whole `/var/run` was skipped. Built in `rnk_snapshot::kaniko::ignore_list` on
`rnk_rootfs::IgnoreList`.

Matching is kaniko's (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1261-1281`): a
path is ignored when its leading components match an entry, so the walk
never descends into an ignored path; `--ignore-path` entries are globs per
component, as Go's `filepath.Match` (`/var/cache/*` ignores what is in
`/var/cache`, not the directory); `/tmp/apt-key-gpghome` ignores only what
is below it (kaniko's `PrefixMatchOnly`). Paths are physical, as kaniko
compares them. The parents of an
ignored path get no special treatment: kaniko writes a parent directory
only when it changed itself or holds an added file
(`pkg/snapshot/snapshot.go:340-445`), and the walk does the same.

Extraction of base layers into the live root (`rnk_rootfs::extract_live`)
checks each entry at its name and at its physical path, so a symlink in the
image cannot lead a layer into the state directory: kaniko resolves only
the physical path (`securejoin`, `fs_util.go:410-433`), so an entry named
below `/var/run` is skipped here while kaniko writes it through the
symlink. Ignored entries and entries on another mount are skipped; a
whiteout of an ignored path, or of a directory holding one, is skipped as
kaniko skips it (`:248-266`); an entry replacing a directory holding an
ignored path removes the rest and is skipped (`FF_KANIKO_PRESERVE_MOUNTED_PATHS`,
`:343-370`); an opaque whiteout keeps ignored paths and mounts. The
physical path is read from `/proc/self/fd` of the directory fd that the
entry is then written through: missing directories are created one name
at a time relative to it, without following a symlink or leaving the
mount, so a concurrent swap of a name for a symlink into an ignored path
cannot redirect a write; directory metadata set at the end re-checks the
parent's physical path. Device nodes and FIFOs are not created, as
kaniko creates none
([ADR-0041](../adr/0041-live-root-extraction-skips-device-nodes.md)); the
image keeps them through its unchanged base layers.

The root and state directory handed to the list must be the kernel's
names for the directories in use (`readlink` of their fds), absolute and
normalized; a state directory that is the root or an ancestor of it is
refused.
On a host the mount part is usually empty. Inside a container it covers
`/proc`, `/dev`, `/sys` and their submounts, `/etc/hosts`, `/etc/hostname`,
`/etc/resolv.conf` and volumes.

## Safe deletion

Emptying a root (end of stage, release of a stage directory) recurses through
file descriptors:

- open each directory with `openat(O_DIRECTORY | O_NOFOLLOW)`, then compare
  `ino` and `mnt_id` from `statx` with the opened fd to detect a swap;
- remove entries with `unlinkat`; a symlink is unlinked, never followed;
- keep entries in the ignore list, entries whose `mnt_id` differs from the
  root's, and entries with `STATX_ATTR_MOUNT_ROOT`;
- **never decide by `st_dev` alone**: a bind mount of the same filesystem
  has the same `st_dev`, and its contents would be wiped;
- independent of `PATH_MAX`; open descriptors grow with tree depth, so
  `RLIMIT_NOFILE` is raised to its hard limit at start-up.

## Host safety checks

ranikust never fills or empties a directory the operator names (ADR-0050,
which replaces the `--workdir` checks of ADR-0038 and ADR-0047): the root
is `/` in a container, chosen by kaniko's heuristic (ADR-0048), or a
directory per stage, `<state>/builds/<id>/stages/<n>/rootfs`, made for the
build (ADR-0059). Before anything is
written, `ranikust::statedir::prepare` checks the state directory:

1. Before it is created, its nearest existing directory and every real
   parent, walked from a descriptor (`..`), never by path, are owned by
   root (or the user) and not writable by group or others, except root's
   sticky directories.
2. It is created `0700`, opened with `RESOLVE_NO_SYMLINKS`, owned by the
   user, not writable by others, and set to `0700` if others may search
   it; then the whole chain is checked again from its descriptor.
3. Before its mode is touched: never `/`, and only a directory just
   created, empty, or holding the store's regular `.ranikust-state`
   marker (Q-111), and, unmarked, none of the thirty system directories of
   `statedir::SYSTEM_DIRS` (`/mnt`, `/srv`, `/opt`…, by device and inode);
   one marked only with the old `.ranikust-root` is refused, named. After `chmod 0700`, the mode must read back so;
   `builds/` likewise.

`ranikust` holds kaniko mode on `/` with `flock` on a descriptor of `/`
for the whole build, and refuses kaniko mode outside a container where
root may not mount (Q-330); `rnk-rootfs` provides the primitives. It also raises
`RLIMIT_NOFILE`'s soft limit to the hard limit for the walk; `RUN`
processes get the original limit back.

## Exporter requirements specific to the walk

The common rules are in [overlay-snapshot](overlay-snapshot.md#exporting-a-diff-as-an-oci-layer).
In addition:

Built as `WalkDiff::export` (`rnk-snapshot`, `src/kaniko/export.rs`), after
kaniko's `writeToTar` (`kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:341-391`):

- The parent-header rule matters most here: a walk diff often holds a deep
  file whose ancestors did not change (the 1,813 pnpm-store files of
  experiment 06 under `/root`), and those ancestors must still be emitted,
  as kaniko does. kaniko adds every parent of an added path and of a
  whiteout, but never the root, with its metadata read from the root at
  export time (`addParentDirectories`, `:432-447`; `addedPaths[RootDir]`,
  `:348`); the exporter does the same through `rnk-layer`'s parent lookup.
  `WalkDiff::root` is therefore never written.
- One whiteout per topmost deleted path, mode 0, owner 0:0, time 0, as
  kaniko's `Tar.Whiteout` (`pkg/util/tar_util.go:162-176`). What a
  directory replaced by a non-directory held gets no whiteout, as kaniko's
  `parentPathIncludesNonDirectory` (`:393-408`).
- A hardlink whose target lies in a lower layer (`ln /etc/passwd /x`) is
  written as a regular file: the walk does not report a link-count change,
  so the target is not in this layer, and `rnk-layer` links only entries
  of one layer. Names of one inode that are both in the layer become a
  file and tar hardlinks to it.
- Of the xattrs, only `security.capability` is kept (ADR-0032); sockets
  are left out, as kaniko (`pkg/util/tar_util.go:74-77`), but without the
  parents kaniko still writes for them (`ADR-0045`).
- What changed is written, nothing more: the unchanged target of a changed
  symlink, which kaniko's `ResolvePaths` adds
  (`pkg/filesystem/resolve.go:69-98`), is not, as BuildKit and overlay
  mode (`ADR-0045`). The exporter never resolves a symlink.
- The root is live: every entry is pinned once with `RootPath::pin`, its
  physical path resolved with no symlink in any component and without
  leaving the root's mount (`RESOLVE_IN_ROOT | NO_SYMLINKS | NO_XDEV`,
  `O_PATH | O_NOFOLLOW`), and the pin's `statx` checked against the walk's
  record (type, device, inode; a regular file's size and a file's or
  symlink's `mtime` and `ctime`, not a directory's, whose entry is written
  from the walk's record): the inode number alone is not enough, since a
  file deleted and created again can get the freed number back at once
  (ext4 on CI's aarch64 runner did). The link target, the xattrs and the
  content are read from that pin, the content through a reopen of exactly
  that inode, so a parent swapped for a symlink or a mount, or a FIFO or
  device swapped in, is never opened. In `full` mode the content written
  and the link target are hashed again (xxh3-128) and must match the
  walk's hashes, which also catches a swap within one timestamp tick and
  a file rewritten in place (unlike kaniko); `time` and `redo` read no
  content in the walk, so there the `statx` check is all. An entry changed
  since the walk fails the export. Names of one inode in the walk that
  disagree on type, size, mode, owner, mtime or content hash (an inode
  number reused during the walk) are written as separate files, not
  linked. Nothing may write the root between walk and export.
- A `RUN` that changes nothing produces an empty tar; the builder adds no
  layer for it, only its history entry with `empty_layer: true`, as kaniko
  (ADR-0058).
- Applying the base layer and a diff's layer rebuilds the walked tree:
  a property test on random trees, which compares content, modes and
  mtimes but not hardlink structure across layers (`ADR-0012`).

## Per-stage directories

Kaniko mode outside a container ([ADR-0059](../adr/0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md)),
`rnk_snapshot::KanikoStages`:

- **Location**: `<state>/builds/<build-id>/stages/<n>/rootfs`, in the
  build's private area of the state directory (the stage directory `0700`,
  its parents checked by the set-up, ADR-0050), never on the machine's own
  root. Each directory is made with `mkdirat` and opened with
  `openat(O_PATH | O_DIRECTORY | O_NOFOLLOW)` on the same parent
  descriptor (`RootPath::create_sub_root`): it must be empty, owned by the
  effective uid and on the parent's mount, else it is refused.
- **Creation**: a stage on an image extracts its base layers into its
  directory (a blob of the store, or streamed and checked at its end,
  ADR-0055; a failure deletes the directory; device nodes and FIFOs left
  out, ADR-0041); `FROM scratch` and an image start at `0755` and the epoch
  unless a layer sets `./`. `FROM <stage>` copies the parent's directory
  (`rnk_rootfs::copy_filtered`: owners, modes, times, xattrs, hardlinks
  inside the tree; a file cloned with `FICLONE` where the filesystem
  shares extents, else only its data ranges, holes kept) with the parent
  root's owner, mode and times; never a hardlink to it (ADR-0017). Device
  nodes, FIFOs and sockets a parent's `RUN` made are left out
  (`CopyOpts::skip_special`, ADR-0041).
- **Scheduling**: as overlay mode's stages: the stage graph and `--jobs`,
  no squashing, nothing kept or emptied between stages; a `COPY --from`
  reads the finished stage's directory as it is. A stage the target does
  not inherit (only read by `COPY --from`) is never walked (ADR-0038).
- **`RUN`**: the full sandbox ([sandbox](sandbox.md)), started with
  `Sandbox::start` as for overlay mode's mount points. The stage root is
  reopened by path for each `RUN`, and must still be the directory the
  stage holds (`RunSpec::expect_root`): checked in the parent after the
  reopening and in the child after the bind.
- **Walk**: each stage has its own walker, made at its first walk and
  dropped when the stage ran its last instruction (`StageRootfs::done`):
  its record of the root and its thread pool do not outlive the stage's
  instructions. The ignore list of a stage root is the fixed paths and
  `--ignore-path`; nothing is mounted in a directory ranikust made.
- **Release**: safe deletion of the stage directory, through descriptors,
  crossing no mount; the rest goes with the build directory.
- **Cost**: one copy of the base per stage (its data on ext4 and tmpfs,
  a reflink on xfs and btrfs), all stage directories held to the end of
  the build, so the peak is about the sum of the stages' roots;
  experiment 12 measured W3 at 3.28 GB against 2.13 GB for overlay mode.
  Releasing a stage's directory once no later stage reads it is not done
  yet.
- **Placed files**: `hosts`, `resolv.conf`, stub mount points and `/dev`
  nodes change the mtime of their parent, so `/etc` would enter every layer.
  The full sandbox records the parent's mtime before placing (m0) and after
  (m1); after `RUN`, if it is still m1, it restores m0, otherwise keeps what
  `RUN` set (`rnk-sandbox`'s `swap` journal records, `ADR-0043`): the
  image's entry is renamed aside in the same directory and back after,
  never rewritten.

## The `--parallel-stages` path (bare `chroot`)

> Not built: the flag is refused until Q-400 is answered (ADR-0060). A
> `RUN` that escapes a bare `chroot` reaches what ADR-0051 keeps out of
> its reach; Q-400 holds a hardened variant (capabilities dropped after
> the `chroot`, no procfs) to measure first. The text below is the plan of
> ADR-0014.

Used only on request where mounting is impossible. `RUN` needs
`CAP_SYS_CHROOT`, which Docker grants by default. At build start ranikust logs
that `RUN` has no `/proc`.

- **No isolation between stages**: root in `RUN` can escape a bare `chroot`
  and read or write a sibling stage's root, including its secrets. Accidental
  writes stay inside the stage's own root.
- **`/dev`**: `mknod` `null`, `zero`, `full`, `random`, `urandom`, `tty` when
  permitted (6/6 on rootful Docker). Where `mknod` is denied, `/dev` stays
  empty: `xargs` warns about `/dev/null`, and `> /dev/null` creates a regular
  file, which the walk ignores.
- **Process tree**: there is no PID namespace, so ranikust becomes a child
  subreaper (`PR_SET_CHILD_SUBREAPER`) and waits for every descendant of a
  `RUN`, such as a Gradle daemon, before removing the fake `/proc`
  [inferred].

### Fake `/proc`

A static snapshot of the files that runtimes read to size themselves, enabled
by default on this path. Fourteen files per cgroup version; `memory.stat`
is added only once a build fails without it (`ADR-0038`):

| # | File | Notes |
|---|---|---|
| 1–3 | `/proc/stat`, `/proc/cpuinfo`, `/proc/meminfo` | libuv `os.cpus()`, `os.freemem()` |
| 4–6 | `/proc/self/cgroup`, `/proc/self/mountinfo`, `/proc/self/stat` | quota lookup; `process.memoryUsage()` (needed by `next build`) |
| 7–9 | `/sys/devices/system/cpu/{online,possible,present}` | glibc `sysconf` |
| 10 | `/proc/cgroups` | JVM cgroup detection: every JDK on v1, older JDKs on v2 |
| 11–14, v2 | `<D>/cgroup.controllers`, `<D>/cpu.max`, `<D>/memory.max`, `<D>/memory.high` | dropping `cgroup.controllers` or `memory.high` loses libuv's quota |
| 11–14, v1 | `<Dcpu>/cpu.cfs_quota_us`, `<Dcpu>/cpu.cfs_period_us`, `<Dmem>/memory.limit_in_bytes`, `<Dmem>/memory.soft_limit_in_bytes` | dropping `memory.soft_limit_in_bytes` loses libuv's memory limit |

The set is derived at run time from ranikust's own `/proc/self/cgroup` and
`/proc/self/mountinfo`, never hard-coded:

- **Version**: `statfs("/sys/fs/cgroup")` reports cgroup2, or `mountinfo` has
  a `cgroup2` line.
- **v2 directory `<D>`**: the `cgroup2` mount point, plus the path of the
  `0::<P>` line minus the mount's root (column 4).
- **v1 directories `<Dcpu>`, `<Dmem>`**: the `cgroup` mount whose super
  options name the controller, plus the path of that controller's line minus
  the mount's root. Under Docker these are `/sys/fs/cgroup/cpu,cpuacct` and
  `/sys/fs/cgroup/memory`.
- A source file that does not exist is skipped silently. No cpuset files: the
  affinity mask passes through `chroot` by itself.

Lifecycle per `RUN`: read each file completely from ranikust's own `/proc`
and `/sys` (procfs reports size 0) just before `exec`; write it into this
stage's `rootfs/proc` or `rootfs/sys` with mode 0444. After the whole process
tree has exited, delete **everything** under `rootfs/proc` and `rootfs/sys`
(a `RUN` can `mkdir -p /proc/...`) and restore both directories' atime and
mtime. Their ctime cannot be restored, which is one more reason the walk
always skips them. Cost: about 1 ms per `RUN`.

Never fake `/proc/self/{maps,exe,fd,status,environ}`, `/proc/<pid>`,
`/proc/sys` or `cpufreq`; never set `LD_ORIGIN_PATH` or `LD_LIBRARY_PATH`;
never substitute environment variables such as `GOMAXPROCS`. Each of these
produced silently wrong results in the experiments.

What it fixes and what it does not:

- Fixed: `os.cpus()` returning 0; CPU quota and memory limits for libuv, V8,
  Go and Rust; `next build` (including Next 16 and Turbopack); JVM limits
  under cgroup v1, and under v2 for JDKs that still read `/proc/cgroups`.
- Not fixed: `/dev/fd`, `/dev/std*` and `<(...)`; `/proc/self/exe` and
  therefore `$ORIGIN` (distro JDKs, TypeScript 7, `current_exe`); `ps` and
  `/proc/<pid>`; live values (memory, load and RSS are frozen at the
  snapshot); JVMs ≥ 21.0.12 and ≥ 17.0.20 under cgroup v2, which detect the
  cgroup with `statfs`.
- Each stage sees the whole container's quota, as in BuildKit.

### Java

- **`JAVA_TOOL_OPTIONS`** is injected under cgroup limits as specified in
  [ADR-0019](../adr/0019-java-tool-options-on-parallel-path.md), and never
  includes GC flags.
- **Early warnings**, after the stage root is prepared and before the first
  `RUN`:
  - for each `*/bin/java`, if `libjli.so` resolved by the dynamic loader's
    cache is missing or is not `<jdk>/lib/libjli.so`, warn that this JDK will
    not start, or will load another JDK's launcher, under
    `--parallel-stages` (distro JDKs from apt, apk or dnf fail; Temurin
    images pass because they run `ldconfig`);
  - if `memory.max` < 1792 MiB and a stage contains Java, warn that the JVM
    will choose G1 instead of SerialGC and may be OOM-killed.
- JDKs downloaded during a `RUN` by a toolchain (Gradle foojay,
  maven-toolchains) cannot be detected in advance.

### After a failed `RUN`

If stderr contains one of these strings, ranikust suggests running without
`--parallel-stages`:

- `libjli.so`
- `readlink /proc/self/exe`
- `uv_resident_set_memory`
- `/dev/fd/`
- `Unpacked JDK archive does not contain a Java home`
- `getMainType`

It never retries by itself: the failed `RUN` may have written part of its
output.

### Tool families

| Tools | On this path |
|---|---|
| Node: Next 15 and 16 (webpack and Turbopack), webpack/terser, jest, tsc 5, esbuild, vite, npm, pnpm | Safe with fake `/proc`: same output, workers and threads as with a real `/proc` |
| Java on Temurin images, one JDK | Runs; under cgroup limits only with `JAVA_TOOL_OPTIONS` |
| Several JDKs in one root (toolchains) | Needs sequential: wrong `libjli`, `getMainType` failure or silent mix |
| JDK from apt, apk or dnf | Needs sequential: does not start |
| TypeScript 7 (`tsgo`) and binaries calling `current_exe` at start | Needs sequential |
| Scripts using `<(...)`, `/dev/fd`, `ps`, `/proc/<pid>` | Needs sequential |

## Lazy snapshots

A stage whose layers are not needed is not snapshotted at all: no baseline
walk, no walk and hash, no tar. The decision is taken once, from the stage
graph and the outputs, before the build (`ADR-0038`): a stage's layers are
needed when it is the target, or when a stage whose layers are needed
builds `FROM` it (squashed or not). Any other stage, used only through
`COPY --from`, is skipped. Nothing changes the decision during the build,
so no deletion ever has to be detected after a skipped walk. With
`--cache` (Phase 4) more stages may need their layers; the rule stays.
On the shared root a unit whose image is kept for a later `FROM` is walked
too, since that image is the only way to rebuild the root for the child
(ADR-0044). `rnk_dockerfile::squash_for_shared_root` sets
`layers_needed` per unit; `rnk_dockerfile::layers_needed` gives the rule
for per-stage directories.

## Memory

Parallel stages in kaniko mode hold more page cache than overlay mode: each
stage has its own extracted base and re-reads its writes to hash and tar
them. The scheduler's memory limit counts page cache
([scheduler](scheduler.md)).

## Failure handling

- Any host-safety check failing stops the build before anything is written.
- A deletion that meets an unexpected mount point, swap or permission error
  stops and reports the path; it never falls back to a path-based delete.
- Stubs are journaled before creation and removed on the next start after a
  crash ([sandbox](sandbox.md)).

## Open questions

- [Q-033](../open-questions.md#q-033-which-performance-conditions-are-still-unmeasured) Which performance conditions are still unmeasured?

## References

- [Survey 02](../research/survey/02-kaniko-mode-requirements.md) §2–§4;
  [survey 06](../research/survey/06-overlay-and-parallelism.md) §5.
- [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md): walk of
  269k entries in 0.10–0.15 s with the stat cache; deletion tests D1–D3
  (`st_dev`-only deletion wiped a same-filesystem bind mount; 14 dangerous
  paths refused).
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md): W3
  parallel −41 to −44 %; `/etc` mtime leak; hardlink store corruption; page
  cache 8.2–8.3 GB.
- [Experiment 12a](../research/experiments/12a-fake-proc.md),
  [experiment 13](../research/experiments/13-docker-rootful-vm.md) §4,
  [experiment 14](../research/experiments/14-real-builds-without-proc.md),
  [experiment 15](../research/experiments/15-java-next-concurrency.md),
  [experiment 16](../research/experiments/16-cgroup-v1-cpuset.md) §9–§10:
  fake `/proc` sets, Java, Next, concurrency (27/27 `RUN`s saw their own
  snapshot; no fake file reached a layer).
- [Experiment 08](../research/experiments/08-parallel-stages-lazy-export.md):
  skipping intermediate-stage snapshots saved 4.7–5.3 s (30–33 %) on W3.
