---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0101: A build runs from a sealed copy of ranikust's binary: a sealed memfd, else an `O_TMPFILE` copy

## Context and Problem Statement

Every process ranikust forks runs its binary until it executes something
else: a `RUN` child until `execve`, a full-sandbox `RUN`'s init
([ADR-0080](0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md))
for good. Their `/proc/<pid>/exe` is that binary, and a command whose `#!`
line names `/proc/self/exe` makes the kernel run it inside the sandbox
(threat T7, section 6 of the [threat model](../security/threat-model.md)).
[ADR-0082](0082-full-sandbox-binds-the-binary-read-only-over-itself.md)
decided to bind the binary read-only over itself in the full sandbox and to
keep today's barriers in `Direct` on `/`. Building it
([#32](https://github.com/tran4774/ranikust/issues/32)) showed that the bind
changes nothing: `/proc/self/exe` names the file the process was executed
from, on the mount it was executed from; a later bind over the path or a new
mount namespace does not change it, and a fork inherits it. Only an `execve`
of another file does. runc does not bind today. From v1.0.0-rc7, the
release with its fix of CVE-2019-5736, it executed `runc init` from a
read-only bind of the binary where it could make one, else from a sealed
copy; it dropped the bind in 1.1.15 and 1.2.0, and since 1.2.0 the parent
makes the copy. How does ranikust give every process it forks a binary no
`RUN` can write?

## Considered Options

- (x) At start-up, run again from a sealed copy of the binary, made as
  runc's `CloneSelfExe` makes it: a read-only overlay mounted nowhere where
  ranikust may mount (no copy), else a sealed memfd, else an `O_TMPFILE`
  copy. The same in the full sandbox and in `Direct` on `/`.
- (y) The same without the overlay: a sealed memfd, else an `O_TMPFILE`
  copy, everywhere.
- ADR-0082's read-only bind over the binary's path: does not change
  `/proc/self/exe` (measured, below).
- Keep the three barriers everywhere (ADR-0082's option (d)): the last one,
  `ETXTBSY`, is a kernel behaviour that a future kernel may drop.

ADR-0082's option (a), a memfd re-exec, is (y); its option (c), a
trampoline executed per `RUN`, would leave the init a copy of the installed
binary.

## Decision Outcome

Chosen option: (y). At start-up a build runs again from a sealed memfd copy
of ranikust's binary, else an `O_TMPFILE` copy; when neither works it
builds on from the installed binary with today's barriers and a warning.
Kaniko mode on `/` refuses a binary on a writable mount other than `/`'s,
reached by one, or on one it cannot tell, sealed or not, and a PID
namespace shared with a process of another mount namespace that ranikust
reaches through `/proc` when it starts (below).
The maintainer chose (x) first, 2026-11-04 ("tất cả theo khuyến nghị", all
as recommended). Building it showed the overlay's limits; asked whether to
keep the overlay first, the maintainer answered on 2026-11-04: "Luôn dùng
memfd (khuyến nghị)" (always the memfd, as recommended). Why:

- The overlay's file is the installed file: the copy runs that file's own
  pages, and while it runs the kernel no longer refuses a write to the
  installed file [measured]. An in-place rewrite of the installed binary (a
  `cp` over it, an upgrade that does not rename) would break the running
  build, and a `RUN` that reaches the installed file, as in kaniko mode on
  `/`, could change the code ranikust runs. With the memfd the build never
  runs the installed file again.
- The overlay would need a gate: it is safe only where no `RUN` reaches the
  installed file, so only where the build's own mount check passes, run in
  a child, since `fsmount` works where `mount(2)` does not (Docker
  `--cap-add SYS_ADMIN` under AppArmor's `docker-default`) [measured]. The
  memfd needs no mount and no gate.
- The overlay would be the one overlay outside `rnk-snapshot`, an exception
  to `rnk-sandbox`'s boundary. The memfd keeps the boundary as it is.

The decision:

- **When.** For `build` only, after the flags are parsed and the kernel
  checked, while the process is single-threaded and has read nothing else.
  The copy starts over from `main` with the same arguments, environment and
  descriptors. `version`, `--help` and a refused command line make no copy,
  so a `#!/proc/self/exe` script is still refused by the command line
  before anything else.
- **The memfd.** `memfd_create` with `MFD_EXEC` (without it before Linux
  6.3), the binary copied in, sealed `F_SEAL_SEAL|SHRINK|GROW|WRITE`.
  `vm.memfd_noexec=2` refuses it, and the next way is tried.
- **The `O_TMPFILE` copy.** In `TMPDIR` (else `/tmp`), `O_EXCL` so it can
  never get a name, run from a read-only descriptor. It has no seal: only
  `ETXTBSY` keeps the running copy from being written, as for the installed
  binary today, but such a write would not reach the installed binary.
  Never made directly on FUSE (`TMPDIR`'s own file system type is read),
  whose daemon could serve other pages when the copy runs than those
  written. Some file systems refuse `O_TMPFILE` (overlayfs on
  Ubuntu's Linux 6.8, below): the build then goes on warned.
- **Writes outside the state directory.** Only the `O_TMPFILE` copy, a file
  with no name in `TMPDIR`, made before the state directory is checked and
  gone with the build. The memfd writes no file.
- **The run of the copy.** `execveat(<n>, "", AT_EMPTY_PATH)` on the copy's
  close-on-exec descriptor, not `execve("/proc/self/fd/<n>")`: the kernel
  refuses to hand a file the exec closes to an interpreter (binfmt_misc,
  as qemu-user registers, with or without its `O` flag, or a `#!` line),
  before the exec's point of no return (`ENOENT`), and the next way is
  tried, where `execve` let the process die silently without `O`
  [measured]. `RANIKUST_SEALED_COPY=<way> <mount>` is added (then `; ` and
  why each way before it failed), `<mount>` being where the installed binary
  lies (below), and the core-file limit ranikust started with is given
  back, which the copy records again
  ([ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)).
  Seeing the variable, the copy makes no copy (no loop), checks
  `/proc/self/exe` (the memfd's seals, or no link), takes its name from
  `argv[0]` again, and logs the way, the ways passed over and the binary's
  mount at debug.
- **Fallback.** When both ways fail, or the copy's check fails, the build
  goes on with a warning and the three barriers ADR-0082 lists. No refusal,
  but the one below. Only when a copy did not run and the core-file limit
  cannot be set back to 0 does the build stop, as when ADR-0051's limit
  cannot be set.
- **A binary on a writable mount, in kaniko mode on `/`.** Before any copy,
  ranikust measures which mount the binary it started from is on (`statx`
  `STATX_MNT_ID` against `/`'s, `statvfs` `ST_RDONLY`) and, on another
  read-only mount or on a root that is not an overlay, whether another
  writable mount reaches the file: whether it has more than one hard link,
  and whether another writable mount of its file system in
  `/proc/self/mountinfo` (the `major:minor` of the line of the binary's
  own mount, not `statx`'s `st_dev`, which differs on btrfs subvolumes)
  has a root that contains the binary's path in that file system (its own
  mount's root joined with its path below that mount, compared by
  component) or, for a root that is a file, is the binary's inode, whatever
  its name. The binary's name and those mounts' mount points are reached
  from `/` through `RootPath` by `statx` only, never opened, with no
  symlink on the way (a path the kernel printed has none), and each mount
  point is checked to be its mount's root (its id and
  `STATX_ATTR_MOUNT_ROOT`); a mount another one covers, told from the
  table alone, is not looked up, and the table is read again once the
  walk is done and must not have changed. The table's `//deleted`, the
  kernel's mark of a root whose name was removed, is read as a mark, never
  a component. The walk of the writable mounts runs only where the build
  may be kaniko mode on a container's `/`, the only one that reads it:
  as root, without `--mode=overlay`, where ranikust finds itself in a
  container (kaniko's test, ADR-0048); a host build looks up no mount
  point, and a binary the walk would have weighed is refused should
  kaniko mode on `/` follow all the same. It passes the answer to the
  copy in the variable; a run that ends without a checked copy measures
  the file it runs from instead. Kaniko mode on `/` refuses, before
  anything is written, a binary on a writable mount other than `/`'s (a
  file or a directory bound without `:ro` from outside the container or
  from inside it, a volume), with the guidance to mount it read-only
  (Docker `:ro`, Kubernetes `readOnly: true` on the `volumeMount`); a
  binary a writable mount reaches (bound `:ro` from a tree also bound
  read-write), with the guidance to put it outside the mounted tree, to
  mount that tree read-only, or to mount a single-link copy read-only from
  outside it; a writable bind of a link of the binary, even one whose name
  was removed, with the same guidance and to bind no link of it
  read-write; and a file with other hard links, with the guidance to give
  the binary a single link (a single-link copy mounted read-only, or, in
  the container's root, no hard-link alias of it). It also refuses, failing
  closed, one whose mount it cannot tell: a mount id of 0, which
  qemu-user's `statx` gives for every file and Linux 6.8 and 7.0 give a
  memfd [measured]; a mount table that cannot be parsed, changes while it
  is walked, does not place the file (its own bound name removed among
  them) or does not type a FUSE root, a writable mount of its file system
  that is a directory whose name was removed or that cannot be reached at
  its mount point (another mount covering it among them), or a file with
  no link left; a binary not weighed because ranikust did not find itself
  in a container when it started; or a failed call (the mount table
  unreadable among them, the second read too, whatever the walk found),
  each with its own guidance; a fallback through `/proc/self/fdinfo` is
  [#95](https://github.com/tran4774/ranikust/issues/95). A binary in the
  container's own root builds when that root is an overlay, where a write
  copies the file up and leaves the image's layers as they are: overlayfs
  (`statfs` magic), as Docker's and containerd's default storage gives, or
  fuse-overlayfs (FUSE whose own line in `/proc/self/mountinfo` has the
  type `fuse.fuse-overlayfs`, a type its mounter declares: the outer
  container runtime, trusted), as rootless podman's can. On any other root
  (Docker's btrfs, zfs or vfs drivers, containerd's native snapshotter, a
  directory root, as podman `--rootfs`, systemd-nspawn or LXC give) it is
  weighed like one on another read-only mount, the root mount itself not
  counting as a writable mount that reaches it. One on a read-only mount
  no writable mount reaches builds; overlay mode and kaniko mode per stage
  do not check, since no `RUN` reaches the binary there. Why, as
  hardening: a `RUN` there is root on the container's `/`, and once the
  build no longer executes the installed file, `ETXTBSY` no longer guards
  it, so the binary must not be reachable through a writable mount other
  than the container's root. The check reads the container's own mount
  table, so a writable view of the binary that this table does not show,
  or one through a writable mount of another file system that the table
  shows without placing the binary on it, is not detected: an accepted
  residual risk, with the guidance to mount the binary from a path that no
  writable mount shares, narrowed by the refusal of a process of another
  mount namespace that ranikust reaches when it starts (the next item).
  The maintainer decided, on 2026-11-04 unless an
  item gives another date (the second to fourth
  [recorded on #32](https://github.com/tran4774/ranikust/issues/32#issuecomment-6626333197),
  the fifth and sixth
  [in a later comment](https://github.com/tran4774/ranikust/issues/32#issuecomment-6627795316),
  the next four
  [in a third](https://github.com/tran4774/ranikust/issues/32#issuecomment-6633392547),
  after the second host-safety pass the next three
  [in a fourth](https://github.com/tran4774/ranikust/issues/32#issuecomment-6634131493),
  and the next
  [in a fifth](https://github.com/tran4774/ranikust/issues/32#issuecomment-6635199388);
  the next two
  [in a sixth](https://github.com/tran4774/ranikust/issues/32#issuecomment-6639451756),
  the next
  [in a seventh](https://github.com/tran4774/ranikust/issues/32#issuecomment-6650030126),
  and the last two after the red team's review of the pull request):
  - after the host-safety review of the pull request that closes #32, to
    refuse a binary bound read-write from outside the container:
    "Từ chối, hướng dẫn `:ro` (khuyến nghị)" (refuse, with guidance to
    mount it `:ro`, as recommended);
  - to refuse a binary whose mount cannot be told, failing closed as
    ADR-0054 refuses a `/proc/sys` whose state cannot be told:
    "Refuse (khuyến nghị)";
  - to refuse a binary bound from inside the container, another mount of
    the same file system, like one from outside (ranikust cannot tell
    where a mount comes from):
    "Accept, refuse it too (khuyến nghị)";
  - not to read the mount id from `/proc/self/fdinfo` yet, filed as
    [#95](https://github.com/tran4774/ranikust/issues/95):
    "Not yet, task issue linked to #68 (khuyến nghị)";
  - after the review of these refusals, to refuse too a binary whose file
    is reachable through another writable mount, read from
    `/proc/self/mountinfo`: a writable mount of the binary's file system
    whose root contains the binary's path, or a file with more than one
    hard link, with the guidance to put the binary outside the mounted
    tree or to mount that tree read-only, failing closed when the mount
    table cannot be read or parsed: "Refuse via mountinfo (khuyến nghị)";
  - to give each cause of the refusal of a binary whose mount cannot be
    told its own guidance, not the `:ro` one (a mount id of 0: run
    natively from a regular file; a failed measurement: mount `/proc` and
    allow `fstatfs`), which confirmed the code: "Accept, per-cause
    guidance (khuyến nghị)";
  - to accept as a residual risk a writable view of the binary that the
    container's own mount table does not show, recorded in general terms
    with the guidance to mount the binary from a path that no writable
    mount shares: "Chấp nhận, ghi chung chung (khuyến nghị)";
  - to take the binary's device from its own `/proc/self/mountinfo` line,
    not from `statx`'s `st_dev`, which differs on btrfs subvolumes:
    "Đồng ý, dùng mountinfo (khuyến nghị)";
  - to refuse as unplaced a binary file with no link left (nlink 0):
    "Đồng ý, từ chối (khuyến nghị)";
  - to name `/proc/self/mountinfo` too in the guidance for a failed
    measurement: "Nêu rõ mountinfo (khuyến nghị)";
  - to check a binary in the container's root too, for its links and the
    other writable mounts that reach it, when `/` is not overlayfs:
    "Kiểm tra cả khi root không phải overlay (khuyến nghị)", adding
    "Overlay roots (Docker, containerd) are not affected";
  - in [the comment with the second to fourth](https://github.com/tran4774/ranikust/issues/32#issuecomment-6626333197),
    "ADR-0101 and the threat model describe the risk of a writable binary
    in general terms"; in the fourth comment, to keep naming the
    configurations that are refused, without a mechanism, in the book,
    `SECURITY.md`, the threat model and the refusal messages: "Giữ, vì
    người dùng cần biết (khuyến nghị)";
  - to accept the measurement the marker carries as it is: whoever starts
    ranikust sets the variable and is trusted, a `RUN` cannot set it (its
    environment is the Dockerfile's only), and the copy's run believes it
    only once `/proc/self/exe` passes its check: "Chấp nhận, chỉ operator
    làm được (khuyến nghị)";
  - to count a container root on fuse-overlayfs (rootless podman) as an
    overlay, because a write copies up, while the other roots that are not
    overlays (Docker's btrfs, zfs or vfs drivers, containerd's native
    snapshotter, podman `--rootfs`, nspawn, LXC) keep the link and
    writable-mount checks, and the guidance asks for a binary with a
    single link: "fuse-overlayfs tính là overlay (khuyến nghị)";
  - on 2026-11-05, to widen the residual risk accepted above, still in
    general terms, to a writable view of the binary through a writable
    mount of another file system that the container's mount table shows
    without placing the binary on it, the guidance unchanged: "Ghi rộng
    ra, vẫn chung chung (khuyến nghị)";
  - on 2026-11-05, to record here that the cases ranikust cannot tell are
    refused (fail closed), and that the mount points are reached through
    `RootPath` by `statx` only, never opened: "Ghi vào ADR-0101 trên nhánh
    (khuyến nghị)";
  - on 2026-11-05, after the third host-safety pass, to walk the writable
    mounts only where the build may be kaniko mode on a container's `/`,
    so that a host build looks up no mount point: "Chỉ đo khi có thể là
    kaniko trên / (khuyến nghị)" (measure only where it may be kaniko on
    `/`, as recommended);
  - on 2026-11-05, after the red team's review, to refuse in #32, before
    the build, kaniko mode on `/` where a visible process lives in another
    mount namespace and ranikust can open its `/proc/<pid>/root`, so that
    a `RUN` could too, with the guidance not to share the PID namespace;
    an unreachable process is not counted, the check runs once at
    start-up, and a process that appears later stays a residual risk:
    "Từ chối trong #32 (khuyến nghị)" (refuse in #32, as recommended);
  - on 2026-11-05, asked whether that refusal should also count the
    processes ranikust cannot open as it starts, to keep counting only
    those it can open then and to record the others as a residual risk,
    the guidance unchanged (give the build container a PID namespace of
    its own): "Ghi là rủi ro còn lại (khuyến nghị)" (record it as a
    remaining risk, as recommended).
- **A process of another mount namespace, in kaniko mode on `/`.** A `RUN`
  there shares the container's PID namespace. Where another container
  shares it too (Kubernetes `shareProcessNamespace: true` or
  `hostPID: true`, Docker or podman `--pid container:<name>` or
  `--pid=host`), a `RUN` could reach the files of that container's
  processes through `/proc`, which the build container's mount table does
  not show. Kaniko mode on `/` refuses to build, with the binary's check
  and before anything is written, when it finds, once as it starts, a
  process of its PID namespace with a thread in another mount namespace
  (the device and inode of the thread's `ns/mnt`, opened `O_PATH`, against
  its own) whose root it opens (`O_PATH`). The guidance is to give the
  build container a PID namespace of its own, shared with no other
  container.

  The scan counts only the processes ranikust can open as it starts. Each
  thread of each process is probed (`/proc/<pid>/task/<tid>`), since
  a thread may have a mount namespace and credentials of its own and the
  first one may have exited, all through one descriptor of `/proc/<pid>`,
  so that a pid reused meanwhile is not taken for the process; a process
  whose threads it may not list is probed as one, through `/proc/<pid>`.
  A process or thread it may not look into or whose root it may not open
  (`EACCES`, `EPERM`) is not counted, such as Kubernetes' pause container
  at uid 65535; one gone meanwhile (`ENOENT`, `ESRCH`) is skipped. Any
  other error (named with the step that failed),
  `/proc` or ranikust's own namespace that cannot be read, or more than
  32768 threads (a process with none left counting as one) refuse the
  build (fail closed), each with its own message and guidance. A
  `--pid=host` container is refused by the same rule when ranikust
  reaches a process of the host or of another container. ranikust looks
  with its start-up privileges, before it trims them to a `RUN`'s
  (ADR-0078), so where it reaches a process a `RUN` would not (a
  container given `CAP_SYS_PTRACE`, a Dockerfile whose `RUN`s are not
  root), the refusal is wider than needed: accepted as failing closed. A
  process it cannot open at that look, and one that appears after it, are
  not counted: accepted residual risks, with the same guidance.
- **Test hook.** `RANIKUST_DEBUG_FAULT=seal-tmpfile` skips the memfd,
  `seal-none` both ways.
- Q-451's answer stands: the init is still a copy-on-write copy of
  ranikust, now of the sealed copy.

### Consequences

- Good, because no process ranikust forks hands a `RUN` a binary it could
  write through `/proc/<pid>/exe`, in the full sandbox and in `Direct` on
  `/` alike, without relying on `ETXTBSY` where the memfd is made
  [measured].
- Good, because the running build never reads the installed file again:
  replacing or rewriting it during a build changes nothing for that build.
- Good, because `rnk-sandbox` mounts no overlay and needs no mount check.
- Bad, because the copy takes the binary's size in memory for the build's
  life, about 11.4 MiB (x86_64 static musl), counted in the build's memory
  cgroup.
- Bad, because start-up runs one more `execveat` and copies the binary:
  about 2.6 ms [measured]. runc tries its overlay first for this cost, which
  matters more to a container start than to a build.
- Bad, because the `O_TMPFILE` copy and the warned fallback still rely on
  `ETXTBSY`.
- Bad, because with the `O_TMPFILE` copy the set-up writes outside the
  state directory, store and outputs: a file with no name in `TMPDIR` (the
  host-safety invariant of `ARCHITECTURE.md` names it).
- Neutral, because the installed file is no longer guarded by `ETXTBSY`
  during a build. The build does not read it; where no `RUN` reaches it
  (overlay mode, kaniko mode per stage) nothing changes, and in kaniko mode
  on `/` a file in an overlay root concerns only the container. A file
  bound from outside the container would lose its last guard there: that
  case is refused, and so is one in a root that is not an overlay
  (overlayfs, fuse-overlayfs) when it has other links or another writable
  mount reaches it.
- Bad, because kaniko mode on `/` refuses a binary bound read-write, which
  built before: the operator adds `:ro` (Docker binds read-write by
  default). It also refuses a binary bound `:ro` from a tree that is bound
  read-write too, and a file with other hard links: cargo's
  `target/release/ranikust` is a hard link of its copy in `deps/`
  [measured], so the operator mounts a copy (`cp`, `install`).
- Bad, because the check sees only the container's own mount table: a
  writable view of the binary that this table does not show, or one
  through a writable mount of another file system that the table shows
  without placing the binary on it, is not detected, a residual risk the
  maintainer accepted, narrowed by the refusal of a process of another
  mount namespace that ranikust reaches when it starts. The operator
  mounts the binary from a path that no writable mount shares.
- Bad, because kaniko mode on `/` refuses a container that shares its PID
  namespace with another one whose processes ranikust reaches when it
  starts (a root sidecar under Kubernetes `shareProcessNamespace: true`,
  Docker or podman `--pid container:<name>`, `--pid=host` with root
  processes in reach), which built before: the operator gives the build a
  PID namespace of its own. It may refuse one that no `RUN` would reach,
  since ranikust looks before it trims its privileges.
- Bad, because that refusal counts only the processes ranikust can open
  as it starts: one it cannot open then, or one that appears later, is
  not counted, a residual risk the maintainer accepted. The operator
  gives the build a PID namespace of its own.
- Neutral, because file capabilities and set-user-ID bits of the installed
  file do not carry over to the copy. ranikust uses neither.
- Neutral, because under an emulator (qemu-user, Rosetta) the process that
  runs is the emulator: `/proc/<pid>/exe` of ranikust, its `RUN` child and
  its init is the interpreter, which no copy covers. Under qemu-user 10.2.1
  the copy is refused before the exec and the run warns, then stops at the
  set-up's single-thread check, qemu-user running a thread of its own
  [measured, `--mode=auto`]; with `--mode=kaniko` on `/` it is refused
  before that check, since `statx` does not tell the binary's mount.
  qemu-user 8.2.2 and Rosetta stop earlier, at the kernel check
  [measured].

## Evidence

- The bind does not work [measured]: with ADR-0082's bind built (in the
  pull request that closes #32, from the commit "feat(sandbox): bind
  ranikust's binary read-only over itself (ADR-0082)" to its revert,
  "revert: drop the read-only bind over the binary's path (ADR-0101)",
  before merge; T2, the VM, Linux 6.8), after a read-only bind over the
  binary's path in the build's mount namespace, `/proc/self/exe` of the
  running process still named the original mount, not the bind's.
- runc [external], v1.0.0-rc7 (commits `0a8e4117e7f7`, the CVE-2019-5736
  fix, and `16612d74de5f`, the bind): `libcontainer/nsenter/cloned_binary.c`
  executes `runc init` from a read-only bind of `/proc/self/exe` where it
  can mount one, else from a sealed copy. The bind guards because runc executes it,
  which ADR-0082's bind did not. runc dropped the bind in 1.1.15
  (`614ce12f0e97`) and on main before 1.2.0 (`b999376fb237`).
- runc [external], v1.2.0 (commit `0b9fa21be2bc`), `libcontainer/dmz`:
  `CloneSelfExe` (`cloned_binary_linux.go`) tries `sealedOverlayfs`
  (`overlayfs_linux.go`: `fsopen`/`fsconfig`/`fsmount`, never attached,
  lower layers the binary's directory and a dummy; "There is no way to
  'unwrap' this (unlike MS_BIND+MS_RDONLY) … This only works for privileged
  users"), then `CloneBinary`: a sealed memfd with `MFD_EXEC`
  (`vm.memfd_noexec=2` handled), then `O_TMPFILE`, then an unlinked
  temporary file. Its comment puts the memfd copy at "around ~60% overhead
  during container startup". `container_linux.go` runs `runc init` as
  `/proc/self/fd/<n>` of the copy. By v1.3.0 (commit `4ca628d1d4c9`) the
  code lives in `libcontainer/exeseal`.
- The copies [measured]: T0 `the_ways_are_tried_in_order_until_one_runs`,
  `a_failed_recovery_stops_the_search`,
  `the_copy_gets_the_environment_with_one_marker` (one marker, whatever was
  inherited: no loop),
  `a_memfd_copy_is_the_binary_sealed_against_writes` (writes and resizes
  `EPERM` through any descriptor),
  `a_tmpfile_copy_is_the_binary_with_no_name_open_read_only`,
  `a_copy_an_interpreter_must_reopen_is_refused_before_the_exec` (a `#!`
  script in a memfd: `ENOENT`, the process goes on; with `execve` of
  `/proc/self/fd/<n>` the shell ran and could not open it),
  `only_fuse_serves_its_own_pages` (the `O_TMPFILE` copy's check, by
  `statfs` magic); the binary's `a_build_runs_from_a_sealed_copy_made_once`
  (a memfd, once; the hook's ways; a marker set from outside only warns);
  the VM smoke and quick suites' `tests/vm/guest/sealed-exe.py`: in
  overlay mode and in kaniko mode per stage ranikust and its init run one
  sealed memfd, and with the memfd skipped one `O_TMPFILE` copy, that root
  cannot open for writing; a kaniko-mode build on a container's `/` runs
  from a sealed memfd ([testing](../testing.md)).
- An emulator [measured] (at the commit before `execveat`; the VM, Linux
  6.8): qemu-user 10.2.1 registered in binfmt_misc without `O` (flags `F`
  or `PF`) ran the memfd copy, could not open `/proc/self/fd/<n>` once the
  exec closed it, and the build exited 1 with an empty stderr; registered
  with `O`, the copy started, failed its check and warned. With
  `execveat` (flags `F` and `OF`) both ways were refused before the exec,
  the memfd with `ENOENT` and the `O_TMPFILE` copy in the container's
  overlayfs `/tmp` with `EOPNOTSUPP`, and the run warned; it then stopped
  at the set-up's single-thread check, since qemu-user runs a thread of
  its own. Rosetta (OrbStack) and qemu-user 8.2.2 stop at the kernel check
  first.
- The binary's mount under qemu-user [measured] (10.2.1, the VM): its
  `statx` sets `STATX_MNT_ID` in the mask and leaves the id at 0, for the
  binary bound read-write or `:ro` and for `/` alike, where
  `/proc/self/fdinfo` gives each its own id; read as equal ids, both binds
  measured `Root`. A zero id is now `NoMountId`, which kaniko mode on `/`
  refuses; a fallback through `/proc/self/fdinfo` is [#95](https://github.com/tran4774/ranikust/issues/95).
- A memfd's and an `O_TMPFILE` file's mount [measured] (the VM, Linux 6.8,
  as root and in a default Docker container; the memfd also on the
  developer's machine, Linux 7.0): `statx` and `/proc/self/fdinfo` both
  give a memfd the mount id 0 (its file system `0x1021994`, the kernel's
  internal tmpfs), and an `O_TMPFILE` file the id of the directory's
  mount. ranikust started by a launcher from a memfd, in kaniko mode on a
  container's `/`, is refused as `NoMountId` (exit 1); so the fdinfo
  fallback would not place a memfd.
- `O_TMPFILE` [measured]: in a default Docker container on Ubuntu's Linux
  6.8, `O_TMPFILE` in the container's overlayfs `/tmp` failed with
  `EOPNOTSUPP`; on OrbStack's Linux 7.2 it worked.
- The binary's mount [measured]: T0
  `the_binarys_mount_is_told_against_the_root` (a zero id: `NoMountId`),
  `the_binarys_mount_is_measured_on_the_file` (`/` is `/`'s mount, `/proc`
  another, writable, a memfd never `/`'s),
  `the_marker_carries_the_binarys_mount`,
  `an_unchecked_marker_is_not_believed_about_the_binary`,
  `a_writable_mount_containing_the_binary_reaches_it` (sample mountinfo: a
  file bind alone; its tree, `/` or the same file bound read-write too; a
  directory bind inside a read-write one; a sibling and a name that only
  starts the same; another file system with the same path; read-only by
  the mount's or the file system's options; `\040` escapes; a path outside
  its mount and a root that is not a path, unplaced),
  `red_team_a_writable_bind_of_a_removed_link_reaches_the_binary` (a
  writable bind of a link of the binary, even one whose name was removed,
  as the red team found the kernel prints it, which built before; the
  binary's own bound name removed beside such a bind),
  `a_writable_file_bind_is_told_by_its_inode_not_its_name` (a renamed
  link), `a_replaced_file_bind_of_another_file_does_not_reach_the_binary`
  (Docker's and kubelet's file binds, one marked removed, still build),
  `a_mount_that_cannot_be_told_places_nothing` (a removed directory, a
  root that is no path, a mount point that leads elsewhere, a binary whose
  name no longer leads to it),
  `a_mount_root_is_reached_at_its_mount_point` (on the host running it:
  `/proc` a directory; a file inside a mount, `/proc/version` among them,
  another mount id or a missing path not reached; a removed file's name
  leading nowhere), `a_symlink_on_the_way_places_nothing` (`/proc` through
  a symlink to `/`, a file through a symlinked directory) and
  `rnk-rootfs`'s `statx_physical_refuses_a_symlink_in_a_parent` (a
  `/proc/<pid>/root` magic link in a parent is `ELOOP` too, and a `..`
  anywhere is refused, as
  `pin_refuses_a_symlink_in_a_parent_and_reads_through_the_pin` shows for
  `pin`),
  `a_covered_mount_is_not_looked_up` (sample tables: a mount covered at
  its mount point or above it, and the binary's own mount covered, not
  looked up; a mount on it, or one it lies on, does not cover it),
  `a_mount_chain_ends_at_its_first_repeat` (a line naming itself as its
  parent, a cycle of two: each mount met once; the line on itself covered
  by `/`),
  `a_table_that_changed_during_the_walk_places_nothing`,
  `a_build_that_cannot_be_kaniko_on_slash_walks_no_mount` (`NotWeighed`;
  links and an overlay root told as before) and the binary's
  `only_a_build_that_may_be_kaniko_on_slash_walks_the_mounts` (as root,
  in a container, without `--mode=overlay`);
  `a_mount_root_is_reached_at_its_mount_point`,
  `a_symlink_on_the_way_places_nothing`,
  `a_covered_mount_is_not_looked_up`,
  `a_table_that_changed_during_the_walk_places_nothing`,
  `a_mount_chain_ends_at_its_first_repeat` and the cases of a `..` before
  the last name in `statx_physical_refuses_a_symlink_in_a_parent` and
  `pin_refuses_a_symlink_in_a_parent_and_reads_through_the_pin` each
  failed with its fix taken out; `STATX_ATTR_MOUNT_ROOT` [measured] in rootless
  podman 5.7.0 on Linux 7.0: set at a file bound read-write or `:ro`, a
  directory bind, `/etc/hostname`, `/proc` and `/`, not at a file inside
  a mount (`/proc/version`, `/etc/passwd`, a file in the directory bind),
  `a_removed_name_of_the_binary_places_nothing` (on the host running it,
  the binary's own name removed while it runs, a link left elsewhere: it
  failed with the stale name placed),
  `a_failed_lookup_keeps_its_errno_unless_the_binary_is_reached` (a name
  too long keeps its errno and a path `RootPath` refuses is `EIO`, at a
  mount point or at the binary's name, unless a writable bind of the
  binary is found, its root given since a host has no file bind; a name
  that leads to the binary on another mount places nothing),
  `a_binary_in_a_root_that_is_not_an_overlay_is_weighed` (an overlay root
  with other links builds and reads no mount table; another root with
  other links, none, or reached by a writable mount is refused, one
  nothing reaches builds; a root the table does not type is unplaced),
  `a_directory_root_is_not_its_own_writable_mount`,
  `a_fuse_overlayfs_root_copies_up_as_overlayfs_does` (sample tables: a
  fuse-overlayfs root copies up, an sshfs one does not, nor an ext4 root,
  for which no table is read; a table without the root's line, or with a
  line that does not parse, the root's or another's, types none),
  `a_file_in_the_root_is_weighed_by_the_roots_file_system` (`mount_of` on
  `/` through its descriptor, as the host running it has `/`; run for this
  change on an ext4 `/` under Linux 7.0, where it is `Linked`, a
  directory's link count counting its subdirectories; the other branches
  are the test's logic, not a measurement),
  `rnk-rootfs`'s `a_mount_is_read_only_by_its_own_options_or_its_filesystems`,
  `a_spilled_option_is_kept_out_of_read_only`,
  `a_line_with_missing_or_shifted_options_is_refused` and
  `a_removed_root_is_marked_not_named`, and the binary's
  `kaniko_mode_on_slash_refuses_a_binary_on_a_writable_mount`; the VM's
  kaniko quick suite, `binary_mounts_on_slash`: in a default Docker
  container a file or a directory bound read-write is refused with the
  `:ro` guidance before the state directory exists, and so are the binary
  bound `:ro` from a tree bound read-write too and a binary file with
  another hard link, each with its own guidance; the binary bound `:ro`,
  its tree bound `:ro` too, or copied into the container's root builds.
  T1 `binary-mounts` (rootless podman, Linux 7.0, both modes), with names
  changed on the host once the binds exist: a writable bind of a link of
  the binary whose name was removed, or beside which the binary's own
  bound name was removed, is refused before the state directory exists;
  another file bound read-write and replaced builds. The red team's
  configuration, run again by hand, is refused the same way, the
  installed binary unchanged [measured].
- A process of another mount namespace: T0
  `a_peer_scan_refuses_the_first_process_reached_in_another_mount_namespace`
  (ranikust itself and entries that are not pids are not probed; a process
  in its own namespace, unreachable or gone is passed over),
  `a_peer_scan_with_no_process_reached_finds_none`,
  `a_peer_scan_refuses_what_it_cannot_tell` (a probe's other error, an
  unreadable `/proc` entry), `a_peer_scan_past_its_bound_refuses`,
  `a_later_thread_reached_refuses_its_process_by_the_process_name`,
  `threads_passed_over_or_gone_pass_their_process`,
  `a_process_whose_threads_may_not_be_listed_is_probed_as_one`,
  `the_threads_of_a_process_count_against_the_bound` (the walk of a
  process's threads on a fake probe),
  `every_thread_of_a_process_is_probed_within_the_bound` (the real probe
  on ranikust's own process, with a second thread: each thread probed and
  counted, a missing process gone),
  `a_process_name_is_trimmed_and_escaped`,
  `a_process_is_named_by_its_pid_and_its_name_when_read` and the binary's
  `kaniko_mode_on_slash_refuses_a_reachable_process_of_another_mount_namespace`
  (messages and guidance). T1 `pid-peers` (rootless podman 5.7.0, Linux
  7.0): in a default container with `--pid container:<peer>` and the
  binary bound `:ro` from a directory no peer binds, a root peer with a
  writable bind of its own, the red team's configuration, is refused
  before the state directory exists and before any `RUN`; a peer run as
  uid 1000, which ranikust's probe meets with `EACCES`, and no peer build
  [measured]. Under rootful Docker (the VM's quick suite) the case has
  not run yet; its outcome there is [inferred].
- The overlay's limits [measured] (option (x), not taken; T2, the VM,
  Linux 6.8, Docker 29.1.3 with runc 1.3.4, AppArmor enabled):
  - as root on the VM, while a binary runs from such an overlay, its
    installed path opens for writing (no `ETXTBSY`, which the same binary
    run from its path gives), and a write there changes the bytes the
    running process maps; `O_TRUNC` kills it with `SIGBUS`; the overlay's
    own file refuses writes (`EROFS`), through `/proc/<pid>/exe` too;
  - in Docker with `--cap-add SYS_ADMIN` (the default seccomp profile and
    `docker-default`), `fsopen`, `fsconfig` and `fsmount` work while
    `mount(2)` on `/` after `unshare(CLONE_NEWNS)` fails with `EACCES`;
    with `apparmor=unconfined` it works; without `SYS_ADMIN`, `fsopen`
    fails with `EPERM`.
- Cost [measured]: on the x86_64 static binary of this ADR's pull request
  (11,901,064 bytes), as a user on Linux 7.0 (the developer's machine,
  while a T2 VM ran), start-up to the set-up refusal took 3.46 ms
  (median of 40, p25 3.19, p75 4.18) with the memfd copy and 0.86 ms
  (p25 0.80, p75 0.92) with none (`seal-none`).

## More Information

Supersedes [ADR-0082](0082-full-sandbox-binds-the-binary-read-only-over-itself.md).
Closes [#32](https://github.com/tran4774/ranikust/issues/32). Refines T7 and
section 6 of the [threat model](../security/threat-model.md); the process
set-up of [sandbox](../design/sandbox.md#process-set-up-once-per-build)
implements it. Related:
[ADR-0003](0003-dockerfile-trust-and-default-isolation.md),
[ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md),
[ADR-0080](0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md).

Revisit if the copy's memory weighs on builds under tight memory limits:
runc's overlay copies nothing, at the price of the gate and the boundary
exception above.
