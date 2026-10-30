# kaniko mode

How ranikust builds when overlay is not available: the workdir is the root
filesystem, each layer is found by walking and hashing it, and the root is
emptied safely between stages. Also covers the per-stage directory variants,
including the opt-in `--parallel-stages` path with its fake `/proc`.

Decisions: [ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md),
[ADR-0017](../adr/0017-stage-rootfs-by-copy-not-hardlinks.md),
[ADR-0018](../adr/0018-no-proot-in-1-0.md),
[ADR-0019](../adr/0019-java-tool-options-on-parallel-path.md),
[ADR-0022](../adr/0022-common-state-dir.md),
[ADR-0023](../adr/0023-mode-flag.md),
[ADR-0038](../adr/0038-kaniko-mode-settings-for-phase-2.md).

## Variants

| Variant | When | Stage root | `RUN` | Stages |
|---|---|---|---|---|
| **Sequential** (default) | root, cannot mount (Docker default, ordinary pods) | the workdir: `/` in a container, `--workdir` on a host | directly on the root (`rnk-sandbox`'s `Isolation::Direct`): on `/`, the container's real `/proc`, `/dev` and network files; on a host `--workdir`, a `chroot` with ranikust's `hosts`, `resolv.conf`, `hostname` and `/dev` nodes placed and journaled, no `/proc` (`ADR-0043`) | one at a time, `--jobs` ignored |
| **Per-stage, sandboxed** | root, can mount, overlay unusable | one directory per stage | full sandbox ([sandbox](sandbox.md)) | DAG, `--jobs` |
| **Per-stage, bare chroot** | as Sequential, plus `--parallel-stages` | one directory per stage | bare `chroot`, no `/proc` | DAG, `--jobs` |

All variants share the walk, the diff, the exporter and safe deletion. They
implement the `StageRootfs` trait of `rnk-snapshot`: `/` (shared, the
scheduler runs one stage at a time) or a private directory (scheduled like
overlay mode).

## Invariants

- Every path inside the root is reached through `rnk-rootfs`'s `RootPath`
  (`openat2` with `RESOLVE_IN_ROOT`); symlinks and `..` cannot escape.
- The walk and the deletion never follow symlinks and never cross a mount
  point.
- The state directory is never walked, snapshotted or deleted; otherwise its
  files leak into layers. With the root `/` (in a container) it is
  `/ranikust/state`, inside the root and on the ignore list, as kaniko's
  `/kaniko` ([ADR-0022](../adr/0022-common-state-dir.md)); with any other
  root it is `<cwd>/.ranikust`, the directory ranikust was started in, and
  on the ignore list whenever it lies inside the root (`ADR-0038`).
  `--state-dir` replaces either default.
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
its image is kept for a later `FROM` (`save_image`); both are kept in the
state directory before the root is emptied. `rnk-sched` runs the units one
at a time, in plan order (`Placement::Shared`).

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
`/etc/mtab`, `/tmp/apt-key-gpghome` and, while `--ignore-var-run` is true
(the default), `/var/run`. `/proc`, `/sys` and `/dev` are always
ignored. Built in `rnk_snapshot::kaniko::ignore_list` on
`rnk_rootfs::IgnoreList`.

Matching is kaniko's (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1261-1281`): a
path is ignored when its leading components match an entry, so the walk
never descends into an ignored path; `--ignore-path` entries are globs per
component, as Go's `filepath.Match` (`/var/cache/*` ignores what is in
`/var/cache`, not the directory); `/tmp/apt-key-gpghome` ignores only what
is below it (kaniko's `PrefixMatchOnly`). Paths are physical: with
`/var/run -> /run`, `/var/run` ignores the symlink, not `/run`, which is
ignored only when it is a mount point, as in kaniko. The parents of an
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
refused. A host workdir's `.ranikust-root` marker is on the list.
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

Before touching a workdir outside a container:

1. Canonicalize, then refuse `/` and any directory that is, or is an
   ancestor of, a system directory (`/usr`, `/etc`, `/home`, `/var/lib`, …,
   30 in total, `SYSTEM_DIRS` in `crates/ranikust/src/workdir.rs`). Compare **`(dev, ino)`** of the directory and each of its
   ancestors, not strings: `//` passes a string comparison.
2. Open the workdir with `openat2(RESOLVE_NO_SYMLINKS)` and take
   `flock(LOCK_EX | LOCK_NB)`: one build per kaniko-mode workdir.
3. A non-empty workdir without a regular file `.ranikust-root` is refused; a
   marker that is a symlink is refused; an empty workdir gets a marker,
   right before the build first writes to it, and loses it again if
   anything else appeared meanwhile. A workdir holding only mount points
   is not empty: it needs the marker.

`ranikust` runs these checks during process set-up and holds the lock for
the whole build; `rnk-rootfs` provides the primitives (`ADR-0038`). It also
raises `RLIMIT_NOFILE`'s soft limit to the hard limit for the walk; `RUN`
processes get the original limit back (`NofileLimit::current()` read before
the raise, `Sandbox::set_nofile_limit`, `ADR-0043`). Every later step reaches the
workdir through the descriptor that was checked, never by its path.

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
  record (type, device, inode). The link target, the xattrs and the
  content are read from that pin, the content through a reopen of exactly
  that inode, so a parent swapped for a symlink or a mount, or a FIFO or
  device swapped in, is never opened. An entry changed since the walk
  fails the export. Names of one inode in the walk that disagree on type,
  size, mode, owner or mtime (an inode number reused during the walk) are
  written as separate files, not linked.
- A `RUN` that changes nothing produces an empty layer, an empty tar as
  kaniko's `TakeSnapshotFS` writes; `WalkDiff::is_empty` tells the caller.
- Applying the base layer and a diff's layer rebuilds the walked tree:
  a property test on random trees, which compares content, modes and
  mtimes but not hardlink structure across layers (`ADR-0012`).

## Per-stage directories

- **Location**: `builds/<build-id>/stages/<n>/rootfs` in the build's private
  area of the state directory, never on the container's own root [inferred;
  see [cache-and-store](cache-and-store.md)].
- **Creation**: extract the layers, or copy from the extracted store with
  reflinks where available; never hardlink to the store (ADR-0017).
- **Release**: safe deletion of the directory.
- **Placed files**: `hosts`, `resolv.conf`, stub mount points and `/dev`
  nodes change the mtime of their parent, so `/etc` would enter every layer.
  Record the parent's mtime before placing (m0) and after (m1); after `RUN`,
  if it is still m1, restore m0, otherwise keep what `RUN` set. Built for
  the sequential `Direct` `RUN` on a host workdir (`rnk-sandbox`'s `swap`
  journal records, `ADR-0043`): the image's entry is renamed aside in the
  same directory and back after, never rewritten.

## The `--parallel-stages` path (bare `chroot`)

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
