---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0059: Kaniko mode off `/` builds each stage in a directory of its own

## Context and Problem Statement

Kaniko mode builds on `/` only in a container ([ADR-0048](0048-container-heuristic-guards-the-root.md),
[ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md)).
Anywhere else root must be able to mount, or the build is refused (Q-330),
so `RUN` there always gets the full sandbox. Until now that case still
built like kaniko on `/`: one shared root, `<state>/builds/<id>/rootfs`,
kaniko's squashed units run one at a time, the root emptied between units,
and what later units read copied into a keep area first.

Nothing ties those stages to one root. The design's variant table
([kaniko-mode](../design/kaniko-mode.md#variants)) already names this case
"per-stage, sandboxed" (root, can mount, overlay unusable: one directory
per stage, the stage graph and `--jobs`), [ADR-0014](0014-kaniko-mode-sequential-by-default.md)
keeps the sequential plan for the shared root only, and
[ADR-0017](0017-stage-rootfs-by-copy-not-hardlinks.md) settles how a stage
directory is made. Phase 2's task 9 builds it. What is decided here is how
it fits what tasks 6 to 8 built on the shared root.

## Considered Options

- (a) Keep the one shared root off `/`, sequential, as on `/`.
- (b) A directory per stage off `/`, scheduled as overlay mode's stages;
  `/` keeps the shared root.
- (c) Offer both off `/`, behind a flag.

## Decision Outcome

Chosen: (b). Off `/`, `rnk_snapshot::KanikoStages` gives every stage
`<state>/builds/<id>/stages/<n>/rootfs`:

- **Made** in the build's private area of the state directory (stage
  directory `0700`, its parents checked by the set-up, ADR-0050): each
  directory is made with `mkdirat` and opened with
  `openat(O_PATH | O_DIRECTORY | O_NOFOLLOW)` on the same parent
  descriptor (`RootPath::create_sub_root`), and must be empty, owned by
  the effective uid and on the parent's mount; a name taken in between is
  refused, never followed. A stage on an image
  extracts its base layers into it (a blob, or streamed and checked at its
  end, ADR-0055; a failure deletes the directory; device nodes and FIFOs
  left out, ADR-0041); `FROM scratch` and a stage on an image start at
  `0755` and the epoch unless a layer sets `./`, as overlay mode's roots.
- **`FROM <stage>`** copies the parent's directory (`copy_filtered`:
  owners, modes, times, xattrs and hardlinks inside the tree kept), never
  a hardlink to it (ADR-0017), and gives the copy the parent root's owner,
  mode and times. A file is cloned (`FICLONE`) where the filesystem shares
  extents, else only its data ranges are copied (`SEEK_DATA`/`SEEK_HOLE`),
  so a sparse file costs its data, not its size. Device nodes, FIFOs and
  sockets a parent's `RUN` made are left out (`CopyOpts::skip_special`),
  as from every kaniko-mode extraction and layer (ADR-0041): nothing is
  `mknod`ed outside a sandbox. Nothing a `RUN` does reaches the parent or
  a sibling.
- **Scheduled** as overlay mode's stages: the stage graph, `--jobs`, no
  squashing, nothing kept or emptied between stages; a `COPY --from` reads
  the finished stage's directory as it is. The lazy-snapshot rule is the
  one of overlay mode's demand (ADR-0038): a stage the target does not
  inherit is never walked.
- **`RUN`** gets the full sandbox (`Sandbox::start`, as overlay mode's
  mount points in the state directory). The sandbox reopens the stage
  root by path; the builder passes the root it holds
  (`RunSpec::expect_root`: device, inode, mount id), checked after the
  reopening in the parent and again in the child after the root's bind
  (device and inode), so a path swapped for another directory fails the
  `RUN`. Overlay mode passes it too.
- **The walk** belongs to the stage: its walker (and thread pool) is made
  at the first walk and dropped when the stage ran its last instruction
  (`StageRootfs::done`, a new default no-op method of ADR-0046's trait,
  called by `rnk-builder` before it hands the root to readers).
- **The ignore list** of a stage root is the fixed paths and
  `--ignore-path` (`kaniko::ignore_list` on the stages directory, where
  nothing is mounted); the walk and deletion never cross a mount anyway.
- **Released** with the stage, its directory deleted through descriptors
  (`delete_contents`, crossing no mount); what is left goes with the build
  directory when the store closes.

`/` in a container keeps the shared root, ADR-0051 and ADR-0056 (the
anonymous store and its fence): none of that applies off `/`, where the
store keeps names. `--parallel-stages` (task 10) will reuse
`KanikoStages` on a container's `/` with a bare `chroot`; how its
directories meet the fence is task 10's.

### Consequences

- Good, because independent stages run in parallel off `/` (`--jobs`), as
  in overlay mode, instead of one at a time.
- Good, because nothing is emptied or copied out between stages: no
  deletion on a root a stage still uses, no keep area, fewer chances for a
  mistake to delete the wrong thing.
- Good, because a stage's walk record and thread pool go when it is done.
- Bad, because each stage costs a copy of its base (ADR-0017's cost: on
  ext4 and tmpfs a copy of its data, on xfs and btrfs a reflink), and the
  stages' directories stay until the build ends, so peak disk is about
  the sum of the stages' roots; the shared root held one root at a time.
  Releasing a stage's directory once no later stage reads it is left for
  later.
- Bad, because parallel stages each walk with their own pool: CPU and
  memory scale with `--jobs`.
- Neutral, because `Kaniko` (the shared root) is now used only on `/`; its
  code for a root other than `/` (the keep area, the final emptying) stays
  for its tests and task 10.

### Confirmation

`rnk_snapshot::kaniko::stages` T0 tests (a private directory per stage,
`FROM <stage>` copied with no shared inode, a failed layer leaves no
directory, an unneeded stage is never walked, a done stage drops its
walk); the T2 quick suite with `RANIKUST_QUICK_MODE=kaniko` (kaniko mode
outside a container, real root) and the T1 smoke cases on `/`.

## More Information

- Accepted by the maintainer on 2026-11-01: "Chấp nhận adr 59" ("ADR-0059
  accepted").

- Option (a) was the state after task 6; it costs nothing in disk but
  serializes every stage and needs the keep area and the deletions between
  stages. Option (c) adds a flag for a choice with no security difference;
  the maintainer prefers few flags.
- Disk: experiment 12 measured W3 in parallel per-stage directories at
  3.28 GB against 2.13 GB for overlay mode (ADR-0017).
