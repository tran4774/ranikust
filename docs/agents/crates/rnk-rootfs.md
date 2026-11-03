# rnk-rootfs

> **Status:** done · **Phase:** 1-2 ·
> **Depends on:** no internal crate (never `rnk-layer`) ·
> **Used by:** `rnk-snapshot`, `rnk-sandbox`, `rnk-builder`, `rnk-store`,
> `rnk-context`, `ranikust` · **Path:** `crates/rnk-rootfs/`

**TL;DR:** All filesystem access inside a build root goes through `RootPath`,
which resolves paths from a root fd with `openat2(RESOLVE_IN_ROOT)`, so `..`
and symlinks cannot escape. Never turn an in-root path into a host path, and
never delete across a mount: compare `mnt_id`, not `st_dev`.

## Owns

- `RootPath`: fd-based resolution of every path inside a root.
- Layer extraction from an already decompressed tar `Read`, writing whiteouts
  in the form the caller asks for (OCI or overlay); copy, ownership, xattrs,
  raw mode bits.
- `USER` and `--chown` name lookup in the root's `/etc/passwd` and
  `/etc/group`, for `rnk-sandbox` and `rnk-builder`.
- The ignore list: mount points below the root plus `--ignore-path`.
- Safe deletion of a root's contents: the mechanism only.
- The operator's host paths the build context's owner must not steer
  (ADR-0076): `OutputDir`, `Fence`, `reach`.
- The kernel check `probe_support` (Linux 5.8, ADR-0028): `openat2` and the
  `statx` mount id, no string-based fallback (`Error::Unsupported`).

## Must not

Boundaries: `crates/rnk-rootfs/src/lib.rs`. In short:

- Reach a root path by joining strings onto a host path: `std::fs` or
  `Path::join` on an image, context or Dockerfile path is a bug.
- Follow symlinks or cross a mount point when deleting, or decide by `st_dev`.
- Decide what to delete and when (`rnk-snapshot` in roots, `rnk-store` in the
  store); mount, create namespaces or run processes (`rnk-sandbox`); write
  tar or decompress: the caller chains `rnk-layer`'s decoder into `extract`.
- Keep process-wide state, but one: the `tar-header` limit (below).

## API

Paths are `impl AsRef<Path>`, absolute or relative, both taken relative to
the root. Errors: one `Error` enum (`Unsupported`, `Io { op, path, source }`,
`InvalidPath`, ...). Design: [design/rootfs.md](../../design/rootfs.md).

```rust
pub struct RootPath { /* O_PATH | O_DIRECTORY | O_CLOEXEC fd, mnt_id */ }
impl RootPath {
    pub fn open(host_dir) -> Result<Self>;               // the only host path; RESOLVE_NO_SYMLINKS
    pub fn open_at(&self, p, how: &OpenHow) -> Result<OwnedFd>; // RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS
    pub fn open_regular(&self, p) -> Result<OwnedFd>;    // reads a file; FIFOs, devices refused unopened
    pub fn open_context_file(&self, p) -> Result<OwnedFd>; // open_regular plus the context rule (ADR-0076): a foreign hardlink or a file the directory's owner could not read is refused
    pub fn sub_root(&self, p) -> Result<RootPath>;       // stages/<n>/rootfs, a COPY --from source
    pub fn create_sub_root(&self, p, mode) -> Result<RootPath>; // mkdirat + openat on one parent fd; empty, euid's, parent's mount, else refused (ADR-0059)
    pub fn move_sub_root(&self, name, moved: &RootPath, to: &RootPath, to_name) -> Result<RootPath>; // renameat2(RENAME_NOREPLACE) of one plain name between two roots' fds; identity (dev, ino, mount) checked before and after (ADR-0061)
    pub fn statx(&self, p) -> Result<Stat>;              // no-follow; carries mnt_id, mount_root
    pub fn read_names(&self, p) -> Result<Vec<OsString>>; // a directory's names, mount points included
    pub fn pin(&self, p) -> Result<Pinned>;              // O_PATH|O_NOFOLLOW, no symlink in any component, root's mount only (the kaniko exporter)
    pub fn stay_on_mount(self) -> Self;                  // every later resolution with RESOLVE_NO_XDEV (rnk-sandbox's journal, ADR-0043)
    pub fn set_meta(&self, p, m: &Meta) -> Result<()>;   // ids, xattrs, raw mode, times (that order)
    pub fn xattrs(&self, p) -> Result<Vec<(OsString, Vec<u8>)>>; // every xattr, overlay's too, sorted; no follow, no open of a FIFO or device
    // create_dir, create_dir_all, symlink, read_link, hard_link, rename, remove_file, remove_dir; mnt_id()
}
pub struct Pinned;   // stat(), read_link(), xattrs(), open_regular() (reopen of the pinned inode)
pub fn extract(dst: &RootPath, tar: impl Read, wh: Whiteouts) -> Result<()>;  // plain tar; Whiteouts::{Apply, Overlay}
pub fn extract_live(dst: &RootPath, tar: impl Read, keep: &IgnoreList) -> Result<()>; // kaniko mode's base into a live root; ignored paths and mounts skipped at their physical path; no device node or FIFO (ADR-0041)
pub fn extract_live_literal(dst, tar, keep) -> Result<()>; // as extract_live with Whiteouts::Literal (a kept stage, ADR-0056): `.wh.*` names are files
pub fn extract_overlay(dst: &RootPath, tar: impl Read, lowers: &[&RootPath]) -> Result<Extracted>; // overlay form over lowers (nearest first); a hardlink into a lower is copied (splits it, as copy-up, ADR-0012); Extracted { root_set }: whether the tar had `./`
pub fn set_extension_header_limit(bytes: u64); // the `tar-header` limit (DEFAULT_EXTENSION_HEADER_LIMIT, 1 MiB; ADR-0079): the crate's only process-wide setting, set before any build; Error::ExtensionHeader past it
pub fn copy(src: &RootPath, from, dst: &RootPath, to, o: &CopyOpts) -> Result<()>; // COPY/ADD of one resolved source; FICLONE, else data ranges only (SEEK_DATA/SEEK_HOLE, copy_file_range), holes kept; a file holding more than the size read first is refused, never cut short
pub fn copy_filtered(src, from, dst, to, o, keep: &dyn Fn(&Path, bool) -> bool) -> Result<()>; // copy without the entries `keep` refuses (.dockerignore; the ignore list; a refused mount point is skipped, not an error)
pub struct CopyOpts { pub owner: Option<Owner>, pub chmod: Option<Chmod>, pub skip_special: bool, pub from_context: bool } // None keeps the source's; skip_special leaves devices, FIFOs, sockets out (ADR-0060); from_context: the context rule per file (ADR-0076)
pub struct Fence;     // Fence::of(&RootPath): a directory (by inode, so its bind mounts too) an output must not lead into, the build context's
pub struct OutputDir; // open(path, create, fence): from / one name at a time, never through a symlink, never into the fence; check(), sub_dir(), write_in_place() (a regular file with one link), write_atomically() (O_EXCL temporary, renameat over a regular file only), open_existing() (ADR-0076)
pub fn reach(path, fence: &Fence) -> Result<Reached>; // walks a host path to read (--dockerfile and its ignore file, ADR-0076); Reached::Inside(rest) once it enters the fence, for the context's RootPath; Reached::Outside(HostFile): pinned, open_regular(), open_beside(name)
pub fn delete_contents(root: &RootPath, keep: &IgnoreList) -> Result<()>;
pub fn prune(root: &RootPath, remove: &dyn Fn(&Path, FileType) -> bool) -> Result<Pruned>; // deletes what `remove` names (with what is below), every touched directory's times set back, overlay's own xattrs (is_overlay_own) removed from kept entries and the root; the machine's own / and a mount below the root refused (ADR-0061); Pruned { linked_outside }: a kept file has a hardlink outside the root
pub struct IgnoreList;  // insert (it and below), insert_below (kaniko's PrefixMatchOnly), insert_glob (--ignore-path, Go filepath.Match per component, linear in the name, never exponential in the stars); covers, has_below, contains (exact literal)
pub fn lookup_user(root: &RootPath, spec: &str) -> Result<User>;   // USER (rnk-sandbox): uid, gid, supplementary gids, home
pub fn lookup_owner(root: &RootPath, spec: &str) -> Result<Owner>; // --chown (rnk-builder): gid = uid without a group
pub fn probe_support() -> Result<(), Unsupported>;             // start-up check (ADR-0028); Cause::{Missing, Blocked}, kernel release
```

## Invariants

- A root's host path is opened once; every path below it is resolved by the
  kernel from that fd with `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`, so
  `..`, absolute symlinks and `/proc/self/root` stay inside. Tar hardlink
  targets outside the root are refused, and the last component of a replaced
  entry is never followed (T1, T2).
- Every fd is `O_CLOEXEC` (T7). Directories are opened by `(parent fd,
  name)`, never by a long path, so tree depth is unlimited. A walk keeps one
  fd per level and a single path buffer, never a path per level: memory
  linear in the entries on the stack (`tests/memory.rs`; the copy walk too,
  `copying_a_deep_chain_takes_memory_linear_in_its_depth`). A file with other
  names whose path is past `PATH_MAX` is refused before it is copied
  (`a_hardlinked_file_past_path_max_is_refused_by_the_copy`).
- Deletion: `openat(O_DIRECTORY | O_NOFOLLOW)`, compare `ino` and `mnt_id`
  from `statx` with the opened fd to catch a swap, `unlinkat` symlinks
  without following; keep ignore-list entries, entries with another `mnt_id`
  and `STATX_ATTR_MOUNT_ROOT` entries. Never `st_dev` alone. An unexpected
  mount, swap or permission error stops it; there is no path-based fallback.
- Modes are raw `st_mode` bits (sticky, setuid, setgid survive). In overlay
  form, content xattrs `user.overlay.*` become `user.overlay.overlay.*`
  (kernel 6.7 or later).
- A layer's `trusted.*` xattrs are never set, in any mode, before any syscall
  (`meta::LAYER_SKIPPED_XATTR`, ADR-0075, as BuildKit's containerd
  extraction): counted as `xattrs_skipped`, one warning per layer. The
  copy-up of a hardlink from a lower layer leaves them out too
  (`Xattrs::Layer`); `COPY` (`Xattrs::Copy`) keeps them. The other namespaces
  are set strictly. `rnk-store` keeps extracted layers in `layers/v3/`, so
  none extracted before this rule is reused.
- Identity is compared by `(dev, ino)` (and mount id), never by string; the
  store's `.ranikust-state` marker is a regular file.

## Refs

- **Design:** [`design/rootfs.md`](../../design/rootfs.md) (resolution, `EAGAIN`, metadata order, extraction, copy, users, deletion); [`design/kaniko-mode.md#safe-deletion`](../../design/kaniko-mode.md#safe-deletion), [`#host-safety-checks`](../../design/kaniko-mode.md#host-safety-checks), [`#ignore-list`](../../design/kaniko-mode.md#ignore-list); [`design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer`](../../design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer); [threat model T1-T4, T7](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0006](../../adr/0006-persistent-store.md) (extract to a temporary directory, rename), [ADR-0016](../../adr/0016-overlay-mount-options.md) (`userxattr`, hence the escaping), [ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md) (copy or `FICLONE`, never hardlinks to the store), [ADR-0032](../../adr/0032-observable-behaviour-follows-buildkit.md) (`copy` follows BuildKit where it differs from kaniko), [ADR-0075](../../adr/0075-layer-trusted-xattrs-are-never-set.md) (no `trusted.*`), [ADR-0076](../../adr/0076-the-build-context-owner-is-a-lower-trust-principal.md) (outputs, context files), [ADR-0079](../../adr/0079-resource-limits-with-a-limit-flag.md) (`tar-header`)
- **Evidence:** [exp-09](../../research/experiments/09-kaniko-mode-prototype.md) §A.2 (cases D1-D3), [exp-01](../../research/experiments/01-overlay-semantics.md) finding 7, [exp-07](../../research/experiments/07-kaniko-vs-overlay.md) (image check), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md) (rootfs preparation)
- **kaniko:** `kaniko@44aa7ddc0 pkg/util/fs_util.go:292` `DeleteFilesystem` (path walk, string ignore list: port as fd + `mnt_id`); `:399` `ExtractFile`, whose `:405-418` resolves only the parent; `:630` ignore list from mountinfo; `pkg/util/command_util.go:534` `LookupUser`, which reads the process's own `/etc/passwd`
- **External:** <https://man7.org/linux/man-pages/man2/openat2.2.html>, <https://man7.org/linux/man-pages/man2/statx.2.html>, <https://docs.rs/rustix>

## Tests

Tiers and commands: [`docs/testing.md`](../../testing.md#tiers).

- **T0** (temp dirs): `tests/escape.rs`: the escape suite for `RootPath` and
  `extract` (`..`, symlinks out, chains and loops, `/proc/self/root`,
  `../etc/passwd`, hardlinks out, real magic links under `/proc/<pid>`, a
  thread swapping a directory for a symlink out during every operation);
  deletion of a 3,004-level tree; raw modes 1777 and 4755. Unit tests per
  module, with the same racing swap under `extract`, `delete_contents` and
  `copy`; `extract_overlay` against whiteouts, opaque directories and
  symlinks in the lowers. `tests/model.rs`: `open_at` against a model
  resolver on 150 random trees. `tests/spans.rs`: the metrics.
  `tests/limits.rs`: a 64 MiB GNU long name, long link or PAX set refused at
  its header with under 2 MiB held, a control with the limit off that holds
  the whole name, and a 2 MiB PAX set that passes once the limit is raised,
  its file extracted (ADR-0079). `copy_data` (`src/copy/tests.rs`,
  ADR-0059): sparse, empty and dense files copy exactly, a source holding
  more than its size is refused, and one cut short between `SEEK_DATA` calls
  is refused rather than zero-filled (a red-team finding, fixed).
- **T0, red team:** `/var/run` (ADR-0052 D4, `src/extract/tests.rs`): through
  a container's `/var/run -> ../run`, ignored `/run/secrets` and
  `/run/.containerenv` skipped at their physical path, an opaque marker below
  the link fails the extraction. `copy` with `skip_special` (ADR-0060):
  FIFOs, sockets and device nodes left out at every depth, a file swapped for
  a FIFO during the walk copied from its pinned inode. `prune` (ADR-0061,
  `red_team_prune_*`): equals `copy_filtered` entry for entry, never follows a
  link out, reports `linked_outside` in every walk order, a racing directory
  exchange deletes nothing outside; `move_sub_root` refuses `/` and a
  swapped name. The glob matcher is linear
  (`the_linear_matcher_agrees_with_every_split`, proptest against the old
  matcher as oracle).
  `red_team_framed_never_falls_out_of_step_with_the_tar_crate`
  (`src/extract/limit/tests.rs`, proptest): the `tar-header` bound never
  changes what the `tar` crate hands on and never lets a name or PAX record
  over the bound through. `red_team_names_swapped_during_an_output_never_redirect_it`
  (`src/host/tests.rs`): a thread exchanging the output directory, the tar
  and the digest file with links to victims while `OutputDir` writes 3000
  times (flaky on CI: [#44](https://github.com/tran4774/ranikust/issues/44)).
- **T1** (`#[ignore = "T1: ..."]`, `RNK_T1_ROOT`): extraction and live
  extraction never write below or skip-wrongly real mounts; foreign owners
  and setuid kept raw in `extract`.
- **T2c** (rootful Docker, `just test-container unit-c`): `mounts_inside_the_root_survive`
  and `prune_refuses_a_mount_below_the_root` (`sysadmin`: a bind mount of a
  directory on the same filesystem and a tmpfs inside a root, with links to
  them, in a private mount namespace), device nodes in `extract`, device
  nodes and foreign owners in `copy`, `trusted.*` xattrs, a symlink owned by
  another user in `host`.
- **To do:** `cargo fuzz` on `extract` (Phase 6).

## Metrics

- `rnk_rootfs.extract` per layer (`mode`: `Apply`, `Overlay` or `Live`;
  `entries`, `files`, `whiteouts`, `ignored`, `nodes_skipped`,
  `xattrs_skipped`, `hardlinks`, `bytes_in`, `bytes_written`),
  `rnk_rootfs.copy` per `COPY` source set, `rnk_rootfs.delete` per tree,
  `rnk_rootfs.prune` per pruned root (`entries`, `files`, `dirs`, `kept`)
  ([rows](../../design/metrics.md#rnk-rootfs)).
- Count in the entry loop, record once at the end: never a span per entry.

## Pitfalls

- `st_dev`-only deletion emptied a same-filesystem bind mount (exp-09 D2b).
- A string check let `//` through the system-directory refusal (exp-09 D3).
- Full-path opens fail past 4,096 bytes and drop the subtree (exp-09).
- Deletion holds one fd per tree level and `copy` two per directory level:
  the caller raises `RLIMIT_NOFILE` for deep trees (exp-09 §A.2).
- Joining a whole tar name writes through a final symlink, giving loops like
  `bin/sh -> dash -> dash` (`kaniko@44aa7ddc0 pkg/util/fs_util.go:406-409`).
- Extraction without raw modes turned `/tmp` 1777 into 777 (exp-07).
- A hardlink farm let one `echo >>` change the store and a sibling stage
  (exp-12 finding 9).
- `RESOLVE_IN_ROOT` returns `EAGAIN` when any rename races a `..` step: retry
  it ([design/rootfs.md#resolution](../../design/rootfs.md#resolution); hit by
  the T0 suite).
- `fchmod`/`fsetxattr` refuse `O_PATH` fds, `rustix` has no `fchmodat2`: go
  through `/proc/self/fd/<n>` ([design/rootfs.md#metadata](../../design/rootfs.md#metadata)).
- `chown` clears setuid, setgid, `security.capability`: owner first.
- Reading a file moves an old atime (relatime): check times before reading.
- An image's file opened as root may be a FIFO (hang) or a device node (the
  host's device): use `open_regular`, or `pin` then `Pinned::open_regular`
  for a physical path (no symlink followed, no mount crossed).
- `extract` never crosses a mount and refuses sparse entries. Extension
  headers are bounded by the `tar-header` limit (1 MiB, ADR-0079). Overlay
  form: a hardlink into a lower layer has no target in `layers/<id>`;
  `extract_overlay` copies it from the lowers (copies are keyed by the lower
  inode: copying once per name let a few MB of tar write a lower file's size
  per name), plain `extract` refuses it.
- `copy` follows the source's own symlink and the destination directory's,
  inside their roots, but never one inside the copied tree; where kaniko and
  BuildKit differ it follows BuildKit (ADR-0032): sockets become empty files,
  every xattr but overlay's own is copied best effort, `--chmod` may be
  symbolic (`Chmod`, `mode.rs`, derived from dchapes-mode, BSD, in NOTICE).
- Source roots never contain live mounts: a rule for the builder. `copy` does
  not cross a mount below the source it is given (`EXDEV` stops it), so a
  `/proc` or `/sys` there is refused rather than read as files.
- A destination inside the source (`COPY . /workspace/sub` on a live root)
  recursed without end; `copy` refuses a source directory that is a directory
  it wrote into, as `cp` does.
