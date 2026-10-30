# `RUN` sandbox

How ranikust runs a `RUN` command inside a stage root with isolation roughly
equivalent to `docker build`, in overlay mode and in kaniko mode where
mounting is possible.

Decisions: [ADR-0003](../adr/0003-dockerfile-trust-and-default-isolation.md),
[ADR-0010](../adr/0010-run-network-host-or-none.md).

## Invariants

- A `RUN` never outlives the build: its children die with it, even on
  `SIGKILL` of ranikust.
- Every mount made for a `RUN` exists only in that `RUN`'s mount namespace;
  the parent, which takes the diff, sees the root as the image left it.
- The command enters its root with `pivot_root`. A bare `chroot` is never
  used where mounting is possible: root inside `RUN` keeps `CAP_SYS_CHROOT`
  and escapes it.
- Users and groups are resolved from the root's `/etc/passwd` and
  `/etc/group` by `rnk-rootfs`, never from the host's.
- Whatever ranikust creates on the real root (stubs) is journaled before it
  is created and removed before the diff.

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
   roots below the state directory: a planted journal or run directory
   cannot point it at host files.

The privilege probes of [modes-and-detection](modes-and-detection.md) also
run in this single-threaded window: `ranikust` runs this crate's probes
(namespaces, uid map, `proc`, `pivot_root`/`MS_MOVE`, `sysfs`) and
`rnk-snapshot`'s overlay probes in order, before the spawner thread starts.

## Per-`RUN` sequence

The child is created with `CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWUTS` and
`CLONE_NEWIPC`, plus `CLONE_NEWUSER` when ranikust lacks `CAP_SYS_ADMIN`, plus
`CLONE_NEWNET` for `--network=none`.

`--network=none` needs `lo` up, which needs `CAP_NET_ADMIN` over the new
network namespace. The first such `RUN` probes it once
([ADR-0033](../adr/0033-network-none-loopback-through-a-user-namespace.md)):
where the build process can bring `lo` up in a namespace of its own (root
on a host, `--privileged`, rootless podman), the clone creates the network
namespace and the child brings `lo` up after entering the root, as before.
Where it cannot (Docker's default capabilities plus `SYS_ADMIN`), the `RUN`
is refused, naming `--allow network.none-userns`. With that opt-in, a
helper process creates a user namespace and a network namespace owned by
it, brings `lo` up there, sends a descriptor of the namespace over a
close-on-exec socket pair (`SCM_RIGHTS`) and exits; the descriptor keeps
the namespace alive until the `RUN` is reaped. The child is cloned without
`CLONE_NEWNET` and joins that namespace with `setns` as its first step,
before any mount, so `sysfs` shows it. The child stays in the build's
user namespace with its uid, gid and capabilities, but as root it holds
every capability in the helper's user namespace (its owner is uid 0):
after every mount and before the capability drop it installs a seccomp
filter. The filter refuses `setns`, `unshare`/`clone` with
`CLONE_NEWUSER`, `io_uring`, `socket(AF_KEY)`, the x_tables, arp_tables,
ebtables, IPVS, IPsec-policy and multicast-routing options of
`setsockopt`/`getsockopt` at `SOL_IP` and `SOL_IPV6`, `TCP_REPAIR`, and
syscalls of another architecture with `EPERM`; netlink other than route,
sock_diag, audit and uevent with `EPROTONOSUPPORT`; `clone3` with `ENOSYS`.
Every other interface `CAP_NET_ADMIN` opens over that namespace stays open,
rtnetlink first; the ADR lists them as residual risk.

1. `prctl(PR_SET_PDEATHSIG, SIGKILL)`, then check that the parent process
   did not die before: `getppid()` is 0 in a new PID namespace, so the child
   reads the parent pid in its own `/proc/self/stat` of the outer procfs,
   opened before entering the root; a reparented child shows another pid.
2. In a new user namespace, map the full id range available and allow
   `setgroups`; otherwise `USER _apt` (used by apt-get) fails with
   `setgroups: EPERM`.
3. Mounts, in this order:
   1. Bind the root onto itself (required by `pivot_root`) and remount the
      bind `nodev`: Docker blocks device nodes with its device cgroup, which
      ranikust does not have, so no node in the root opens, neither one a
      `RUN` creates with `mknod` nor one from a layer.
   2. A new `proc`. If `/proc` is masked and a new user namespace was created,
      the kernel refuses (`EPERM`); fall back to an rbind of the existing
      `/proc`.
   3. A read-only `sysfs`. It needs privileges over the user namespace that
      owns the network namespace, so it fails with the host network plus a
      new user namespace; fall back to an rbind of `/sys` remounted
      read-only.
   4. A `nodev` tmpfs on `/dev`, with `null`, `zero`, `full`, `random`,
      `urandom` and `tty` bind-mounted from the outer `/dev` (`mknod` needs
      `CAP_MKNOD` in the initial user namespace). Each bind is its own mount
      and keeps the outer flags, so the node opens; it is remounted
      read-only, because it is the host's inode and `chmod` from `RUN` would
      change it. Add `devpts` (`newinstance,ptmxmode=0666`, plus `gid=5` only
      if gid 5 is mapped), `shm`, and the symlinks `ptmx`, `fd`, `stdin`,
      `stdout`, `stderr`.
   5. `resolv.conf`, `hosts`, `hostname`: bind a **writable copy**, as
      Docker lets `RUN` write `resolv.conf`. Resolve the target inside the
      root with `openat2(RESOLVE_IN_ROOT)`, because it may be a symlink. A
      read-only remount must keep the source mount's locked flags
      (`statfs().f_flags`: `nosuid`, `nodev`, `noexec`, `relatime`) or it
      fails with `EPERM` in a user namespace.
   6. `RUN --mount`: `type=cache` binds a cache directory leased from
      the store with its `sharing=` (`Store::cache_lease`), `type=bind`
      binds the context or another stage's root. `type=secret` follows
      BuildKit's secret mount: after the other mounts, the child mounts a
      tmpfs (`nodev`, `nosuid`, `noexec`, `0700`) over the `RUN`'s scratch
      directory, writes each secret there from its own memory, gives it
      its owner and mode (`0400`, root's, unless the mount asks
      otherwise), binds it read-only (`nosuid`, `nodev`, `noexec`) at its
      target and detaches the tmpfs. The secret never reaches a disk and
      dies with the `RUN`'s mount namespace. A secret with `env=` is a
      variable of that `RUN` instead. Every bind is remounted `nodev`
      (ADR-0032).
      The child opens each source in its own mount namespace (`O_PATH`,
      no symlink as the last component, type checked) and binds it through
      `/proc/self/fd`: a bind refuses (`EINVAL`) a source opened in another
      mount namespace, such as a descriptor of the parent.
   7. Docker's read-only and masked kernel files (moby
      `daemon/pkg/oci/defaults.go`): `/proc/bus`, `fs`, `irq`, `sys` and
      `sysrq-trigger` are bound read-only onto themselves, because writing
      a sysctl such as `kernel/core_pattern` needs only uid 0 and reaches
      the host kernel; `kcore`, `keys`, `timer_list`, `sched_debug`,
      `/sys/firmware` and the rest are masked, a file by a read-only bind of
      `/dev/null`, a directory by an empty read-only tmpfs. A path the
      kernel lacks is skipped.
   8. `pivot_root(".", ".")`, then `umount2(".", MNT_DETACH)`. Where seccomp
      blocks `pivot_root` (Docker's default profile, even with
      `CAP_SYS_ADMIN`, `EPERM`) and `/` is a mount root:
      `mount(root, "/", MS_MOVE)` then `chroot(".")`, as runc's
      `--no-pivot` ([Q-077](../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape)).
4. `sethostname`; ignore a seccomp refusal.
5. Drop capabilities, for **every** `USER`: reduce the **bounding** set to
   Docker's 14 default capabilities, clear the ambient set, keep the
   defaults in the effective and permitted sets and leave the inheritable
   set empty, as Docker since 20.10.14. A non-root user has no effective
   capabilities but a full bounding set, which a setuid binary in the image
   would regain.
6. `setsid`: no controlling terminal, so a `RUN` cannot read the
   operator's keys or push input into the operator's shell (`TIOCSTI`);
   stdin is `/dev/null` unless the caller says otherwise, as under Docker.
   Then a new anonymous session keyring, so ranikust's keys stay out of
   reach (root's user keyring stays reachable until `--seccomp`).
7. `setgroups`, `setgid`, `setuid` to the ids of `USER`, which
   `rnk-rootfs` looked up in the root's `/etc/passwd` and `/etc/group` before
   the fork [inferred]. A numeric uid with no passwd entry gets gid 0, as in
   Docker. A change of uid or gid clears the death signal, so it is armed
   and checked again.
8. `chdir` to `WORKDIR` (before the id switch, and again after if only root
   could traverse it); make every descriptor but stdio close-on-exec
   (`close_range`, or the list in `/proc/self/fd` before Linux 5.11), set
   the umask to 022, reset `SIGPIPE` (Rust ignores it, and ignored signals
   survive `execve`) and the signal mask; search `PATH` **after**
   `pivot_root` and **after** `setuid`; `execve`.

Between `clone` and `execve` the child of a multi-threaded process only
makes system calls on arguments built before the clone: no allocation, no
lock. A failing step writes the step and the errno to a close-on-exec pipe
and exits 127; the parent reads end-of-file once `execve` succeeded.

The parent waits for the child. The child is PID 1 of its namespace; when it
exits, the kernel kills everything left in the namespace, so no `tini` and no
process-group kill are needed. A cancel (`Sandbox::cancel`) sends that PID 1
`SIGKILL`, as BuildKit kills the process on a cancel; the child is
forgotten only once it has exited and before it is reaped, so the kill
never reaches a reused pid. A command running as PID 1 ignores `SIGTERM`
unless it installs a handler, as under Docker and BuildKit.

## Stub mount points

An image may lack a bind target (`alpine:3.20` has no `/etc/resolv.conf`,
`debian` no `/etc/hosts`). The empty file or directory created as a target is
a real file in the root: it would enter the layer, and it survives a killed
build. So, as in BuildKit's `executor/stubs.go`, with one journal per `RUN`
in `<state-dir>/sandbox/runs/`, locked while the `RUN` lives:

1. write the stub's path to the journal in the state directory, then create
   it;
2. remove stubs after the `RUN`, before the diff, and restore the parent's
   metadata;
3. on start-up, remove any stub left in the journal.

Because all other mounts live in the child's namespace, no ignore-list entry
is needed for `/proc`, `/dev`, `/sys`, `resolv.conf` or `hosts`.

## Variants by environment

| Environment | Differences |
|---|---|
| Real root on a host | Every step works without a fallback |
| `CAP_SYS_ADMIN` with Docker's seccomp profile | `MS_MOVE` instead of `pivot_root`, only where `/` is a mount root: the old root stays mounted underneath, and `..` stops at the moved root only then [inferred] ([Q-077](../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape)) |
| Rootless (user namespace) | Multi-range id map; `proc` may be an rbind; `sysfs` rbind; `sethostname` may fail |
| kaniko mode, cannot mount | `Isolation::Direct`, no sandbox: `RUN` runs on `/` like kaniko, in its own session, whose process group is killed once the command exits ([kaniko-mode](kaniko-mode.md)) |
| `--parallel-stages` | Bare `chroot`, no namespaces ([kaniko-mode](kaniko-mode.md#the---parallel-stages-path-bare-chroot)) |

## Differences from Docker

- A device node a `RUN` creates with `mknod` does not open, even one
  Docker's device cgroup allows (`/dev/null`'s 1:3): every mount but the
  `/dev` node binds and `devpts` is `nodev`.
- `/dev/null` and the other nodes are read-only binds: `chmod` and `chown`
  on them fail with `EROFS`.

## Optional hardening

- `--seccomp=default|unconfined|<file>`: a seccomp filter for `RUN`; profiles
  are generated per architecture.
- `--userns`: the build runs in a user namespace, so uid 0 inside maps to an
  unprivileged uid on the host. Every parent directory of the workdir must be
  traversable by the mapped uid.

## Failure handling

- A failing mount step aborts the `RUN` with the step and errno; fallbacks
  are chosen once by the probe ([modes-and-detection](modes-and-detection.md)),
  not by trial during the build.
- A non-zero exit of the command fails the stage; ranikust still removes
  stubs and lets the namespace collapse.
- Overhead per `RUN` is about 1 ms (1–2.5 ms in the Go prototype including
  its re-exec), negligible next to any real command.

## Open questions

- [Q-032](../open-questions.md#q-032-what-remains-to-verify-in-the-run-sandbox) What remains to verify in the RUN sandbox?
- [Q-077](../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape) How is the MS_MOVE fallback kept from a chroot escape?

Settled: a `RUN` sees the fixed host name `ranikust`
([Q-075](../open-questions.md#q-075-which-host-name-does-a-run-see)) and,
with the host network, ranikust's own `/etc/hosts`, with `--network=none`
loopback names, both plus the host name
([ADR-0031](../adr/0031-run-host-name-and-hosts-file.md)).

Settled elsewhere: `RUN` gets no cgroup limits, so a fork bomb is bounded
only by the environment ([ADR-0007](../adr/0007-run-anywhere-shared-store.md)).

## References

- [Survey 02](../research/survey/02-kaniko-mode-requirements.md) §3;
  [survey 05](../research/survey/05-decisions.md) Q1.
- [Experiment 02](../research/experiments/02-run-sandbox.md): sequence
  verified on alpine, debian and node images; minimum privileges; `chroot`
  escape; capability drop; PDEATHSIG (without it `RUN sleep 1000` survived
  and kept writing); stubs left after `SIGKILL`; overhead.
- [Experiment 06a](../research/experiments/06a-root-rerun.md): as real root,
  all steps without fallback; bare-`chroot` escape reached the host's `/`;
  overhead 1.05 ms against a 0.11 ms baseline.
