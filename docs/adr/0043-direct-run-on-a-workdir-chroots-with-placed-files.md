---
status: proposed
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0043: A `Direct` `RUN` on a workdir other than `/` enters it with `chroot`, with journaled placed files and the original `RLIMIT_NOFILE`

## Context and Problem Statement

Sequential kaniko mode runs `RUN` with `Isolation::Direct`: no namespace,
no mount, as kaniko's `runCommandInExec` (`kaniko@44aa7ddc0
pkg/commands/run.go:288-411`, `exec.Command` with `Setpgid`, the `USER`
credentials, `WORKDIR` and the environment). Phase 1 allowed it on `/`
only. The design also builds in kaniko mode on a host `--workdir`
([kaniko-mode](../design/kaniko-mode.md#variants)), where the root is not
`/` and nothing gives the command the container's `/etc/hosts`,
`resolv.conf` or `/dev`. ranikust also raises its own `RLIMIT_NOFILE` for
the walk (`ADR-0038`), which every `RUN` would inherit. Phase 2 task 7
decides how such a `RUN` is entered and what it sees.

## Considered Options

- Enter the root: `chroot` (needs only `CAP_SYS_CHROOT`); or refuse
  `Direct` off `/` and require the full sandbox wherever the root is not
  `/`.
- Network files and `/dev`: put ranikust's copies at the fixed paths for
  the `RUN`, the image's entries renamed aside in the same directory and
  renamed back after; copy the image's content to the state directory and
  write it back after (experiment 12's `nocap`); or place nothing.
- `RLIMIT_NOFILE`: a per-`RunSpec` field; a `Sandbox`-wide setting given
  the limit captured before the raise; or a process-global record.

## Decision Outcome

Proposed: `Direct` on a root other than `/` `chroot`s into it; ranikust's
`resolv.conf`, `hosts` and `hostname` and the `/dev` nodes are put in place
for the `RUN`, journaled before anything changes and undone after it, the
image's entries renamed aside and back; every `RUN` gets the
`RLIMIT_NOFILE` set with `Sandbox::set_nofile_limit`.

- On `/` nothing changes and nothing is placed: the command sees the
  container's own mounts, as under kaniko.
- Off `/` only where the full sandbox cannot run: with a probe result
  (mounting possible) such a `RUN` is refused and the caller uses
  `Isolation::Full` on that root, as ADR-0014 keeps a bare `chroot` from
  any environment that can mount. In the `chroot` the child checks that
  the new `/` is the directory the parent checked (device and inode), then
  cuts the bounding, effective and permitted sets to Docker's defaults
  before switching user, as the full sandbox does.
- A root that is the host's `/`, `/etc` or `/dev` by device and inode,
  whatever path or bind mount (`/proc/1/root` included) reaches it, is
  refused before anything is placed, in `Sandbox::run` and in
  `Sandbox::start_with_root`.
- Placed paths are fixed (`journal::PLACEABLE`): `/etc/resolv.conf`,
  `/etc/hosts` (the host's, plus the host name), `/etc/hostname` (the real
  host name: there is no UTS namespace), `/dev/{null,zero,full,random,
  urandom,tty}` by `mknod` with Linux's numbers. A node the kernel refuses
  (`EPERM` in a user namespace) is left out and its original put back at
  once. A missing `/etc` or `/dev` is a journaled stub; a symlinked one,
  or a directory at a placed path, fails the `RUN`.
- A `swap` record (path, aside name, parent times) is synced before the
  rename; a `placed` record holds the parent's mtime after. Undo, newest
  first: the aside entry is renamed back over whatever is at the path, or,
  with none, the placed entry is removed unless it became a directory; the
  parent's times go back to m0 only if its mtime is still m1, as for stubs
  (`design/kaniko-mode.md#per-stage-directories`). Placing and undoing stay
  on the root's own mount (`rnk_rootfs::RootPath::stay_on_mount`,
  `RESOLVE_NO_XDEV`); an entry on another mount, or a mount point, is left
  alone. A failed rename aside forgets its record.
- Replay: `Sandbox::start_with_root(caps, dir, workdir)` opens the workdir
  once and also replays journals whose root is that directory, matched by
  device, inode and mount on the opened descriptor, never by path; on it
  only `swap` records and the `/etc` and `/dev` stubs are undone. A `swap`
  record naming another path, or an aside name not of the form
  `<dir>/.ranikust-aside-<id>-<name>`, is skipped, so a planted journal
  cannot remove or replace other files.
- No `/proc` and no `/sys`: `Direct` cannot mount. The child opens the
  host's `/dev/null` for `Stdio::Null` before entering the root.
- `NofileLimit::current()` is read by `ranikust` before it raises its own
  limit and passed to `Sandbox::set_nofile_limit`; the child sets it right
  before `execve`, for `Full` and `Direct` alike. Without it a `RUN`
  inherits ranikust's limit, as before.

### Consequences

- Good, because a `Direct` `RUN` on a host workdir resolves names and has
  `/dev/null`, and nothing placed reaches a layer or survives a crash.
- Good, because the image's entries keep their inode, owner, mode, times
  and xattrs: they are renamed, never rewritten.
- Bad, because the `RUN` can see the aside entries (`ls -a /etc`) and has
  no `/proc`; a host that can mount runs that workdir with the full sandbox
  instead, which the sandbox enforces (the caller picks it, Phase 2 tasks 6
  and 8).
- Bad, because a bare `chroot` is no boundary for root, even with Docker's
  default capabilities (`CAP_SYS_CHROOT` is one): as on `/`, the host or
  container ranikust runs in is the only isolation (threat T5).
- Neutral, because a `RUN` that rewrites a placed file loses the change,
  as under BuildKit's bind and kaniko's mount point.

## Evidence

- kaniko source at `44aa7ddc0`: `pkg/commands/run.go:288-411`, no `chroot`,
  no placed file; the container's `/etc/hosts` and `resolv.conf` are
  mount points its ignore list skips.
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md)
  (`-sandbox nocap`): copying `hosts` and `resolv.conf` into a `chroot`
  root and restoring them with the mtime of `/etc` built W3 with an image
  identical to the sequential one; the unrestored mtime put `/etc` in
  every layer.
- `rnk-sandbox` T0 tests: the swap, undo and replay
  (`journal::tests::swapped_files_*`, `replay_puts_swapped_files_*`,
  `planted_swap_records_*`, `replay_on_the_shared_root_skips_*`,
  `a_failed_rename_aside_*`), a `Direct` `RUN` on a temp workdir that
  stops at `chroot` as a normal user with everything restored, and the
  refusals (`direct_off_slash_is_refused_*`: with a probe result, and on
  the host's `/etc` and `/dev`); `rnk-rootfs`
  `stay_on_mount_refuses_to_resolve_into_another_mount`. The host-safety
  review of the first version found the capability, identity and mount
  gaps these close. The T2 test
  `direct_runs_in_a_chroot_with_placed_files_as_root` covers the real run
  in the VM [not run yet].

## More Information

Extends [ADR-0014](0014-kaniko-mode-sequential-by-default.md) and the
sandbox design ([sandbox](../design/sandbox.md#variants-by-environment));
the `RLIMIT_NOFILE` raise is `ADR-0038`'s (Phase 2 task 1). Revisit the
aside names if a build tool trips over them.
