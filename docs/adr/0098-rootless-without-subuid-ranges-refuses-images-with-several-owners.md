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
ranges get (#12, Q-040)?

## Considered Options

- (a) Refuse images with several owners.
- (b) An `ignore_chown_errors`-style flag, as podman offers: the failed
  `chown` is ignored and the file keeps the build user's owner.

(b) silently gives files the wrong owner, so a file meant for a service user
becomes the build user's, a change of permissions nobody reviewed; podman's
flag is not a reason to copy it (the recommendation on #12, 2026-10-31).

## Decision Outcome

Where a rootless build's user namespace maps only one uid, ranikust refuses an
image with several owners and offers no `ignore_chown_errors`-style flag: option
(a) (the maintainer, 2026-11-04: "Q-040 a").

- **Planned, Phase 3.** Nothing is built by this ADR. Today rootless is refused
  whole (`NEEDS_ROOT`, [modes-and-detection](../design/modes-and-detection.md#refusal-and-guidance)),
  so no behaviour changes until rootless overlay mode exists.
- **Scope.** Only where the user namespace maps one uid, which is a rootless
  user without `newuidmap` or without subuid ranges. Where a multi-range map is
  written, the situation does not arise.
- **No flag.** No option makes a failed `chown` pass, now or as a later
  convenience; a user who needs several owners gets ranges or another privilege
  level.
- **Not settled here,** for the Phase 3 plan, and each a `question` issue if the
  maintainer must choose:
  - whether "several owners" counts uids only or gids too (one gid maps without
    `newgidmap` and `/etc/subgid` as well);
  - the exact test: which single owner an image may have, and whether it is
    checked per layer or for the whole image, before the build or when a `chown`
    meets the limit, and what covers owners a `RUN` or `COPY --chown` creates;
  - how ranikust learns that only one uid maps (the probe's map write, as
    [ADR-0004](0004-support-every-privilege-level.md) says it tries real
    operations, or a read of `/etc/subuid`);
  - the refusal's wording and the way out it names.

### Consequences

- Good, because no file silently gets the wrong owner: a file meant for a
  service user never becomes the build user's.
- Good, because it keeps ADR-0004's line that there is no silent degradation to
  a mode that cannot `chown`.
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
- Nothing is measured for the refusal itself: it is not built.

## More Information

Answers #12 (Q-040), which blocked Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Related:
[ADR-0004](0004-support-every-privilege-level.md) (refuse with guidance);
[ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md) listed
Q-040 as not decided, and stays `accepted` with its body unchanged.

Follow-ups, with Phase 3's rootless mode: the refusal in `ranikust` and
`rnk-sandbox` (the issue's area), with the open points above settled first and a
test of a user without subuid ranges and an image with several owners.
When it is built, change `docs/design/modes-and-detection.md`, the `ranikust` and
`rnk-sandbox` cards and the book from "planned" to what is true, add its
`## Changelog` entry, and check the threat model.
