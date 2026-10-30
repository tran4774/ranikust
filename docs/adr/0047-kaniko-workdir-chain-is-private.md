---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0047: A kaniko-mode host workdir must be out of other users' reach

## Context and Problem Statement

In kaniko mode on a host `--workdir`, ranikust runs as root and unpacks
the image into the workdir with its raw modes, setuid and setgid files
included (`rnk_rootfs::extract_live`; device nodes and FIFOs are skipped,
ADR-0041), and gives the root `0755` so that a `RUN` after `USER` can
enter it. On a filesystem mounted without `nosuid`, any local user who can
reach the workdir could run a setuid-root binary from the image, or
hardlink it out of the workdir and keep it. Kaniko mode cannot mount, so
it cannot remount the workdir `nosuid` either. The workdir checks of
ADR-0038 refuse system directories and unmarked workdirs, but check
neither owner nor mode of the workdir or its ancestors (Q-320).

## Considered Options

- (a) Require a private chain: a directory above the workdir that no one
  but root can search, and it and everything between it and the workdir
  owned and writable by root only.
- (b) Only warn.
- (c) Mount the workdir `nosuid`: impossible without mounting.

## Decision Outcome

Chosen by the maintainer: (a), checked as root before anything is
written in the workdir (`ranikust::workdir::check_private`, the rule in
`check_chain`). The chain is the workdir, then its real parents up to
`/`, walked from the workdir's locked descriptor with `..` (each opened
`O_PATH`), never by path, so a rename between the claim and the check
cannot show a decoy:

1. some ancestor lets neither group nor others search it (no `g+x`, no
   `o+x`): the private one, the nearest that qualifies;
2. it, every directory between it and the workdir, and the workdir are
   owned by root and not writable by group or others;
3. directories above the private one may belong to anyone: no one but
   root gets past it.

Of the two ways Q-320 named to keep others out, a parent without search
for others or a `0700` wrapper ranikust would create, the check takes the
first: it keeps the workdir itself the root of the build, as kaniko mode
designs it. A workdir in a home needs a root-owned `0700` directory in
between, which the refusal names: `sudo install -d -m 0700
/home/<user>/rnk`, then `--workdir=/home/<user>/rnk/w`; a `/home/<user>`
of the user's own does not count, since its owner could reach the setuid
files or open it up later. When the workdir's filesystem lacks `nosuid`
or `nodev`, a warning says so.

The rule applies inside a container too: a `--workdir` there is usually a
volume whose host side the check cannot see, so it fails closed; a build
on the container's own `/` is not a host workdir and is not checked.

### Consequences

- Good, because no other local user can reach a setuid file from the
  image, nor hardlink it, while the build runs or after.
- Good, because a planted or swapped directory in the chain needs the
  write access the rule denies.
- Bad, because a workdir under a home or straight under `/tmp` is
  refused until the operator adds a root-owned `0700` directory above it.

## Evidence

- `rnk_rootfs::extract_live` keeps raw modes; ADR-0041 skips device nodes
  and FIFOs only.
- The host-safety review of `feat/kaniko-root` (Phase 2 task 6), which
  found the gap.

## More Information

Answers [Q-320](../open-questions.md#q-320-what-keeps-a-kaniko-mode-host-workdir-out-of-other-users-reach).
Extends the workdir checks of [ADR-0038](0038-kaniko-mode-settings-for-phase-2.md).
Design: [kaniko-mode](../design/kaniko-mode.md#stage-roots-on-the-shared-root).
