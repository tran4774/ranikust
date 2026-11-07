# kaniko mode

**TL;DR:** kaniko mode builds where overlay is not available. It finds each
layer by walking and hashing the root. In a container it builds on `/`, one
root emptied safely between stages; anywhere else in a directory per stage of
the state directory (ADR-0050, ADR-0059). The `--parallel-stages` path of
ADR-0014 (a bare `chroot` per stage with a fake `/proc`) is refused for good
(ADR-0060).

Decisions: [ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md),
[ADR-0017](../adr/0017-stage-rootfs-by-copy-not-hardlinks.md),
[ADR-0018](../adr/0018-no-proot-in-1-0.md),
[ADR-0019](../adr/0019-java-tool-options-on-parallel-path.md),
[ADR-0023](../adr/0023-mode-flag.md),
[ADR-0038](../adr/0038-kaniko-mode-settings-for-phase-2.md),
[ADR-0050](../adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md),
[ADR-0059](../adr/0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md).

## Variants

| Variant | When | Stage root | `RUN` | Stages |
|---|---|---|---|---|
| **Sequential** (default in a container) | root in a container, cannot mount (Docker default, ordinary pods) | `/` | directly (`rnk-sandbox`'s `Isolation::Direct`), with the container's real `/proc`, `/dev` and network files | one at a time; `--jobs` bounds only the uploads |
| **Per-stage, sandboxed** | root outside a container, able to mount (a host where root may not mount is refused, ADR-0050); `--mode=auto` picks it there when overlayfs refuses the state directory | `<state>/builds/<id>/stages/<n>/rootfs`, one per stage (ADR-0059) | full sandbox ([sandbox](sandbox.md)) | DAG, `--jobs` |
| ~~Per-stage, bare chroot~~ | refused for good: `--parallel-stages` exits 2 (ADR-0060) | | | |

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
  files leak into layers. On `/` it is `/var/lib/ranikust` (or `--state-dir`),
  inside the root and on the ignore list, like kaniko's `/kaniko`. Off `/`
  every stage root lies inside it.
- Whatever ranikust places in a root (stubs, `hosts`, `resolv.conf`, `/dev`
  nodes) is removed, and its parent directory's metadata restored, before the
  diff.
- **On `/`, a `RUN` reaches nothing of ranikust's**
  ([ADR-0051](../adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)):
  - **The store keeps nothing with a path.** `Store::open_anonymous`: pulled
    blobs and encoded layers are anonymous files on disk, held by descriptor
    and named `/proc/self/fd/<n>` to ranikust's own code
    ([ADR-0056](../adr/0056-kaniko-mode-state-on-disk-behind-a-run-fence.md)).
    - `O_TMPFILE` in the state directory; where the filesystem refuses it (a
      container's overlayfs root, kernels 6.8 and 5.15, measured), a file made
      under a random name and unlinked at once; `memfd` only when no file can
      be made on disk.
    - The unlink must remove the name. A state directory on NFS, FUSE, SMB or
      AFS (by `statfs`), which keep `.nfs*` or `.fuse_hidden*` names for open
      files, or a file still linked after its unlink, fails the build with
      `Error::NameOutlivesUnlink`.
    - A fence keeps names and `RUN`s apart: `Store::enter_run` and
      `Store::leave_run` around each `RUN`, `Store::named_window` for a name.
      A `RUN` that ended with an error once its command may have run poisons
      it. A start on the traces of a build cut short in this container (its
      `.ranikust-building` marker, holding the PID namespace and boot id, or
      names left with none) makes no name, so a `COPY --from` a kept stage or
      an image fails
      ([ADR-0066](../adr/0066-kaniko-mode-on-root-makes-no-names-after-a-build-cut-short.md)).
    - After each `RUN` the state directory's path must still lead, with no
      link, to the directory held. What must outlive the emptying of the root
      (a kept stage) is a tar in an anonymous file (`Store::scratch_file`).
  - **The state directory is never a persistent store, and no other build
    shares it**
    ([ADR-0077](../adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md);
    errors and locks in [cache-and-store](cache-and-store.md#locks)). A
    `Direct` `RUN` writes it as root, and persistent builds reuse their
    entries without hashing them again.
    - Nothing is kept between builds in this mode: no layer cache, and cache
      mounts are left out (ADR-0057). A safe cache is [open: #30].
    - The `.ranikust-building` marker is made this build's again after every
      `RUN` and at the end: whatever a `RUN` put under its name is replaced,
      the build then makes no more names, and the marker stays. A marker the
      close cannot make at first (a filesystem a `RUN` filled) is made once
      the directory is emptied; if it still cannot be, the build fails with
      `Error::Unmarked`, saying the directory must be deleted.
    - A `RUN` cannot stop the build with `SIGINT` or `SIGTERM`: both are
      blocked before any thread starts and read through a `signalfd`
      (`rnk_sandbox::block_stop_signals`). Only the kernel's, those sent from
      outside the PID namespace and those from ranikust's parent count; the
      marker is written back before the second one's exit. Any other stop
      signal cancels the build, which still closes its store, so a `RUN`
      flooding them cannot hide the operator's.
    - A `RUN` can still end ranikust (`SIGKILL` where it is not PID 1, the
      OOM killer) and leave what it wrote. A state volume used here therefore
      never serves overlay or per-stage builds later, is mounted at
      `--state-dir` itself, and gets a warning when it is a mount root, or a
      directory of a volume mounted at a parent.
  - **Not dumpable.** `/proc/<ranikust>/{fd,environ,mem,maps,root}` need
    `CAP_SYS_PTRACE` (a root `RUN` still lists descriptor numbers).
  - **A sealed binary.** ranikust runs from a sealed memfd copy of its
    binary, counted in the container's memory, else an `O_TMPFILE` copy,
    else the installed binary, warned ([sandbox step 0](sandbox.md#process-set-up-once-per-build),
    [ADR-0101](../adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md)).
    The installed binary is refused on a writable mount other than `/`'s
    (bound without `:ro`, from outside the container or inside it, a
    volume), when a writable mount reaches it (its tree bound read-write
    too, by `/proc/self/mountinfo`; a link of it bound read-write, even one
    whose name was removed; a file with another hard link; in a
    root that is not an overlay (overlayfs or fuse-overlayfs), a binary in
    the root too), and on a mount ranikust cannot tell (qemu-user, a
    memfd, a mount table that does not place it (its own name removed
    among them) or changes while it is read, a writable directory of its
    file system whose name was removed, a writable mount that another
    mount covers or that cannot be reached at its mount point, a binary
    not weighed because ranikust did not find itself in a container when
    it started), before anything is written: a `RUN` reaches it by name,
    and the build no longer runs it (ADR-0101). Only a build that finds
    itself in a container looks the writable mount points up.
  - **No capability beyond the `RUN`'s**
    ([ADR-0078](../adr/0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md)).
    The `RUN` child drops every capability outside Docker's defaults from its
    bounding set (`CAP_SETPCAP`); without it, any capability outside them stops
    the `RUN` ([sandbox](sandbox.md#direct-kaniko-mode-on-)); the ambient,
    inheritable, permitted and effective sets go to the defaults in every case.
    ranikust itself keeps no more: `rnk_sandbox::trim_to_run_capabilities`,
    first thing in the set-up while single-threaded, cuts its bounding set to
    Docker's defaults (else the build is refused), clears its ambient and
    inheritable sets and trims the rest; every later thread inherits them. A
    link a `RUN` plants at an operator path (an output, the metrics report)
    then reaches only what that `RUN` could reach itself. ranikust's own soft
    `RLIMIT_CORE` is 0; the `RUN` gets the original back, as it gets
    `RLIMIT_NOFILE`.
  - **Secrets are read once** while the build is prepared and never opened by
    path again (`Secret::load`). A `--secret src=` file on `/` still gets a
    warning: every `RUN` there can read it, and copying it into the image is
    the user's choice. Secret files on `/` are decided but not built
    ([ADR-0084](../adr/0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md),
    [ADR-0085](../adr/0085-a-secret-file-on-slash-goes-at-its-target-is-ignored-by-its-own-path-and-removed-after-its-run.md),
    [#35](https://github.com/tran4774/ranikust/issues/35)).
  - **Registry credentials go only over verified TLS.** A `RUN` may edit
    `/etc/hosts` and `/etc/resolv.conf`, which are read at every lookup, as
    ADR-0051 accepts.
  - **Refused where root may mount**, or where `/proc/sys` is not a read-only
    mount
    ([ADR-0054](../adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md));
    devices passed with `--device` stay the operator's choice.
  - **Refused with a process of another mount namespace in reach.** A `RUN`
    shares the container's PID namespace; where another container shares
    it too (Kubernetes `shareProcessNamespace: true` or `hostPID: true`,
    `--pid container:<name>`, `--pid=host`), a `RUN` could reach that
    container's files through `/proc`, which this container's mount table
    does not show. With the binary's check, before anything is written,
    ranikust looks once through `/proc` (`exposure::scan_peers`): a
    process with a thread in another mount namespace
    (`/proc/<pid>/task/<tid>/ns/mnt` by device and inode) whose root it
    opens, every thread probed through one descriptor of `/proc/<pid>`
    (the process as one where its threads may not be listed), refuses the
    build. A thread counts only when ranikust opens it and its root
    (`EACCES` and `EPERM` mean not opened); one gone is skipped, and any
    other error or more than 32768 threads refuse too, each with its own
    guidance. Its own mount namespaces are the one it started in, opened
    before the set-up and held open until the scan ends, and the one it is
    in when it looks (a set-up that failed after its `unshare` left it in
    a copy of its own). A `--pid=host` container is refused by the same
    rule when a process of the host or of another container is in reach.
    It counts only the processes it can open as it starts, a residual risk
    accepted in ADR-0101; its mitigation is a PID namespace of the build
    container's own.
  - **No process outlives its `RUN`.** ranikust is a child subreaper. After
    each `RUN` it kills everything that became its child during the `RUN`, and
    their descendants, through a pidfd after checking the parent and start time
    again, and reaps them by pid without blocking, never `waitpid(-1)`. Where
    seccomp refuses `pidfd_open`, a plain `kill` for its own children only (a
    pid stays theirs until reaped); deeper processes follow on the next rounds,
    once reparented to ranikust.
  - **Warnings for what it cannot close:** a `--build-arg NAME=value` whose
    name looks like a secret (`/proc/<pid>/cmdline`), another process holding
    `DOCKER_AUTH_CONFIG` in its environment, a `--secret` file.

## Sequential flow

Per stage, as in kaniko: extract the base into the root, take the baseline
walk, then for each instruction run it, walk and diff, and record the layer.
At the end of the stage, keep what later stages need (files for `COPY --from`,
the stage image when another stage builds `FROM` it) in the state directory,
then empty the root.

Sequential mode keeps kaniko's stage squashing, which folds a parent stage
into the child that builds on it
[inferred: survey 06 §4.1 drops squashing only for overlay mode]:
`rnk_dockerfile::squash_for_shared_root`, from `kaniko@44aa7ddc0
pkg/dockerfile/dockerfile.go:297-415`.

- A base folds into its only child when no `COPY --from` or `RUN --mount
  from=` reads it and it is not the result; chains fold whole.
- Each unit lists what later units read from it (`readers`: `COPY --from`
  sources, `RUN --mount from=` roots) and whether its image is kept for a
  later `FROM` (`save_image`); both are kept before the root is reused.
- `rnk-sched` runs the units one at a time, in plan order
  (`Placement::Shared`).

## Stage roots on the shared root

`rnk_snapshot::Kaniko` is the kaniko-mode `Snapshotter`, `KanikoRoot` its
`StageRootfs`, on the traits as
[ADR-0046](../adr/0046-snapshotter-signature.md) settles them.

- **One root, one holder.** `Kaniko` holds the root as a `RootPath` opened
  once by `ranikust`'s set-up (`/` in a container), never a path opened
  again. At most one `KanikoRoot` holds it at a time;
  `prepare(Base::Stage(parent))` takes it over from `parent` when nothing
  emptied the root since (a squashed chain), and fails otherwise.
- **Base layers are blobs.** `base_layer` records the blob in the store and
  extracts nothing: the root is the only copy. `prepare` decodes each base
  layer into the root with `rnk_rootfs::extract_live`, checking its diff ID as
  overlay mode does.
- **When the root is emptied** follows kaniko on `/` (`kaniko@44aa7ddc0
  pkg/executor/build.go`):
  - the first base goes over what is there (`:632-641`; kaniko deletes first
    only with `--pre-cleanup`, `:1944-1947`);
  - every later base starts from an emptied root (`DeleteFilesystem` between
    stages, `:2179`);
  - the end of the build leaves `/` as it is (kaniko empties it only with
    `--cleanup`, `:1955-1957`; ranikust refuses `--cleanup`).
- **A build on `/` never inherits an earlier one** (ADR-0052, D1). At the
  first base on `/` (under the hold and the lock of `/`, ADR-0050), before
  anything is extracted, `new_base` looks for `/ranikust/.built`.
  - Found: the first base empties `/` (the ignore list kept) as every later
    base does, credential files outside `/ranikust`, mounts and the ignore
    list included (read at start-up, so that build still has them; a third
    build does not). A second build in the same CI container ships nothing the
    first left.
  - Missing: the first base goes over `/` as kaniko's, so the container's own
    tools stay.
  - Either way the marker is made then: a build that stops before its first
    base (a typo, a failed pull) leaves none, one killed during the
    extraction leaves it.
  - `/ranikust` is made, or set, `0700` (a non-root process of the container
    loses access to it). It must be a directory on `/`'s own mount; a symlink
    or a volume there, which could outlive the container and empty a later
    one's `/`, is refused. It is opened once and checked against its name,
    and the marker is looked up and made (`O_CREAT|O_EXCL|O_NOFOLLOW`)
    relative to that descriptor without leaving its mount; anything under the
    marker's name, also one made in between, counts as a marker.
  - It is not ADR-0056's `.ranikust-building`, which marks a build in
    progress for the store and goes with a clean close: this one stays for the
    container's life. Builds in one container trust one another: a `RUN` runs
    as root on `/` and can delete the marker, as it can replace anything
    under `/ranikust`.
- **The ignore list** is built once per build ([Ignore list](#ignore-list))
  and used by extraction, the walk and deletion alike.
- **The operator's paths stay out of the root.** The context, the outputs
  (`--tar-path`, `--oci-layout-path`, the digest files), `--metrics-file` and
  file secrets are refused before the root changes when they lie in it, unless
  the ignore list keeps them (a volume, `/ranikust`, `--ignore-path`). In
  every mode an output must also lie outside the build context and be reached
  without a symlink, and `--context` must have no symlink in it (ADR-0076).
- **Layers.** A stage whose layers are needed (`set_layers_needed`, from the
  [lazy-snapshot rule](#lazy-snapshots)) takes a baseline walk after its base;
  then `end_layer` walks and diffs, and the diff, held with the root, is the
  layer source. `rnk-builder` encodes it before the next instruction, into a
  temporary blob dropped on error (`LayerExporter::export`). An unchanged
  `RUN` gives an empty tar, which the builder drops: the step adds only its
  history entry (ADR-0058). A stage whose layers are not needed is never
  walked: `end_layer` returns an empty diff that is never encoded.
  `begin_layer` and `view` only hand out the root.
- **Release** gives the root up; emptying (`rnk_rootfs::delete_contents` with
  the ignore list, so the state directory and mounts stay, then
  `Walker::forget`) happens at the next base or at the end.
- **A vanished or swapped entry fails the walk** (`Error::Changed`),
  directories and files alike, as kaniko's walk fails on a callback or hash
  error (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1688-1691`,
  `pkg/snapshot/layered_map.go:160-163`): nothing may change the root while it
  is walked.
- **`RUN`** gets `Isolation::Direct` on `/` (ADR-0043), with `RLIMIT_NOFILE`
  as ranikust started (`NofileLimit::current()` before the raise,
  `Sandbox::set_nofile_limit`). The sandbox reopens the root by path for each
  `RUN` and refuses it when it is no longer the directory the caller checked
  (device, inode, mount id; `RunSpec::root_id`). On `/`:
  - a `#!` heredoc runs from a sealed memfd (`RunSpec::script`);
  - a secret as a variable works;
  - a cache mount is left out with a warning: the `RUN` runs without it, and
    what it writes at the target lands in its layer;
  - a secret as a file and `--network=none` are refused before any secret is
    read ([ADR-0057](../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md);
    ADR-0084 lifts the refusal of a secret file, not built yet).
- **What later units read is kept** before the root is reused, in the build's
  keep area `<state>/builds/<id>/kept`, beside the root and never inside it (on
  `/` it lies in the ignored state directory). At the end of a unit that later
  units read, `ranikust` asks for it (`rnk_builder::SourceRoot::keep`), and
  `KanikoRoot::keep` copies it out (`rnk_rootfs::copy_filtered`: owners,
  modes, times, xattrs and hardlinks kept, the ignore list's paths left out, a
  path to keep on another mount refused) and gives the root up. Each copy is a
  new directory of the keep area, opened and checked to be the one just made
  (device, inode, mount) before anything is written or deleted in it; from
  then `view` reads the copy.
  - A unit only read by `COPY --from` keeps the entries its sources name, with
    every symlink on the way to them; the copy must resolve each source to the
    same entries. Otherwise the whole root is kept: for a source naming the
    root (`/`, `.`, a link to `/`), a source with a variable, a `RUN --mount
    from=` or an `ONBUILD` trigger. `Keep::Paths` takes physical paths only: a
    path with a symlink before its last name, or `..`, is refused, since the
    ignore list judges the path as given.
  - A unit a later `FROM` builds on (`save_image`) is kept whole, and
    `prepare(Base::Stage(kept))` copies it back into the emptied root, whose
    baseline walk then starts from it.
  - An image read by `COPY --from=<image>` is unpacked into the keep area
    (`Snapshotter::prepare_source`), never into the shared root.
  - A kept copy is deleted when its stage is released at the end of the
    build, with the build directory otherwise.
- **On `/`, what is kept lives in anonymous files** (ADR-0056).
  - A kept stage is a plain tar in an anonymous file, written by a
    metadata-only walk and the walk's exporter. A `COPY --from=<image>` stays
    its blobs, spooled and verified on first read, before any directory is
    made (ADR-0055); the spool's lifetime and its verification are in
    [streaming](streaming.md#credentials-and-failures).
  - Every layer extraction of kaniko mode reads its plain tar from a thread
    that decompresses the blob and computes the diff ID up to 5 MiB ahead (4
    queued chunks of 1 MiB and the one it fills; 6 MiB held with the one being
    read). It writes nothing, stops at the blob when the extraction stops
    early, and is joined before the extraction returns; where no thread can be
    started, the extraction decompresses itself (ADR-0081).
  - A reader's view is unpacked into a directory of the state directory named
    by the store, inside a fence window, and deleted at the end of the
    instruction that read it (`end_layer`, also `prepare`, `keep`, `release`,
    the end of the build). `FROM` a kept tar extracts it into the emptied
    root, device nodes and FIFOs left out (ADR-0041). Kept tars are extracted
    with names taken literally (`Whiteouts::Literal`), so a stage holding a
    `.wh.*` file cannot be kept. Device nodes and FIFOs made in a view are
    acceptable only because no `RUN` runs while it exists.
- **Refused:** kaniko mode without root, and `--parallel-stages` (ADR-0060).
  Off `/` the stages do not share a root: see
  [Per-stage directories](#per-stage-directories).

`Kaniko` also takes a directory as its root, its keep area beside it; the
binary does not use it that way since ADR-0059, tests do. Such a root is private
by ADR-0047's rule moved to the state directory by ADR-0050: below the state
directory, owned by the user running ranikust, `0700`, every parent likewise
and not writable by others (root's sticky directories excepted), walked by
descriptor before anything is created. Each emptying gives it its own owner and
mode back, and the sandbox refuses a `RUN` on any other path. It is emptied
before the first base too, and at the end of a build in which a stage changed
it (`Kaniko::finish`), then removed with the build directory; a build that
fails before its first base touches neither.

## Walk, hash and diff

- **Parallel walk:** one task per directory on a `rayon` pool, over `openat2`
  and `getdents64`
  ([ADR-0039](../adr/0039-kaniko-mode-walk-and-hash-crates.md)). A directory
  is opened relative to its parent's descriptor (`openat2` with
  `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS` and
  `RESOLVE_NO_XDEV`), never by a full path; each entry gets `statx(dirfd,
  name, AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT)`. A subdirectory is opened in
  its own task from its parent's shared descriptor, so waiting directories
  hold no descriptor of their own.
- **Workers:** 8 by default; 16 and 32 were slower on the benchmark machine.
  `--snapshot-workers N` changes the count and never a layer.
- **Depth:** an entry whose path below the root is longer than `PATH_MAX`
  (4095 bytes) fails the walk with the overlay exporter's error. Each
  directory task and each change keeps its full path, so a chain a `RUN` made
  far deeper would make the walk's memory grow with the square of its depth
  (T15); such a tree cannot be exported anyway. On `/`, a stage kept as a tar
  for `COPY --from` walks only below the kept paths (`Walker::diff_below`), so
  a deep tree elsewhere does not stop it; keeping the whole root, as an image
  stage does, refuses such a tree like every layer walk.
- **Mount guard:** an entry whose `stx_mnt_id` differs from the root's is
  skipped, a second line of defence after the ignore list.
- **Hash:** xxh3-128 of the content, plus a hash of the xattrs excluding
  `*.overlay.*`. xattrs are hashed on regular files and directories only,
  through the descriptor the walk already holds. A regular file is opened only
  after its `O_PATH` pin matches its `statx` (type, inode, device, mount),
  then through `/proc/self/fd`, so nothing swapped in is ever opened; hashing
  reads at most `size + 1` bytes.
- **Stat cache:** key `{dev, ino, size, mtime, ctime, mode, uid, gid}`. An
  entry is reused only if `max(mtime, ctime)` is more than 2 s older than the
  start of the scan (racy window). The cache is the previous walk's record at
  the same path in the path arena, so it costs no memory of its own.
- **Diff rule:** added or modified when any of kind, mode, uid, gid, size,
  mtime, rdev, symlink target, content hash or xattr hash changed. A deletion
  becomes one whiteout at the topmost deleted path. The diff is computed
  during the walk (entries seen are marked; unseen old entries are deletions)
  rather than by comparing two maps afterwards, and entries are stored
  compactly (path arena): a 269k-entry map took about 200 MB.
- **`--snapshot-mode`** picks what is compared, as in kaniko
  ([ADR-0023](../adr/0023-mode-flag.md)): `full` (default) everything above,
  content included; `redo` mode, mtime, size, uid and gid; `time` mtime only.
  `redo` and `time` read no file, so the stat cache does not apply to them. In
  every mode a change of the entry's type is a change, which kaniko's `time`
  mode misses when the mtime matches.

## Ignore list

Built once per build in `rnk_snapshot::kaniko::ignore_list` on
`rnk_rootfs::IgnoreList`, from physical paths:

- the mount points below the root, read from `/proc/self/mountinfo` (not the
  root itself): on a host usually none; in a container `/proc`, `/dev`,
  `/sys` and their submounts, `/etc/hosts`, `/etc/hostname`,
  `/etc/resolv.conf` and volumes;
- `--ignore-path`;
- the state directory, only when it lies inside the root (`/`), by its
  canonical path opened without following a symlink;
- as kaniko (ADR-0038): all of `/ranikust` (an image may keep the binary
  there, as kaniko keeps `/kaniko/executor`), `/etc/mtab` and
  `/tmp/apt-key-gpghome`;
- always `/proc`, `/sys` and `/dev`, and the runtime's markers `/.dockerenv`
  and `/run/.containerenv`, which are not image content: emptying `/` keeps
  them, so a later build still detects its container.

The root and state directory handed to the list must be the kernel's names for
the directories in use (`readlink` of their fds), absolute and normalized. A
state directory that is the root or an ancestor of it is refused, and so is
one that is a directory of the root reached through another mount (a bind
mount found by the mounts' device and `fs_root` in mountinfo), or that holds
the root that way, since the list knows it by path only.

**`/var/run` is not on the list**, unlike kaniko: it is ignored only where it,
or something below it, is a mount point, as every mount point is. An image's
`/var/run -> /run` symlink (alpine 3.12 and later) is extracted and kept
(ADR-0052, D4); kaniko's `--ignore-var-run` and `--whitelist-var-run` would
change nothing and are refused (ADR-0049). Where the container's `/var/run` is
itself a link (`../run`), a base layer with `var/run/.wh..wh..opq` but no
`var/run/` entry of its own fails the build: the opaque marker's directory is
a symlink, which live extraction never follows, so nothing under `/run` is
emptied (fail closed; with the layer's own `var/run/` the link is replaced by
its directory).

**Matching** is kaniko's (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1261-1281`):
a path is ignored when its leading components match an entry, so the walk
never descends into an ignored path. `--ignore-path` entries are globs per
component, as Go's `filepath.Match` (`/var/cache/*` ignores what is in
`/var/cache`, not the directory); `/tmp/apt-key-gpghome` ignores only what is
below it (kaniko's `PrefixMatchOnly`). The parents of an ignored path get no
special treatment: kaniko writes a parent directory only when it changed
itself or holds an added file (`pkg/snapshot/snapshot.go:340-445`), and the
walk does the same.

**Extraction of base layers into the live root** (`rnk_rootfs::extract_live`)
checks each entry at its name and at its physical path, so a symlink in the
image cannot lead a layer into the state directory. kaniko resolves only the
physical path (`securejoin`, `fs_util.go:410-433`), so an entry named below
`/var/run` is skipped here while kaniko writes it through the symlink.

- Ignored entries and entries on another mount are skipped.
- A whiteout of an ignored path, or of a directory holding one, is skipped as
  kaniko skips it (`:248-266`).
- An entry replacing a directory holding an ignored path removes the rest and
  is skipped (`FF_KANIKO_PRESERVE_MOUNTED_PATHS`, `:343-370`).
- An opaque whiteout keeps ignored paths and mounts.
- The physical path is read from `/proc/self/fd` of the directory fd the entry
  is then written through. Missing directories are created one name at a time
  relative to it, without following a symlink or leaving the mount, so a
  concurrent swap of a name for a symlink into an ignored path cannot redirect
  a write; directory metadata set at the end re-checks the parent's physical
  path.
- Device nodes and FIFOs are not created, as kaniko creates none
  ([ADR-0041](../adr/0041-live-root-extraction-skips-device-nodes.md)); the
  image keeps them through its unchanged base layers.

## Safe deletion

Emptying a root (end of stage, release of a stage directory) recurses through
file descriptors:

- open each directory with `openat(O_DIRECTORY | O_NOFOLLOW)`, then compare
  `ino` and `mnt_id` from `statx` with the opened fd to detect a swap;
- remove entries with `unlinkat`; a symlink is unlinked, never followed;
- keep entries in the ignore list, entries whose `mnt_id` differs from the
  root's, and entries with `STATX_ATTR_MOUNT_ROOT`;
- **never decide by `st_dev` alone**: a bind mount of the same filesystem has
  the same `st_dev`, and its contents would be wiped;
- independent of `PATH_MAX` and of `RLIMIT_NOFILE`: it holds a fixed number
  of directory descriptors whatever the depth
  ([rootfs](rootfs.md#deletion)). The snapshot walk (ADR-0038) and `COPY`
  hold descriptors per level, so kaniko mode raises the soft limit to its hard
  limit at start-up (`RUN` processes get the original limit back).

## Host safety checks

ranikust never fills or empties a directory the operator names (ADR-0050,
which replaces the `--workdir` checks of ADR-0038 and ADR-0047). The root is
`/` in a container, chosen by kaniko's heuristic (ADR-0048), or a directory
per stage made for the build (ADR-0059). Before anything is written,
`ranikust::statedir::prepare` checks the state directory:

1. Before it is created, its nearest existing directory and every real
   parent, walked from a descriptor (`..`), never by path, are owned by root
   (or the user) and not writable by group or others, except root's sticky
   directories.
2. It is created `0700`: each missing name with `mkdirat` below the
   descriptor of the directory checked or made before it, and opened from it
   (`RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV`), never by path. A symlink planted
   at a missing name in `/tmp` meanwhile makes nothing elsewhere, and a name
   that appeared meanwhile must pass the rule of step 1. It is owned by the
   user, not writable by others, and set to `0700` if others may search it;
   then the whole chain is checked again from its descriptor.
3. Before its mode is touched: never `/`, and only a directory just created,
   empty, or holding the store's regular `.ranikust-state` marker (or
   `.ranikust-pruning`, what a `prune --all` cut short leaves, which the store
   then refuses itself); unmarked, none of the thirty system directories of
   `statedir::SYSTEM_DIRS` (`/mnt`, `/srv`, `/opt`..., by device and inode);
   one marked only with the old `.ranikust-root` is refused, named. After
   `chmod 0700` the mode must read back so; `builds/` likewise.
4. On `/`, the store then refuses a marked directory too, and anything but its
   own marker and leftovers (ADR-0077; [locks](cache-and-store.md#locks)).

`ranikust prune --all` checks an existing state directory by these rules
through `statedir::check`, which makes nothing and sets no mode: its chain
as step 2 checks it again, never `/`, and its contents as
`Store::check_prune`, which refuses what `Store::prune_all` would; not the
system directories, since it empties only a marked directory
([prune](cache-and-store.md#prune)).

`ranikust` holds kaniko mode on `/` with `flock` on a descriptor of `/` for
the whole build, and refuses kaniko mode outside a container where root may
not mount; `rnk-rootfs` provides the primitives.

## Exporter requirements specific to the walk

The common rules are in
[overlay-snapshot](overlay-snapshot.md#exporting-a-diff-as-an-oci-layer). The
walk's export is `WalkDiff::export` (`rnk-snapshot`,
`src/kaniko/export.rs`), after kaniko's `writeToTar` (`kaniko@44aa7ddc0
pkg/snapshot/snapshot.go:341-391`):

- **Parent headers.** A walk diff often holds a deep file whose ancestors did
  not change (the 1,813 pnpm-store files of experiment 06 under `/root`), and
  those ancestors must still be emitted, as kaniko does. kaniko adds every
  parent of an added path and of a whiteout, but never the root, with its
  metadata read at export time (`addParentDirectories`, `:432-447`;
  `addedPaths[RootDir]`, `:348`); the exporter does the same through
  `rnk-layer`'s parent lookup. `WalkDiff::root` is never written.
- **Whiteouts.** One per topmost deleted path, mode 0, owner 0:0, time 0, as
  kaniko's `Tar.Whiteout` (`pkg/util/tar_util.go:162-176`). What a directory
  replaced by a non-directory held gets no whiteout, as kaniko's
  `parentPathIncludesNonDirectory` (`:393-408`).
- **Hardlinks.** One whose target lies in a lower layer (`ln /etc/passwd /x`)
  is written as a regular file: the walk reports no link-count change, so the
  target is not in this layer, and `rnk-layer` links only entries of one
  layer. Names of one inode that are both in the layer become a file and tar
  hardlinks to it.
- **xattrs.** Only `security.capability` is kept (ADR-0032). Sockets are left
  out, as kaniko (`pkg/util/tar_util.go:74-77`), but without the parents
  kaniko still writes for them (ADR-0045).
- **What changed, nothing more.** The unchanged target of a changed symlink,
  which kaniko's `ResolvePaths` adds (`pkg/filesystem/resolve.go:69-98`), is
  not written, as BuildKit and overlay mode (ADR-0045). The exporter never
  resolves a symlink.
- **The root is live.** Every entry is pinned once with `RootPath::pin`: its
  physical path resolved with no symlink in a parent (a last one is pinned as
  the link itself), no `..` and without leaving the root's mount
  (`RESOLVE_IN_ROOT | NO_SYMLINKS | NO_XDEV`, `O_PATH | O_NOFOLLOW`). The
  pin's `statx` is checked against the walk's record: type, device, inode; a
  regular file's size; a file's or symlink's `mtime` and `ctime`, not a
  directory's, whose entry is written from the walk's record. The inode
  number alone is not enough, since a file deleted and created again can get
  the freed number back at once (ext4 on CI's aarch64 runner did).
  - The link target, the xattrs and the content are read from that pin, the
    content through a reopen of exactly that inode, so a parent swapped for a
    symlink or a mount, or a FIFO or device swapped in, is never opened.
  - In `full` mode the content written and the link target are hashed again
    (xxh3-128) and must match the walk's hashes, which also catches a swap
    within one timestamp tick and a file rewritten in place (unlike kaniko);
    `time` and `redo` read no content in the walk, so there the `statx` check
    is all.
  - An entry changed since the walk fails the export. Names of one inode in
    the walk that disagree on type, size, mode, owner, mtime or content hash
    (an inode number reused during the walk) are written as separate files,
    not linked. Nothing may write the root between walk and export.
- **A `RUN` that changes nothing** produces an empty tar; the builder adds no
  layer for it, only its history entry with `empty_layer: true`, as kaniko
  (ADR-0058).
- **Round trip.** Applying the base layer and a diff's layer rebuilds the
  walked tree: a property test on random trees compares content, modes and
  mtimes, not hardlink structure across layers (ADR-0012).

## Per-stage directories

Kaniko mode outside a container
([ADR-0059](../adr/0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md)),
`rnk_snapshot::KanikoStages`.

- **Location:** `<state>/builds/<build-id>/stages/<n>/rootfs`, in the build's
  private area of the state directory (the stage directory `0700`, its
  parents checked by the set-up, ADR-0050), never on the machine's own root.
  Each is made with `mkdirat` and opened with `openat(O_PATH | O_DIRECTORY |
  O_NOFOLLOW)` on the same parent descriptor (`RootPath::create_sub_root`): it
  must be empty, owned by the effective uid and on the parent's mount, else it
  is refused.
- **Creation:** a stage on an image extracts its base layers into its
  directory (a blob of the store, or streamed and checked at its end,
  ADR-0055; a failure deletes the directory; device nodes and FIFOs left out,
  ADR-0041). `FROM scratch` and an image start at `0755` and the epoch unless
  a layer sets `./`. `FROM <stage>` copies the parent's directory
  (`rnk_rootfs::copy_filtered`: owners, modes, times, xattrs, hardlinks inside
  the tree; a file cloned with `FICLONE` where the filesystem shares extents,
  else only its data ranges, holes kept) with the parent root's owner, mode,
  times and xattrs (overlay's own left out), never a hardlink to it (ADR-0017).
  Device nodes, FIFOs and sockets a parent's `RUN` made are left out
  (`CopyOpts::skip_special`, ADR-0041).
- **Hand-over to the only reader**
  ([ADR-0061](../adr/0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md),
  [ADR-0062](../adr/0062-stage-directory-hand-over-decided-from-the-stage-graph.md)).
  From the stage graph the build counts, per stage, the stages that read its
  root after it is built: each `FROM` it counts one, each stage reading it with
  `COPY --from` or `RUN --mount from=` one (a stage doing both counts twice). A
  stage built `FROM` a stage whose count is exactly one takes that stage's
  directory over (`Snapshotter::prepare_last`, `take_base` in `rnk-builder`)
  instead of copying it; every other `FROM <stage>` copies. The choice never
  depends on scheduling, because a directory taken over is not byte for byte the
  copy: ext4 and xfs keep the size of a directory emptied in place, a copy makes
  it compact, and the walk compares directory sizes. The steps:
  1. `rnk_rootfs::prune` deletes in place what the copy leaves out (the ignore
     list, device nodes, FIFOs, sockets; through descriptors, the machine's own
     `/` and a mount below the root refused), sets each directory's times back,
     and removes overlay's own xattrs (`rnk_rootfs::is_overlay_own`, as the copy
     does).
  2. `RootPath::move_sub_root` moves `stages/<p>/rootfs` to `stages/<c>/rootfs`
     (`renameat2(RENAME_NOREPLACE)` between the stage directories' descriptors,
     device, inode and mount checked before and after).

  The result has the copy's entries, contents, owners, modes, modification
  times, xattrs, links and hardlinks. What a `RUN` can still tell apart from a
  copy, and no layer holds: access and change times, directory sizes, the
  order `readdir` lists a directory in, and inode flags the copy does not carry
  (`chattr` flags a non-privileged owner may set, such as `A` or `d`).

  It falls back to a copy of the pruned root when a file there has a hardlink
  the prune did not find inside the root (moved, it would reach outside), or
  when the move failed before anything moved (`EINVAL` without
  `RENAME_NOREPLACE`, `EBUSY`, `EXDEV`). After a hand-over or such a fallback
  the parent is read no more: its view, root, a second child and a layer are
  refused, and its release deletes an empty directory.
- **Special files and `COPY`** (scope of ADR-0060): here, unlike on `/`,
  `COPY` and `COPY --from` keep device nodes, FIFOs and sockets, as BuildKit,
  because every `RUN` enters the root `nodev` (the full sandbox). An image
  built off `/` can hold a device node that came from the context or another
  stage by `COPY`; ADR-0060's "the image never holds a device node a `RUN` did
  not make in its own layer" holds for kaniko mode on `/` only. Base layers
  and `FROM <stage>` copies leave them out in both.
- **Scheduling:** as overlay mode's stages: the stage graph and `--jobs`, no
  squashing, nothing kept or emptied between stages; a `COPY --from` reads the
  finished stage's directory as it is. A stage the target does not inherit
  (only read by `COPY --from`) is never walked (ADR-0038).
- **`RUN`:** the full sandbox ([sandbox](sandbox.md)), started with
  `Sandbox::start` as for overlay mode's mount points. The stage root is
  reopened by path for each `RUN` and must still be the directory the stage
  holds (`RunSpec::expect_root`): checked in the parent after the reopening
  and in the child after the bind.
- **Walk:** each stage has its own walker, made at its first walk and dropped
  when the stage ran its last instruction (`StageRootfs::done`): its record of
  the root and its thread pool do not outlive the stage's instructions. The
  ignore list of a stage root is the fixed paths and `--ignore-path`; nothing
  is mounted in a directory ranikust made.
- **Release:** safe deletion of the stage directory, through descriptors,
  crossing no mount; the rest goes with the build directory. A directory whose
  readers are all done (its own stage done too), and the root of a `COPY
  --from` image once the stage that read it is done, go at once to one release
  thread (ADR-0061); at the end the build hands it every root left and waits
  for it before the store closes. A second signal exits at once and leaves the
  rest to the next build's clean-up.
- **Cost:** one copy of the base per stage built `FROM` a stage, except where
  it is that stage's only reader and takes the directory over (a chain of
  stages copies nothing); the copy is the data on ext4 and tmpfs, a reflink on
  xfs and btrfs. The peak is the roots read at the same time plus the copies
  being made, not every stage's root: a directory goes once nobody reads it.
  Per-stage builds still hold more page cache than overlay mode, since each
  stage has its own extracted base and re-reads its writes to hash and tar
  them. The memory gate sees it only once it is resident; counting what a stage
  will re-read is not designed ([scheduler](scheduler.md#limits)).
- **Placed files:** `hosts`, `resolv.conf`, stub mount points and `/dev` nodes
  change the mtime of their parent, so `/etc` would enter every layer. The
  full sandbox records the parent's mtime before placing (m0) and after (m1);
  after `RUN`, if it is still m1 it restores m0, otherwise it keeps what `RUN`
  set (`rnk-sandbox`'s `swap` journal records, ADR-0043): the image's entry is
  renamed aside in the same directory and back after, never rewritten.

## The `--parallel-stages` path (bare `chroot`)

> **Not built:** the flag is refused with exit 2 (ADR-0060). A `RUN` that
> escapes a bare `chroot` reaches what ADR-0051 keeps out of its reach. A
> hardened variant (capabilities dropped after the `chroot`, no procfs) may be
> reconsidered only after a VM measurement of its escape vectors. What follows
> is ADR-0014's plan, kept for that case; the evidence is in experiments 12a,
> 13, 14, 15 and 16.

It would run only on request where mounting is impossible. `RUN` needs
`CAP_SYS_CHROOT`, which Docker grants by default; ranikust logs at build start
that `RUN` has no `/proc`.

- **No isolation between stages:** root in `RUN` can escape a bare `chroot`
  and read or write a sibling stage's root, including its secrets.
- **`/dev`:** `mknod` of `null`, `zero`, `full`, `random`, `urandom` and `tty`
  when permitted (6/6 on rootful Docker). Where `mknod` is denied `/dev` stays
  empty, and `> /dev/null` creates a regular file, which the walk ignores.
- **Process tree:** with no PID namespace, ranikust becomes a child subreaper
  (`PR_SET_CHILD_SUBREAPER`) and waits for every descendant of a `RUN`, such
  as a Gradle daemon, before removing the fake `/proc` [inferred].

### Fake `/proc`

A static snapshot of the files runtimes read to size themselves, on by default
on this path ([ADR-0038](../adr/0038-kaniko-mode-settings-for-phase-2.md)):
`/proc/{stat,cpuinfo,meminfo,cgroups}`, `/proc/self/{cgroup,mountinfo,stat}`,
`/sys/devices/system/cpu/{online,possible,present}`, and four cgroup files
(v2: `cgroup.controllers`, `cpu.max`, `memory.max`, `memory.high`; v1:
`cpu.cfs_quota_us`, `cpu.cfs_period_us`, `memory.limit_in_bytes`,
`memory.soft_limit_in_bytes`). That is fourteen files per cgroup version;
`memory.stat` is added only once a build fails without it. Which runtime reads
which file is in [experiment 12a](../research/experiments/12a-fake-proc.md).

- The set is derived at run time from ranikust's own `/proc/self/cgroup` and
  `/proc/self/mountinfo`, never hard-coded. The version is cgroup2 when
  `statfs("/sys/fs/cgroup")` says so or `mountinfo` has a `cgroup2` line. The
  v2 directory is the `cgroup2` mount point plus the `0::<P>` path minus the
  mount's root (column 4); the v1 directories are the `cgroup` mounts whose
  super options name `cpu` and `memory`, plus that controller's path minus the
  mount's root.
- A source file that does not exist is skipped. No cpuset files: the affinity
  mask passes through `chroot` by itself.
- Per `RUN`, each file is read completely from ranikust's own `/proc` and
  `/sys` just before `exec` (procfs reports size 0) and written into the
  stage's `rootfs/proc` or `rootfs/sys` with mode 0444, about 1 ms in all.
  After the whole process tree has exited, **everything** under both is
  deleted (a `RUN` can `mkdir -p /proc/...`) and their atime and mtime
  restored; their ctime cannot be, one more reason the walk always skips them.
- Never fake `/proc/self/{maps,exe,fd,status,environ}`, `/proc/<pid>`,
  `/proc/sys` or `cpufreq`; never set `LD_ORIGIN_PATH` or `LD_LIBRARY_PATH`;
  never substitute environment variables such as `GOMAXPROCS`: each produced
  silently wrong results in the experiments.
- What it fixes and what it cannot (`/dev/fd`, `/proc/self/exe` and so
  `$ORIGIN`, `ps`, live values, JVMs >= 21.0.12 and >= 17.0.20 under cgroup v2)
  is in experiments 12a, 14 and 15. Each stage sees the whole container's
  quota, as in BuildKit.

### Java

`JAVA_TOOL_OPTIONS` is injected under cgroup limits as
[ADR-0019](../adr/0019-java-tool-options-on-parallel-path.md) specifies, and
never includes GC flags. Before the first `RUN`, ranikust would warn when a
`*/bin/java` cannot start because `libjli.so` resolves outside its JDK (distro
JDKs from apt, apk or dnf; Temurin images run `ldconfig` and pass), and when
`memory.max` < 1792 MiB and a stage contains Java (the JVM picks G1 instead of
SerialGC and may be OOM-killed). JDKs a toolchain downloads during a `RUN`
(Gradle foojay, maven-toolchains) cannot be detected in advance.

### After a failed `RUN`

If stderr contains `libjli.so`, `readlink /proc/self/exe`,
`uv_resident_set_memory`, `/dev/fd/`, `Unpacked JDK archive does not contain a
Java home` or `getMainType`, ranikust would suggest running without
`--parallel-stages`. It never retries by itself: the failed `RUN` may have
written part of its output.

### Tool families

Node tools (Next 15 and 16, webpack, jest, tsc 5, esbuild, vite, npm, pnpm) and
Java on a Temurin image with one JDK are safe with the fake `/proc` (the latter
under cgroup limits only with `JAVA_TOOL_OPTIONS`). Several JDKs in one root, a
JDK from apt, apk or dnf, TypeScript 7 (`tsgo`), binaries calling `current_exe`
at start and scripts using `<(...)`, `/dev/fd`, `ps` or `/proc/<pid>` need
sequential mode.

## A cache hit

For the layer cache of Phase 4
([#128](https://github.com/tran4774/ranikust/issues/128),
[ADR-0113](../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)), the snapshotter
can put a cached layer into the open stage root in place of running the
step: `KanikoRoot::apply_layer` on the shared root, `KanikoStage::apply_layer`
per stage. The layer is made by `base_layer` or `streamed_layer`, as a base
layer is.

- **Extracted as a base layer.** A cached layer is content anyone who can
  push to the cache wrote, and its tag proves nothing. It takes the
  base-layer path, `rnk_rootfs::extract_live` with the stage's ignore list,
  under [its rules](#ignore-list), and its diff ID is checked at the end. On
  `/` a streamed layer is spooled and verified before its first entry
  reaches `/` ([streaming](streaming.md#credentials-and-failures)), so a
  blob whose digest or diff ID is wrong is a miss. The same pass, in the same
  decompression, checks the tar headers and `--limit` as the extraction will,
  by `rnk_rootfs::check_headers` ([rootfs](rootfs.md#extraction),
  [ADR-0123](../adr/0123-a-cache-hit-that-fails-makes-its-stage-start-again-once-with-no-cache.md)),
  so a hit with a header every extraction refuses from the header alone, or
  one over the limit, is a miss too (values only the kernel refuses still fail
  while writing,
  [#179](https://github.com/tran4774/ranikust/issues/179));
  per stage a layer is extracted as it arrives (ADR-0055). The scan ADR-0098
  plans for base layers in a user namespace (Phase 3) must cover cached layers
  too.
- **The walk catches up lazily.** Applying a layer marks the walk's record
  of the root stale; the next `begin_layer` of a stage whose layers are
  needed takes a new baseline (`forget`, then a full walk), so the next diff
  holds only that step's changes. A run of hits costs no walk; since a stage
  uses its hits only up to its first miss
  ([ADR-0114](../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)), it takes one baseline, as a cold build does:
  a warm or partial build walks no more than a cold one, toward
  [ADR-0117](../adr/0117-warm-and-partial-builds-are-no-slower-than-kanikos-cache.md)'s target.
- **No step goes on from a half-applied layer.** A failure before the layer's
  extraction began (the source refuses the request; on `/`, the spool pass)
  leaves the root as it was, usable. A later one leaves it
  broken: no layer, applied layer, view, keep or child stage (`FROM`,
  squashed or handed over) goes on from it, so no later layer is computed
  over part of a cached one. A broken stage directory is only released, and
  the builder runs the stage again from its base
  ([ADR-0113](../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)).
  On `/` it does the same, for a failure found while writing only (an entry
  refused under the base-layer rules, a full disk or another I/O error, or
  any failure while extracting a blob already in the store, which is never
  spooled): the new base empties `/`, as
  between stages, and the root is usable again then. The restart
  happens at most once, with no cache at all, and always rebuilds its base from
  the layers of its chain
  ([ADR-0123](../adr/0123-a-cache-hit-that-fails-makes-its-stage-start-again-once-with-no-cache.md),
  decided; the builder's part is planned in
  [#130](https://github.com/tran4774/ranikust/issues/130), not built). What a
  restart does when a layer of its chain is held by neither the store nor a
  registry is not decided [open: #171]; nor are five cases of the restart, among
  them the restart on `/` of a stage built `FROM <stage>`, whose parent's root
  `/` cannot give back, and, with a directory per stage, a failure that is not the
  blob's fault (a full disk, an I/O error) [open: #172].
- **Refused, nothing changed:** a layer open, a root not held (shared root)
  or handed over (per stage).
- Span `rnk_snapshot.apply`, the extraction's `rnk_layer.decode` and
  `rnk_rootfs.extract` inside; on `/` a streamed layer's spool pass adds one
  `rnk_layer.decode`, which the header check reads through.

## Lazy snapshots

A stage whose layers are not needed is not snapshotted at all: no baseline
walk, no walk and hash, no tar. The decision is taken once, from the stage
graph and the outputs, before the build (ADR-0038), and nothing changes it
during the build, so no deletion ever has to be detected after a skipped walk.

- A stage's layers are needed when it is the target, or when a stage whose
  layers are needed builds `FROM` it (squashed or not). Any other stage, used
  only through `COPY --from`, is skipped.
- On the shared root a unit whose image is kept for a later `FROM` is walked
  too, since that image is the only way to rebuild the root for the child
  (ADR-0044).
- `rnk_dockerfile::squash_for_shared_root` sets `layers_needed` per unit;
  `rnk_dockerfile::layers_needed` gives the rule for per-stage directories.
- These stages are exactly the ones whose layers the target's image inherits,
  and the only ones `ranikust` pushes or writes out (`in_target`, in every
  mode). Widening that set would send a stage the target only copies from,
  such as a secrets stage, to a destination.
- With `--cache` (Phase 4) more stages may need their layers; the rule stays.
  A further reason to snapshot a stage goes into `rnk-builder`'s
  `LayerDemand`, never into this rule.

## Failure handling

- Any host-safety check failing stops the build before anything is written.
- A deletion that meets an unexpected mount point, swap or permission error
  stops and reports the path; it never falls back to a path-based delete.
- Stubs are journaled before creation and removed on the next start after a
  crash ([sandbox](sandbox.md)).

## Open questions

- [Q-441](https://github.com/tran4774/ranikust/issues/30) How can kaniko mode on `/` keep a cache across builds safely?
- [Q-421](https://github.com/tran4774/ranikust/issues/29) Which performance conditions does Phase 6 measure?

## References

- [Survey 02](../research/survey/02-kaniko-mode-requirements.md) §2-§4;
  [survey 06](../research/survey/06-overlay-and-parallelism.md) §5.
- [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md): walk of
  269k entries in 0.10-0.15 s with the stat cache; deletion tests D1-D3
  (`st_dev`-only deletion wiped a same-filesystem bind mount; 14 dangerous
  paths refused).
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md): W3
  parallel -41 to -44 %; `/etc` mtime leak; hardlink store corruption; page
  cache 8.2-8.3 GB.
- [Experiment 12a](../research/experiments/12a-fake-proc.md),
  [experiment 13](../research/experiments/13-docker-rootful-vm.md) §4,
  [experiment 14](../research/experiments/14-real-builds-without-proc.md),
  [experiment 15](../research/experiments/15-java-next-concurrency.md),
  [experiment 16](../research/experiments/16-cgroup-v1-cpuset.md) §9-§10: fake
  `/proc` sets, Java, Next, concurrency (27/27 `RUN`s saw their own snapshot;
  no fake file reached a layer).
- [Experiment 08](../research/experiments/08-parallel-stages-lazy-export.md):
  skipping intermediate-stage snapshots saved 4.7-5.3 s (30-33 %) on W3.
