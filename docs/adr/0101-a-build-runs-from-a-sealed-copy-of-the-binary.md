---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0101: A build runs from a sealed copy of ranikust's binary: a detached read-only overlay, else a sealed memfd, else an `O_TMPFILE` copy

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
of another file does. runc does not bind either: since 1.2 it executes
`runc init` from a sealed copy. How does ranikust give every process it
forks a binary no one can write?

## Considered Options

- (x) At start-up, run again from a sealed copy of the binary, as runc's
  `libcontainer/exeseal`: a read-only overlay mounted nowhere where ranikust
  may mount (no copy), else a sealed memfd, else an `O_TMPFILE` copy. The
  same in the full sandbox and in `Direct` on `/`.
- ADR-0082's read-only bind over the binary's path: does not change
  `/proc/self/exe` (measured, below).
- Keep the three barriers everywhere (ADR-0082's option (d)): the last one,
  `ETXTBSY`, is a kernel behaviour that a future kernel may drop.

ADR-0082's option (a), a memfd re-exec, is (x)'s second way; its option
(c), a trampoline executed per `RUN`, would leave the init a copy of the
installed binary.

## Decision Outcome

At start-up a build runs again from a sealed copy of ranikust's binary,
tried in order: a read-only overlay of the binary's directory that is
mounted nowhere, where ranikust may mount; else a sealed memfd; else an
`O_TMPFILE` copy; and when none works it builds on from the installed
binary with today's barriers and a warning (the maintainer, 2026-11-04:
"tất cả theo khuyến nghị", option (x)).

- **When.** For `build` only, after the flags are parsed and the kernel
  checked, while the process is single-threaded and has read nothing else.
  The copy starts over from `main` with the same arguments, environment and
  descriptors. `version`, `--help` and a refused command line make no copy,
  so a `#!/proc/self/exe` script is still refused by the command line
  before anything else.
- **The overlay.** `fsopen`, `fsconfig` and `fsmount`; lower layers the
  binary's directory and an empty directory made for it in `TMPDIR` (else
  `/tmp`) and removed once mounted, both passed as `/proc/self/fd/<n>`;
  `xino=off`; no upper layer; mounted `MOUNT_ATTR_RDONLY|NODEV|NOSUID` and
  never attached. The file opened from it must have the running binary's
  inode number, size and modification time. With no upper layer the kernel
  has no way to write through it, and with no attachment nothing can
  unmount or remount it.
- **The memfd.** `memfd_create` with `MFD_EXEC` (without it before Linux
  6.3), the binary copied in, sealed `F_SEAL_SEAL|SHRINK|GROW|WRITE`.
  `vm.memfd_noexec=2` refuses it, and the next way is tried.
- **The `O_TMPFILE` copy.** In `TMPDIR` (else `/tmp`), `O_EXCL` so it can
  never get a name, run from a read-only descriptor. It has no seal: only
  `ETXTBSY` keeps the running copy from being written, as for the installed
  binary today, but such a write would not reach the installed binary.
- **The run of the copy.** `execve("/proc/self/fd/<n>")` with
  `RANIKUST_SEALED_COPY=<way>` added and the core-file limit ranikust
  started with given back, which the copy records again
  ([ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)).
  Seeing the variable, the copy makes no copy (no loop), checks
  `/proc/self/exe` (on an overlay mounted read-only, the memfd's seals, no
  link), takes its name from `argv[0]` again, and logs the way at debug.
- **Fallback.** When every way fails, or the copy's check fails, the build
  goes on with a warning and the three barriers ADR-0082 lists. No refusal.
- **Test hook.** `RANIKUST_DEBUG_FAULT=seal-memfd|seal-tmpfile|seal-none`
  skips the ways before the one named, or all of them.
- **Boundary.** The copy is made in `rnk-sandbox`. Its overlay seals
  ranikust's own binary and is never attached: the one exception to the
  crate's rule that overlays belong to `rnk-snapshot`.
- Q-451's answer stands: the init is still a copy-on-write copy of
  ranikust, now of the sealed copy.

### Consequences

- Good, because no process ranikust forks hands a `RUN` a binary it could
  write through `/proc/<pid>/exe`, in the full sandbox and in `Direct` on
  `/`, without relying on `ETXTBSY` (the overlay and the memfd) [measured].
- Good, because the overlay copies nothing, and the memfd needs no mount,
  which is why `Direct` on `/` gets it too.
- Bad, because where ranikust may not mount the copy takes the binary's size
  in memory for the build's life, about 11.4 MiB (x86_64 static musl),
  counted in the build's memory cgroup.
- Bad, because start-up runs one more `execve` and, without the overlay,
  copies the binary: about 2.6 ms [measured].
- Bad, because the `O_TMPFILE` copy and the warned fallback still rely on
  `ETXTBSY`.
- Neutral, because the installed file is no longer the running binary, so
  `ETXTBSY` no longer guards it during a build. The running build no longer
  reads it, and in `Direct` on `/` a `RUN`, root on the container's `/`,
  could already replace it by name.
- Neutral, because file capabilities and set-user-ID bits of the installed
  file do not carry over to the copy (`nosuid`; a memfd has none). ranikust
  uses neither.

## Evidence

- The bind does not work [measured]: on branch
  `feat/sandbox-ro-self-exe-bind` (T2, the VM, Linux 6.8), after a read-only
  bind over the binary's path in the build's mount namespace,
  `/proc/self/exe` of the running process still named the original mount,
  not the bind's.
- runc [external] (main, `libcontainer/exeseal`, since 1.2):
  `sealedOverlayfs` (`overlayfs_linux.go`) builds an overlay with
  `fsopen`/`fsconfig`/`fsmount`, never attached, lower layers the binary's
  directory and a dummy, and opens the binary from it: "There is no way to
  'unwrap' this (unlike MS_BIND+MS_RDONLY) … This only works for privileged
  users"; its fallbacks (`cloned_binary_linux.go`) are a sealed memfd with
  `MFD_EXEC` (`vm.memfd_noexec=2` handled), then `O_TMPFILE`, then an
  unlinked temporary file; `container_linux.go:521-525,600` runs
  `runc init` as `/proc/self/fd/<n>` of that file.
- The copies [measured]: T0 `the_ways_are_tried_in_order_until_one_runs`,
  `a_memfd_copy_is_the_binary_sealed_against_writes` (writes and resizes
  `EPERM` through any descriptor), `a_tmpfile_copy_is_the_binary_with_no_name_open_read_only`;
  the binary's `a_build_runs_from_a_sealed_copy_made_once` (a memfd as a
  user, once; the hook's ways; a marker set from outside only warns); T2
  `an_overlay_copy_is_the_binary_read_only_and_mounted_nowhere` (`EROFS` for
  root, a mount ID in no mount table, the empty layer gone); the VM smoke
  and quick suites' `tests/vm/guest/sealed-exe.py`: in overlay mode, in
  kaniko mode per stage and with the overlay skipped, ranikust and its init
  run one copy (a detached read-only overlay, or a sealed memfd) that root
  cannot open for writing; a kaniko-mode build on a container's `/` runs
  from a sealed memfd ([testing](../testing.md)).
- Cost [measured]: on this branch's x86_64 static binary (11,901,064 bytes),
  as a user on Linux 7.0 (the developer's machine, while a T2 VM ran),
  start-up to the set-up refusal took 3.46 ms
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

Revisit when rootless overlay mode lands (Phase 3): runc notes that an
overlay can be made inside a user namespace since Linux 5.11, which would
let rootless builds take the overlay instead of the memfd.
