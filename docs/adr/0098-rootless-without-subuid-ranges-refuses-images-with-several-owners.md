---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0098: A build in a user namespace, rootless or `--userns`, refuses an image with an owner the namespace does not map, with one id or with ranges, and has no `ignore_chown_errors` flag

## Context and Problem Statement

Rootless overlay mode, planned for Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)), runs in a user
namespace, as does a `--userns` build
([ADR-0104](0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)).
Without `newuidmap` and an `/etc/subuid` range only one uid maps,
and `chown` to any other uid fails with `EINVAL`
([modes-and-detection](../design/modes-and-detection.md#probe-order---modeauto)).
Layers and `COPY --chown` set owners, so an image with several owners cannot
be extracted or built as it is. With `newuidmap` ranges the usual map is
`0 <uid> 1` and `1 <subuid> 65536` (`useradd` allocates 65536 ids by default),
so an owner above 65536 fails the same way **[inferred]**. What does a rootless
user without subuid ranges get (#12, Q-040)? What does a build with ranges do
with an owner outside them (#92)? And how does the refusal find the owners, and
when (raised while planning Phase 3)?

## Considered Options

What a user without ranges gets (#12):

- (a) Refuse images with several owners.
- (b) An `ignore_chown_errors`-style flag, as podman offers: the failed
  `chown` is ignored and the file keeps the build user's owner.

(b) silently gives files the wrong owner, so a file meant for a service user
becomes the build user's, a change of permissions nobody reviewed; podman's
flag is not a reason to copy it (the recommendation on #12, 2026-10-31).

How the refusal finds the owners, given (a); primed here to keep them apart
from the two above:

- (a') Refuse at the first `chown` or `setuid` that fails with `EINVAL`
  (extraction, `COPY --chown`, a `RUN`, `USER`), naming the file or id.
- (b') Scan every base layer's tar headers before the build and refuse when
  any owner is not the mapped id (0:0).
- (c') (b') for base layers, (a') for what the build creates.

(a') alone fails late, after the image was pulled and perhaps partly
extracted. (b') alone cannot see an owner that a `COPY --chown`, a `RUN` or a
`USER` creates. (c') has neither gap.

What a build with ranges does with an owner outside them (#92, where these
three are (a), (b) and (c); the same three apply to a `--userns` build and its
`ranikust` range):

- (d) As (a) and (c') do for one id: scan the base layers' headers
  before the build and refuse when an owner falls outside the map, refuse at the
  first `chown` or `setuid` failing with `EINVAL` for what the build creates;
  the message names the owner and the range needed (widen the ranges in
  `/etc/subuid` and `/etc/subgid`).
- (e) An `ignore_chown_errors`-style flag: rejected above for the one-id case,
  for the same reason (a file silently gets the wrong owner).
- (f) Remap owners outside the ranges to one id (for example `nobody`): builds
  go through, but owners and permissions change without review.

(d) is the reasoning of (a) for any map, not only a one-id map: a wrong owner is
a permission change nobody reviewed, and the scan and the refusal are the same
code with the map's ranges as input (the recommendation on #92).

## Decision Outcome

Where a rootless build's user namespace maps only one uid, ranikust refuses an
image with several owners and offers no `ignore_chown_errors`-style flag: option
(a) (the maintainer, 2026-11-04: "Q-040 a"). It finds the owners as (c') says,
counting uids and gids, and the message names the owners found and the ways out
(the maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as recommended,
which takes the recommendation on how the refusal finds the owners). Where the
map has ranges, the same scan and refusal apply with the map's ranges as input:
option (d), (a) in #92 (the maintainer, 2026-11-04: "Từ chối như ADR-0098
(khuyến nghị)"). A `--userns` build, whose root maps the `ranikust` range
([ADR-0104](0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)),
refuses an owner outside that range the same way (the maintainer, 2026-11-04,
asked as a follow-up to #92: "Từ chối như ADR-0098 (khuyến nghị)").

- **Planned, Phase 3**
  ([#80](https://github.com/tran4774/ranikust/issues/80)). Nothing is built by
  this ADR. Today rootless is refused whole (`NEEDS_ROOT`,
  [modes-and-detection](../design/modes-and-detection.md#refusal-and-guidance)),
  so no behaviour changes until rootless overlay mode exists.
- **Scope.** Every user namespace ranikust creates: rootless mode and
  `--userns`. With one mapped uid (a rootless user without `newuidmap` or
  without subuid ranges) the input of the scan is that one id; where a
  multi-range map is written (a rootless user with ranges, or the `ranikust`
  range of a `--userns` build) it is the map's ranges: owners inside them map,
  an owner outside them is refused the same way.
- **No flag.** No option makes a failed `chown` pass, now or as a later
  convenience; a user who needs several owners gets ranges or another privilege
  level.
- **Before the build.** ranikust scans the tar headers of the base layers and
  refuses before anything is extracted when any owner, uid or gid, is not
  mapped: with one id, not the one mapped id (0 inside the namespace, the
  invoking user's uid and gid outside); with ranges, outside the map's ranges.
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
- **The message** names the owners found and the ways out: with one id, install
  `uidmap` and add ranges to `/etc/subuid` and `/etc/subgid`, run as root, or
  use a container; with ranges, it names the owner and the range needed, to be
  widened in `/etc/subuid` and `/etc/subgid` (for `--userns`, the `ranikust`
  entry).

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
- Bad, because in every rootless build and every `--userns` build each base
  blob is decompressed once more to read its headers: overlay mode downloads a
  blob, then extracts it
  ([ADR-0055](0055-stream-layers-keep-nothing-proportional-to-the-image.md)).
- Bad, because a user whose ranges (for `--userns`, the `ranikust` entry) are
  narrower than an image's owners must widen them or build at another privilege
  level: no remapping is offered.
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
([#41](https://github.com/tran4774/ranikust/issues/41)), how the refusal finds
the owners, raised while planning Phase 3, #92 (an owner outside a build's
ranges) and what a `--userns` build does with an owner outside its `ranikust`
range. Related:
[ADR-0004](0004-support-every-privilege-level.md) (refuse with guidance);
[ADR-0104](0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)
(`--userns` and its range);
[ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md) listed
Q-040 as not decided, and stays `accepted` with its body unchanged.
[ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)
settles where the store's operations run in the same namespace.

Follow-ups, with [#80](https://github.com/tran4774/ranikust/issues/80) (task 9
of #41): the refusal in `ranikust` and `rnk-sandbox` (the issue's area) and the
scan in `rnk-snapshot` and `rnk-builder`, with tests of a user without subuid
ranges and an image with several owners, and of a user with ranges and an image
with an owner outside them (T0 for the check, T2 in the VM for a user without
`uidmap`, one without ranges and one with). The test of a `--userns` build and
an owner outside the `ranikust` range goes with
[#83](https://github.com/tran4774/ranikust/issues/83) (task 12), which builds
`--userns`. When it is built, change
`docs/design/modes-and-detection.md`, the `ranikust`, `rnk-sandbox`,
`rnk-snapshot` and `rnk-builder` cards and the book from "planned" to what is
true, add its `## Changelog` entry, and check the threat model.
