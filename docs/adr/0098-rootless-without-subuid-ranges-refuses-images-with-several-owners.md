---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0098: A rootless build without subuid ranges refuses images with several owners, and has no `ignore_chown_errors` flag

## Context and Problem Statement

Rootless overlay mode, planned for Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)), runs in a user
namespace. Without `newuidmap` and an `/etc/subuid` range only one uid maps,
and `chown` to any other uid fails with `EINVAL`
([modes-and-detection](../design/modes-and-detection.md#probe-order---modeauto)).
Layers and `COPY --chown` set owners, so an image with several owners cannot
be extracted or built as it is. What does a rootless user without subuid
ranges get (#12, Q-040)? And, once that is settled, how does the refusal find
the owners, and when (question N3 of the Phase 3 plan)?

## Considered Options

What a user without ranges gets (#12):

- (a) Refuse images with several owners.
- (b) An `ignore_chown_errors`-style flag, as podman offers: the failed
  `chown` is ignored and the file keeps the build user's owner.

(b) silently gives files the wrong owner, so a file meant for a service user
becomes the build user's, a change of permissions nobody reviewed; podman's
flag is not a reason to copy it (the recommendation on #12, 2026-10-31).

How the refusal finds the owners (N3), given (a); the Phase 3 plan labels these
(a) to (c), primed here to keep them apart from the two above:

- (a') Refuse at the first `chown` or `setuid` that fails with `EINVAL`
  (extraction, `COPY --chown`, a `RUN`, `USER`), naming the file or id.
- (b') Scan every base layer's tar headers before the build and refuse when
  any owner is not the mapped id (0:0).
- (c') (b') for base layers, (a') for what the build creates.

(a') alone fails late, after the image was pulled and perhaps partly
extracted. (b') alone cannot see an owner that a `COPY --chown`, a `RUN` or a
`USER` creates. (c') has neither gap.

## Decision Outcome

Where a rootless build's user namespace maps only one uid, ranikust refuses an
image with several owners and offers no `ignore_chown_errors`-style flag: option
(a) (the maintainer, 2026-11-04: "Q-040 a"). It finds the owners as (c') says,
counting uids and gids, and the message names the owners found and the ways out
(the maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as recommended,
which takes the recommendation on N3).

- **Planned, Phase 3.** Nothing is built by this ADR. Today rootless is refused
  whole (`NEEDS_ROOT`, [modes-and-detection](../design/modes-and-detection.md#refusal-and-guidance)),
  so no behaviour changes until rootless overlay mode exists.
- **Scope.** Only where the user namespace maps one uid, which is a rootless
  user without `newuidmap` or without subuid ranges. Where a multi-range map is
  written, the situation does not arise.
- **No flag.** No option makes a failed `chown` pass, now or as a later
  convenience; a user who needs several owners gets ranges or another privilege
  level.
- **Before the build.** ranikust scans the tar headers of the base layers and
  refuses before anything is extracted when any owner, uid or gid, is not the
  one mapped id (0 inside the namespace, the invoking user outside).
- **During the build.** ranikust refuses at the first `chown` or `setuid` that
  fails with `EINVAL` for what the build creates: `COPY --chown`, a `RUN`,
  `USER`. This covers what no scan can see.
- **Gids count as uids do.** One gid maps without `newgidmap` and `/etc/subgid`,
  as one uid does without `newuidmap` and `/etc/subuid`, and an unmapped gid
  alone stops the namespace's root from renaming a `0555` directory (R6, R7
  below); a `USER` other than 0 fails the same way, since `setuid` to an
  unmapped id fails.
- **How ranikust learns that one id maps.** From the map write of the probe
  (Phase 3 task 4 of #41), which is a real operation as
  [ADR-0004](0004-support-every-privilege-level.md) says, not a read of
  `/etc/subuid`.
- **The message** names the owners found and the ways out: install `uidmap`
  and add ranges to `/etc/subuid` and `/etc/subgid`, run as root, or use a
  container.

### Consequences

- Good, because no file silently gets the wrong owner: a file meant for a
  service user never becomes the build user's.
- Good, because it keeps ADR-0004's line that there is no silent degradation to
  a mode that cannot `chown`.
- Good, because the scan refuses before any layer is extracted, and the refusal
  at the first failing `chown` still covers what the build creates.
- Bad, because a user without subuid ranges can build only images that the
  refusal lets through, and many images have files of more than one owner
  (`/etc/shadow` shows group `65534` in a one-id map), so few **[inferred]**.
- Bad, because podman's `ignore_chown_errors` users, such as HPC sites
  that cannot get ranges, have no flag to turn to.
- Neutral, because the ranges, or root or a container, remain the way out; both
  are ADR-0004's modes.

## Evidence

- [Experiment 01](../research/experiments/01-overlay-semantics.md), verdict
  §1.d and question 1: **[measured]** `unshare -Urm` maps one uid (`0 0 1`);
  `chown 1000:1000` fails with `Invalid argument`; files of other uids show as
  `65534`; a `RUN` that switches to `_apt` or runs `chown` would break.
- [Experiment 03](../research/experiments/03-privilege-matrix.md#what-rootless-overlay-actually-requires):
  the same, **[measured]**. Podman's `ignore_chown_errors=true`, made for HPC:
  every file becomes the user's, and the image may run incorrectly
  (**[external]**, high, [Red Hat](https://www.redhat.com/en/blog/controlling-access-rootless-podman-users)).
  Rootless buildah without subuid fails with `lchown … invalid argument`, and its
  workaround is `ignore_chown_errors` on the overlay and vfs drivers
  (**[external]**, medium, [RHEL-26594](https://issues.redhat.com/browse/RHEL-26594)).
- `poc-runner` in the test VM, 2026-11-04 (details in
  [ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md#evidence)),
  **[measured]**: with a single map, `chown` to another uid gives `EINVAL`; a
  `0555` directory owned by a mapped uid but an unmapped gid (the host's
  `docker` group, seen as `65534`) cannot be renamed to another parent even by
  the namespace's root, `EACCES` (R6, with the multi-range map in R7), while a
  directory with both ids mapped can (R2 to R4). Ubuntu does not install
  `uidmap` (`newuidmap`, `newgidmap`) by default, so on an Ubuntu without it
  only one id maps.
- Nothing is measured for the refusal itself: it is not built.

## More Information

Answers #12 (Q-040), which blocked Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)), and question N3 of the
Phase 3 plan (the four points this ADR first left open). Related:
[ADR-0004](0004-support-every-privilege-level.md) (refuse with guidance);
[ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md) listed
Q-040 as not decided, and stays `accepted` with its body unchanged.
[ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)
settles where the store's operations run in the same namespace.

Follow-ups, with Phase 3's rootless mode (task 9 of #41): the refusal in
`ranikust` and `rnk-sandbox` (the issue's area) and the scan in `rnk-snapshot`
and `rnk-builder`, with a test of a user without subuid ranges and an image with
several owners (T0 for the check, T2 in the VM for a user without `uidmap` and
one without ranges). When it is built, change
`docs/design/modes-and-detection.md`, the `ranikust` and `rnk-sandbox` cards and
the book from "planned" to what is true, add its `## Changelog` entry, and check
the threat model.
