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
    pub fn sub_root(&self, p) -> Result<RootPath>;       // built: stages/<n>/rootfs, a COPY --from source
    pub fn statx(&self, p) -> Result<Stat>;              // built: no-follow; carries mnt_id, mount_root
    pub fn set_meta(&self, p, m: &Meta) -> Result<()>;   // built: ids, xattrs, raw mode, times (that order)
    // built: create_dir, create_dir_all, symlink, read_link, hard_link,
    // rename, remove_file, remove_dir; mnt_id()
}
pub fn extract(dst: &RootPath, tar: impl Read, wh: Whiteouts) -> Result<()>;  // [design] plain tar; Whiteouts::{Apply, Overlay}
pub fn copy(src: &RootPath, from: &Path, dst: &RootPath, to: &Path, o: &CopyOpts) -> Result<()>; // [design] FICLONE, else copy
pub fn delete_contents(root: &RootPath, keep: &IgnoreList) -> Result<()>;  // built; IgnoreList: exact cleaned paths (insert, contains)
pub fn lookup_user(root: &RootPath, spec: &str) -> Result<User>;   // built: USER (rnk-sandbox): uid, gid, supplementary gids, home
pub fn lookup_owner(root: &RootPath, spec: &str) -> Result<Owner>; // built: --chown (rnk-builder): gid = uid without a group
pub fn check_workdir(dir: &Path) -> Result<Workdir>;             // [open: Q-011] here or `ranikust`
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
- A dangerous workdir is found by `(dev, ino)` of the directory and of each
  ancestor, never by string; the `.ranikust-root` marker is a regular file.

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
  `cargo fuzz` on `extract`.
  Built: `tests/escape.rs` (`..` chains, absolute, relative and chained
  symlinks out, loops and 50-hop chains, `/proc/self/...` targets, real magic
  links under `/proc/<pid>`, hardlinks out, `sub_root`, a swap between calls,
  and a thread exchanging a directory with a symlink out during 2,000 rounds
  of every operation); unit tests per operation, raw modes 4755 and 1777;
  deletion of a 3,004-level tree, the ignore list, a faked foreign mount id,
  and a swap racing `delete_contents`.
- **T1:** a same-filesystem bind mount and a tmpfs inside the root survive
  `delete_contents` (written, `#[ignore]`; the harness sets `RNK_T1_ROOT`);
  overlay-form extraction in a user namespace.
- **T2** (VM only): `--workdir` at `/`, `//`, `/home`, an unmarked directory.

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
- `openat2(RESOLVE_IN_ROOT)` returns `EAGAIN` when any rename on the system
  races a `..` step: retry it (`design/rootfs.md#resolution`; hit by the T0
  suite).
- `fchmod`/`fsetxattr` refuse `O_PATH` fds and `rustix` has no `fchmodat2`:
  go through `/proc/self/fd/<n>` (`design/rootfs.md#metadata`).
- `chown` clears setuid, setgid and `security.capability`: owner first, mode
  and xattrs after.

## Phase 1

- Build first. Order: `RootPath` with the T0 escape suite → overlay-form
  `extract` → `copy` (`--chown`, `--chmod`, `--from`) and `lookup_user` →
  fd-based deletion of `builds/<id>` and `tmp/` leftovers → the workdir
  checks.
- Deferred to Phase 2: `Whiteouts::Apply` into a live root, the mountinfo
  ignore list, `delete_contents` on `/`, `FICLONE` stage copies.
- [open: Q-011] A workdir holding only mount points has no marker and is
  refused today (kaniko-mode design, open questions). Its owner is open too:
  `check_workdir` is not built here; `RootPath::open` and `statx`
  (`dev`, `ino`, `mnt_id`) are the primitives either owner needs.
- [open: Q-050] Needs Linux 5.8 (`openat2`, `STATX_MNT_ID`); fails with
  `Error::Unsupported`, no fallback.
- Done: `RootPath` with the T0 escape suite; `lookup_user` and
  `lookup_owner` (two functions: `USER` and `--chown` default the gid
  differently, `design/rootfs.md#users-and-groups`); `delete_contents`
  (`design/rootfs.md#deletion`), built before extraction, which reuses its
  walker.
