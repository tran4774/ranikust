---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0017: Per-stage rootfs by copy or reflink, never hardlink farms

## Context and Problem Statement

When kaniko mode gives each stage its own rootfs directory (`--parallel-stages`,
or a mount-capable environment without usable overlay,
[ADR-0014](0014-kaniko-mode-sequential-by-default.md)), every stage needs a
private, writable copy of its base image. Overlay is not available on this
path by definition. A hardlink farm (`cp -al`) from the store is the fastest
and costs no disk space, but every link shares an inode with the store. How
is a stage rootfs created?

## Considered Options

- Extract the base layers into each stage directory.
- Full copy from the extracted store (`cp -a` semantics).
- Hardlink farm from the store (`cp -al`).
- Forced reflink (`cp --reflink=always`).

## Decision Outcome

Each stage rootfs is created by extracting its layers or by a full copy from
the store, using reflinks where the filesystem supports them, and never by
hardlinking to the store.

- On xfs and btrfs, a copy reflinks automatically (`cp -a` does so with
  coreutils ≥ 9.0), so each stage costs only a few MB of extra disk.
- On ext4 and tmpfs, a copy costs one full base image per stage.
- ranikust runs as one static binary, possibly in an empty image, so it does
  not shell out to `cp`: it clones with the `FICLONE` ioctl where supported
  and falls back to a plain copy [inferred].
- Releasing a stage deletes its directory with the safe, fd-based deletion of
  [kaniko-mode](../design/kaniko-mode.md).

### Consequences

- Good, because nothing a `RUN` does can change the store or a sibling stage.
- Bad, because ext4 and tmpfs pay one base copy per stage in disk and time.

## Evidence

- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md), compat
  §5: preparing a `node` base on ext4 took 0.122–0.172 s by extraction,
  0.116–0.170 s by `cp -a`, and 0.032–0.044 s by `cp -al`; `--reflink=always`
  failed with `EOPNOTSUPP` on ext4 and tmpfs. One `echo >> /etc/profile` in a
  hardlinked stage grew the file in the store and in the sibling stage
  (828 → 861 bytes). apk, dpkg and `sed -i` write a temporary file and rename
  it, which is safe, but `>>`, `truncate`, `chmod`, `chown` and `touch` act on
  the shared inode.
- Same experiment, measured as real root: on xfs and btrfs, `cp -a` reflinked
  automatically, and each stage cost 3–12 MB instead of 231 MB. It was not
  faster (0.08–0.1 s), because the cost is in metadata.
- Same experiment, perf §6: W3 parallel in kaniko mode used 3.28 GB of disk
  against 2.13 GB for overlay mode.

## More Information

Hardlinks remain fine inside one stage and inside image layers; the rule is
only about sharing inodes between the store and a writable root.
