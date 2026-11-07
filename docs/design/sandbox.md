# `RUN` sandbox

**TL;DR:** how ranikust runs a `RUN` inside a stage root with isolation
roughly equal to `docker build`: in overlay mode, and in kaniko mode where
mounting is possible. Without mounts, kaniko mode runs the `RUN` directly
(`Isolation::Direct`).

Decisions: [ADR-0003](../adr/0003-dockerfile-trust-and-default-isolation.md),
[ADR-0010](../adr/0010-run-network-host-or-none.md),
[ADR-0109](../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md).

## Invariants

- A `RUN` never outlives the build: its children die with it, even on
  `SIGKILL` of ranikust.
- Every mount made for a `RUN` exists only in that `RUN`'s mount namespace;
  the parent, which takes the diff, sees the root as the image left it.
- The command enters its root with `pivot_root`. A bare `chroot` is never used
  where mounting is possible: root inside `RUN` keeps `CAP_SYS_CHROOT` and
  escapes it.
- Users and groups are resolved from the root's `/etc/passwd` and `/etc/group`
  by `rnk-rootfs`, never from the host's.
- Whatever ranikust creates on the real root (stubs) is journaled before it is
  created and removed before the diff.

## Process set-up, once per build

0. **A sealed copy of the binary**
   ([ADR-0101](../adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md),
   `seal.rs`): right after the flags and the kernel check, a build runs
   again from a copy of its binary no `RUN` can write, as runc does (the
   first two copies of runc 1.2.0's `CloneBinary`; runc tries a read-only
   overlay first, which ranikust does not, ADR-0101), so every process it
   forks, the `RUN` child until `execve` and the init, has that copy as
   `/proc/self/exe`, in every mode. The ways, in order:
   - a memfd made with `MFD_EXEC` (without it before Linux 6.3), the binary
     copied in, sealed `SEAL|SHRINK|GROW|WRITE`; `vm.memfd_noexec=2`
     refuses it. It costs the binary's size in memory;
   - an `O_TMPFILE` file in `TMPDIR` (else `/tmp`), never made directly
     on FUSE (the check reads `TMPDIR`'s own file system type, so an
     overlay over FUSE passes it), `O_EXCL` so it never gets a name, run
     from a read-only descriptor. No seal: only `ETXTBSY` guards it while
     it runs. Some file systems refuse `O_TMPFILE` (a container's
     overlayfs `/tmp` on Ubuntu's Linux 6.8).

   First, the mount of the binary as installed is measured against `/`'s
   (`statx` `STATX_MNT_ID`, `statvfs` `ST_RDONLY`: `BinaryMount`; a mount
   id of 0, which qemu-user's `statx` gives for every file and the kernel
   for a memfd, is `NoMountId`; a fallback through `/proc/self/fdinfo` is
   [#95](https://github.com/tran4774/ranikust/issues/95)). On another
   read-only mount, and on a root that is not an overlay (`copies_up`:
   overlayfs by `fstatfs` magic, or FUSE whose own mountinfo line has the
   type `fuse.fuse-overlayfs`; `verdict`), a file with other hard links is
   `Linked`, and one that another writable mount of the same `major:minor`
   in `/proc/self/mountinfo` reaches is `Exposed` (`reached_writable`): a
   directory root that contains the file's path in its file system
   (compared by component), or a file root that is the binary's inode
   whatever its name (mount points and the binary's name reached from `/`
   through `RootPath::statx_physical`, never opened, with no symlink on
   the way, each mount point checked to be its mount's root by its id and
   `STATX_ATTR_MOUNT_ROOT`; a mount another one covers, at its mount point
   or above it and not one it lies on, told from the table alone and not
   looked up; `//deleted` is read as a mark, `MountInfo::root_deleted`).
   The table is read again once the walk is done: one that changed
   meanwhile (mount ids are reused) places nothing. A table that cannot be
   parsed or that changed, that does not place the file (its own name
   removed among them) or does not type a FUSE root (`root_type`), a
   writable directory of its file system whose name was removed, a
   writable mount of it that is covered or whose mount point leads
   nowhere, to another mount or inside its mount, or a file with no link
   left, is `Unplaced`; a lookup of the binary's name or of a mount point
   that fails otherwise (`openat2`, `statx`; an error with no errno as
   `EIO`) is `Unknown` unless a writable mount was found to reach the
   binary (`Exposed`), and a table that cannot be read, the first time or
   once the walk is done, is `Unknown` whatever the walk found. Only
   kaniko mode on a container's `/` uses the answer, so `ranikust` asks
   for the walk only where that mode may follow (`may_be_kaniko_on_slash`,
   before the copy: as root, without `--mode=overlay`, where
   `container::detect` finds a container, ADR-0048): a host build looks up
   no mount point, and a binary the walk would weigh is `NotWeighed`,
   refused should kaniko mode on `/` follow all the same.
   The copy runs with `execveat(<n>, "", AT_EMPTY_PATH)` on its
   close-on-exec descriptor, with the same arguments, environment and
   descriptors, plus `RANIKUST_SEALED_COPY=<way> <mount>`, and the core-file
   limit ranikust started with (`forbid_core_dumps` records it again). A copy
   that needs an interpreter (binfmt_misc, with or without `O`; a `#!` line)
   is refused before the exec (`ENOENT`), and the next way is tried. The
   copy's run makes no copy, checks `/proc/self/exe` (the memfd's seals,
   or no link), takes its name from `argv[0]` again and logs at debug the
   way, the binary's mount and why each way before it failed. When no way
   works, or the check fails, the build goes on with a warning, the
   binary's mount then measured on the file it runs from. `ranikust`
   refuses kaniko mode on `/` with the binary on a writable mount other
   than `/`'s, reached by one, or on one it cannot tell
   ([kaniko mode](kaniko-mode.md#invariants)).
1. While the process is still **single-threaded** (before tokio or rayon
   start), `unshare(CLONE_NEWNS)` and make `/` a slave mount
   (`--make-rslave`). From then on every build mount disappears with the
   process. In a multi-threaded process `unshare(CLONE_NEWNS)` silently moves
   only the calling thread, and `unshare(CLONE_NEWUSER)` fails with `EINVAL`.
2. Start one **spawner thread** that lives for the whole build. Every `RUN`
   child is cloned from it, never from tokio's `spawn_blocking`:
   `PR_SET_PDEATHSIG` fires when the parent *thread* exits, and blocking-pool
   threads are reaped when idle. Any thread may wait for the child.
3. Replay the stub journals of previous crashed runs and remove their stubs.
   The replay trusts no symlink in the sandbox's directory and touches only
   roots below the state directory, so a planted journal or run directory
   cannot point it at host files. It opens a journal for I/O only once its
   pinned (`O_PATH | O_NOFOLLOW`) entry shows a regular file, reopened with
   `O_NONBLOCK | O_NOCTTY`, and reads at most 16 MiB, the most a `RUN` writes:
   a FIFO or a device node planted at a journal's name is left in place with a
   warning, never blocked on or opened.
4. Where the full sandbox runs and ranikust's cgroup v2 tree is delegated,
   probe the cgroups each `RUN` gets there, nesting ranikust first where it
   may ([`RUN` cgroups](#run-cgroups)).

The privilege probes of [modes-and-detection](modes-and-detection.md) run in
the same single-threaded window, before the spawner thread starts.

## Per-`RUN` sequence

The child is created with `CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWUTS` and
`CLONE_NEWIPC`, plus `CLONE_NEWNET` for `--network=none` (`clone_flags` in
`spawner.rs`; a `Direct` `RUN` is cloned with no namespace flag). It is never
cloned with `CLONE_NEWUSER`: a `RUN` stays in the user namespace ranikust runs
in, with the uid and gid of its `USER` as the image gives them. The user
namespaces ranikust creates are the helper's for `--allow network.none-userns`
(below), the probe's children (`--mode=auto`, reported only), and the build's
own, which `rnk_sandbox::enter_build_user_namespace` makes for the whole
process ([#75](https://github.com/tran4774/ranikust/issues/75)). A `RUN` in
it runs this sequence with the fallbacks of steps 2 and 3
([#78](https://github.com/tran4774/ranikust/issues/78)), but no build enters
it yet ([#79](https://github.com/tran4774/ranikust/issues/79)). Rootless builds
and `--userns` are planned for Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)), see
[Hardening planned for Phase 3](#hardening-planned-for-phase-3); the parts that
belong to them are marked *planned*.

**`--network=none`** needs `lo` up, which needs `CAP_NET_ADMIN` over the new
network namespace. The first such `RUN` probes once
([ADR-0033](../adr/0033-network-none-loopback-through-a-user-namespace.md)):

- Where the build process can bring `lo` up in a namespace of its own (root on
  a host, `--privileged`, rootless podman, the build's user namespace, which
  owns the network namespace it clones), the clone creates the network
  namespace and the child brings `lo` up after entering the root.
- Where it cannot (Docker's default capabilities plus `SYS_ADMIN`), the `RUN`
  is refused, naming `--allow network.none-userns`.
- With that opt-in, a helper process creates a user namespace and a network
  namespace owned by it, brings `lo` up there and passes a descriptor of the
  namespace to the child (`SCM_RIGHTS`), which keeps it alive until the `RUN`
  is reaped. The child is cloned without `CLONE_NEWNET` and joins it with
  `setns` as its first step, before any mount, so `sysfs` shows it.
- The child stays in the build's user namespace, but as root it holds every
  capability in the helper's. So after every mount it installs a seccomp
  filter (`seccomp.rs`), before the `RUN`'s profile, which goes on top and
  so cannot fake its install, and under `--seccomp=unconfined` too, that
  refuses what that would open: `setns`, new user namespaces, `io_uring`,
  the netfilter, IPVS and IPsec interfaces, all netlink but route,
  sock_diag, audit and uevent, and `clone3`. The exact list and the
  residual risk (every other interface `CAP_NET_ADMIN` opens, rtnetlink
  first) are in the ADR.

**Seccomp profile**
([ADR-0103](../adr/0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md),
[ADR-0109](../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)):
every full-sandbox `RUN` runs under Docker's default profile (moby/profiles
seccomp v0.2.4's `default.json`, embedded verbatim), with or without
`--seccomp=default`; `--seccomp=unconfined` turns it off (what that opens:
[threat model](../security/threat-model.md#6-accepted-risks)), and
`--seccomp=<file>` puts a profile in Docker's JSON format in its place.

- `seccomp/profile.rs` compiles it to one classic BPF filter before the
  set-up, for the native architecture, the running kernel (`minKernel`) and
  the `RUN`'s capabilities, Docker's 14 (`includes.caps`), as moby's
  `setupSeccomp` reads a profile. Arguments compare as 64-bit values, each
  rule keeps its action, and where several match one call the strictest
  wins; of two refusals with different errnos, the first in the profile.
- A name the architecture does not know is skipped under `SCMP_ACT_ALLOW`, as
  runc does, and refuses the profile under any other action. Under a default
  action that refuses, a number `libc` does not name gets `ENOSYS`, so that C
  libraries fall back. A call of another architecture (i386 or x32 on x86_64,
  arm on aarch64) gets `EPERM`.
- Refused with exit 2, before anything is written: `SCMP_ACT_NOTIFY`,
  `SCMP_ACT_TRACE`, a `listenerPath`, a flag other than `TSYNC`, `LOG` and
  `SPEC_ALLOW`, an `SCMP_ACT_ERRNO` whose errno is 0 (a faked success) or from
  512 up, where the kernel keeps its own codes (on 516 it may run its restart
  block instead of returning the refusal), either named in the message as
  `syscalls[N]` or `defaultErrnoRet`, two conditions on one argument in a
  rule, a rule without names (the message names the rule, `syscalls[N]`;
  ADR-0109), a filter longer than the kernel's 4096 instructions (past them
  the compiler only counts, so a profile describing millions takes memory
  linear in its size, not in theirs), and anything malformed; a file that is
  not a regular file or is over 1 MiB.
- The limits of a profile's rules are in the
  [threat model](../security/threat-model.md#6-accepted-risks).
- The profile is the operator's and may refuse any call with any errno
  `--seccomp` accepts, so every step that need not follow it comes before
  it, in the order of step 4, the init's fork included: the init and the
  command each install it after the fork, the command once in its cgroup
  namespace ([`RUN` cgroups](#run-cgroups)). After it come the `capset` to
  the default sets, since the install needs `CAP_SYS_ADMIN` without
  `no_new_privs`, so that a setuid binary of the `RUN` keeps working, and
  the init's and the command's steps that must follow the fork (step 5 on).
  A filter around ranikust may still fake a success; the child checks what
  it can (the init's fork, an `execve` or `execveat` back without an error
  and a blocking `waitpid` back with no child fail the `RUN`, and a read
  goes into a value set beforehand), and the rest is in the
  [threat model](../security/threat-model.md#6-accepted-risks). A failed
  install names the filter: the profile's points to `--seccomp=unconfined`,
  ADR-0033's to `--allow network.none-userns`.
- A `Direct` `RUN` gets none: in kaniko mode on `/` the container's own
  profile applies, and `--seccomp` is refused (exit 2).

The steps, in order:

1. **Parent check and descriptors.** `prctl(PR_SET_PDEATHSIG, SIGKILL)`, then
   check that the parent did not die before: `getppid()` is 0 in a new PID
   namespace, so the child reads the parent pid in its own `/proc/self/stat` of
   the outer procfs, opened before entering the root; a reparented child shows
   another pid.
   - Close every descriptor the clone copied from ranikust except stdio, the
     error and status pipes, that `stat` file, the network namespace to join,
     the stdio sources and a `#!` script (`close_range`, else the list in
     `/proc/self/fd`, past a `close` refused, then, unless the list closed
     them all, every number up to the hard limit), in
     `Isolation::Direct` too. ranikust holds directories outside the root, and
     close-on-exec acts only at `execve`: until then `/proc/self/fd/<n>` of the
     `RUN`'s `proc` would lead a `WORKDIR`, a `PATH` entry or a mount target to
     them (runc's CVE-2024-21626). None of those kept is a directory. The
     caller's stdio descriptors then go on 0-2, so that ranikust's own streams
     are gone too.
   - `/dev/null` is never opened by name in the child: a `RUN --mount` over
     `/dev` (or, in `Direct` on `/`, an earlier `RUN`) could make it a link to
     ranikust's stdout. The full sandbox opens the `null` node of its own `/dev`
     tmpfs by descriptor right after populating it (step 3.4), before any
     `RUN --mount`, and never takes the parent's, which a root `RUN` could
     `fchmod` or `fchown`; the read-only bind prevents it (`EROFS`). `Direct`
     gets the host's `/dev/null` from the parent, reached without symlinks and
     checked to be the null device.
2. **User namespace**: a `RUN` gets none of its own; in the build's
   ([#75](https://github.com/tran4774/ranikust/issues/75)) it keeps that
   namespace's map, the full id range available, so `setgroups` works and
   `USER _apt` (used by apt-get) gets its groups. Where the map gives one
   gid and denies `setgroups` (`/proc/self/setgroups` is `deny`,
   [ADR-0098](../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)),
   step 7 skips it, so the `RUN` keeps the build process's supplementary
   groups, as rootless runc does. Denied with several ids, the call is
   still made and fails the `RUN` (`Step::Groups`): there the skip would
   hand a `USER` groups that are not its own. `devpts` gets no `gid=5`
   where gid 5 is unmapped (3.4). A `USER` whose ids the map lacks fails
   at the first step 7 call that names one (`Step::Groups`, `Step::Gid` or
   `Step::Uid`).
   *Planned, Phase 3:* the command's own user namespace leaves the init's uid
   out ([below](#hardening-planned-for-phase-3)).
3. **Mounts**, in this order:
   1. Bind the root onto itself, recursively (`pivot_root` requires it), and
      remount the bind `nodev`: Docker blocks device nodes with its device
      cgroup, which ranikust does not have, so no node in the root opens,
      neither one a `RUN` creates with `mknod` nor one from a layer. The
      remount reaches the top mount only, so every mount of the bind,
      submounts below the root included, is then made `nodev` with
      `mount_setattr(AT_RECURSIVE)` (Linux 5.12); on an older kernel a root
      with any mount below it (`/proc/thread-self/mountinfo`, the root given
      as the kernel names it) is refused before the clone. Because of this
      `nodev`, `COPY` into such a root keeps device nodes, FIFOs and sockets
      (as BuildKit); only where a `RUN` enters the root without it
      (`Isolation::Direct`, kaniko mode on `/`) does `COPY` leave them out
      (ADR-0060). An image built with the full sandbox can therefore hold a
      device node copied in; it opens in no `RUN`.
   2. A new `proc`. If `/proc` is masked (a container's) and the build runs in
      a user namespace of its own, the kernel refuses (`EPERM`). An rbind of
      the existing `/proc` takes its place only at the operator's request
      ([ADR-0126](../adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md);
      `--allow proc.rbind`, planned with
      [#79](https://github.com/tran4774/ranikust/issues/79)): through it a
      `RUN` keeps a PID namespace of its own, but sees every process of the
      container, as BuildKit's `--oci-worker-no-process-sandbox` documents.
      It reads the command line and mount table of each, ranikust's
      included (build-argument values, secret paths), and reaches the
      environment and files of those of its uid, such as a shell that
      started ranikust; ranikust itself, not dumpable
      ([ADR-0051](../adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)),
      stays closed. So that no `RUN` reaches another's processes, the
      `RUN`s of such a build run one at a time: `Sandbox::run` holds a lock
      from before the clone until the `RUN`'s init is reaped, the processes
      of its PID namespace gone with it; stages still schedule in parallel
      ([ADR-0127](../adr/0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md)).
      The rest is an accepted residual
      ([threat model, section 6](../security/threat-model.md#6-accepted-risks)):
      start ranikust as the container's PID 1, with its secrets in its
      environment rather than on its command line or in a shell left
      running, and ask for the rbind only with trusted base images. The
      rbind's masks come along, every mount of it made `nosuid`, `nodev`
      and `noexec` with `mount_setattr(AT_RECURSIVE)` (without it the `RUN`
      fails); the sandbox's probe chooses it once and logs it once per
      build. Unasked, the probe fails at `Step::MountProc` and warns how to
      get a new `proc`: build as a regular user on a host that allows user
      namespaces, where a new `proc` mounts, or in a container where
      ranikust may mount (overlay mode as root), both in
      [where environments land](modes-and-detection.md#where-environments-land);
      or ask for the rbind. So a `RUN` never sees the container's `/proc`
      unasked, and always gets a new `proc` where one mounts. Wherever the
      probe chose a new `proc`, a refusal at a later `RUN` fails it
      (`Step::MountProc`); the choice is not tried again during the build.
      `proc`, `sysfs` (3) and the `/dev` tmpfs (4) go
      only on a real directory of the root: `/proc`, `/sys` or `/dev` as a
      symlink is refused before any stub is placed, and the child resolves
      them again with `RESOLVE_NO_SYMLINKS`, as runc requires for `proc` and
      `sysfs`. Through a symlink, `proc` could land below a directory a
      later mount hides, out of reach of step 7.
   3. A read-only `sysfs`. It needs privileges over the user namespace that
      owns the network namespace, so it fails (`EPERM`) with the host network
      in the build's user namespace: then an rbind of `/sys`, every mount of
      it made read-only, `nosuid`, `nodev` and `noexec` the same way, which
      the sandbox's probe chooses once with no request, only where the
      user-namespace probe met the same refusal
      ([#74](https://github.com/tran4774/ranikust/issues/74)). A
      `--network=none` `RUN` then sees the outer network devices in
      `/sys/class/net`. Otherwise a refused `sysfs` mount
      fails the `RUN` (`Step::MountSys`).
   4. A `nodev` tmpfs on `/dev`, with `null`, `zero`, `full`, `random`,
      `urandom` and `tty` bind-mounted from the outer `/dev` (`mknod` needs
      `CAP_MKNOD` in the initial user namespace). Each bind is its own mount
      and keeps the outer flags, so the node opens; it is remounted read-only,
      because it is the host's inode and `chmod` from `RUN` would change it.
      Add `devpts` (`newinstance,ptmxmode=0666`, plus `gid=5` only if gid 5 is
      mapped), `shm`, and the symlinks `ptmx`, `fd`, `stdin`, `stdout`,
      `stderr`.
   5. `resolv.conf`, `hosts`, `hostname`: bind a **writable copy**, as Docker
      lets `RUN` write `resolv.conf`. Resolve the target inside the root with
      `openat2(RESOLVE_IN_ROOT)`, because it may be a symlink. A read-only
      remount must keep the source mount's locked flags (`statfs().f_flags`:
      `nosuid`, `nodev`, `noexec`, `relatime`) or it fails with `EPERM` in a
      user namespace. With the host network `hosts` is ranikust's own
      `/etc/hosts`, with `--network=none` loopback names, both plus the host
      name `ranikust`
      ([ADR-0031](../adr/0031-run-host-name-and-hosts-file.md)).
   6. `RUN --mount`:
      - `type=cache` binds a cache directory leased from the store with its
        `sharing=` (`Store::cache_lease`); `type=bind` binds the context or
        another stage's root.
      - `type=secret` follows BuildKit: after the other mounts, the child
        mounts a tmpfs (`nodev`, `nosuid`, `noexec`, `0700`) over the `RUN`'s
        scratch directory, writes each secret there from its own memory with
        its owner and mode (`0400`, root's, unless the mount asks otherwise),
        binds it read-only (`nosuid`, `nodev`, `noexec`) at its target and
        detaches the tmpfs. The secret never reaches a disk and dies with the
        mount namespace. A secret with `env=` is a variable of that `RUN`
        instead.
      - Every bind is remounted `nodev` (ADR-0032) through the new mount
        itself: made detached with `open_tree(OPEN_TREE_CLONE)`, attached with
        `move_mount`, remounted by its own descriptor. A lookup by name would
        cross into the new bind and could follow symlinks inside it (a cache's
        contents), so `nodev` and read-only would reach another mount.
      - Where a seccomp profile refuses those calls (probed once; only a
        profile older than they are does), a target that passes through a
        symlink (`/var/run -> /run`, a usrmerge `/lib -> usr/lib`) fails the
        `RUN` with `ELOOP`, fail-closed, and the mount found again must show
        the source's inode on a new mount that is not one the child made
        before (their ids are recorded in room the parent sizes before the
        clone), since one source may be bound at two targets.
      - The child opens each source in its own mount namespace (`O_PATH`, no
        symlink as the last component, type checked) and binds it through
        `/proc/self/fd`: a bind refuses (`EINVAL`) a source opened in another
        mount namespace.
   7. Docker's read-only and masked kernel files (moby
      `daemon/pkg/oci/defaults.go`): `/proc/bus`, `fs`, `irq`, `sys` and
      `sysrq-trigger` are bound read-only onto themselves, because writing a
      sysctl such as `kernel/core_pattern` needs only uid 0 and reaches the
      host kernel; `kcore`, `keys`, `timer_list`, `sched_debug`,
      `/sys/firmware` and the rest are masked, a file by a read-only bind of
      `/dev/null`, a directory by an empty read-only tmpfs, each bind made and
      remounted as in step 6. Each path is found under the descriptor of the
      `proc` or `sysfs` mount made in steps 2 and 3, without any symlink, never
      by its name from the root, which the image's symlinks and later mounts
      could lead elsewhere. A path missing there is one the kernel lacks, and
      is skipped.
   8. `pivot_root(".", ".")`, then `umount2(".", MNT_DETACH)`. Where seccomp
      blocks `pivot_root` (Docker's default profile, even with
      `CAP_SYS_ADMIN`, `EPERM`) and `/` is a mount root:
      `mount(root, "/", MS_MOVE)` then `chroot(".")`, as runc's `--no-pivot`
      (see [Variants](#variants-by-environment)).
4. `sethostname` to the fixed `ranikust`, whatever the environment (so a `RUN`
   that records its host name gives the same bytes on every host); a seccomp
   refusal is ignored. Then the full sandbox's child, in this order, joins
   the `RUN`'s new session keyring (step 6), drops the bounding set and
   clears the ambient and inheritable sets (step 5), sets itself not dumpable
   and starts a session of its own (the init's, step 5), sets the umask to
   022 and forks the command (step 5). Then the init and the command each
   install ADR-0033's filter and the seccomp profile on top of it
   ([above](#per-run-sequence)), the command once in its cgroup namespace
   where the `RUN` has cgroups, and only then set the effective and
   permitted sets (step 5).
5. **Drop capabilities**, for **every** `USER`: reduce the **bounding** set to
   Docker's 14 default capabilities, clear the ambient set, keep the defaults
   in the effective and permitted sets and leave the inheritable set empty, as
   Docker since 20.10.14. A non-root user has no effective capabilities but a
   full bounding set, which a setuid binary in the image would regain. The
   drops go up to the kernel's last capability; an `EINVAL` ends them only
   where it could not be read and 63 was assumed. In the full sandbox the
   init and the command keep ranikust's effective and permitted sets until
   each has installed its filters (step 4), which needs `CAP_SYS_ADMIN`,
   already out of the bounding set; the command makes its cgroup namespace
   before them ([`RUN` cgroups](#run-cgroups)). The init then drops every
   capability, the command all but the defaults, both before the go.

   Then the full sandbox's child becomes the `RUN`'s **init**
   ([ADR-0080](../adr/0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md)):
   - Not dumpable and in a session of its own since step 4, it zeroes its
     copy of the argument and environment strings ranikust started with
     (`/proc/self/stat` fields 48 to 51, refused as `Step::Init` if zero;
     `/proc/1/cmdline` has no ptrace check) and forks. The new process, PID 2,
     checks that its parent is PID 1 (a fork a filter around ranikust faked
     fails as `Step::Init` with `EPROTO`), joins its cgroup and makes its
     cgroup namespace where the `RUN` has cgroups
     ([`RUN` cgroups](#run-cgroups)), installs its filters and sets its
     capability sets (step 4), waits on a close-on-exec go pipe, then runs
     steps 6 to 8 and becomes the command.
   - The init keeps the death signal of step 1, installs its filters (step
     4), drops every capability, names itself `ranikust-init`, checks the
     death signal is still armed and the parent alive, and closes every
     descriptor but the status pipe's write end and the go pipe's, the
     error pipe last. It counts only what the kernel closed: `close_range`,
     else every descriptor `/proc/self/fd` lists, each `close` made as a
     raw syscall (musl's `close` turns `EINTR` into success) and checked.
     A refused `close`, whatever its errno, or a list that cannot be read
     or does not shrink, fails the `RUN` as `Step::Init`, its command
     killed, with the listing's own errno where it has one (before
     Linux 5.9, `close_range`'s `ENOSYS` would hide it); the cost is
     bounded by the descriptors open. Last, it writes
     the go byte on the go pipe and closes its end, `close_range` first.
   - The command goes on only on that byte, never on the end of the pipe:
     the end (`EPIPE`, a read a filter around ranikust faked with no error
     included), another byte (`EPROTO`) or an error fails the `RUN` as
     `Step::Init`. So a failure of the init's steps comes before the command
     ran, reported as `Step::Init` or as the filter's step; the init also
     kills the command. An init that cannot write the byte closes its end,
     and the command fails at its read, which is what is reported (the
     error pipe is closed by then); one that can do neither ends with 125,
     never read as the command's code (`Error::InitExited`, or the
     command's `Step::Init` record, below).
     A command already gone fails only the write: the init, PID 1 of its
     namespace, gets no `SIGPIPE` it has no handler for.
   - A read of the init's or the command's answered with `EINTR` (the
     argument areas', the parent check's, the go pipe's) fails the `RUN` as
     `Step::Init` instead of reading again: with no handler, the kernel
     restarts a read a signal interrupts, so only a filter answers so, and
     it could forever. The parent check fails with its read's errno, `EINVAL`
     for a line that does not parse (a read faked with no bytes); only
     another parent is `ESRCH`.
   - It then reaps every process that reparents to it until the command exits
     or is killed (detaching from a command that made it its tracer), writes
     the command's wait status on the status pipe, and exits; a `waitpid`
     back with no child (0, or an error, `EINTR` included: with no handler,
     the kernel restarts one a signal interrupts) ends it with 125, the
     init's own code (`Error::InitExited`).
   - Every child of ranikust resets every signal disposition to the default
     first, so the init has no handler and the kernel drops every signal a
     process of the namespace sends it. A `RUN` cannot signal, stop, trace or
     read the memory of its init (not dumpable, and no `CAP_SYS_PTRACE`); it
     can read its zeroed `/proc/1/cmdline`, lower its scheduling priority or
     pin it to a CPU (same uid), which slows its death but cannot stop it
     (threat-model section 6), and make itself the init's tracee.
   - `Direct` has no init: its child runs steps 6 to 8 itself.
6. `setsid`: no controlling terminal, so a `RUN` cannot read the operator's
   keys or push input into the operator's shell (`TIOCSTI`); stdin is
   `/dev/null` unless the caller says otherwise, as under Docker. The command
   runs in a new anonymous session keyring, so ranikust's keys stay out of
   reach: in the full sandbox the child joins it before the profile (step 4),
   which refuses `keyctl` (runc's order; ADR-0109 refines ADR-0080 there),
   and the command inherits it; in `Direct` the child joins it before
   `setsid`. Root's user keyring stays reachable under
   `--seccomp=unconfined` and in `Direct`; the default profile refuses
   `keyctl`, `add_key` and `request_key`. After a refused join, whatever the
   errno, the full sandbox's child looks ranikust's session keyring up,
   still before the profile (`KEYCTL_GET_KEYRING_ID`): found, it fails a
   `RUN` whose profile lets one of those three calls run, as
   `Step::KeyringRefused`; where the lookup is refused too, as under
   Docker's default profile around ranikust, which refuses `keyctl` whole,
   the `RUN` goes on. In `Direct` a join refused with `EPERM`, `ENOSYS`
   or `EACCES` is ignored (ADR-0109).
7. `setgroups` (except where the gid map gives one id and it is denied,
   step 2),
   `setgid`, `setuid` to the ids of `USER`, which `rnk-rootfs` looked up in
   the root's `/etc/passwd` and `/etc/group` before the fork [inferred].
   A numeric uid with no passwd entry gets gid 0, as in Docker. A change of uid
   or gid clears the death signal, so it is armed again: in `Direct` toward
   ranikust and checked; in the full sandbox's command toward the init, only
   as a second line (the init's signal is what ends the namespace with
   ranikust, whatever the command does to its own: a set-id `execve` or
   `prctl(PR_SET_PDEATHSIG, 0)` clears it).
8. **Enter `WORKDIR` and `execve`.**
   - Resolve `WORKDIR` in the root with `openat2(RESOLVE_IN_ROOT |
     RESOLVE_NO_MAGICLINKS)` and `fchdir` to it (before the id switch, and
     again after if only root could traverse it), then check with `getcwd` that
     it is below the root, as runc since CVE-2024-21626.
   - Make every descriptor but stdio close-on-exec, set the umask to 022, reset
     `SIGPIPE` (Rust ignores it, and ignored signals survive `execve`) and the
     signal mask.
   - Search `PATH` **after** `pivot_root` and **after** `setuid`, each
     candidate resolved with `RESOLVE_NO_MAGICLINKS` (until `execve` the child
     is a copy of ranikust, so `/proc/self/exe` is ranikust's binary on the
     host); `execve`. The check covers the candidate's path only: the kernel
     opens a `#!` script's interpreter itself, so a `#!/proc/self/exe` line, or
     a symlink swapped in after the check, still runs ranikust's binary in the
     sandbox, which then refuses its arguments (threat-model section 6). That
     binary is the sealed copy of the process set-up (step 0), so the `RUN`
     cannot write it, in the full sandbox and in `Direct` alike.

Between `clone` and `execve` the child of a multi-threaded process only makes
system calls on arguments built before the clone: no allocation, no lock. A
failing step writes the step and the errno to a close-on-exec pipe and exits
127; the parent reads end-of-file once `execve` succeeded.

**Waiting.** The parent waits for the child, in the full sandbox the init. When
the command exits, the init reports its status and exits, and the kernel kills
everything left in the namespace, so no `tini` and no process-group kill are
needed; exit codes and signals read as they would from the command itself. An
init that ended before it wrote that status, before its go byte or after it,
leaves no status of the command's, which may or may not have started; its own
status is never passed off as the command's. Killed by a signal (a profile's
kill or trap of a call the init makes: its own steps after its filters, its
closes, its wait, its status write; or a `SIGKILL` from outside), the `RUN`
fails as `Error::SetupKilled`, which names the signal and, for `SIGSYS` under
a profile, the profile as the likely cause. Exited with a code and no failure
record (125 where its go or its wait cannot go on, 127 where its filters
refused the record), it fails as `Error::InitExited`, which names the code as
the init's own. Neither is one that ran no command. `SetupKilled` wins over a
failure record of `Step::Init`, which the init's end may cause (the go pipe
ended under a command still waiting), and loses to any other record, the more
precise, the one not returned logged; `InitExited` loses to every record, its
code logged. A kill or trap of a call only the command makes after its filters
and before `execve` (its go read, `setsid`, the `USER` switch) leaves the init
alive, and the `RUN` fails with the command's status (`SIGSYS`, code 159), as
one killed after `execve` does: nothing tells the two apart. A
cancel (`Sandbox::cancel`) sends the init `SIGKILL`, as BuildKit kills the
process on a cancel; the child is forgotten only once it has exited and before
it is reaped, so the kill never reaches a reused pid. The command is PID 2, as
under `docker run --init` and as kaniko's `RUN`; it is not PID 1 as under
BuildKit [inferred], so it gets the default signal semantics of any process.

Every `RUN` starts with the `RLIMIT_NOFILE` set by `Sandbox::set_nofile_limit`,
the limit ranikust had before it raised its own for the kaniko-mode walk; the
child sets it right before `execve`.

## Stub mount points

An image may lack a bind target (`alpine:3.20` has no `/etc/resolv.conf`,
`debian` no `/etc/hosts`). The empty file or directory created as a target is a
real file in the root: it would enter the layer, and it survives a killed build.
So, as in BuildKit's `executor/stubs.go`, with one journal per `RUN` in
`<state-dir>/sandbox/runs/`, locked while the `RUN` lives:

1. write the stub's path to the journal in the state directory, then create it;
2. remove stubs after the `RUN`, before the diff, and restore the parent's
   metadata; a stub counts as gone only where its lookup says nothing can be
   at its path (`ENOENT`, or `ENOTDIR` for a parent the `RUN` made a file),
   and any other lookup error fails the clean-up with its step and errno,
   the journal kept, once the older stubs are undone all the same;
3. on start-up, remove any stub left in the journal.

Because all other mounts live in the child's namespace, no ignore-list entry is
needed for `/proc`, `/dev`, `/sys`, `resolv.conf` or `hosts`.

The same journal carries the files a `Direct` `RUN` on a root other than `/`
gets without mounts
([ADR-0043](../adr/0043-direct-run-on-a-workdir-chroots-with-placed-files.md)).
The binary never runs that (ADR-0050, ADR-0060); tests do
(`Sandbox::start_with_root`, `#[cfg(test)]`, which still hands the host's
`/dev/null` node to the `RUN`).

- A `swap` record, synced before anything changes, names one of the fixed paths
  (`resolv.conf`, `hosts`, `hostname`, six `/dev` nodes) and the name the
  image's entry is renamed to in the same directory. After the `RUN` the entry
  is renamed back, or the placed one removed, and the parent's times restored as
  for stubs. Placing and undoing never resolve through another mount below the
  root.
- A replay accepts only those paths and aside names, and a root below the state
  directory or the shared root given to `start_with_root`, opened once and
  matched by device, inode and mount. A root that is the host's `/`, `/etc` or
  `/dev` by device and inode is refused before anything is placed, and a
  `Direct` `RUN` off `/` is refused wherever the full sandbox can run. In its
  `chroot` the child checks the root's identity again and drops to Docker's
  capabilities.
- A replay or a crashed run releases the journal's `flock` explicitly: the lock
  belongs to the open file description, which a child cloned meanwhile shares
  until it executes or exits.

## `RUN` cgroups

Where ranikust's cgroup v2 tree is delegated, each full-sandbox `RUN` gets
cgroups of its own, ended with `cgroup.kill`
([ADR-0110](../adr/0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md),
refining [ADR-0083](../adr/0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md)
(a)); elsewhere nothing changes and cgroups are only read (ADR-0007). They
set no resource limit, only how many cgroups a `RUN` may make below its own,
and how deep (`cgroup.rs`).

**Where.** `Sandbox::start` reads ranikust's own cgroup `D` from
`/proc/self/cgroup` and opens it under `/sys/fs/cgroup`, which must be
cgroup2 and list ranikust's pid in `D/cgroup.procs`. `D` counts as delegated
only if it carries a delegation mark ranikust trusts, set to `1` on `D`
itself, not an ancestor, or is the root of ranikust's cgroup namespace, and
then only on a read-write cgroupfs, with `cgroup.kill` (Linux 5.14) and
`cgroup.type` `domain`. A root ranikust trusts systemd's `trusted.delegate`
alone, which only `CAP_SYS_ADMIN` in the initial user namespace can set;
`user.delegate`, which anyone who may write `D`'s inode can set, a root
`RUN` included, counts only for a ranikust that is not root and owns `D`
(rootless, [#79](https://github.com/tran4774/ranikust/issues/79)). A
namespace's root counts whoever made it, delegated or not. A `D` with
neither a mark nor a namespace's root, or on a read-only cgroupfs, is read
no further. An unreadable mark, or an unreadable file of a marked or
namespace-root `D` on a read-write cgroupfs, other than one that is absent,
means no `RUN` cgroups, logged with its errno at `warn`:

| Environment | `RUN` cgroups |
|---|---|
| A systemd unit or scope with `Delegate=yes` (marked `trusted.delegate` by Ubuntu 22.04's systemd 249 and 24.04's 255 **[measured]**) | Yes |
| `docker run --privileged` (its own cgroup namespace, cgroupfs read-write) | Yes |
| Another cgroup namespace's root on a read-write cgroupfs, delegated or not: root's own `unshare -C` with cgroup2 remounted; a systemd 257 unit with `ProtectControlGroups=private` **[inferred]** | Yes |
| `Delegate=yes` under a systemd that sets no `trusted.delegate` | No |
| Root on a host, an ssh session's scope, a unit without `Delegate=` | No |
| Docker's default options, most pods (cgroupfs read-only) | No |
| A kernel before 5.14 (no `cgroup.kill`); `Direct` on `/`; the probe's own `RUN`s | No |

**Layout**, logged at `info` once the probe passes (span `rnk_sandbox.cgroups`,
field `layout`):

- **Nested**, where ranikust is alone in `D` and `cpu` is in
  `D/cgroup.controllers`: ranikust moves into `D/ranikust-<pid>` and enables
  `+cpu` in `D`; each `RUN` gets `D/run-<pid>-<n>` with `+cpu` and two leaves
  side by side, `init` and `command`. In T2, in the spinner cases of threat
  model section 6 (8 and 32 spinners), the `RUN`'s PID namespace died
  within 4.1 ms of the death signal, over five runs **[measured]**
  ([ADR-0110](../adr/0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md#evidence)).
  That is not a bound: a `RUN` can still lengthen its init's death delay
  after ranikust is killed, up to the delay of the layout without nesting,
  an accepted risk
  ([threat model, section 6](../security/threat-model.md#6-accepted-risks)).
- **Single** otherwise: each `RUN` gets `D/run-<pid>-<n>`, with no
  controller, and one leaf, `command`; the init stays in `D` with ranikust.

A process that joins `D` between its read and `+cpu` makes the kernel refuse
`+cpu` (`EBUSY`): the probe fails, as below, and no `RUN` gets a cgroup.

**Per `RUN`.** The parent makes the cgroups before the clone and opens their
`cgroup.procs`, `cgroup.kill` and `cgroup.events`. In `D/run-<pid>-<n>`,
before anything else, it writes `cgroup.max.descendants` 4095 and
`cgroup.max.depth` 32, the removal's bound below. The init, nested, writes `0`
to its leaf's `cgroup.procs` right after arming its death signal, the command
to `command`'s once the init forked it, before it waits for the go; each
closes the descriptor at once, so no `RUN` process holds one when the command
starts. The command then makes a cgroup namespace of its own, rooted at
`command`, before its seccomp filters, which refuse it, and before it gives
up `CAP_SYS_ADMIN` (steps 4 and 5), as Docker and containerd give a
container a private one on cgroup v2. So a `RUN` reads
`0::/` in `/proc/self/cgroup`, and its init's cgroup in `/proc/1/cgroup` as
`0::/../init` nested or `0::/../..` single, whatever ranikust's pid and the
`RUN`'s number: a layer that records them does not depend on `--jobs` or the
run. The `RUN` has no cgroupfs mounted and no capability in ranikust's
user namespace to mount one. Docker's default profile refuses it a user
namespace (ADR-0109) **[measured]**. Under `--seccomp=unconfined`, a `RUN`
that may create one can still mount a cgroupfs of its own, rooted at the
cgroup it is in, and make cgroups below it, as on Ubuntu 22.04; Ubuntu
24.04's AppArmor leaves it no capability in that namespace **[measured]**.
Once the init is reaped, ranikust
writes `cgroup.kill` (which reaches every cgroup below), waits up to 10 s for
`populated 0` and removes the cgroups with every cgroup below them, the
deepest first, before the diff. Each is opened relative to its parent's
descriptor with `openat2`, never through a link or onto another mount; at most
4096 cgroups, 32 levels below the `RUN`'s. The kernel refuses a `RUN` a cgroup
past the limits (`EAGAIN`). The command runs in `command` in both layouts, so
its own cgroup namespace and every one a `RUN` makes are rooted at `command`
or below, under the cgroup with the limits, whose files no cgroupfs it can
mount shows. The limits hold whether or not the cgroupfs is mounted with
`nsdelegate`
**[measured]** ([ADR-0110](../adr/0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md)).
A cancel, and so a handled `SIGINT` or `SIGTERM`, writes `cgroup.kill` before
it kills the init.

**Failures.** The probe at start-up runs every step once: nesting where the
layout says so, a `RUN`'s cgroups, a child moved into each leaf that then
makes a cgroup namespace in `command`, `cgroup.kill` and the removal. If a
step fails, ranikust logs it with the errno at `warn`, undoes what it changed,
and builds as it would without a delegated tree. A step that fails during a
`RUN` fails that `RUN`: before the clone a `Cgroup` error naming the step and
the cgroup, `Step::JoinCgroup` or `Step::CgroupNamespace` in the child (both
before any command ran), and the removal as an `Io` error naming the step
and the cgroup, the one below the `RUN`'s that failed if one did, or past
its bound, which the limits keep every `RUN` within. A `RUN` whose cgroups
could not be removed leaves them; nested, their `+cpu` then makes the `-cpu`
of the `Sandbox`'s drop fail (`EBUSY`), so ranikust stays in its leaf and `D`
keeps `+cpu`, with a warning, until the unit or container ends.

**What is left.** Dropping the `Sandbox` undoes the nesting: `-cpu`, ranikust
back in `D`, its leaf removed (at once refused, with a warning, if another
process joined the leaf). A `SIGKILL`ed ranikust, one stopped at once by a
second `SIGINT` (exit 130), or a panic (the release build aborts), runs no
clean-up: its empty `RUN` cgroups, and when nested its leaf and `+cpu`, stay
in `D` until the unit or container ends. Nothing in ranikust removes them; a
later ranikust with the same pid skips the `run-` names they hold (64 at
most, then the `RUN` fails), but a `probe-` or `ranikust-` name left makes
its probe fail, and it builds without `RUN` cgroups. systemd and Docker
removed them with the scope or container **[measured]**, and `docker exec`
into a nested `--privileged` container still runs, in ranikust's leaf
([ADR-0110's evidence](../adr/0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md#evidence)).
The cgroups a `RUN` that got no cgroup of its own made in `D` stay too;
ranikust never removes them, and they go with the unit or container
**[inferred]**.

## Variants by environment

| Environment | Differences |
|---|---|
| Real root on a host | Every step works without a fallback |
| `CAP_SYS_ADMIN` with Docker's seccomp profile | `MS_MOVE` instead of `pivot_root`, only where `/` is a mount root, keeping Docker's 14 capabilities: the old root stays mounted underneath, and `..` stops at the moved root only then [measured] (the `sysadmin` variant of `tests/vm/guest/smoke.sh`, kernels 5.15 and 6.8). Inside a plain `chroot` ranikust refuses before changing anything ([modes-and-detection](modes-and-detection.md#refusal-and-guidance)) |
| Rootless (the build's user namespace, #75) | The namespace's map: multi-range for root, `newuidmap`'s for a user, one id where neither ([ADR-0098](../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)). Chosen once per build by the probe (#78): a new `proc` where it mounts (a user on a host); else, only on request ([ADR-0126](../adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)), an rbind of `/proc`, logged, its `RUN`s one at a time ([ADR-0127](../adr/0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md)) (a container whose `/proc` is masked: root of a default rootless `podman run`), and unasked a refusal at `Step::MountProc`; a read-only `sysfs` where the build owns the network namespace, else an rbind of `/sys`, read-only; `setgroups` except where the gid map gives one id and `setgroups` is denied; `--network=none` with no helper. With an rbind of `/proc` a `RUN`'s `/proc/1` is the container's PID 1, so the smoke check of PID 2 under `ranikust-init` is left out there ([smoke tests](../testing.md#smoke-tests-tier-t1)). `sethostname` fails where a seccomp profile refuses it without `CAP_SYS_ADMIN` (Docker's and Podman's defaults): the `RUN` goes on with the outer host name. No build enters this namespace yet (#79); started inside one that something else made (rootless `podman run --privileged`), ranikust runs the sequence above as written (T1, `just smoke`) |
| kaniko mode, cannot mount | `Isolation::Direct`: no sandbox, `RUN` runs on `/` like kaniko (below) |
| `--parallel-stages` | Refused (ADR-0060); planned: bare `chroot`, no namespaces ([kaniko-mode](kaniko-mode.md#the---parallel-stages-path-bare-chroot)) |

### `Direct` (kaniko mode on `/`)

`RUN` runs on `/` like kaniko, in its own session, whose process group is
killed once the command exits ([kaniko-mode](kaniko-mode.md)).
`Sandbox::start_direct` starts it:

- **No directory, no journal** (nothing is placed on `/`).
- **Runs on `/` only.** `start_direct` refuses any other root, a sandbox that
  can run the full sandbox refuses `Direct` on a root other than `/`
  (`Isolation::Full` runs it), and a host where root may not mount is refused
  before a build starts (ADR-0050, ADR-0060).
- **Processes:** ranikust is a child subreaper, so every process the `RUN`
  leaves, `setsid` ones included, becomes its child and is killed and reaped by
  pid once the command exits.
- **Capabilities:** the child drops every capability outside Docker's defaults
  from its bounding set (`CAP_SETPCAP`); its other sets go to the defaults in
  every case. Without `CAP_SETPCAP`, a `RUN` that would keep any capability
  outside Docker's defaults (`CAP_SYS_PTRACE`, `CAP_DAC_READ_SEARCH`,
  `CAP_SYS_ADMIN`...) is refused at `start_direct`
  ([ADR-0051](../adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md),
  [ADR-0057](../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md)).
  Before it, still single-threaded, `trim_to_run_capabilities` gives
  ranikust's own process the sets its `RUN` keeps
  ([ADR-0078](../adr/0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md)).
- **Refusals** ([ADR-0057](../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md)):
  mounts, secret files and `--network=none` (`Error::Direct`; `rnk-builder`
  leaves cache mounts out first, with a warning).
- **Heredoc:** a `#!` heredoc runs from a sealed memfd with `execveat`, read by
  its interpreter as `/dev/fd/<n>`.

## Differences from Docker

- The default profile is Docker's ([above](#per-run-sequence)), compiled by
  ranikust's own code, which differs from runc's libseccomp:
  - Only the native architecture: Docker also compiles a profile's calls for
    i386 and x32 on x86_64 (arm on aarch64), which get `EPERM` here, so a
    32-bit program in a `RUN` needs `--seccomp=unconfined` (ADR-0109).
  - Only the calls `libc` names: one it does not (`cachestat`, `futex_wait`,
    `statmount`, `uretprobe`, and `fchmodat2` on aarch64) gets `ENOSYS`
    where Docker allows it, until `libc` numbers it (ADR-0109). The C
    libraries fall back from most; the known exceptions follow.
  - `uretprobe` is made by the kernel's return-probe trampoline, not a C
    library (x86_64, Linux 6.11 and later): a uretprobe a host tool sets on
    a `RUN`'s process can crash it where the kernel's seccomp does not let
    that call through.
  - A ranikust built for `aarch64-unknown-linux-gnu` (not a release target)
    has no number for `renameat` and `sync_file_range` either. The C
    libraries implement `rename(3)` with `renameat` there, so `rename`
    fails in every `RUN` of such a build.
  - A profile that asks for what ranikust does not support is refused, never
    weakened: two conditions on one argument (runc ORs them), a rule
    without names (Docker skips it), and a field Docker's format does not
    have (Docker ignores it).
- A `RUN` gets stdout and stderr as pipes of its own, owned by its user and
  copied by a thread to ranikust's streams, never ranikust's own descriptors:
  runc instead gives the container's own streams to the container's user.
  Either way `/dev/stdout` reopens (kaniko#2928), and here a `RUN` cannot
  truncate or rewrite an operator's log file (ADR-0057).
- A device node a `RUN` creates with `mknod` does not open, even one Docker's
  device cgroup allows (`/dev/null`'s 1:3): every mount but the `/dev` node
  binds and `devpts` is `nodev`.
- `/dev/null` and the other nodes are read-only binds: `chmod` and `chown` on
  them fail with `EROFS`.
- A `RUN` gets no resource limits, so a fork bomb is bounded only by the
  environment ([ADR-0007](../adr/0007-run-anywhere-shared-store.md)). Where a
  delegated tree exists it gets cgroups of its own, limited only in how many
  cgroups it may make below them, and how deep
  ([`RUN` cgroups](#run-cgroups)).

## Hardening planned for Phase 3

Not built yet (ADR-0003).

- `--userns`: the build runs in a user namespace, so uid 0 inside maps to an
  unprivileged uid on the host. Every parent directory of the state directory
  must be traversable by the mapped uid. Decided
  ([ADR-0104](../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)):
  the whole build runs in the namespace, as rootless mode does
  ([ADR-0099](../adr/0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)),
  with a range for root read from a dedicated `ranikust` entry of `/etc/subuid`
  and `/etc/subgid` (the build is refused before anything is written where it is
  missing) and extracted layers keyed by their id map; an image with an owner
  outside that range is refused
  ([ADR-0098](../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md));
  a `RUN`'s command runs where the init's uid is unmapped, in rootless mode too
  ([ADR-0083](../adr/0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md)
  (b)). How (a namespace nested per `RUN`, or a map without the init's uid) is
  settled with [#83](https://github.com/tran4774/ranikust/issues/83).

## Failure handling

- A failing mount step aborts the `RUN` with the step and errno; fallbacks are
  chosen once by the probe ([modes-and-detection](modes-and-detection.md)), not
  by trial during the build.
- A non-zero exit of the command fails the stage; ranikust still removes stubs
  and lets the namespace collapse.
- The `RUN` cgroups are probed once at start-up; a failed probe leaves the
  build as it would be without them, a failing step during a `RUN` fails it
  ([`RUN` cgroups](#run-cgroups)).
- Overhead per `RUN` is about 1 ms (1-2.5 ms in the Go prototype including its
  re-exec), negligible next to any real command.

## Open questions

None. What Q-032 asked to verify is built or tracked as tasks: `--seccomp`
([#82](https://github.com/tran4774/ranikust/issues/82),
[ADR-0109](../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md))
and `--userns` ([#83](https://github.com/tran4774/ranikust/issues/83),
[ADR-0104](../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md))
in Phase 3; the bind targets against changes during set-up
([#88](https://github.com/tran4774/ranikust/issues/88)) and the host-safety
attack suite across modes ([#89](https://github.com/tran4774/ranikust/issues/89))
in Phase 6. `--network=none` was verified end to end at
[Phase 1's acceptance](../agents/phase-1.md#acceptance-criteria); its rootless
form in the build's user namespace by T1 and T2
([#78](https://github.com/tran4774/ranikust/issues/78)), and in a rootless
build with [#79](https://github.com/tran4774/ranikust/issues/79).

## References

- [Survey 02](../research/survey/02-kaniko-mode-requirements.md) §3;
  [survey 05](../research/survey/05-decisions.md) Q1.
- [Experiment 02](../research/experiments/02-run-sandbox.md): sequence
  verified on alpine, debian and node images; minimum privileges; `chroot`
  escape; capability drop; `PDEATHSIG` (without it `RUN sleep 1000` survived
  and kept writing); stubs left after `SIGKILL`; overhead.
- [Experiment 06a](../research/experiments/06a-root-rerun.md): as real root,
  all steps without fallback; bare-`chroot` escape reached the host's `/`;
  overhead 1.05 ms against a 0.11 ms baseline.
