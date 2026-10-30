---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0034: Unchanged copy-ups are left out of a layer, as BuildKit's overlay differ

## Context and Problem Statement

Overlay copies a file up to the upper directory whenever a `RUN` opens it
for writing, renames it or changes its metadata, even when the bytes end up
the same: an `npm install` or `apt-get install` that rewrites a file with
identical content and time puts a full copy into the diff. Before this
decision the exporter left out only unchanged directories, so such files
reached the layer, which made it larger than BuildKit's and different from
it. BuildKit's overlay differ leaves out any upper entry equal to its lower
counterpart, and for files whose times are whole seconds it compares their
contents, which costs reads. ADR-0032 sends performance-relevant choices to
the maintainer, so this one was escalated
([Q-049](../open-questions.md#q-049-which-overlay-optimizations-come-after-phase-1)).

## Considered Options

- (a) BuildKit's full rule: stat fields, capabilities, then the symlink
  target or file contents when both times are whole seconds.
- (b) The stat part only: free, but a file whose time is whole seconds and
  whose content changed at the same size would have to be kept or dropped
  without knowing, so it differs from BuildKit in those cases.
- (c) Directories only, as before.

## Decision Outcome

The exporter leaves out every upper entry that the lower stack also shows
and that BuildKit's `sameDirent` finds unchanged, comparing contents when
both modification times are whole seconds (option (a)).

- **The rule**, ported from `buildkit@cd8ee244
  util/overlay/overlay_linux.go:348-387` (continuity's `sameFile`): the
  same inode is equal; otherwise the raw `st_mode` (type and permission
  bits), owner, device numbers and `security.capability` must match; a
  directory needs nothing more (its time is not compared); a non-directory
  needs the same size and modification second; then, when both times have
  zero nanoseconds, a symlink's target or a regular file's bytes decide
  (an empty file is equal), else the nanoseconds must match and nothing is
  read.
- **One deviation from BuildKit.** When the lower entry comes from a diff
  this build made (an earlier instruction of the stage or of its parent
  stage), equal nanoseconds prove nothing either: two `COPY`s of same-size
  files within one clock tick, `cp -a` or `touch -r` give a file the same
  time to the nanosecond with other bytes. There the symlink target or
  content is compared whenever the stat check says equal. BuildKit has the
  same latent gap and drops such a file, shipping the old content. For
  extracted image layers the nanoseconds stay trusted, as in BuildKit.
- **Reading.** Contents are compared in 32 KiB blocks through both layers'
  `RootPath`s (`open_regular`, regular files only), stopping at the first
  difference; each opened file must be the inode `statx` saw, else it
  counts as changed. Both files were checked to have the same size, so the
  read is bounded by it.
- **Coarse clocks.** On a state directory whose filesystem keeps only
  whole seconds, every file has zero nanoseconds, so every copy-up that
  matches in stat is compared by content: correct, but slower.
- **Opaque directories.** An opaque upper directory over a lower one is
  walked against the lower stack as continuity's double-walking differ,
  which BuildKit runs there: a name only below gets a whiteout (one for a
  whole directory), a name only in the upper is added with all it holds, a
  name in both is compared with the rule above. The layer never carries
  `.wh..wh..opq`, which containerd's `ChangeWriter` never writes.
- **Hardlinks.** Outside opaque directories an unchanged name is left out
  whatever its link count, as BuildKit's differ. Under an opaque directory
  an unchanged name whose inode has other links is written only as a link
  to a changed name of the same inode (containerd's `inodeRefs`).
- **Whiteouts** carry time 0, as containerd 2's `ChangeWriter`.
- `rnk_snapshot.diff` records `unchanged` and `bytes_compared`.

### Consequences

- Good, because layers no longer carry files a `RUN` only rewrote, so they
  are smaller and match BuildKit's.
- Good, because the reads happen only for a copied-up file whose size,
  mode, owner, capabilities and whole-second time already equal the lower
  one's.
- Bad, because those files are read twice at export time: a reinstall that
  rewrites many files with the same times pays a full read of both copies,
  and so does a copy-up over this build's own diffs whose stat matches.
- Neutral, because ranikust's layers now hold per-name whiteouts where they
  held an opaque marker; extractors treat both the same.

## Evidence

- BuildKit: `buildkit@cd8ee244 util/overlay/overlay_linux.go:233-241` (the
  differ skips an entry when `sameDirent` is true), `:256-272` (opaque
  directories walked with `fs.Changes`), `:348-445` (`sameDirent`,
  `compareSysStat`, `compareCapabilities`, `compareFileContent` with a
  32 KiB buffer) [code].
- continuity: `vendor/github.com/containerd/continuity/fs/diff.go:273-365`
  (double walk, `ChangeKindUnmodified` for unchanged linked files),
  `fs/path.go:88-130` (`sameFile`) [code].
- containerd: `vendor/github.com/containerd/containerd/v2/pkg/archive/tar.go:557-700`
  (`ChangeWriter`: whiteouts at time 0, no opaque marker, `inodeRefs`)
  [code].
- Measurements on the e2e workloads: see the `rnk_snapshot.diff` and
  `rnk_snapshot.export` spans in `docs/testing.md` (`just test-vm e2e`).

## More Information

- Settles the filter part of Q-049; keeping one mount across instructions
  is split out as
  [Q-245](../open-questions.md#q-245-should-one-overlay-mount-serve-consecutive-instructions).
- Refines [ADR-0032](0032-observable-behaviour-follows-buildkit.md), which
  escalated the question.
- How it works: [overlay-snapshot](../design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer).
- Revisit if the reads show up in `bytes_compared` on real builds.
