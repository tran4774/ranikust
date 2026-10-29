---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0016: Overlay mount options; `EXDEV` on lower directory renames is accepted

## Context and Problem Statement

Overlay mode mounts one overlay per layer being built. The option set decides
whether the mount works at all in a user namespace, which xattr namespace
`layers/` uses, whether copy-up fsyncs, and whether `rename(2)` of a
directory from a lower layer works. The survey's first proposal
(`redirect_dir=off,...`) fails in a user namespace. Which options does every
overlay mount use?

## Considered Options

- Kernel defaults.
- `redirect_dir=on` so that directory renames succeed. Rejected by the kernel
  in a user namespace (`EINVAL`), and BuildKit's upperdir differ refuses
  redirects.
- `trusted.*` xattrs when running as real root, `userxattr` otherwise: two
  on-disk formats for the same store.
- One fixed option set for every privilege level, with `volatile`.

## Decision Outcome

Every overlay mount uses
`userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile`,
as real root and in a user namespace alike, and the resulting `EXDEV` on
renaming a lower directory is accepted.

- `userxattr` always, so `layers/` has one format (`user.overlay.*`). Cost:
  `user.overlay.*` xattrs inside image content are hidden by overlay; they are
  escaped as `user.overlay.overlay.*` on extraction (kernel ≥ 6.7).
- `redirect_dir=nofollow`: never creates or follows redirects.
  `redirect_dir=off` is `EINVAL` in a user namespace and whenever `userxattr`
  is set, even as root.
- `metacopy=off`: every upper directory is a self-contained diff.
- `volatile`: no fsync on copy-up. An upper directory is not trusted after a
  crash, and a `volatile` work directory cannot be mounted again; see the
  lifecycle in [overlay-snapshot](../design/overlay-snapshot.md).
- **`EXDEV` accepted (maintainer, 2026-10-29):** `redirect_dir` stays off, so
  `rename(2)` of a directory that exists in a lower layer returns `EXDEV`.
  Programs that do not fall back to copying fail in overlay mode. When a `RUN`
  fails with `EXDEV`, ranikust prints a hint to rerun with
  `--snapshot-mode=kaniko`.
- Mounts use `fsopen`/`fsconfig(lowerdir+)`/`fsmount` on kernels ≥ 6.8, with
  a `mount(2)` fallback on older kernels.

### Consequences

- Good, because one code path and one store format serve root and rootless.
- Good, because copy-up is fast: `volatile` is the difference between
  overlay winning and losing against a walk.
- Bad, because a program that renames lower directories without an `EXDEV`
  fallback fails in overlay mode, and large trees are copied instead of
  renamed (not measured for multi-GB packages).

## Evidence

- [Experiment 01](../research/experiments/01-overlay-semantics.md) §4: in a
  user namespace the full set mounts; `redirect_dir=off`, `on`, `follow` and
  `metacopy=on` are `EINVAL`.
- [Experiment 06](../research/experiments/06-overlay-vs-walk.md) §2.3: without
  `volatile`, editing about 4,000 lower files took 12.92 s instead of 0.37 s.
- [Experiment 06a](../research/experiments/06a-root-rerun.md): the same set
  mounts as real root; without `volatile` the step took 13.0 s.
- [Experiment 10](../research/experiments/10-exdev-package-managers.md): npm,
  pnpm, apt/dpkg, pip, yarn 1 and 4, and cargo all produced correct results;
  only npm (2 renames) and pip (6) hit `EXDEV`, and both fell back to copying.
- [Experiment 11](../research/experiments/11-root-extras.md): identical
  behaviour on ext4, xfs and btrfs; `volatile` leaves
  `work/work/incompat/volatile`, after which remounting the same upper/work
  pair is `EINVAL`.
- [Experiment 13](../research/experiments/13-docker-rootful-vm.md): on kernel
  5.15, `lowerdir+` fails at `FSCONFIG_CMD_CREATE` with `EINVAL`, while the
  full option set works through `mount(2)`.

## More Information

Mount procedure, layer limits and the exporter:
[overlay-snapshot](../design/overlay-snapshot.md). Support for `lowerdir+` is
detected by creating a mount, not by the kernel version: kernel 5.15 accepts
the `FSCONFIG_SET_STRING` step and fails only at creation. The 6.8 boundary
for `lowerdir+` is [external]. Revisit `redirect_dir` only if a widely used
tool turns out to fail on `EXDEV`.

The `EXDEV` hint reads "rerun with `--mode=kaniko`"
([ADR-0023](0023-mode-flag.md)).
