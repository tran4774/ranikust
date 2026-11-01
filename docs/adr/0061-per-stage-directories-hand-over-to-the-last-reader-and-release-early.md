---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0061: Per-stage directories go to their last reader and are released as soon as nobody reads them

## Context and Problem Statement

[ADR-0059](0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md)
gives each kaniko-mode stage off `/` a directory of its own: a stage built
`FROM <stage>` copies its parent's directory (`rnk_rootfs::copy_filtered`,
never a hardlink, [ADR-0017](0017-stage-rootfs-by-copy-not-hardlinks.md)),
and every directory stays until the build ends. The first VM `e2e` run with
kaniko mode (Q-410) measured W1 at 31.3 s in this mode against kaniko's
23.7 s: the `FROM <stage>` copies took 7.2 s on ext4, which has no reflink,
and deleting the directories at the end 4.2 s. Phase 2 requires that no
build be slower than kaniko's.

## Considered Options

- (a) Hand a parent's directory to its last child by renaming it, copying
  only for the other children.
- (b) Release stage directories in the background, as soon as nothing
  depends on them.
- (c) Both.
- (d) Accept the cost where no reflink is available, and document it.

## Decision Outcome

(c): the last stage to read a parent takes its directory over instead of
copying it, and a stage directory nobody reads any more is released at
once, on a thread of its own.

- **Readers.** Per stage, the build counts the stages that read its root
  after it is built: one for each stage built `FROM` it, one for each stage
  reading it with `COPY --from` or `RUN --mount from=` (however often),
  among the stages the targets need. A stage doing both counts twice, so it
  never takes over a root it still reads. A stage built `FROM` another
  counts its read off as soon as its own root is made, on every path; any
  other reader when it ends.
- **Hand-over.** A stage built `FROM <stage>` whose count is down to its
  own read takes the parent's directory over
  (`Snapshotter::prepare_last`), decided under the parent's lock, where a
  reader still to come keeps the count higher. Otherwise it copies, as
  before. The hand-over gives exactly what the copy gives:
  - `rnk_rootfs::prune` deletes from the parent's root, in place, what the
    copy leaves out (the ignore list's paths, device nodes, FIFOs and
    sockets), through descriptors, never following a link or crossing a
    mount (a mount below the root is an error, as for the copy), and sets
    each directory's times back;
  - `RootPath::move_sub_root` moves `stages/<parent>/rootfs` to
    `stages/<child>/rootfs` with `renameat2(RENAME_NOREPLACE)` between the
    two stage directories' descriptors, checking device, inode and mount
    before and after; the child's `RUN`s get the moved root's identity
    (`RunSpec::root_id`).
  - The copy now also gives the new root the parent root's own xattrs
    (overlay's own left out), which the moved directory keeps.
  - Overlay-own xattrs (`trusted.overlay.*`, `user.overlay.*`) a copy
    leaves out stay on the moved entries; no layer ever holds them.
    Access and change times differ, which no layer records.
  The parent is then read no more: its view, its root, a second child and
  a layer are refused, and its release deletes its empty directory.
- **Release.** A stage directory whose count is zero once its stage and a
  reader are done, and the root of a `COPY --from` image once the stage
  that read it is done, go to one release thread (one at a time, through
  descriptors, as before). At the end every root left goes there too, and
  the build waits for that thread before the store closes; a second signal
  still exits at once and leaves the rest to the next build's clean-up.
  Overlay mode and kaniko mode on `/` keep their release at the end.
- No flag. Layers and digests do not depend on which path a stage took,
  which depends on scheduling: the two give the same entries (T0
  `the_last_reader_takes_the_parent_over_and_gets_what_a_copy_gets`,
  `prune_gives_what_a_filtered_copy_gives`; the `stage-dirs` smoke case
  compares `--jobs 1` and `--jobs 4` on a parent with two children).

### Consequences

- Good, because a chain of stages (W1) copies nothing: each stage's root
  moves to the next, and only one tree is deleted at the end.
- Good, because peak disk drops: a directory goes as soon as nobody reads
  it, instead of every stage's directory living until the end.
- Good, because nothing a `RUN` can reach changes: the parent is read no
  more, no inode is shared between two live stages (ADR-0017), and the
  child gets no device node, FIFO or socket (ADR-0041).
- Bad, because a pruned root is the parent's tree: a parent stage that
  fails to prune (a mount below it, an entry changed meanwhile) fails the
  build, where the copy would have failed too.
- Neutral, because a stage with several children still copies for all but
  the last, and whether a given child copies depends on scheduling.

## Evidence

- Q-410 (2026-11-01, noble VM, 4 vCPUs, one round): W1 per-stage 31.3 s
  against kaniko's 23.7 s; `rnk_rootfs.copy` 7.2 s, `rnk_rootfs.delete`
  4.2 s.
- The shortened `e2e` run on this change: see `docs/testing.md` and the
  branch report (W1 and W3 per builder).

## More Information

- Accepted by the maintainer on 2026-11-01 ("1. ok", answering the
  recommendation of Q-410's option (c); then "ok làm Q410c trước").
- Refines [ADR-0059](0059-kaniko-mode-off-root-builds-each-stage-in-its-own-directory.md);
  keeps [ADR-0017](0017-stage-rootfs-by-copy-not-hardlinks.md)
  and [ADR-0041](0041-live-root-extraction-skips-device-nodes.md).
- Design: [design/kaniko-mode.md](../design/kaniko-mode.md#per-stage-directories).
