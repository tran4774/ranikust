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
of another file does. runc does not bind today: its fix of CVE-2019-5736
(v1.0.0-rc7) executes `runc init` from a read-only bind of the binary where
it can make one, else from a sealed copy; it dropped the bind in 1.1.15 and
1.2.0, and since 1.2.0 the parent makes the copy. How does ranikust give
every process it forks a binary no `RUN` can write?

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
- **Writes outside the state directory.** Only the `O_TMPFILE` copy, a file
  with no name in `TMPDIR`, made before the state directory is checked and
  gone with the build. The memfd writes no file.
- **The run of the copy.** `execve("/proc/self/fd/<n>")` with
  `RANIKUST_SEALED_COPY=<way>` added (then `; ` and why each way before it
  failed) and the core-file limit ranikust started with given back, which
  the copy records again
  ([ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)).
  Seeing the variable, the copy makes no copy (no loop), checks
  `/proc/self/exe` (the memfd's seals, or no link), takes its name from
  `argv[0]` again, and logs the way and the ways passed over at debug.
- **Fallback.** When both ways fail, or the copy's check fails, the build
  goes on with a warning and the three barriers ADR-0082 lists. No refusal.
  Only when a copy did not run and the core-file limit cannot be set back
  to 0 does the build stop, as when ADR-0051's limit cannot be set.
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
- Bad, because start-up runs one more `execve` and copies the binary:
  about 2.6 ms [measured]. runc tries its overlay first for this cost, which
  matters more to a container start than to a build.
- Bad, because the `O_TMPFILE` copy and the warned fallback still rely on
  `ETXTBSY`.
- Bad, because with the `O_TMPFILE` copy the set-up writes outside the
  state directory, store and outputs: a file with no name in `TMPDIR` (the
  host-safety invariant of `ARCHITECTURE.md` names it).
- Neutral, because the installed file is no longer guarded by `ETXTBSY`
  during a build. The build does not read it, and in `Direct` on `/` a
  `RUN`, root on the container's `/`, could already replace it by name.
- Neutral, because file capabilities and set-user-ID bits of the installed
  file do not carry over to the copy. ranikust uses neither.

## Evidence

- The bind does not work [measured]: with ADR-0082's bind built (commits
  `0cfbb80` to `fc22d06` of the pull request that closes #32, reverted by
  `1081130` before merge; T2, the VM, Linux 6.8), after a read-only bind
  over the binary's path in the build's mount namespace, `/proc/self/exe`
  of the running process still named the original mount, not the bind's.
- runc [external], v1.0.0-rc7 (commits `0a8e4117e7f7`, CVE-2019-5736, and
  `16612d74de5f`): `libcontainer/nsenter/cloned_binary.c` executes
  `runc init` from a read-only bind of `/proc/self/exe` where it can mount
  one, else from a sealed copy. The bind guards because runc executes it,
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
  `a_tmpfile_copy_is_the_binary_with_no_name_open_read_only`; the binary's
  `a_build_runs_from_a_sealed_copy_made_once` (a memfd, once; the hook's
  ways; a marker set from outside only warns); the VM smoke and quick
  suites' `tests/vm/guest/sealed-exe.py`: in overlay mode and in kaniko
  mode per stage ranikust and its init run one sealed memfd, and with the
  memfd skipped one `O_TMPFILE` copy, that root cannot open for writing; a
  kaniko-mode build on a container's `/` runs from a sealed memfd
  ([testing](../testing.md)).
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
