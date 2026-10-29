# Confined filesystem access

How `rnk-rootfs` reaches paths inside a build root, an extracted layer or a
store entry without ever leaving it, and how it writes metadata, extracts
layers, looks up users and deletes trees on top of that. Every other crate
reaches untrusted paths only through it.

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
  ([ADR-0028](../adr/0028-minimum-kernel-linux-5-8.md)).

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
  (`exp-09`). Callers that walk trees open each directory by
  `(parent fd, name)` instead.

## Metadata

`set_meta` pins the entry with an `O_PATH | O_NOFOLLOW` fd before changing
anything, then applies, in this order:

1. **Owner**, `fchownat(fd, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW)`.
   First, because `chown` clears setuid, setgid and `security.capability`.
2. **Xattrs**, before the mode, because setting `user.*` needs write
   permission that the final mode may remove.
3. **Mode**, the raw bits `& 0o7777` (setuid, setgid and sticky kept, no
   umask). Skipped for symlinks.
4. **Times**, `utimensat(fd, "", AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW)`, last,
   so no other step moves them.

`fchmod` and `fsetxattr` refuse `O_PATH` fds, and `rustix` has no
`fchmodat2`. So mode and xattrs go through ranikust's own
`/proc/self/fd/<n>`: `fchmodat` and `setxattr` follow that magic link to
exactly the pinned inode, as glibc's `fchmodat(AT_SYMLINK_NOFOLLOW)` does.
`/proc/self/fd` is checked to be procfs first. A symlink's xattrs are set
with `lsetxattr` on `/proc/self/fd/<parent>/<name>`, which never follows the
last name. Regular files being written use plain `fchown`, `fsetxattr`,
`fchmod` and `futimens` on their open fd.

## Extraction

`extract(dst, tar, whiteouts)` reads a plain tar stream; the caller chains
`rnk-layer`'s decoder in front and publishes the result
([overlay snapshots](overlay-snapshot.md#exporting-a-diff-as-an-oci-layer),
`ADR-0006`). The `tar` crate only parses (ustar, GNU long names and sparse
files, PAX records); its `unpack`, which joins names onto a host path, is
never called. A hand-written reader would repeat that parsing for no gain.

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
  resolved with `RESOLVE_NO_XDEV`, and an existing entry on another mount
  or at a mount root is never replaced, merged into or whited out; such
  entries fail with `Error::Entry`. Skipping them, as kaniko's ignore list
  does, waits for the ignore list in `extract` (Phase 2, kaniko mode).
- **Bombs.** GNU sparse entries are refused: the `tar` crate writes their
  holes as zeros, so a tiny entry could claim 2^62 bytes. GNU long names and
  PAX headers are read whole into memory by the `tar` crate, with no bound
  for now (Q-056, answered: revisit with fuzzing in Phase 6).
- **Metadata.** Numeric uid and gid from the header (PAX `uid`/`gid`
  included), raw mode bits, PAX `SCHILY.xattr.*`, PAX `mtime`/`atime` with
  nanoseconds (else the header's seconds; `atime` defaults to `mtime`), in
  the order of [Metadata](#metadata); a regular file gets them through its
  open fd. Directories are created `0700` and get their mode and times last,
  deepest first, so later entries neither move their mtime nor hit a
  read-only directory.
- **Hardlinks** link the target named in the archive with `linkat(…, 0)`,
  so a symlink target is linked itself. In the overlay form the target may
  be in a lower layer, which `layers/<id>` does not hold:
  `extract_overlay(dst, tar, lowers)` takes the lower layers' extracted
  directories, the nearest first as in `lowerdir=`, and copies the target
  from the first one that shows it (Q-055). "Shows" follows overlay's merged
  view: each layer, this one first, is walked by name without following a
  symlink; a whiteout, a non-directory (a symlink included) on the way, or
  a missing directory below an opaque one hides everything below; an
  opaque directory hides what its layer lacks. The copy is a regular file
  (or symlink, FIFO, device node) with the lower inode's owner, mode, times
  and every xattr as stored, which splits the hardlink as overlay's copy-up
  does (`ADR-0012`); later links in the layer to the same target link to
  that copy. A target in no layer, hidden, or a directory is an
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
  leading to them, at the point the marker appears in the stream. Writing
  into a live root, mount points included, is Phase 2
  ([kaniko mode](kaniko-mode.md)).
- Other `.wh..wh.*` names (AUFS bookkeeping) are skipped; global PAX
  headers are ignored; unknown entry types are refused. Device nodes need
  `CAP_MKNOD` (T2); foreign owners need root or a user namespace (T1).

## Copy

`copy(src, from, dst, to, opts)` copies one source of a `COPY` or local
`ADD` between two roots: the context and a stage root, or two stage roots
for `--from`. The builder expands wildcards and `.dockerignore` and derives
`to` from the Dockerfile's destination (Q-080); `ADD`'s archive extraction
is `extract`.

- **Source.** `from` is resolved in `src` following symlinks there, the
  last one included, as BuildKit's `FollowSymlinks` copy does [inferred];
  kaniko copies a symlinked source as a symlink
  (`kaniko@44aa7ddc0 pkg/commands/copy.go:150`) [open: Q-120]. A
  directory has its contents copied; anything else is copied to exactly
  `to`.
- **Reading.** The source tree is walked by `(directory fd, name)`: each
  entry is pinned with `O_PATH | O_NOFOLLOW` and `statx`ed through that fd.
  Only a pinned regular file or directory is reopened, through ranikust's
  own `/proc/self/fd`, for reading; a FIFO or device node is recreated with
  `mknodat`, never opened; a symlink is copied with `readlinkat` on the
  pinned fd; a socket is skipped. Names are sorted, so neither the copy nor
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
  (`--chmod`, octal, directories included, not symlinks; `None` keeps the
  raw bits), the source's mtime as mtime and atime, and
  `security.capability`, the only xattr kaniko copies
  (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1533`) [open: Q-120]. Hardlinks
  inside the copied tree stay hardlinks. Directories the copy creates
  above `to`, and `to` itself for a directory's contents, are `0755`,
  owned by `opts.owner` (`to`: else the source directory's owner, with its
  times), as kaniko's `MkdirAllWithPermissions` and
  `createParentDirectory` (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1287`,
  `:1562`); an existing `to` keeps its metadata. Directory modes and times
  are set last, deepest first.
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

- Iterative, one frame per directory level holding an `O_RDONLY` fd: no
  recursion, no `PATH_MAX` limit, one fd per level (a 3,004-level tree in
  T0). Names are read before any is removed.
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
  the caller's: [Q-035](../open-questions.md#q-035-what-does-the-kaniko-mode-ignore-list-hold).
- The walk starts from an opened directory fd: the root, a path opened
  with `RESOLVE_NO_XDEV`, or the directory a caller just checked by
  `statx`, so the directory checked is the one emptied.
- An entry that vanished before its `statx` (`ENOENT`) is skipped; a
  directory that vanishes while being emptied is `Error::Changed`; any
  other error stops the deletion and leaves the rest in place.

## Observability

Coarse `tracing` spans only, named `<crate>.<operation>`, with counters
recorded once at the end, never per entry
([metrics](metrics.md#rnk-rootfs)): `rnk_rootfs.extract` (`mode`,
`lowers`; `entries`, `files`, `dirs`, `symlinks`, `hardlinks`,
`hardlinks_copied`, `nodes`, `whiteouts`, `bytes_in` read from the tar
stream, `bytes_written`), `rnk_rootfs.copy` (`files`, `dirs`, `bytes`) and
`rnk_rootfs.delete` (`entries`, `files`, `dirs`, `kept`). Tested with
`rnk_metrics::Capture` (`crates/rnk-rootfs/tests/spans.rs`).

## Failure handling

- Every error carries the operation, the in-root path and the errno; none
  is dropped.
- `set_meta` stops at the first failing step; earlier steps stay applied.

## Open questions

- [Q-011](../open-questions.md#q-011-who-owns-the-workdir-checks-and-what-counts-as-empty)
  Who owns the workdir checks?
- [Q-120](../open-questions.md#q-120-where-kaniko-and-buildkit-copy-differently-which-does-copy-follow)
  Where kaniko and BuildKit copy differently, which does `copy` follow?
  What is the minimum kernel, and who checks it?
  How does overlay-form extraction handle a hardlink into a lower layer?
  How are tar extension headers bounded?

## References

- `openat2(2)`: <https://man7.org/linux/man-pages/man2/openat2.2.html>
- `statx(2)`: <https://man7.org/linux/man-pages/man2/statx.2.html>
- [kaniko mode](kaniko-mode.md#safe-deletion) (deletion),
  [overlay snapshots](overlay-snapshot.md#exporting-a-diff-as-an-oci-layer)
  (extraction into the overlay form),
  [sandbox](sandbox.md) (users).
- [exp-09](../research/experiments/09-kaniko-mode-prototype.md) (long
  paths, `mnt_id`).
