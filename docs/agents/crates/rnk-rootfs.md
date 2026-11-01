# rnk-rootfs

> **Status:** in progress · **Phase:** 1 ·
> **Depends on:** no internal crate (never `rnk-layer`) · **Used by:** `rnk-snapshot`, `rnk-sandbox`, `rnk-builder`, `rnk-store`, `rnk-context` · **Path:** `crates/rnk-rootfs/`

**TL;DR:** All filesystem access inside a build root goes through `RootPath`,
which resolves paths from a root fd with `openat2(RESOLVE_IN_ROOT)`, so `..`
and symlinks cannot escape. Never turn an in-root path into a host path, and
never delete across a mount: compare `mnt_id`, not `st_dev`.

## Owns

- `RootPath`: fd-based resolution of every path inside a root.
- Layer extraction from an already decompressed tar `Read`, writing
  whiteouts in the form the caller asks for (OCI or overlay); copy,
  ownership, xattrs, raw mode bits.
- `USER` and `--chown` name lookup in the root's `/etc/passwd` and
  `/etc/group`, for `rnk-sandbox` and `rnk-builder`.
- The ignore list: mount points below the root plus `--ignore-path`.
- Safe deletion of a root's contents: the mechanism only.

## Must not

- Reach a root path by joining strings onto a host path: `std::fs` or
  `Path::join` on an image, context or Dockerfile path is a bug.
- Follow symlinks or cross a mount point when deleting, or decide by `st_dev`.
- Decide what to delete and when (`rnk-snapshot` in roots, `rnk-store` in
  the store); mount, create namespaces or run processes (`rnk-sandbox`);
  write tar or decompress: the caller chains `rnk-layer`'s decoder into
  `extract`.

## API (planned)

Built items say `built`; paths are `impl AsRef<Path>`, absolute or relative,
both taken relative to the root. Errors: one `Error` enum (`Unsupported`,
`Io { op, path, source }`, `InvalidPath`, …).

```rust
pub struct RootPath { /* O_PATH | O_DIRECTORY | O_CLOEXEC fd, mnt_id */ }  // built
impl RootPath {
    pub fn open(host_dir) -> Result<Self>;               // built: the only host path; RESOLVE_NO_SYMLINKS
    pub fn open_at(&self, p, how: &OpenHow) -> Result<OwnedFd>; // built: RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS
    pub fn open_regular(&self, p) -> Result<OwnedFd>;    // built: reads a file; FIFOs, devices refused unopened
    pub fn sub_root(&self, p) -> Result<RootPath>;       // built: stages/<n>/rootfs, a COPY --from source
    pub fn create_sub_root(&self, p, mode) -> Result<RootPath>; // built: mkdirat + openat(O_PATH|O_DIRECTORY|O_NOFOLLOW) on one parent fd; empty, euid's, parent's mount, else refused (kaniko stage dirs, ADR-0059)
    pub fn statx(&self, p) -> Result<Stat>;              // built: no-follow; carries mnt_id, mount_root
    pub fn read_names(&self, p) -> Result<Vec<OsString>>; // built: a directory's names, mount points included (a directory listing)
    pub fn pin(&self, p) -> Result<Pinned>;              // built: O_PATH|O_NOFOLLOW, no symlink in any component, root's mount only (a physical path: the kaniko exporter)
    pub fn stay_on_mount(self) -> Self;                  // built: every later resolution with RESOLVE_NO_XDEV, sub_root included (rnk-sandbox's journal, ADR-0043)
    pub fn set_meta(&self, p, m: &Meta) -> Result<()>;   // built: ids, xattrs, raw mode, times (that order)
    pub fn xattrs(&self, p) -> Result<Vec<(OsString, Vec<u8>)>>; // built: every xattr, overlay's too, sorted; no follow, no open of a FIFO or device (rnk-snapshot's exporter)
    // built: create_dir, create_dir_all, symlink, read_link, hard_link,
    // rename, remove_file, remove_dir; mnt_id()
}
pub struct Pinned;                                      // built: stat(), read_link() (readlinkat(fd, "")), xattrs() (none for a symlink), open_regular() (reopen of the pinned inode)
pub fn extract(dst: &RootPath, tar: impl Read, wh: Whiteouts) -> Result<()>;  // built: plain tar; Whiteouts::{Apply, Overlay}
pub fn extract_live(dst: &RootPath, tar: impl Read, keep: &IgnoreList) -> Result<()>; // built: kaniko mode's base into a live root; ignored paths and mounts skipped at their physical path; no device node or FIFO (ADR-0041)
pub fn extract_live_literal(dst, tar, keep) -> Result<()>; // built: as extract_live with Whiteouts::Literal (a kept kaniko-mode stage, ADR-0056): `.wh.*` names are files
pub fn extract_overlay(dst: &RootPath, tar: impl Read, lowers: &[&RootPath]) -> Result<Extracted>; // built: overlay form over lowers (nearest first); a hardlink into a lower is copied (Q-055); Extracted { root_set }: whether the tar had `./`
pub fn copy(src: &RootPath, from, dst: &RootPath, to, o: &CopyOpts) -> Result<()>; // built: COPY/ADD of one resolved source; FICLONE, else data ranges only (SEEK_DATA/SEEK_HOLE, copy_file_range), holes kept; a file holding more than the size read first is refused ("changed while it was copied"), never cut short
pub fn copy_filtered(src, from, dst, to, o, keep: &dyn Fn(&Path, bool) -> bool) -> Result<()>; // built: copy without the entries below `from` that `keep` refuses (.dockerignore; kaniko's ignore list, a refused mount point skipped, not an error)
pub struct CopyOpts { pub owner: Option<Owner>, pub chmod: Option<Chmod>, pub skip_special: bool } // built: None keeps the source's; skip_special leaves devices, FIFOs, sockets out (ADR-0059)
pub fn delete_contents(root: &RootPath, keep: &IgnoreList) -> Result<()>;  // built
pub struct IgnoreList;  // built: insert (it and below), insert_below (kaniko's PrefixMatchOnly), insert_glob (--ignore-path, Go filepath.Match per component); covers, has_below, contains (exact literal)
pub fn lookup_user(root: &RootPath, spec: &str) -> Result<User>;   // built: USER (rnk-sandbox): uid, gid, supplementary gids, home
pub fn lookup_owner(root: &RootPath, spec: &str) -> Result<Owner>; // built: --chown (rnk-builder): gid = uid without a group
pub fn probe_support() -> Result<(), Unsupported>;             // built: start-up check (ADR-0028); Cause::{Missing, Blocked}, kernel release
```

## Invariants

- A root's host path is opened once; every path below it is resolved by the
  kernel from that fd with `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`, so `..`,
  absolute symlinks and `/proc/self/root` stay inside. Tar hardlink targets
  outside the root are refused, and the last component of a replaced entry is
  never followed (threats T1, T2).
- Every fd is `O_CLOEXEC` (T7). Directories are opened by `(parent fd, name)`,
  never by a long path, so tree depth is unlimited.
- Deletion: `openat(O_DIRECTORY | O_NOFOLLOW)`, compare `ino` and `mnt_id`
  from `statx` with the opened fd to catch a swap, `unlinkat` symlinks without
  following; keep ignore-list entries, entries with another `mnt_id` and
  `STATX_ATTR_MOUNT_ROOT` entries. Never `st_dev` alone. An unexpected mount,
  swap or permission error stops it; there is no path-based fallback.
- Modes are raw `st_mode` bits (sticky, setuid, setgid survive). In overlay
  form, content xattrs `user.overlay.*` become `user.overlay.overlay.*`
  (kernel ≥ 6.7).
- Identity is compared by `(dev, ino)` (and mount id), never by string;
  the store's `.ranikust-state` marker is a regular file.

## Refs

- **Design:** [`design/rootfs.md`](../../design/rootfs.md) (resolution, `EAGAIN`, metadata order); [`design/kaniko-mode.md#safe-deletion`](../../design/kaniko-mode.md#safe-deletion), [`#host-safety-checks`](../../design/kaniko-mode.md#host-safety-checks), [`#ignore-list`](../../design/kaniko-mode.md#ignore-list); [`design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer`](../../design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer) (its last paragraph: extraction); [threat model T1–T4, T7](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0006](../../adr/0006-persistent-store.md) (extract to a temporary directory, rename), [ADR-0016](../../adr/0016-overlay-mount-options.md) (`userxattr`, hence the escaping), [ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md) (copy or `FICLONE`, never hardlinks to the store)
- **Evidence:** [exp-09](../../research/experiments/09-kaniko-mode-prototype.md) §A.2 (cases D1–D3), [exp-01](../../research/experiments/01-overlay-semantics.md) finding 7, [exp-07](../../research/experiments/07-kaniko-vs-overlay.md) (image check), [exp-12](../../research/experiments/12-kaniko-mode-parallel.md) (rootfs preparation)
- **kaniko:** `kaniko@44aa7ddc0 pkg/util/fs_util.go:292` `DeleteFilesystem`
  (path walk, string ignore list: port as fd + `mnt_id`); `:399`
  `ExtractFile`, whose `:405-418` resolves only the parent; `:630` ignore list
  from mountinfo; `pkg/util/command_util.go:534` `LookupUser`, which reads the
  process's own `/etc/passwd`.
- **External:** <https://man7.org/linux/man-pages/man2/openat2.2.html>, <https://man7.org/linux/man-pages/man2/statx.2.html>, <https://docs.rs/rustix>

## Tests

- **T0** (temp dirs): escape suite for `RootPath` and `extract`: `..`,
  symlinks out, chains and loops, `/proc/self/root`, `../etc/passwd`,
  hardlinks out. Deletion of a 3,004-level tree; raw modes 1777 and 4755;
  marker cases of exp-09 D3. Property test against a model resolver;
  `cargo fuzz` on `extract`. Built: `tests/escape.rs` (all of the above
  but the marker, plus real magic links under `/proc/<pid>` and a thread
  swapping a directory for a symlink out during every operation); unit
  tests per module, with the same racing swap under `extract`,
  `delete_contents` and `copy` (both roots); `extract_overlay` against
  whiteouts, opaque directories and symlinks in the lowers;
  `tests/model.rs` checks `open_at` against a model resolver on 150 random
  trees; `tests/spans.rs` the metrics. Red team of `/var/run` no longer
  ignored (ADR-0052 D4, `src/extract/tests.rs`): through a container's
  `/var/run -> ../run`, files, a hardlink, links, whiteouts aimed at an
  ignored `/run/secrets` and `/run/.containerenv` skipped at their
  physical path, a link climbing above the root kept in it, one into
  `/proc` leading nowhere; an opaque marker below the `var/run` link fails
  the extraction (never followed) unless the layer's own `var/run/`
  replaces the link first. Still to do: `cargo fuzz` on `extract`.
- **T1** (written, `#[ignore]`): a bind mount and a tmpfs in the root
  survive `delete_contents` (harness sets `RNK_T1_ROOT`); foreign owners
  and setuid in `extract`.
- **T2** (VM only): device nodes in `extract`, device nodes and foreign
  owners in `copy` (written, `#[ignore]`, `just test-vm unit`).

## Metrics

- `rnk_rootfs.extract` per layer (`mode`: `Apply`, `Overlay` or `Live`;
  `entries`, `files`, `whiteouts`, `ignored`, `nodes_skipped`, `hardlinks`, `bytes_in`,
  `bytes_written`), `rnk_rootfs.copy` per `COPY`
  source set, `rnk_rootfs.delete` per tree ([rows](../../design/metrics.md#rnk-rootfs)).
- Count in the entry loop, record once at the end: never a span per entry.

## Pitfalls

- `st_dev`-only deletion emptied a same-filesystem bind mount (exp-09 D2b).
- A string check let `//` through the system-directory refusal (exp-09 D3).
- Full-path opens fail past 4,096 bytes and drop the subtree (exp-09).
- Deletion holds one fd per tree level: raise `RLIMIT_NOFILE` (exp-09 §A.2).
- Joining a whole tar name writes through a final symlink, giving loops like
  `bin/sh -> dash -> dash` (`kaniko@44aa7ddc0 pkg/util/fs_util.go:406-409`).
- Extraction without raw modes turned `/tmp` 1777 into 777 (exp-07).
- A hardlink farm let one `echo >>` change the store and a sibling stage
  (exp-12 finding 9).
- `RESOLVE_IN_ROOT` returns `EAGAIN` when any rename races a `..` step:
  retry it (`design/rootfs.md#resolution`; hit by the T0 suite).
- `fchmod`/`fsetxattr` refuse `O_PATH` fds, `rustix` has no `fchmodat2`:
  go through `/proc/self/fd/<n>` (`design/rootfs.md#metadata`).
- `chown` clears setuid, setgid, `security.capability`: owner first.
- Reading a file moves an old atime (relatime): check times before reading.
- An image's file opened as root may be a FIFO (hang) or a device node (the
  host's device): use `open_regular`, or `pin` then `Pinned::open_regular`
  for a physical path (no symlink followed, no mount crossed).
- `extract` never crosses a mount and refuses sparse entries; extension
  headers are unbounded for now (Q-056). Overlay form: a hardlink into a
  lower layer has no target in `layers/<id>`; `extract_overlay` copies it
  from the lowers (Q-055), plain `extract` refuses it.
- `copy` follows the source's own symlink and the destination
  directory's, inside their roots, but never one inside the copied tree;
  where kaniko and BuildKit differ it follows BuildKit (Q-120,
  `ADR-0032`): sockets become empty files, every xattr but overlay's own
  is copied best effort, `--chmod` may be symbolic (`Chmod`, `mode.rs`,
  derived from dchapes-mode, BSD, in NOTICE).
- Source roots never contain live mounts: a rule for the builder. `copy`
  does not cross a mount below the source it is given (`EXDEV` stops it),
  so a `/proc` or `/sys` there is refused rather than read as files.
- A destination inside the source (`COPY . /workspace/sub` on a live
  root) recursed without end; `copy` refuses a source directory that is a
  directory it wrote into, as `cp` does.
- `copy` holds two fds per directory level, deletion one: the caller
  raises `RLIMIT_NOFILE` for deep trees.
- Copying a hardlink target from a lower layer once per name let a few
  MB of tar write a lower file's size per name; copies are keyed by the
  lower inode.

## Phase 1

- Build first. Order: `RootPath` with the T0 escape suite → overlay-form
  `extract` → `copy` (`--chown`, `--chmod`, `--from`) and `lookup_user` →
  fd-based deletion of `builds/<id>` and `tmp/` leftovers.
- Deferred to Phase 2: `delete_contents` on `/`, `FICLONE` stage copies.
  Built in Phase 2 task 3: `extract_live` and `IgnoreList`'s kaniko
  matching; the mountinfo list itself is `rnk-snapshot`'s.
- The state directory's checks belong to `ranikust` (ADR-0050); this
  crate provides primitives such as `RootPath::open`, `statx` (`dev`,
  `ino`, `mnt_id`), `open_at` and `read_names`.
- Needs Linux 5.8; `Error::Unsupported`, no fallback. The binary checks
  it at start-up (ADR-0028).
- Done: `RootPath`, `extract` and `extract_overlay` (Q-055),
  `lookup_user`/`lookup_owner` (`USER` and `--chown` default the gid
  differently), `delete_contents` (built before `extract`, which reuses
  its walker), `copy`. The builder still
  resolves `COPY` sources and the destination name (Q-080) and extracts
  `ADD` archives.
