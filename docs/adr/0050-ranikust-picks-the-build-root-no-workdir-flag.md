---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0050: ranikust picks the build root itself: `/` in a container, else a directory of the state directory; no `--workdir`, no `--force`

## Context and Problem Statement

Kaniko mode builds on a root it fills and empties. Until now the operator
named that root on a host with `--workdir`, which needed the host-safety
checks of ADR-0038 (system directories, the `.ranikust-root` marker, a
lock) and ADR-0047 (a root-owned private ancestor, so other users cannot
reach the image's setuid files), and a setup step on a host
(`sudo install -d -m 0700 /home/<user>/rnk`). `--force` let kaniko mode
empty a host's `/` (ADR-0040, ADR-0048). The state directory defaulted to
`<workdir>/.ranikust` in overlay mode and `/ranikust/state` or
`<cwd>/.ranikust` in kaniko mode (ADR-0022, ADR-0038), and overlay mode
without `--workdir` or `--state-dir` stopped (Q-110). The maintainer asked
whether ranikust could pick the root itself and drop the flag.

## Considered Options

- Keep `--workdir` with its checks (ADR-0038, ADR-0047).
- ranikust picks the root: `/` in a container (kaniko mode), otherwise a
  directory it creates in its own state directory; one fixed state
  directory default, like BuildKit's `/var/lib/buildkit` and podman's
  `/var/lib/containers`.

## Decision Outcome

Chosen: ranikust picks the root. No path the operator gives is ever filled
or emptied.

- **State directory (A3, supersedes ADR-0022's defaults, ADR-0038's
  Q-036 answer).** `/var/lib/ranikust` as root, in every mode;
  `--state-dir` replaces it. The rootless default
  (`~/.local/share/ranikust`) comes with rootless mode in Phase 3; until
  then a build needs root, as before. Answers Q-110.
- **Kaniko mode's root (A1).** In a container (the heuristic of
  ADR-0048): `/`, with the state directory on the ignore list, as
  kaniko's `/kaniko`. Outside one: `<state>/builds/<id>/rootfs`, created by
  ranikust for the build and removed with it, with `RUN` in the full
  sandbox. Concurrent builds on a host never share a root. Kaniko mode on
  `/` takes `flock` on a descriptor of `/` itself, one build per
  container whatever the state directories.
- **A host where root may not mount (Q-330).** Kaniko mode outside a
  container needs mount privileges: `RUN` there would otherwise run in a
  bare `chroot` on the host, which root can leave (`CAP_SYS_CHROOT`) and
  where it may make device nodes (`CAP_MKNOD`). Refused before anything is
  written, with the advice to run in a container or give ranikust mount
  privileges; neither an opt-in flag nor a warning.
- **No `--workdir`, no `--force` (A1, A2; supersedes ADR-0011's
  `--workdir`, ADR-0040's and ADR-0048's `--force`).** kaniko's `--force`
  is refused with its reason: ranikust never builds on a host's `/`.
- **The private chain moves to the state directory (A5, supersedes
  ADR-0047 in part, ADR-0038's workdir checks).** As root, the state
  directory must be a real directory owned by root and not writable by
  group or others, and either new, empty, or marked by the store's own
  `.ranikust-state` file (a directory marked only with the old
  `.ranikust-root` is refused, named); `/` never. Only then does ranikust
  set its mode, and that of `builds/`, to `0700` and check it reads back
  so: it is the
  private directory every root and upper directory of a build lies below.
  Every directory above it must be owned by root and not writable by group
  or others, or be root's sticky directory (`/tmp`): the store reaches its
  files by path, so no other user may rename a directory on the way.
  Checked before anything is created, walking the real parents by
  descriptor. The system-directory list, the workdir marker, and the
  workdir lock go away with the flag; the store's marker, renamed
  `.ranikust-state` so an old workdir is never taken for a store, keeps
  refusing a non-empty directory that is not a state directory (Q-111).
- **Overlay mode in a container (A4).** The container's own root is
  overlayfs, which cannot hold an upper directory: a state directory on
  overlayfs is refused before any probe, with the advice to mount a
  volume there (`-v ranikust-state:/var/lib/ranikust`), as BuildKit and
  podman need in a container.

### Consequences

- Good, because a typo can no longer point a build at the operator's data:
  ranikust fills and empties only `/` in a container or directories it
  made under its own `0700` state directory.
- Good, because no setup step and no flag is needed on a host: the default
  state directory is private already.
- Good, because builds on a host run side by side without choosing
  directories.
- Bad, because a `--state-dir` below a directory another user owns (for
  example under a home directory) is refused; it must sit below
  directories root owns.
- Bad, because overlay mode in a privileged container needs a volume at
  the state directory.
- Neutral, because ADR-0048's risk stays: a host that matches the
  container heuristic by accident builds on its `/`.

## Evidence

- The host-safety reviews of the workdir checks (Phase 2 tasks 1 and 6)
  [code]; the maintainer's answers of 2026-10-31.

## More Information

Supersedes in part [ADR-0011](0011-accept-common-kaniko-flags.md)
(`--workdir`), [ADR-0022](0022-common-state-dir.md) (defaults),
[ADR-0038](0038-kaniko-mode-settings-for-phase-2.md) (workdir checks,
Q-036), [ADR-0040](0040-kaniko-mode-selection-container-check-and-flags.md)
and [ADR-0048](0048-container-heuristic-guards-the-root.md) (`--force`),
[ADR-0047](0047-kaniko-workdir-chain-is-private.md) (the chain moves to
the state directory). Answers Q-110 and Q-330.
