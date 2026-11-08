# Confined filesystem access

**TL;DR:** how `rnk-rootfs` reaches paths inside a build root, an extracted
layer or a store entry without ever leaving it, and how it writes metadata,
extracts layers, looks up users and deletes trees on top of that. Every other
crate reaches untrusted paths only through it.

Threats: [T1–T3, T7](../security/threat-model.md#5-threats-and-mitigations).
Card: [rnk-rootfs](../agents/crates/rnk-rootfs.md).

## Invariants

- A root's host path is opened once, by `RootPath::open`, with
  `openat2(RESOLVE_NO_SYMLINKS)`. Every path below it is resolved by the
  kernel from that fd with `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`: `..`
  stops at the root, absolute symlinks are relative to the root, and
  `/proc/<pid>/root`-style magic links fail with `ELOOP`.
- Operations on a directory entry (create, link, rename, unlink, `statx`,
  metadata) resolve the parent that way, then act on the last name with an
  `*at` syscall that never follows it.
- Every fd is `O_CLOEXEC`; opens other than `O_PATH` add `O_NOCTTY`.
- No string-based fallback. When `openat2` or the `statx` mount id is
  missing (before Linux 5.8) or blocked by seccomp, the call fails with
  `Error::Unsupported`. The binary checks for Linux 5.8 at start-up
  ([ADR-0028](../adr/0028-minimum-kernel-linux-5-8.md)). That start-up
  check reports `ENOSYS`, or `statx` without the mount id, on a 5.8 or
  later release as coming from something in between (`Cause::Hidden`: an
  emulator such as Rosetta, or a seccomp filter answering `ENOSYS`), not
  from an old kernel; the release only shapes the message.

## Resolution

- **Paths** may be absolute or relative; both are taken relative to the
  root. `""`, `"/"` and `"."` name the root itself. A path is handed to
  `openat2` as given, not normalized, so `file/.` and `file/` fail with
  `ENOTDIR` as POSIX says (a model-resolver property test caught the
  normalized version opening `file`). Operations on an entry split off the
  last name lexically; only a path ending in a plain name can be created,
  linked, renamed or removed.
- **The last symlink** is followed, inside the root, by `open_at` (unless
  `no_follow`), `sub_root` and `create_dir_all`; never by `statx`,
  `set_meta`, `read_link`, `hard_link`, `rename`, `remove_*`.
- **No symlink in a parent** for a physical path, one the kernel printed
  (a mount point in `/proc/self/mountinfo`, a `readlink` of a descriptor):
  `statx_physical` and `pin` resolve with `RESOLVE_NO_SYMLINKS`, so a
  symlink on the way is `ELOOP`, and a last one is the link itself, not
  followed; a `..` anywhere, which `RESOLVE_IN_ROOT` would clamp, is
  `InvalidPath`; `pin` also stays on the root's mount (the kaniko exporter),
  `statx_physical` crosses mounts unless `stay_on_mount` (`rnk-sandbox`'s
  mount walk, ADR-0101).
- **`EAGAIN`.** With `RESOLVE_IN_ROOT`, the kernel answers `EAGAIN` when a
  rename or mount anywhere on the system raced with a `..` step, because it
  cannot prove the walk stayed inside. The T0 suite hit it as soon as one
  test renamed in a loop while another resolved `..`: parallel stages do
  this all the time. `openat2` is retried up to 128 times, then the error is
  returned.
- **`ENOSYS` and `EPERM`.** `ENOSYS` from any `openat2` is
  `Error::Unsupported`. `EPERM` is too, but only from `RootPath::open`: an
  `O_PATH` open checks no permission on the entry, so `EPERM` there comes from
  a seccomp filter; later `EPERM`s are ordinary errors.
- **`create_dir_all`** re-resolves each prefix from the root after creating
  it, so a symlink in the path is followed relative to the root, not to the
  directory before it. A dangling symlink in the way fails with `ENOENT`, as
  `mkdir -p` does.
- **Long paths.** One `openat2` fails with `ENAMETOOLONG` past 4,096 bytes
  (`exp-09`); callers that walk trees open each directory by
  `(parent fd, name)` instead.

## Metadata

`set_meta` pins the entry with an `O_PATH | O_NOFOLLOW` fd before changing
anything, then applies, in this order:

1. **Owner**, `fchownat(fd, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW)`.
   First, because `chown` clears setuid, setgid and `security.capability`.
   An id of `4294967295` in a layer entry is passed on as `-1`, which
   `chown` reads as "unchanged", as kaniko
   (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1323,1336`) and BuildKit
   (`buildkit@cd8ee244 vendor/github.com/containerd/containerd/v2/pkg/archive/tar.go:409-417`)
   pass it ([#212](https://github.com/tran4774/ranikust/issues/212); the
   [owner scans](modes-and-detection.md#owners-the-user-namespace-does-not-map)
   agree). A new entry keeps the extracting process's id. A directory that
   an earlier layer made keeps that layer's id in kaniko mode, which
   extracts every layer into one root as kaniko does, and gets the
   process's id in overlay mode, which extracts each layer into a directory
   of its own as BuildKit's overlayfs snapshotter does: an accepted
   difference between the modes
   ([ADR-0141](../adr/0141-an-owner-of-4294967295-over-an-earlier-layers-directory-is-an-accepted-difference-between-the-modes.md)).
2. **Xattrs**, before the mode, because setting `user.*` needs write
   permission that the final mode may remove. An empty ACL value removes the
   ACL (`system.posix_acl_access` or `_default`, with `removexattr`, no
   error if there is none), in an extraction, a `COPY` and every `set_meta`:
   Linux 6.2 and later take the empty value as a removal, an older kernel
   refuses it (`EINVAL`).
3. **Mode**, the raw bits `& 0o7777` (setuid, setgid and sticky kept, no
   umask). Skipped for symlinks.
4. **Times**, `utimensat(fd, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW)`, last,
   so no other step moves them.

`fchmod` and `fsetxattr` refuse `O_PATH` fds, and `rustix` has no
`fchmodat2`. So mode and xattrs go through ranikust's own
`/proc/self/fd/<n>` (checked to be procfs first): `fchmodat` and `setxattr`
follow that magic link to exactly the pinned inode, as glibc's
`fchmodat(AT_SYMLINK_NOFOLLOW)` does. A symlink's xattrs are set with
`lsetxattr` on `/proc/self/fd/<parent>/<name>`, which never follows the last
name; the name is checked to still be the pinned symlink (same inode) before
the sets and after them, as reading and stripping xattrs check it. That
detects a swap but cannot prevent a write to an entry swapped in between (one
in the same pinned parent), and a swap undone before the second check is not
seen. `setxattrat(2)` (Linux 6.13) with `AT_EMPTY_PATH` on the `O_PATH` fd
might close the window if the kernel accepts an `O_PATH` fd there: unverified
(a symlink can only be pinned with `O_PATH`). Regular files being written use
plain `fchown`, `fsetxattr`, `fchmod` and `futimens` on their open fd.

## Extraction

`extract(dst, tar, whiteouts)` reads a plain tar stream; the caller chains
`rnk-layer`'s decoder in front and publishes the result
([overlay snapshots](overlay-snapshot.md#exporting-a-diff-as-an-oci-layer),
`ADR-0006`). The `tar` crate only parses (ustar, GNU long names and sparse
files, PAX records); its `unpack`, which joins names onto a host path, is
never called.

- **Names.** Each entry name and hardlink target is cleaned lexically; a
  `..` that climbs above the root is refused (`Error::Entry`), as Docker
  refuses a "breakout", instead of being clamped. Absolute names are taken
  relative to the root. The cleaned path is then reached through
  `RootPath`, so a symlink in the root or earlier in the same layer cannot
  carry an entry outside.
- **Replacing.** An existing entry is unlinked (a directory removed
  through the deletion walker, started from the very directory just
  `statx`ed) and the new one created with `O_EXCL`, `mkdirat`, `symlinkat`,
  `linkat` or `mknodat`, none of which follow the last name; a directory
  over a directory merges. Missing parents are created `0755`.
- **Mounts.** Extraction never crosses a mount inside the root: parents are
  resolved with `RESOLVE_NO_XDEV`, and an existing entry on another mount or at
  a mount root is never replaced, merged into or whited out; such entries fail
  with `Error::Entry`. Extraction into a live root (`extract_live`, kaniko
  mode) takes an ignore list instead and skips what it covers, as kaniko does
  ([kaniko-mode](kaniko-mode.md#ignore-list)).
- **Bombs.** GNU sparse entries are refused: the `tar` crate writes their
  holes as zeros, so a tiny entry could claim 2^62 bytes. GNU long names,
  long links and PAX record sets, which the `tar` crate reads whole into
  memory, are bounded by the `tar-header` limit (1 MiB by default,
  `--limit`, ADR-0079): a reader in front of the `tar` crate follows the
  block framing as it does, PAX `size` included, and fails at the header of
  a larger one, before its body is read. The limit is the crate's only
  process-wide setting (`set_extension_header_limit`). Fuzzing the extractor
  is a Phase 6 task.
- **Headers alone.** `check_headers(tar)` reads a tar as an extraction that
  applies whiteouts does, through the same code (`for_each_entry`,
  `read_header`), and writes nothing: the framing, the `tar-header` limit and
  every refusal a header shows on its own (names and hardlink targets out of
  the root, sparse and unknown types, whiteouts naming nothing, a link with an
  empty target, a file or symlink as the layer's root, ids and times that do
  not parse, and the kernel's limits below), with the extraction's errors. It
  refuses nothing such an extraction takes but a directory path of 4095 bytes,
  which a live root at `/` refuses (below); what the root or the ignore list
  decides is the extraction's. Kaniko mode's spool pass on `/` calls it on the
  bytes it already decompresses, so a header it refuses fails the layer before
  `/` changes ([cache hit](kaniko-mode.md#a-cache-hit), ADR-0123). In a user
  namespace it calls `check_headers_and_owners`, which also notes each owner
  the extraction would set that a `MappedIds` lacks, skipping the paths the
  ignore list it is given keeps, as the extraction does; `scan_owners` does
  the same as the overlay form reads a tar (devices left out). Both fill one
  `Unmapped`, the refusal `Error::Unmapped` carries, as a `chown` that fails
  with `EINVAL` does
  ([owners](modes-and-detection.md#owners-the-user-namespace-does-not-map),
  ADR-0098).
- **The kernel's limits.** `read_header` refuses values the kernel refuses,
  so the entry fails before anything of it is written, in every extraction
  and the check alike
  ([#179](https://github.com/tran4774/ranikust/issues/179)):
  - **Names and paths:** a NUL byte in a name or link target; a component
    over 255 bytes (`NAME_MAX`); a directory path over 4095 bytes
    (`PATH_MAX` counts the NUL), the entry's name aside, since the kernel
    gets the directory, then the name; in a live root, over what
    `/proc/self/fd` shows of it with the root's own path, 4094 bytes on `/`
    (where a directory lies is read there), which the check holds to as it
    guards `/`; a symlink target over 4095 bytes.
  - **Xattrs** on a directory, file or symlink, as they are set (escaped in
    the overlay form): a name that is empty, over 255 bytes
    (`XATTR_NAME_MAX`) or holds a NUL byte; a value over 64 KiB
    (`XATTR_SIZE_MAX`); a non-empty `security.capability` that is not 20
    bytes of revision 2 or 24 of revision 3 with a root id other than -1
    (`cap_convert_nscap` in `security/commoncap.c`); `user.*` on a symlink.
  - **POSIX ACLs** (`system.posix_acl_access` or `_default`) that the
    kernel's VFS refuses before a filesystem sees them, as
    `posix_acl_from_xattr` and `posix_acl_valid` in `fs/posix_acl.c` do: a
    version other than 2, a size other than 4 + 8n bytes, an unknown tag, a
    named user or group of id -1, permissions past `rwx`, entries missing,
    repeated or out of order; and a default ACL with entries on anything but
    a directory (`set_posix_acl`). An empty value is a removal
    ([Metadata](#metadata)).
  - **The same answer on every host.** Two limits hold where a kernel would
    take more, so whether a layer is taken never depends on the host's
    kernel or filesystem (real images carry neither): a name over 255 bytes,
    which a FUSE filesystem may take; and a malformed POSIX ACL on Linux 5.8
    to 6.1, where the VFS does not check ACLs yet (the ACL handler most
    filesystems share does) and 9p outside `access=client`, CIFS, and FUSE
    mounted in the initial user namespace without ACLs pass the value on
    unparsed.
  - **Devices and FIFOs:** their name and xattrs are refused by the
    extraction that makes them, before it or its parents are made: a live
    root makes none, a user namespace no device.
  - **Taken**, as the kernel never gets them: the `.wh.` prefix of a
    whiteout (the name it hides is checked), AUFS metadata, a hardlink's
    xattrs and `trusted.*` ones.
  - **Refused all the same:** an entry at an ignored path, a whiteout whose
    directory is missing and a hardlink to a device left out: whether they
    are skipped depends on the root or on an earlier entry, which a header
    does not show.
  - **Left to write time**, as they depend on the filesystem, the namespace
    or the root: an xattr name the filesystem does not take (most take only
    some of the kernel's namespaces, `user.`, `security.` and the like; a
    FUSE or Ceph one may take any name), a POSIX ACL on a filesystem without
    them, a value too big for its blocks, `security.*` in a user namespace,
    and an id the user namespace does not map: a named user or group in an
    ACL, or the root id of a revision 3 `security.capability` (`EINVAL`,
    measured on Linux 6.8 and 7.0).
  - **In a live root**, an entry that names a directory a symlink puts past
    what `/proc/self/fd` shows (its own, a whiteout's or a hardlink
    target's) is refused before anything of it is made or removed.
- **Metadata.** Numeric uid and gid from the header (PAX `uid`/`gid`
  included), raw mode bits, PAX `SCHILY.xattr.*` but `trusted.*` (never
  set, in every mode, as BuildKit's containerd extraction: a filesystem may
  leave its checks to the privileged caller, as XFS does for the raw ACLs
  `trusted.SGI_ACL_*`; counted as `xattrs_skipped`, one warning per layer,
  `ADR-0075`), PAX `mtime`/`atime` with
  nanoseconds (else the header's seconds; `atime` defaults to `mtime`), in
  the order of [Metadata](#metadata); a regular file gets them through its
  open fd. Directories are created `0700` and get their mode and times last,
  deepest first, so later entries neither move their mtime nor hit a
  read-only directory. In a user namespace, a `security.*` xattr other
  than `security.capability` needs `CAP_SYS_ADMIN` over the filesystem's
  namespace where no security module manages the name
  (`cap_inode_setxattr`; SELinux decides `security.selinux` by policy), so
  the kernel refuses it and the layer fails, as in BuildKit rootless
  (`buildkit@cd8ee244 vendor/github.com/containerd/containerd/v2/pkg/archive/tar.go:419-436`;
  T1 `base_layers_extract_in_the_user_namespace_as_overlay_as_root_extracts_them`,
  whose `security.rnktest`, a name no security module claims, is refused on
  every host).
- **Hardlinks** link the target named in the archive with `linkat(…, 0)`,
  so a symlink target is linked itself. In the overlay form the target may
  be in a lower layer, which `layers/<id>` does not hold:
  `extract_overlay(dst, tar, lowers)` (which also reports whether the tar
  had an entry for `./`, so `rnk-snapshot` knows the layer's root
  metadata is its own) takes the lower layers' extracted
  directories, the nearest first as in `lowerdir=`, and copies the target
  from the first one that shows it
  ([ADR-0012](../adr/0012-accept-divergence-between-modes.md)). "Shows" follows
  overlay's merged view: each layer, this one first, is walked by name without
  following a symlink; a whiteout, a non-directory (a symlink included) on the
  way, or a missing directory below an opaque one hides everything below; an
  opaque directory hides what its layer lacks. The copy is a regular file
  (or symlink, FIFO, device node) with the lower inode's owner, mode, times
  and xattrs as stored, overlay's own (`trusted.overlay.*`,
  `user.overlay.*` but the escaped `user.overlay.overlay.*`) and every
  other `trusted.*` (`ADR-0075`) left behind,
  which splits the hardlink as overlay's copy-up does (`ADR-0012`). Copies
  are keyed by lower layer and inode: later links to any name of the same
  lower inode link to that copy while it is still there (a copy the layer
  whites out or replaces is forgotten, and one it removed by another name
  is found gone, its directory or name missing or holding another inode,
  type, size or mtime, so a later link copies it again rather than linking
  what took its place), so the bytes written stay bounded by the
  lower layers' size, however many names a layer links. Only the
  `user.overlay.opaque=y` form ranikust writes is recognised, as `layers/` is
  written by ranikust alone. A target in no layer, hidden, or a directory is an
  `Error::Entry`; so is any missing target for plain `extract`. Lower
  layers are only read.
- **`Whiteouts::Overlay`** (`layers/<id>`, mounted with `userxattr`,
  `ADR-0016`): `.wh.<name>` becomes the character device 0/0, which an
  unprivileged user may create (measured on Linux 7.0; allowed since the
  kernel change "vfs: allow unprivileged whiteout creation", Linux 5.8
  [external]), so T0 covers it;
  `.wh..wh..opq` becomes `user.overlay.opaque=y` on its directory; content
  xattrs `user.overlay.X` are stored as `user.overlay.overlay.X`.
- **`Whiteouts::Apply`** (a live root, kaniko mode): `.wh.<name>` removes
  `<name>`; `.wh..wh..opq` removes everything in its directory that this
  layer did not create, keeping this layer's entries and the directories
  leading to them, at the point the marker appears in the stream. "This
  layer's entries" are those it still has: what it created is kept as a
  tree of names, each name once however long the (physical) paths through
  it, and a whiteout or a replacement that deletes an entry forgets it with
  everything below, so the memory follows what the layer leaves on disk, not
  how many entries it had (T15). Extraction into a live root, mount points
  included, is [kaniko mode](kaniko-mode.md)'s. A directory the
  layer listed and a later entry deleted gets no metadata at the end; when
  that deletion had to keep an ignored path, the directories below it that
  are still on disk keep theirs, whatever the ignore list says might have
  been kept. A directory that led to an entry the layer created stays the
  layer's when that entry goes, as it stays on disk: an opaque marker
  later keeps it, as overlay's merged view does.
- Other `.wh..wh.*` names (AUFS bookkeeping) are skipped; global PAX
  headers are ignored; unknown entry types are refused. Device nodes need
  `CAP_MKNOD` in the initial user namespace (T2): `extract_overlay` with
  `DeviceNodes::Skip`, as a build in a user namespace extracts, makes no
  block or character device and counts each in `nodes_skipped`, as
  `extract_live` does (ADR-0106). The entry already at a device's name (a
  lower layer's, or an earlier one of this layer) then stays, as in kaniko
  mode, in kaniko, whose `ExtractFile` has no case for a device
  (`kaniko@44aa7ddc0 pkg/util/fs_util.go:439-581`), and in BuildKit
  rootless, which skips one before it clears the path
  (`buildkit@cd8ee244 vendor/github.com/containerd/containerd/v2/pkg/archive/tar.go:251-262`;
  [ADR-0128](../adr/0128-a-canary-guards-nested-namespaces-a-budget-bounds-device-names-and-a-device-left-out-keeps-the-lower-entry.md)).
  A hardlink to a device left out by the same layer is left out with it
  while its name is remembered. Names are remembered within a budget of
  about 1 MiB a layer; once a name passes it, with one warning, no further
  name is remembered, and a link to one of those takes the normal hardlink
  path: linked to what the layer has at that name, else copied from the
  nearest lower layer that shows it, else refused (ADR-0128). A link to a
  device a lower layer left out takes that path too, as the names are
  remembered per layer: a deeper layer's entry at that name is copied, and
  with none the layer is refused
  ([ADR-0131](../adr/0131-a-link-to-a-device-a-lower-layer-left-out-takes-the-normal-hardlink-path.md)).
  A device as the layer's root is refused either way. Foreign owners need
  root or a user namespace (T1).

## Copy

`copy(src, from, dst, to, opts)` copies one source of a `COPY` or local
`ADD` between two roots: the context and a stage root, or two stage roots
for `--from`. The builder expands wildcards and `.dockerignore` and derives
`to` from the Dockerfile's destination (ADR-0032); `ADD`'s archive extraction
is `extract`.

- **Source.** `from` is resolved in `src` following symlinks there, the
  last one included, as BuildKit's `FollowSymlinks` copy does
  (`buildkit@cd8ee244 frontend/dockerfile/dockerfile2llb/convert_copy.go:283-293`);
  kaniko copies a symlinked source as a symlink
  (`kaniko@44aa7ddc0 pkg/commands/copy.go:150`). Where the two differ,
  `copy` follows BuildKit (ADR-0032). A directory has its
  contents copied; anything else is copied to exactly `to`.
- **Reading.** The source tree is walked by `(directory fd, name)`: each
  entry is pinned with `O_PATH | O_NOFOLLOW` and `RESOLVE_NO_XDEV`, so the
  walk never enters another mount below `from` (source roots must not hold
  live mounts: a `/proc` there would be read as files), and `statx`ed
  through that fd. One frame per directory level holds its two fds and
  the names not yet copied; the source and destination paths of the
  entry at hand are two buffers the walk extends and shortens, never a
  copy per level, so a deep chain a `RUN` made costs memory linear in its
  depth (400 levels of 200-byte names: 64 MB before, 0.5 MB now,
  `tests/memory.rs`), whichever way it is copied (a per-stage base from
  its parent, `COPY --from`, a kept stage copied out). A file with other
  names is remembered by the path of its first copy, to link the others,
  so one past `PATH_MAX` is refused before it is written, keeping that map
  at about 4 KB a file (the link would fail with `ENAMETOOLONG` anyway).
  Only a pinned regular file or directory is reopened, through ranikust's
  own `/proc/self/fd`, for reading; a FIFO or device node is recreated with
  `mknodat`, never opened; a symlink is copied with `readlinkat` on the
  pinned fd; a socket becomes an empty regular file with its mode, as
  BuildKit's `copyDevice` clears the socket bit (`fsutil
  copy/copy_nowindows.go:33-45`). Names are sorted, so neither the copy nor
  which name of a hardlinked inode comes first depends on directory order.
- **Writing.** As extraction writes: parents resolved in `dst` with
  `RESOLVE_NO_XDEV`, entries created with `O_EXCL`, `mkdirat`, `symlinkat`,
  `linkat` or `mknodat`, never following the last name; an entry on
  another mount or at a mount root is refused. A directory merges into a
  directory and replaces anything else; any other entry replaces a
  non-directory and is refused over a directory. The destination
  directory of a directory's contents, and only it, is followed when it is
  a symlink, inside `dst`. Content goes through `copy_file_range`
  (`std::io::copy`), which reflinks where the filesystem shares extents.
- **Metadata.** Every copied entry gets `opts.owner` (`--chown`; `None`
  keeps the source's, as `COPY --from` without `--chown`), `opts.chmod`
  (`--chmod`, directories included, not symlinks: an octal mode replaces
  the bits, a symbolic one such as `go+rX` changes each entry's own, as
  BuildKit parses it with dchapes-mode; `None` keeps the raw bits), the
  source's mtime as mtime and atime, and its xattrs: BuildKit copies them
  all and only logs a failure (`fsutil copy/copy_nowindows.go:15-31`,
  `solver/llbsolver/file/backend.go:262-265`), so does `copy`, except
  overlay's own (`trusted.overlay.*`, `user.overlay.*`), with a warning
  for one the destination refuses. kaniko copies only
  `security.capability` (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1533`);
  layers carry only that one either way (ADR-0032). Hardlinks inside the
  copied tree stay hardlinks. Directories the copy creates above `to`, and
  `to` itself for a directory's contents, get an octal `--chmod`'s
  permission bits, else `0755` (BuildKit's `MkdirAll` in `fsutil
  copy/copy.go:85-89`), owned by `opts.owner`, or by the process when it is
  `None`, as `MkdirAll` leaves them (`to` takes the source directory's
  times);
  an existing `to` keeps its metadata. A directory's mode and
  times are set through the fd it was written through when its entries
  are done, deepest first; nothing is re-resolved by path, and memory and
  fds stay bounded by the depth (two fds per level: the caller raises
  `RLIMIT_NOFILE` for deep trees; deletion, unlike the copy, holds a fixed
  number, [below](#deletion)).
- **Into itself.** The device and inode of every destination directory
  the copy writes into are recorded; a source directory among them (the
  destination inside the source, one root as both, as kaniko mode on a
  live root has) is refused before it is listed, as `cp` does, instead
  of recursing until the fds or the disk run out.
- **Owners by source.** The builder passes `0:0` for context sources
  without `--chown` (Docker's documented rule; kaniko only behind its
  `FF_KANIKO_COPY_AS_ROOT`, `kaniko@44aa7ddc0 pkg/commands/copy.go:84`),
  and `None` for `--from` without `--chown`.

## Users and groups

`/etc/passwd` and `/etc/group` are read with `RootPath::open_regular` (so a
symlinked `/etc/passwd` resolves inside the root), up to 16 MiB each; a
missing file is an empty database. `open_regular` pins the entry with
`O_PATH`, refuses anything but a regular file, and only then reopens that
inode for reading through `/proc/self/fd`: an image could otherwise make
ranikust open a FIFO (the build hangs) or a device node (the host's device,
as root). Empty lines, `#` comments, NIS `+`/`-` lines and lines
with a malformed or out-of-range id are skipped, never read as id 0. Ids
above `2147483647` are refused, as runc does: `chown(2)` reads `u32::MAX` as
"unchanged".

| | `USER` (`lookup_user`, for `rnk-sandbox`) | `--chown` (`lookup_owner`, for `rnk-builder`) |
|---|---|---|
| user | name in `passwd`, else numeric uid, looked up by uid for its group and home | name in `passwd`, else numeric uid |
| unknown numeric uid | gid 0, home `/`, as Docker ([sandbox](sandbox.md)) | allowed |
| group given | name in `group`, else numeric gid; no supplementary groups | name in `group`, else numeric gid |
| group omitted | the passwd entry's gid; supplementary groups are the groups listing the user | gid = uid, Docker's documented rule and `kaniko@44aa7ddc0 pkg/util/command_util.go:495` |
| home | passwd home, `/` when unknown or empty | n/a |

kaniko reads the process's own `/etc/passwd`
(`kaniko@44aa7ddc0 pkg/util/command_util.go:534`); ranikust never reads the
host's.

## Deletion

`delete_contents(root, keep)` implements
[safe deletion](kaniko-mode.md#safe-deletion); the same walker removes a
directory an extracted entry replaces, and applies OCI whiteouts.

- Iterative, one frame per directory level holding its own name and its
  `statx`: no recursion, no `PATH_MAX` limit (a 3,004-level tree in T0). The
  path of the entry at hand is one buffer the walk extends and shortens, never
  a copy per level, so memory is linear in the entries on the stack (each
  level's name, its `statx` and the names of its directory not yet visited),
  not quadratic in depth (a 400-level chain of 200-byte names: 0.4 MB, against
  32 MB before; `tests/memory.rs`; `prune` alike). Names are read before any
  is removed.
- At most 32 directory fds (`OPEN_DIRS`), whatever the depth or
  `RLIMIT_NOFILE`: the directory being emptied and its nearest ancestors keep
  an `O_RDONLY` fd, a farther one's is closed. Climbing back to it, the walk
  opens `..` of its child (still open; `RESOLVE_NO_XDEV`, no symlink) and
  checks it is the same inode, device and mount id as when first opened,
  else `Error::Changed` with nothing done in what `..` reached: a directory
  moved or swapped out from under it meanwhile leads elsewhere. GNU `fts`
  climbs the same way for `rm -rf`. `prune` nests a deletion walk in its
  own, so it holds at most 64 (`tests/descriptors.rs`: 2,000 levels with
  only 32 or 64 fds left free under the soft `RLIMIT_NOFILE`).
- The reopen check is inode identity (inode, device, mount id), as in GNU
  `fts`. On an overlay's merged directory, the deletions below an ancestor
  copy it up before the walk climbs back to it; it kept its inode number
  with every layer on one filesystem (as Docker's and containerd's
  `overlay2` mount it) and with the upper on a tmpfs, `xino` off or on (T2
  `deep_trees_on_an_overlay_are_deleted_and_pruned_whatever_its_layers`,
  kernels 5.15 and 6.8) [measured]. Where copy-up records no origin (a
  lower that cannot encode file handles: FUSE, 9p, an overlay without
  `nfs_export`), a copied-up directory's inode number may change: past 30
  levels the walk then stops with `Error::Changed`, failing closed
  [inferred].
- Each entry gets `statx(AT_SYMLINK_NOFOLLOW)`. Another mount id or
  `STATX_ATTR_MOUNT_ROOT` keeps it untouched. A directory is opened with
  `O_DIRECTORY | O_NOFOLLOW` and `RESOLVE_NO_XDEV`, and its `ino`, device and
  mount id must match that `statx`; otherwise, or when `rmdir` then finds a
  non-directory or new entries under the name, deletion stops with
  `Error::Changed`.
- The ignore list holds exact, lexically cleaned, physical paths inside the
  root (the walk never follows a symlink, so a path through one never
  matches); `..` above the root and the root itself are refused. A kept
  entry keeps its ancestors, which are not empty. What goes on the list is
  the caller's ([kaniko-mode](kaniko-mode.md#ignore-list), `ADR-0038`).
  The walk asks about each entry with `IgnoreList::covers_walked`, in time
  linear in the tree's entries whatever their depth
  ([kaniko-mode](kaniko-mode.md#ignore-list); a 100,000-level chain in T0,
  with and without a list).
- The walk starts from an opened directory fd: the root, a path opened
  with `RESOLVE_NO_XDEV`, or the directory a caller just checked by
  `statx`, so the directory checked is the one emptied.
- An entry that vanished before its `statx` (`ENOENT`) is skipped; a
  directory that vanishes while being emptied is `Error::Changed`; any
  other error stops the deletion and leaves the rest in place.

## Observability

Coarse `tracing` spans, `rnk_rootfs.extract`, `.copy`, `.delete` and `.prune`,
with counters recorded once at the end, never per entry: the fields are listed
in [metrics](metrics.md#rnk-rootfs). Tested with `rnk_metrics::Capture`
(`crates/rnk-rootfs/tests/spans.rs`).

## Failure handling

- Every error carries the operation, the in-root path and the errno; none
  is dropped.
- `set_meta` stops at the first failing step; earlier steps stay applied.

## Open questions

None.

## References

- `openat2(2)`: <https://man7.org/linux/man-pages/man2/openat2.2.html>
- `statx(2)`: <https://man7.org/linux/man-pages/man2/statx.2.html>
- [kaniko mode](kaniko-mode.md#safe-deletion) (deletion),
  [overlay snapshots](overlay-snapshot.md#exporting-a-diff-as-an-oci-layer)
  (extraction into the overlay form),
  [sandbox](sandbox.md) (users).
- [exp-09](../research/experiments/09-kaniko-mode-prototype.md) (long
  paths, `mnt_id`).
