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
  `Error::Unsupported` ([Q-050](../open-questions.md#q-050-what-is-the-minimum-kernel-and-who-checks-it)).

## Resolution

- **Paths** may be absolute or relative; both are taken relative to the
  root. `""`, `"/"` and `"."` name the root itself. Only a path ending in a
  plain name can be created, linked, renamed or removed.
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

## Users and groups

`/etc/passwd` and `/etc/group` are read through `RootPath` (so a symlinked
`/etc/passwd` resolves inside the root), up to 16 MiB each; a missing file is
an empty database. Empty lines, `#` comments, NIS `+`/`-` lines and lines
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

## Failure handling

- Every error carries the operation, the in-root path and the errno; none
  is dropped.
- `set_meta` stops at the first failing step; earlier steps stay applied.

## Open questions

- [Q-011](../open-questions.md#q-011-who-owns-the-workdir-checks-and-what-counts-as-empty)
  Who owns the workdir checks?
- [Q-050](../open-questions.md#q-050-what-is-the-minimum-kernel-and-who-checks-it)
  What is the minimum kernel, and who checks it?

## References

- `openat2(2)`: <https://man7.org/linux/man-pages/man2/openat2.2.html>
- `statx(2)`: <https://man7.org/linux/man-pages/man2/statx.2.html>
- [kaniko mode](kaniko-mode.md#safe-deletion) (deletion),
  [overlay snapshots](overlay-snapshot.md#exporting-a-diff-as-an-oci-layer)
  (extraction into the overlay form),
  [sandbox](sandbox.md) (users).
- [exp-09](../research/experiments/09-kaniko-mode-prototype.md) (long
  paths, `mnt_id`).
