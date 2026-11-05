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

The privilege probes of [modes-and-detection](modes-and-detection.md) run in
the same single-threaded window, before the spawner thread starts.

## Per-`RUN` sequence

The child is created with `CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWUTS` and
`CLONE_NEWIPC`, plus `CLONE_NEWNET` for `--network=none` (`clone_flags` in
`spawner.rs`; a `Direct` `RUN` is cloned with no namespace flag). It is never
cloned with `CLONE_NEWUSER`: a `RUN` stays in the user namespace ranikust runs
in, with the uid and gid of its `USER` as the image gives them. The only user
namespace ranikust creates is the helper's for `--allow network.none-userns`
(below). User namespaces for rootless mode and `--userns` are planned for Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)), see
[Hardening planned for Phase 3](#hardening-planned-for-phase-3); the parts that
belong to them are marked *planned*.

**`--network=none`** needs `lo` up, which needs `CAP_NET_ADMIN` over the new
network namespace. The first such `RUN` probes once
([ADR-0033](../adr/0033-network-none-loopback-through-a-user-namespace.md)):

- Where the build process can bring `lo` up in a namespace of its own (root on
  a host, `--privileged`, rootless podman), the clone creates the network
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
  rule keeps its action, and where several match one call the strictest wins.
- A name the architecture does not know is skipped under `SCMP_ACT_ALLOW`, as
  runc does, and refuses the profile under any other action. Under a default
  action that refuses, a number `libc` does not name gets `ENOSYS`, so that C
  libraries fall back. A call of another architecture (i386 or x32 on x86_64,
  arm on aarch64) gets `EPERM`.
- Refused with exit 2, before anything is written: `SCMP_ACT_NOTIFY`,
  `SCMP_ACT_TRACE`, a `listenerPath`, a flag other than `TSYNC`, `LOG` and
  `SPEC_ALLOW`, an `SCMP_ACT_ERRNO` whose errno is 0 (a faked success; the
  message names `syscalls[N]` or `defaultErrnoRet`), one from 512 to 530,
  the kernel's `ERESTART*` codes and their neighbours (on 516 the kernel may
  run its restart block instead of returning the refusal), or above 4095,
  two conditions on one argument in a rule, a rule without names (the
  message names the rule, `syscalls[N]`; ADR-0109), a filter longer than the
  kernel's 4096 instructions, and anything malformed; a file that is not a
  regular file or is over 1 MiB.
- The profile is the operator's and may refuse any call with any errno
  `--seccomp` accepts, so every step that need not follow it comes before
  it, in the order of step 4. After it come the `capset` to the default
  sets, since the install needs `CAP_SYS_ADMIN` without `no_new_privs`, so
  that a setuid binary of the `RUN` keeps working, and the init's and the
  command's own steps (step 5 on). A filter around ranikust may still fake a
  success; the child checks what it can (the init's fork, an `execve` or
  `execveat` back without an error and a blocking `waitpid` back with no
  child fail the `RUN`, and a read goes into a value set beforehand), and
  the rest is in the
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
     `/proc/self/fd`, else every number up to the hard limit), in
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
2. **User namespace** (*planned, Phase 3; no `RUN` runs this step today*): the
   command's user namespace leaves the init's uid out
   ([below](#hardening-planned-for-phase-3)) and maps the full id range
   available, allowing `setgroups`; otherwise `USER _apt` (used by apt-get)
   fails with `setgroups: EPERM`.
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
   2. A new `proc`. If `/proc` is masked and a new user namespace was created,
      the kernel refuses (`EPERM`); *planned, Phase 3:* fall back to an rbind
      of the existing `/proc`. Today a refused `proc` mount fails the `RUN`
      (`Step::MountProc`). `proc`, `sysfs` (3) and the `/dev` tmpfs (4) go only
      on a real directory of the root: `/proc`, `/sys` or `/dev` as a symlink
      is refused before any stub is placed, and the child resolves them again
      with `RESOLVE_NO_SYMLINKS`, as runc requires for `proc` and `sysfs`.
      Through a symlink, `proc` could land below a directory a later mount
      hides, out of reach of step 7.
   3. A read-only `sysfs`. It needs privileges over the user namespace that
      owns the network namespace, so it fails with the host network plus a new
      user namespace; *planned, Phase 3:* fall back to an rbind of `/sys`
      remounted read-only. Today a refused `sysfs` mount fails the `RUN`
      (`Step::MountSys`).
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
   022, installs ADR-0033's filter and the seccomp profile on top of it
   ([above](#per-run-sequence)), and only then sets the effective and
   permitted sets (step 5).
5. **Drop capabilities**, for **every** `USER`: reduce the **bounding** set to
   Docker's 14 default capabilities, clear the ambient set, keep the defaults
   in the effective and permitted sets and leave the inheritable set empty, as
   Docker since 20.10.14. A non-root user has no effective capabilities but a
   full bounding set, which a setuid binary in the image would regain. The
   drops go up to the kernel's last capability; an `EINVAL` ends them only
   where it could not be read and 63 was assumed.

   Then the full sandbox's child becomes the `RUN`'s **init**
   ([ADR-0080](../adr/0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md)):
   - Not dumpable and in a session of its own since step 4, it zeroes its
     copy of the argument and environment strings ranikust started with
     (`/proc/self/stat` fields 48 to 51, refused as `Step::Init` if zero;
     `/proc/1/cmdline` has no ptrace check) and forks. The new process, PID 2,
     checks that its parent is PID 1 (a fork a filter around ranikust faked
     fails as `Step::Init` with `EPROTO`), waits on a close-on-exec go pipe,
     then runs steps 6 to 8 and becomes the command.
   - The init keeps the death signal of step 1, drops every capability, names
     itself `ranikust-init`, checks the death signal is still armed and the
     parent alive, and closes every descriptor but the status pipe's write end
     and the go pipe's, the latter last: that lets the command go on (a failure
     before kills the command unrun and is reported as `Step::Init`, unless
     a filter around ranikust fakes the command's read of that pipe).
   - A read after the profile answered with `EINTR` (the argument areas',
     the parent check's, the go pipe's) fails the `RUN` as `Step::Init`
     instead of reading again: with no handler, the kernel restarts a read
     a signal interrupts, so only a filter answers so, and it could
     forever.
   - It then reaps every process that reparents to it until the command exits
     or is killed (detaching from a command that made it its tracer), writes
     the command's wait status on the status pipe, and exits; a `waitpid`
     back with no child (0, or an error, `EINTR` included: with no handler,
     the kernel restarts one a signal interrupts) ends it with 125.
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
7. `setgroups`, `setgid`, `setuid` to the ids of `USER`, which `rnk-rootfs`
   looked up in the root's `/etc/passwd` and `/etc/group` before the fork
   [inferred]. A numeric uid with no passwd entry gets gid 0, as in Docker. A
   change of uid or gid clears the death signal, so it is armed again: in
   `Direct` toward ranikust and checked; in the full sandbox's command toward
   the init, only as a second line (the init's signal is what ends the
   namespace with ranikust, whatever the command does to its own: a set-id
   `execve` or `prctl(PR_SET_PDEATHSIG, 0)` clears it).
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
     sandbox, which then refuses its arguments (threat-model section 6).
     [ADR-0082](../adr/0082-full-sandbox-binds-the-binary-read-only-over-itself.md)
     has the full sandbox bind the binary read-only over itself (not built yet,
     [#32](https://github.com/tran4774/ranikust/issues/32)); `Direct` on `/`
     keeps these barriers.

Between `clone` and `execve` the child of a multi-threaded process only makes
system calls on arguments built before the clone: no allocation, no lock. A
failing step writes the step and the errno to a close-on-exec pipe and exits
127; the parent reads end-of-file once `execve` succeeded.

**Waiting.** The parent waits for the child, in the full sandbox the init. When
the command exits, the init reports its status and exits, and the kernel kills
everything left in the namespace, so no `tini` and no process-group kill are
needed; exit codes and signals read as they would from the command itself. A
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
   metadata;
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

## Variants by environment

| Environment | Differences |
|---|---|
| Real root on a host | Every step works without a fallback |
| `CAP_SYS_ADMIN` with Docker's seccomp profile | `MS_MOVE` instead of `pivot_root`, only where `/` is a mount root, keeping Docker's 14 capabilities: the old root stays mounted underneath, and `..` stops at the moved root only then [measured] (the `sysadmin` variant of `tests/vm/guest/smoke.sh`, kernels 5.15 and 6.8). Inside a plain `chroot` ranikust refuses before changing anything ([modes-and-detection](modes-and-detection.md#refusal-and-guidance)) |
| Rootless (user namespace) | Planned, Phase 3: multi-range id map; `proc` may be an rbind; `sysfs` rbind; `sethostname` may fail. Today ranikust creates no user namespace; started inside one that something else made (rootless `podman run --privileged`), it runs the sequence above as written (T1, `just smoke`) |
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
- A `RUN` gets no cgroup limits, so a fork bomb is bounded only by the
  environment ([ADR-0007](../adr/0007-run-anywhere-shared-store.md)).

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
form is in [#78](https://github.com/tran4774/ranikust/issues/78) and
[#79](https://github.com/tran4774/ranikust/issues/79).

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
