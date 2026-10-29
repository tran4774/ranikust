# Overlay snapshots

How overlay mode builds a stage: one overlay mount per stage, one upper
directory per layer, and an exporter that turns an upper directory into an
OCI layer.

Decisions: [ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0012](../adr/0012-accept-divergence-between-modes.md),
[ADR-0016](../adr/0016-overlay-mount-options.md),
[ADR-0022](../adr/0022-common-state-dir.md),
[ADR-0023](../adr/0023-mode-flag.md).

## Invariants

- Every byte ranikust writes is under the state directory (store and build
  areas; by default `<workdir>/.ranikust`) or in the requested output.
  Nothing is written to the host's `/`.
- Extracted layers in `layers/` are immutable once published and are only
  ever used as lower directories.
- Every overlay mount uses the option set of ADR-0016, so `layers/` has one
  on-disk format (`user.overlay.*` xattrs) at every privilege level.
- Mounts live in ranikust's private mount namespace and disappear when the
  process dies, even on `SIGKILL`.

## State directory layout

```
<state-dir>/                    default <workdir>/.ranikust
├── .ranikust-root              marker file
├── blobs/sha256/<digest>       downloaded compressed blobs
├── layers/<id>/                extracted layers, immutable, overlay whiteout form
├── l/<short> -> ../layers/<id> short names, only for the mount(2) fallback
├── caches/<sha256(id)>/        RUN --mount=type=cache
└── builds/<build-id>/
    ├── context/
    └── stages/<n>/
        ├── diff-<k>/           upper of instruction k = diff of layer k
        ├── work-<k>/           overlay work directory of instruction k
        └── rootfs/             mount point of stage n
```

`blobs/`, `layers/` and `caches/` form the persistent store, shared between
stages and builds and owned by `rnk-store`
([cache-and-store](cache-and-store.md)). `builds/<id>` is private to one
build and deleted afterwards. Builds that share a workdir share its state
directory; overlay mode takes no workdir lock.

## One layer per instruction

For instruction `k` of stage `n`:

1. **Lower stack.** `lowerdir = diff-(k-1) : … : diff-1 : <base layers>`,
   topmost first. Nothing is copied. `FROM <stage>` reuses the parent stage's
   stack, so stages are never squashed in overlay mode.
2. **Fresh upper and work.** Create `diff-k` and `work-k`. Before mounting,
   give `diff-k`'s root directory the metadata of the topmost lower root
   (mode, uid, gid, mtime, xattrs). Overlay takes `/`'s metadata from the
   upper directory; a plain `mkdir` would turn an image's `/` from 555 into
   755 and add a `/` entry to the layer.
3. **Mount** `rootfs/` with
   `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile`.
4. **Execute.** `RUN` enters `rootfs/` through the sandbox
   ([sandbox](sandbox.md)); `COPY`/`ADD` write through `rootfs/` so that
   symlinks at the destination resolve as in kaniko.
5. **Unmount.** `diff-k` now holds the complete diff. No walk of the whole
   rootfs and no baseline snapshot are needed.
6. **Export** `diff-k` only if the layer is needed (lazy export, below).

`--snapshot-mode` has no effect here: it is accepted and ignored with a
warning ([ADR-0023](../adr/0023-mode-flag.md)). Instructions that change
only the image config (`ENV`, `LABEL`, `USER`, …) mount nothing and produce
no layer. Keeping one mount across several
consecutive instructions is a later optimization; see the `volatile` rules.

## Mounting

- **Kernel ≥ 6.8:** `fsopen("overlay")`; one `FSCONFIG_SET_STRING` call
  with key `lowerdir+` per layer; `FSCONFIG_CMD_CREATE`; `fsmount`;
  `move_mount`. Path length does not matter.
- **Older kernels:** `mount(2)` with a single option string using the short
  `l/<short>` names. `mount(8)` is never used.
- Whether `lowerdir+` works is probed by creating a mount: a probe of this
  crate that `ranikust` runs at start-up
  ([modes-and-detection](modes-and-detection.md)).
- Limits:
  - At most 500 lower layers; layer 501 is `EINVAL` ("too many lower
    directories, limit is 500").
  - `mount(2)` truncates its data at one page (4096 bytes) and then returns
    **`ENOENT`**, not `EINVAL`. Check the length before calling and report a
    clear error.
  - `fsconfig(FSCONFIG_SET_STRING, "lowerdir", s)` takes at most 256 bytes;
    never pass a joined list there.
  - Close to 500 layers, merge the oldest diffs into one temporary layer.

## The `volatile` lifecycle

`volatile` skips fsync on copy-up; without it, overlay mode loses to a walk.
It leaves `work-k/work/incompat/volatile/dirty` behind after unmounting, and
the kernel then refuses to mount that upper/work pair again (`EINVAL`), with
or without `volatile`. Using the old upper as a lower is fine.

- Default: every layer gets a fresh `work-k`; the old upper becomes a lower.
- When one mount is kept for several instructions and then remounted:
  delete `work-k/work/incompat/volatile` first (and `syncfs` if the data must
  be durable).
- An upper written under `volatile` is not trusted after a crash. On the
  next start ranikust deletes leftover `builds/<id>` directories.

## Exporting a diff as an OCI layer

The exporter walks `diff-k` and looks up each entry in the lower stack, like
BuildKit's `WriteUpperdir`. It decides which entries the layer holds and
feeds them to `rnk-layer`'s writer, which sorts them, adds the missing
parent directory headers, compresses in parallel and computes the digest
while streaming.

| In `diff-k` | In the OCI tar |
|---|---|
| character device 0/0 (overlay whiteout) | `.wh.<name>` if a lower layer has `<name>`; otherwise dropped |
| directory with `user.overlay.opaque=y` | directory plus `.wh..wh..opq` if a lower layer has that directory; otherwise a plain directory (overlay also marks new directories that were renamed) |
| directory present in a lower layer with identical metadata (copied-up parent) | not a change in itself (BuildKit's `sameDirent`) |
| `user.overlay.*` and `trusted.overlay.*` xattrs (`origin`, `impure`, `uuid`, `opaque`) | stripped; `security.capability` and other xattrs kept |
| `user.overlay.redirect` or `trusted.overlay.redirect` | error: cannot occur with `redirect_dir=nofollow` |
| files, symlinks, hardlinks within the same upper | kept |

Requirements found by comparing images:

- **Whiteouts before hardlinks.** Overlay creates many whiteouts as links to
  one inode (`nlink=4` observed). Convert whiteouts first, then detect
  hardlinks by `(dev, ino)` among the remaining entries.
- **Parent directory headers.** Every emitted entry's ancestor directories
  are emitted too, with their current metadata, even when unchanged; the
  `rnk-layer` writer adds the missing ones, asking the exporter for their
  metadata through a lookup ([open: Q-013], interim).
  Otherwise extractors such as containers-storage create missing parents as
  0755 and mask the real mode below (`/root` 700 → 755, `/tmp` 1777 → 755).
  The `sameDirent` rule only stops an unchanged directory from being emitted
  on its own: the exporter skips it, and the writer adds it back through the
  lookup when a descendant is emitted (same interim).
- **Raw mode bits.** Read and set modes with the raw `st_mode` bits
  (`fchmodat` with the full value) so sticky, setuid and setgid survive;
  `/tmp` must stay 1777.
- **Root directory metadata** comes from the lower root (step 2 above).
- **Deterministic output.** Sorted entries, in the order the OCI layer
  specification recommends (Q-053); same bytes for any thread count. Timestamps follow
  kaniko: files keep the times they have on disk by default; under
  `--reproducible` every timestamp is a fixed epoch, or `SOURCE_DATE_EPOCH`
  when set.

Optionally, the exporter can compare copied-up files with their lower version
and drop unchanged ones; BuildKit does not, and it is off by default.

**The reverse direction**, extracting a base or cache layer into
`layers/<id>`: `.wh.<name>` becomes `mknod c 0 0`; `.wh..wh..opq` becomes the
xattr `user.overlay.opaque=y`; `user.overlay.*` xattrs in the content are
escaped as `user.overlay.overlay.*` (kernel ≥ 6.7); modes are set raw.
`rnk-snapshot` chains `rnk-layer`'s decoder (gzip or zstd to a plain tar
stream) into `rnk-rootfs::extract_overlay`, which writes into a temporary
entry that `rnk-store` renames into place; it gets the layers below, so
that a hardlink into one of them is copied from it (Q-055,
[rootfs](rootfs.md#extraction)).

## Lazy export

The diff is already on disk. It is turned into tar, compressed and digested
only when the layer is used:

- the stage is pushed or is the `--target`;
- `--cache` is on and the layer goes to the cache repository;
- a tar or OCI layout of that stage is written.

A stage used only through `COPY --from` or `FROM <stage>` is never encoded.
When a layer is needed, it is exported as soon as it is complete, in parallel
with the next instruction, while its pages are still in the page cache; a
limit on compression threads keeps export from starving `RUN`.

## Constraints and their handling

| Constraint | Handling |
|---|---|
| Upper/work cannot be on overlayfs | The probe detects it; the result is kaniko mode. |
| Nesting depth is at most 2 | A container on nested overlay fails the probe. |
| Upper filesystems: ext4, xfs, btrfs, tmpfs behave identically; NFS cannot be an upper | Probe. |
| Copy-up of unchanged files and of `chmod`/`touch`ed files (whole 8 MB file copied) | Accepted, as in BuildKit; optional hash filter at export. |
| Copy-up splits hardlinks | Accepted difference ([ADR-0012](../adr/0012-accept-divergence-between-modes.md)). |
| `rename(2)` of a lower directory returns `EXDEV` | Accepted ([ADR-0016](../adr/0016-overlay-mount-options.md)); on failure, hint "rerun with `--mode=kaniko`" ([ADR-0023](../adr/0023-mode-flag.md)). |
| `trusted.*` xattrs need `CAP_SYS_ADMIN` in the initial user namespace | Always `userxattr`. |
| Stage teardown | Unmount and delete `diff-*`; no walk-and-delete. |

`COPY --from=<stage>` reads the source stage's final rootfs directly, mounted
read-only. There is no copy into a dependency area and no deletion between
stages as in kaniko.

## Failure handling

- A failed mount aborts the stage with the errno and the option string
  length; `ENOENT` from `mount(2)` with a long option string is reported as
  truncation.
- A `RUN` failing with `EXDEV` gets the hint "rerun with `--mode=kaniko`".
- On cancellation or failure, mounts vanish with the mount namespace; the
  build directory is deleted on exit or on the next start.

## Open questions

- [Q-049](../open-questions.md#q-049-which-overlay-optimizations-come-after-phase-1) Which overlay optimizations come after Phase 1?
- [Q-025](../open-questions.md#q-025-are-other-user-xattrs-kept-in-exported-layers) Are other user xattrs kept in exported layers?
- [Q-033](../open-questions.md#q-033-which-performance-conditions-are-still-unmeasured) Which performance conditions are still unmeasured?
- [Q-026](../open-questions.md#q-026-when-and-how-are-layers-merged-near-the-500-lower-limit) When and how are layers merged near the 500-lower limit?
- [Q-013](../open-questions.md#q-013-how-does-the-layer-writer-learn-missing-parent-metadata) How does the layer writer learn missing parent metadata?

## References

- [Survey 06](../research/survey/06-overlay-and-parallelism.md) §2–§3.
- [Experiment 01](../research/experiments/01-overlay-semantics.md) (options,
  upper contents, whiteout inode sharing, lowerdir limits, BuildKit's differ),
  [experiment 06](../research/experiments/06-overlay-vs-walk.md) (cost,
  `volatile`: 12.92 s vs 0.37 s), [experiment 07](../research/experiments/07-kaniko-vs-overlay.md)
  (parent headers, raw modes, export variance),
  [experiment 09](../research/experiments/09-kaniko-mode-prototype.md) §3.1
  (root directory metadata),
  [experiment 10](../research/experiments/10-exdev-package-managers.md),
  [experiment 11](../research/experiments/11-root-extras.md) (`volatile`
  lifecycle, filesystems, nesting).
