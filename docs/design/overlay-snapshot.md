# Overlay snapshots

**TL;DR:** overlay mode builds a stage with one overlay mount, one upper
directory per layer, and an exporter that turns an upper directory into an
OCI layer.

Decisions: [ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0012](../adr/0012-accept-divergence-between-modes.md),
[ADR-0016](../adr/0016-overlay-mount-options.md),
[ADR-0023](../adr/0023-mode-flag.md),
[ADR-0034](../adr/0034-unchanged-copy-ups-left-out-of-layers.md).

## Invariants

- Every byte ranikust writes is under the state directory (store and build
  areas; by default `/var/lib/ranikust`, ADR-0050) or in the requested output.
  Nothing is written to the host's `/`.
- Extracted layers in `layers/` are immutable once published and are only
  ever used as lower directories.
- Every overlay mount uses the option set of ADR-0016, so `layers/` has one
  on-disk format (`user.overlay.*` xattrs) at every privilege level.
- Mounts live in ranikust's private mount namespace and disappear when the
  process dies, even on `SIGKILL`.

## State directory layout

The store (`blobs/`, `layers/`, `caches/`) is laid out in
[cache-and-store](cache-and-store.md#store-layout). `builds/<id>` is private to
one build and deleted afterwards; builds share the state directory and its
store, each with its own `builds/<id>`:

```
<state-dir>/builds/<build-id>/
├── overlay-probe/          the start-up probe's mount, emptied after it
├── context/
└── stages/<n>/
    ├── diff-<k>/           upper of instruction k = diff of layer k
    ├── work-<k>/           overlay work directory of instruction k
    ├── empty-<i>/          an empty lower: FROM scratch, the read-only view
    ├── lowers-<m>/<i> -> <lower>   links of one mount(2), only while it mounts
    └── rootfs/             mount point of stage n
```

`layers/` is `layers/v3/<algorithm>/<hex>/`, named by the layer's chain ID, as
BuildKit and containerd key unpacked snapshots (ADR-0032): a hardlink into a
lower layer is copied from the layers below, so an extraction depends on its
stack. A bottom layer's chain ID is its diff ID; the same diff on another stack
is extracted again. Each layout is a new directory, so nothing extracted by an
earlier rule is reused: `v2` layers may hold an image's `trusted.*` xattrs,
which `v3` never sets, from the tar or copied up from a lower layer
(ADR-0075). Earlier layouts stay on disk until `prune` (Phase 5) or the
operator removes them. Planned, Phase 3: a rootless or `--userns` build's key
also includes the digest of its id map
([cache-and-store](cache-and-store.md#store-layout)).

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
only the image config (`ENV`, `LABEL`, `USER`, ...) mount nothing and produce
no layer. Keeping one mount across several consecutive instructions is a later
optimization ([open: #18]; see the `volatile` rules).

In code (`rnk-snapshot`, [ADR-0046](../adr/0046-snapshotter-signature.md)):
`Snapshotter::prepare` makes a stage on `Base::Scratch`, `Base::Image(layers)`
or `Base::Stage(parent)`; `StageRootfs::begin_layer` is steps 2 and 3,
`end_layer` step 5 and returns the `LayerDiff` that step 6 exports. Every
diff becomes the topmost lower of the next layer, empty or not. Between
layers, `view` mounts the stack read-only on `rootfs/` (overlay without an
upper needs two lowers, so an empty directory pads it) for reads such as
`USER` lookups and `COPY --from`; the next `begin_layer` replaces it.
`release` unmounts and deletes the stage directory, so a stage is
released only after every stage built on it.

**The root's metadata** (step 2) is the merged view's: the topmost lower
whose root a layer set. Extraction records that: a layer whose tar has no
entry for `./` gets the xattr `user.ranikust.inherit-root` on its root and
is skipped, as a flattening extractor leaves the root alone for such a
layer. No layer sets it: `0755`, the epoch, ranikust's owner (root).
Overlay's own xattrs on a lower root are never copied: an opaque upper root
would hide every lower layer.

## Mounting

- **Kernel ≥ 6.8:** `fsopen("overlay")`; one `FSCONFIG_SET_STRING` call
  with key `lowerdir+` per layer; `FSCONFIG_CMD_CREATE`; `fsmount`;
  `move_mount`. Path length does not matter.
- **Older kernels:** `mount(2)` with a single option string, the lowers
  relative names, as containerd mounts many lowers (ADR-0032): each
  mount makes a fresh `lowers-<m>/` in the stage directory with links `0`,
  `1`, ... to its lowers, mounts from a short-lived thread that does
  `unshare(CLONE_FS)` and `fchdir` into it, with `lowerdir=0:1:...`, and
  removes the links again whatever the result; overlay follows them once,
  at mount time. The process's working directory never changes, and the
  500 lowers overlay takes fit one page. `mount(8)` is never used.
- Whether `lowerdir+` works is probed by creating a mount: a probe of this
  crate that `ranikust` runs at start-up
  ([modes-and-detection](modes-and-detection.md)). The probe mounts a small
  overlay in `builds/<id>/overlay-probe` with `lowerdir+`, then with
  `mount(2)`, writes through it (a new file, a deleted file, a directory
  made again) and checks that the upper holds the file, two whiteouts and
  `user.overlay.opaque=y`.
- Unmounting uses `MNT_DETACH`; the read-only view also sets
  `MOUNT_ATTR_RDONLY` (`MS_RDONLY` with `mount(2)`) and leaves out
  `volatile`, which needs an upper.
- Limits:
  - At most 500 lower layers; layer 501 is `EINVAL` ("too many lower
    directories, limit is 500").
  - `mount(2)` truncates its data at one page (4096 bytes) and then returns
    **`ENOENT`**, not `EINVAL`. Check the length before calling and report a
    clear error.
  - `fsconfig(FSCONFIG_SET_STRING, "lowerdir", s)` takes at most 256 bytes;
    never pass a joined list there.
  - Layers are not merged near the limit: a stage that needs more lowers than
    the kernel allows fails with a clear error, as BuildKit's overlay
    snapshotter (ADR-0032).

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
| directory with `user.overlay.opaque=y` over a lower one | walked against the lower stack (continuity's double walk, as BuildKit): `.wh.<name>` for each name below that the upper lacks (one for a whole directory), the rest compared as any entry; never `.wh..wh..opq`, which containerd's writer never writes |
| directory with `user.overlay.opaque=y` and nothing below | a plain directory (overlay also marks new directories that were renamed) |
| any entry the lower stack also shows, unchanged by BuildKit's `sameDirent` | left out (ADR-0034): same inode, or same type and permission bits, owner, device numbers and `security.capability`; a non-directory also needs the same size and second, then, with zero nanoseconds on both sides, the same symlink target or bytes, else the same nanoseconds; a directory's time is not compared |
| xattrs | only `security.capability` kept, as BuildKit and kaniko (ADR-0032); overlay's own (`origin`, `impure`, `uuid`, `opaque`) and every other one stripped, POSIX ACLs, SELinux labels and IMA/EVM signatures included |
| `user.overlay.redirect` or `trusted.overlay.redirect` | error: cannot occur with `redirect_dir=nofollow` |
| files, symlinks, hardlinks within the same upper | kept when changed; under an opaque directory an unchanged name sharing an inode with a changed one is written as a link to it (containerd's `inodeRefs`) |
| whiteout time | 0, as containerd 2's `ChangeWriter` |

Requirements found by comparing images:

- **Whiteouts before hardlinks.** Overlay creates many whiteouts as links to
  one inode (`nlink=4` observed). Convert whiteouts first, then detect
  hardlinks by `(dev, ino)` among the remaining entries.
- **Parent directory headers.** Every emitted entry's ancestor directories
  are emitted too, with their current metadata, even when unchanged; the
  `rnk-layer` writer adds the missing ones, asking the exporter for their
  metadata through a lookup into the merged view, as containerd's
  `includeParents` does for BuildKit (ADR-0032). Otherwise extractors such as
  containers-storage create missing parents as 0755 and mask the real mode
  below (`/root` 700 → 755, `/tmp` 1777 → 755).
  The `sameDirent` rule only stops an unchanged directory from being emitted
  on its own: the exporter skips it, and the writer adds it back through the
  lookup when a descendant is emitted.
- **Content comparison.** For a copied-up file whose metadata, size and
  whole-second time equal the lower one's, both copies are read through
  their `RootPath`s in 32 KiB blocks up to the first difference
  (`bytes_compared` on `rnk_snapshot.diff`); an opened file that is not the
  inode `statx` saw counts as changed. Over a diff this build made (not an
  extracted image layer) the content is compared whenever the stat
  matches, nanoseconds included: two `COPY`s within one clock tick or
  `cp -a` give equal times to different bytes. This is a deliberate
  deviation from BuildKit, which trusts the nanoseconds there (ADR-0034).
  On a state directory with whole-second timestamps every matching copy-up
  is compared, which is correct but slower.
- **Merged listings.** Under an opaque directory the lower stack's names
  are gathered in one pass from the top, as overlay's `readdir`: each
  layer's directory is reached without following anything, a whiteout
  hides a name of the layers below, and the pass stops at a layer where
  the directory is opaque, a whiteout or not a directory.
- **Raw mode bits.** Read and set modes with the raw `st_mode` bits
  (`fchmodat` with the full value) so sticky, setuid and setgid survive;
  `/tmp` must stay 1777.
- **Root directory metadata** comes from the lower root (step 2 above).
- **Deterministic output.** Sorted entries, in the order the OCI layer
  specification recommends (depth first, an opaque marker and whiteouts before
  their siblings; files really named `.wh.*` are rejected); same bytes for any
  thread count. Files
  keep the times they have on disk by default; under `--reproducible` a
  time later than the fixed epoch (`SOURCE_DATE_EPOCH` when set, else
  1970-01-01) is clamped to it, as BuildKit's `rewrite-timestamp` does
  (ADR-0032).

**The reverse direction**, extracting a base or cache layer into
`layers/<id>`: `.wh.<name>` becomes `mknod c 0 0`; `.wh..wh..opq` becomes the
xattr `user.overlay.opaque=y`; `user.overlay.*` xattrs in the content are
escaped as `user.overlay.overlay.*` (kernel ≥ 6.7); modes are set raw.
`rnk-snapshot` chains `rnk-layer`'s decoder (gzip or zstd to a plain tar
stream) into `rnk-rootfs::extract_overlay`, which writes into a temporary
entry that `rnk-store` renames into place; it gets the layers below, so
that a hardlink into one of them is copied from it
([rootfs](rootfs.md#extraction)).

Planned, Phase 3 (rootless mode, nothing built): extraction, publication and
clean-up of extracted layers run inside the build's user namespace as its mapped
root, and publication stays a rename
([ADR-0099](../adr/0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md),
[cache-and-store](cache-and-store.md#atomic-publication));
device nodes of a base layer are skipped and counted as `nodes_skipped`
([ADR-0106](../adr/0106-extraction-in-a-user-namespace-skips-device-nodes-and-counts-them.md)).

## Lazy export

The diff is already on disk. It is turned into tar, compressed and digested
only when the layer is used (the rule is in
[scheduler](scheduler.md#lazy-export)), and then as soon as it is complete, in
parallel with the next instruction, while its pages are still in the page
cache.

## Constraints and their handling

| Constraint | Handling |
|---|---|
| Upper/work cannot be on overlayfs | The probe detects it; the result is kaniko mode. |
| Nesting depth is at most 2 | A container on nested overlay fails the probe. |
| Upper filesystems: ext4, xfs, btrfs, tmpfs behave identically; NFS cannot be an upper | Probe. |
| Copy-up of unchanged files and of `chmod`/`touch`ed files (whole 8 MB file copied) | Accepted on disk, as in BuildKit; the exporter leaves unchanged ones out of the layer (ADR-0034). |
| Copy-up splits hardlinks | Accepted difference ([ADR-0012](../adr/0012-accept-divergence-between-modes.md)). |
| `rename(2)` of a lower directory returns `EXDEV` | Accepted ([ADR-0016](../adr/0016-overlay-mount-options.md)); on failure, hint "rerun with `--mode=kaniko`" ([ADR-0023](../adr/0023-mode-flag.md)). |
| `trusted.*` xattrs need `CAP_SYS_ADMIN` in the initial user namespace | Always `userxattr`. |
| Stage teardown | Unmount and delete `diff-*`; no walk-and-delete. |

`COPY --from=<stage>` reads the source stage's final rootfs directly, mounted
read-only: no copy and no deletion between stages, unlike kaniko.

## Failure handling

- A failed mount aborts the stage with the errno and the option string
  length; `ENOENT` from `mount(2)` with a long option string is reported as
  truncation.
- A `RUN` failing with `EXDEV` gets the hint "rerun with `--mode=kaniko`".
- On cancellation or failure, mounts vanish with the mount namespace; the
  build directory is deleted on exit or on the next start.

## Open questions

- [Q-245](https://github.com/tran4774/ranikust/issues/18) Should one overlay mount serve consecutive instructions?
- [Q-421](https://github.com/tran4774/ranikust/issues/29) Which performance conditions does Phase 6 measure?

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
