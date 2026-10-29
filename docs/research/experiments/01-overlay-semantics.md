# Experiment 01: Overlay semantics

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Do the overlayfs assumptions in survey chapter 06 (§1 mode selection, §3 overlay snapshots) hold at the privilege levels available inside rootless podman? |
| **Answer** | Mostly, with corrections: overlay mounts at every tested privilege level once upper and work sit on tmpfs or ext4, but in a user namespace `redirect_dir=off` fails with `EINVAL`, a default container cannot mount a fresh `proc` in a new user namespace, and the upper directory carries traps the exporter must handle (whiteouts sharing one inode, `nlink=4`; hardlinks broken by copy-up; leftover `user.overlay.*` xattrs). |
| **Related** | [ADR-0002](../../adr/0002-two-snapshot-modes.md), [ADR-0004](../../adr/0004-support-every-privilege-level.md), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md), [ADR-0016](../../adr/0016-overlay-mount-options.md); [overlay-snapshot](../../design/overlay-snapshot.md), [modes-and-detection](../../design/modes-and-detection.md); experiments [02](02-run-sandbox.md), [03](03-privilege-matrix.md), [06a](06a-root-rerun.md), [10](10-exdev-package-managers.md), [11](11-root-extras.md) |
| **Original** | `docs-rust/poc/01-overlay/` (Vietnamese, not in this repository) |

## Summary

- Overlayfs was exercised as root inside rootless podman's user namespace:
  six privilege settings, four upper locations, six mount-option
  configurations, up to 501 lower layers. Real root in the initial user
  namespace was not available here; [experiment 06a](06a-root-rerun.md)
  repeats these tests as real root.
- Overlay mounts with only `--cap-add SYS_ADMIN` and podman's default seccomp
  profile, and even in a default container through `unshare -Urm`, provided
  upper and work are on tmpfs or ext4. An upper on the container's own overlay
  rootfs fails with `EINVAL`.
- In a user namespace, kernel 7.0 turns on `userxattr` by itself and rejects
  `redirect_dir=off`. The set that works is
  `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off`.
- A default container can mount overlay but cannot mount a fresh `proc` in a
  new user namespace, because podman masks parts of `/proc`. Mode detection
  must try without a new user namespace first.
- The upper directory still has to be walked: whiteouts are hardlinks to one
  inode, copy-up breaks hardlinks (a content difference between the two
  modes, not just a digest difference), and `user.overlay.origin`, `impure`
  and `uuid` xattrs must be stripped.
- Legacy `mount(2)` truncates option data at one page and fails with `ENOENT`;
  `fsconfig("lowerdir")` takes at most 256 bytes; only `lowerdir+` scales, up
  to the hard limit of 500 lower layers.

## Questions

The experiment checked the assumptions in
[survey chapter 06](../survey/06-overlay-and-parallelism.md) §1 and §3
(section numbers refer to the original chapter):

1. **Privilege.** At which podman privilege levels, and with the upper
   directory where, can overlay and a fresh `proc` be mounted? (§1)
2. **Semantics.** What does the upper directory contain after each kind of
   file operation, and how do package managers cope with `EXDEV` when they
   rename a lower directory? (§3.3, §3.4)
3. **Options.** Which mount options are accepted, what are the kernel
   defaults, and can a layer tarball be unpacked into the overlay on-disk
   format (whiteouts, opaque directories) inside a user namespace? (§3.1,
   §3.3)
4. **Lowerdir limits.** How many lower layers, and how long an option string,
   do the mount APIs accept? (§3.4)
5. **Prior art.** How do containerd and BuildKit turn an upper directory into
   a tar layer? (§3.3)

## Environment

- Host: Ubuntu, Linux `7.0.0-34-generic`, x86_64, 32 cores. No sudo.
- Host sysctl `kernel.apparmor_restrict_unprivileged_userns=1`. On the host,
  `unshare -Ur` fails with
  `write failed /proc/self/uid_map: Operation not permitted`.
- Podman 5.7.0 rootless, runtime `crun`, storage driver `overlay` (the
  kernel's native overlay, not fuse-overlayfs). AppArmor is off for podman
  (`podman info`: `apparmor=false`; `/proc/self/attr/current` is
  `crun (unconfined)`). Podman's default seccomp profile unless a row says
  otherwise.
- Every test ran in a `--rm` container. "Root" means root of podman's user
  namespace (`uid_map: 0 1000 1; 1 100000 65536`), **not** real root in the
  initial user namespace.
- Images: `5d8d6618be1c` (golang image; Debian 13, util-linux 2.41, python3)
  and `e99a3c58e292` (verdaccio 6.1.6; Alpine, Node 22, npm 10).
- Upper locations: the container's rootfs (overlay), `--tmpfs` inside the
  container, a bind of host `/tmp` (tmpfs), a bind of host `/home` (ext4).
- No timings were taken, so CPU limits, cache state and machine load do not
  apply.

## Method

Seven scripts, each starting one or more throwaway podman containers. Short
names used below:

| Name | Question | What it does |
|---|---|---|
| privilege | 1 | For six podman privilege settings, mounts overlay with the upper on each of the four locations, once directly in podman's user namespace and once in a new user namespace from `unshare -Urm`, with and without `userxattr`. |
| proc | 1 | For five privilege settings, tries a fresh `proc` in a new PID namespace without a new user namespace, a fresh `proc` in a new user + PID namespace, and an `rbind` of the container's existing `/proc`. |
| semantics | 2, 3 | In `--privileged` rootless podman, applies a fixed set of file operations (Table 4) through a merged overlay and dumps the upper directory (mode, inode, nlink, size, blocks, rdev, xattrs) under configurations A–F. |
| options | 3 | Tries mount options alone and in sets; records success and the option string shown in mountinfo. Run in podman's user namespace and in a nested one, with identical results. |
| lowerdir | 4 | Mounts N lower layers (directories on tmpfs) whose paths are 12, 40 or 120 characters long, through legacy `mount(2)`, `fsconfig("lowerdir")` and `fsconfig("lowerdir+")`. |
| npm-pnpm | 2 | Simulates two `RUN` steps: RUN#1 installs `pkga@1.0.0` (lower = container rootfs); RUN#2 installs `pkga@2.0.0` (lower = RUN#1's upper + rootfs). Runs in a chroot with `/proc` and `/dev`, under strace. Needs network in the container. |
| unpack | 3 | The reverse direction (tar → layer directory): creates whiteouts with `mknod c 0 0` and sets opaque and other xattrs directly in an upper directory, then checks the merged view. Run in a default container with `unshare -Urm`, and in `--privileged`. |

Semantics configurations (upper and work on tmpfs except F):

| Config | Where the mount happens | Options |
|---|---|---|
| A | podman's user namespace | kernel defaults |
| B | podman's user namespace | `userxattr` |
| C | podman's user namespace | `metacopy=off,redirect_dir=nofollow,index=off,xino=off,uuid=off` |
| D | podman's user namespace | `userxattr` + C |
| E | new nested user namespace (`unshare -Urm`) | `userxattr` + C |
| F | podman's user namespace, upper/work on ext4 (host volume) | `userxattr` + C |

Question 5 was answered by reading BuildKit, containerd and continuity source
at pinned commits (**[code]**, see [References](#references)).

## Results

All results are **[measured]** unless labelled otherwise.

### Verdict per survey assumption

Table 1. Assumptions in survey chapter 06 and what the experiment found.

| § in 06 | Assumption | Verdict | Evidence |
|---|---|---|---|
| §1.a | Privileged, or `CAP_SYS_ADMIN` with seccomp allowing it → overlay can be mounted | **Holds** | `--cap-add SYS_ADMIN` with podman's **default** seccomp is enough; `seccomp=unconfined` is not needed (privilege) |
| §1.b | Upper and work must not sit on the container's overlay | **Holds** | `overlay: filesystem on /r/u not supported as upperdir`; `mount(2)` returns `EINVAL` (Table 2) |
| §1.c | No `CAP_SYS_ADMIN`, but a user namespace can be created → overlay with `userxattr` | **Holds for overlay, wrong for proc** | Default container: `unshare -Urm` + overlay mount **OK** on tmpfs and on a volume. Mounting a new `proc` **fails** with `EPERM` because the container's `/proc` is masked (Table 3) |
| §1.d | Unprivileged host user + user namespace | **Needs correction** | Ubuntu sets `kernel.apparmor_restrict_unprivileged_userns=1`: `unshare -Ur` → `write failed /proc/self/uid_map: Operation not permitted`. The binary needs an AppArmor profile |
| §1.d | Without `newuidmap`, only one uid can be mapped | **Holds** | `unshare -Urm`: `chown 1000:1000` → `Invalid argument`. Files of other uids show up as `65534` |
| §1 steps 1–3 | Try `unshare(NEWNS)`, then `NEWUSER\|NEWNS`, then mount overlay, then `proc` | **Needs reordering** | With `CAP_SYS_ADMIN`, a new `proc` mounts **only without a new user namespace** (`--cap-add SYS_ADMIN`: PID namespace OK, user + PID namespace FAIL) |
| §3.1 | Options `redirect_dir=off,metacopy=off,index=off,xino=off` | **Wrong in a user namespace** | `redirect_dir=off` → `EINVAL`. With `redirect_dir=nofollow` the whole set is accepted (options) |
| §3.1 | The upper is a full, self-contained diff | **Holds** | With `metacopy` off, `chmod`/`touch` on an 8 MB file puts all 8 MB in the upper (`blocks=16384`) |
| §3.1 | "No walk needed" | **Needs correction** | The **upper directory** must still be walked (only the diff) and compared with the lower, as BuildKit does, to drop redundant whiteouts and unchanged parent directories (prior art, below) |
| §3.3 | char device 0/0 → `.wh.<name>` | **Holds, misses a trap** | Whiteouts are **hardlinks of one inode** (`ino=38 nlink=4`). The tar writer must not treat them as hardlinks |
| §3.3 | Opaque: `trusted.overlay.opaque` or `user.overlay.opaque` | **Holds (user namespace)** | In a user namespace the kernel **turns on `userxattr` by itself**, even without the option; the xattr is `user.overlay.opaque=y`. `trusted.*`: not testable without real root |
| §3.3 | "Other xattrs: keep as is" | **Wrong** | The upper also has `user.overlay.origin`, `user.overlay.impure`, `user.overlay.uuid`. Every `*.overlay.*` xattr must be dropped on export |
| §3.3 | Hardlinks: keep as is | **Needs correction** | Copy-up **breaks hardlinks**: after `hl1` is modified, `hl2` keeps the old content |
| §3.3, reverse | `.wh.x` → `mknod` 0/0; `.wh..wh..opq` → set the opaque xattr | **Holds with `user.overlay.opaque`** | In a user namespace: `mknod c 0 0` OK, `user.overlay.opaque` OK and honoured by overlay. `trusted.overlay.opaque` → `EPERM` (unpack) |
| §3.4 | One page for the option string; at most 500 lower directories | **Holds, needs detail** | The 500 limit holds. Option strings longer than 4,095 bytes are **truncated** and fail with `ENOENT`. `fsconfig("lowerdir", string)` is limited to **256 bytes**. `lowerdir+` does not depend on length and stops at 500 (Table 6) |
| §3.4 | Copy-up puts files whose mtime or mode changed, or that were rewritten identically, into the layer | **Holds** | `f_same`, `f_touch_big`, `f_chmod_big` all appear in the upper |
| §3.4 | "Flattened content is the same in both modes" (06 §3.4, §6; [summary](../survey/00-summary.md) §2, item 6) | **Wrong with hardlinks** | In kaniko mode, writing `hl1` changes `hl2`; in overlay mode it does not. This is a **content** difference, not only a digest difference |
| §3.4 | `trusted.*` needs `CAP_SYS_ADMIN` in the initial user namespace → use `userxattr` | **Holds** | `setxattr trusted.overlay.opaque` → `EPERM`, even in rootless `--privileged` |
| (missing) | Renaming a lower directory | **Addition** | `rename(2)` → `EXDEV`. `mv` and npm 10 handle it; pnpm 9 never renames a lower directory. Results correct (npm-pnpm) |

### Question 1: privilege levels

Table 2. Overlay mount by podman privilege and upper location (privilege
test). Each cell is "mount directly in podman's user namespace / mount in a
new user namespace from `unshare -Urm`". With and without `userxattr` gave
the same result.

| Podman privilege | Upper on container rootfs (overlay) | `--tmpfs` in container | Bind of host `/tmp` (tmpfs) | Bind of host `/home` (ext4) |
|---|---|---|---|---|
| `--privileged` | FAIL / FAIL | OK / OK | OK / OK | OK / OK |
| `--cap-add SYS_ADMIN` | FAIL / FAIL | OK / OK | OK / OK | OK / OK |
| `--cap-add SYS_ADMIN --security-opt seccomp=unconfined` | FAIL / FAIL | OK / OK | OK / OK | OK / OK |
| as above + `apparmor=unconfined` | FAIL / FAIL | OK / OK | OK / OK | OK / OK |
| default | FAIL (`unshare: Operation not permitted`) / FAIL | FAIL / **OK** | FAIL / **OK** | FAIL / **OK** |
| default + `seccomp=unconfined` | FAIL / FAIL | FAIL / **OK** | FAIL / **OK** | FAIL / **OK** |

The error when the upper is on the container's rootfs:

```
mount: /r/m: fsconfig() failed: overlay: filesystem on /r/u not supported as upperdir.
mount(2) upper on rootfs overlay: -1 Invalid argument
```

The container's rootfs works as a **lower**: `lowerdir=/usr` with the upper on
tmpfs mounts, and `/usr/bin/python3` is visible. Overlay on overlay (stacking
depth 2) works.

Notes:

- Podman's default seccomp profile does **not** block `mount` when
  `CAP_SYS_ADMIN` is present, and does **not** block `unshare(CLONE_NEWUSER)`
  in a default container. Docker (no real Docker on the machine) and
  Kubernetes/containerd (no cluster) could not be tested; their default
  seccomp profiles may differ (see [experiment 03](03-privilege-matrix.md)).
- `apparmor=unconfined` has no effect here: podman does not enable AppArmor
  on this machine (`/proc/self/attr/current = crun (unconfined)`).
- The sysctl `apparmor_restrict_unprivileged_userns=1` blocks `unshare -Ur`
  on the host, yet **inside a podman container a nested user namespace can
  be created**. The reason is unclear. **[inferred]** AppArmor's
  `userns_create` hook applies only to unconfined tasks, and a task in the
  container is already inside a child user namespace.

Table 3. A fresh `proc` in a new PID namespace (proc test).

| Podman privilege | New PID ns, no new user ns | New user ns + PID ns | `rbind` of existing `/proc` |
|---|---|---|---|
| `--privileged` | OK | OK | OK |
| `--cap-add SYS_ADMIN` | OK | **FAIL `EPERM`** | OK |
| default | FAIL (cannot create mount ns) | **FAIL `EPERM`** | OK |
| default + `--security-opt unmask=ALL` | FAIL | **OK** | OK |
| `--cap-add SYS_ADMIN` + `unmask=ALL` | OK | OK | OK |

Cause: podman masks `/proc/acpi`, `/proc/sys`, `/proc/sysrq-trigger` and
others with overmounts. The kernel refuses a new `proc` mount in a child user
namespace while the existing `proc` is partly covered (the
`mount_too_revealing` check in `fs/namespace.c`, **[code]**, Linux source,
commit not recorded). Removing the masks (`unmask=ALL`) makes the mount work.

Consequences for the design:

- In a default container overlay works, but step 3 of survey 06 §1 (mount
  `proc`) fails. As §1 is written, the tool falls back to kaniko mode.
- Two workarounds: (a) ask the user for `--security-opt unmask=ALL`
  (Kubernetes: `securityContext.procMount: Unmasked`, which needs
  `hostUsers: false`) **[external]**, read but not tested on Kubernetes;
  (b) `rbind` the container's `/proc` into the rootfs. (b) works, but `RUN`
  then sees every process in the container: no PID isolation.
- With `CAP_SYS_ADMIN` available, **do not create a new user namespace**
  before mounting `proc`. The probe order of §1 must try without a user
  namespace first, then with one.

UID mapping in a self-made user namespace: `unshare -Urm` maps only one uid
(`0 0 1`). `chown 1000:1000 /tmp/f` → `Invalid argument`; `/etc/shadow` shows
group `65534`. `RUN` steps such as `apt-get` (which switches to user `_apt`)
or `chown` would break. `newuidmap`/`newgidmap` and `/etc/subuid` are needed,
as §1 already says. `newuidmap` inside the container was not tried (the image
lacks it, and it needs setuid).

### Question 2: the upper directory after file operations

The semantics test gave **the same result in all six configurations A–F**,
apart from inode numbers and `user.overlay.uuid` (present with defaults,
absent with `uuid=off`). The upper after configuration D, abridged:

```
drwxr-xr-x nlink=5 .            xattr={'user.overlay.impure': 'y'}
drwxr-xr-x d_recreate           xattr={'user.overlay.opaque': 'y'}   <- rm -rf, then mkdir again
drwxr-xr-x d_up2                xattr={'user.overlay.opaque': 'y'}   <- new dir created then renamed; absent from lower
c--------- ino=242 nlink=4 d_del     rdev=0/0                        <- 3 whiteouts share one inode
c--------- ino=242 nlink=4 d_mv_sh   rdev=0/0
c--------- ino=242 nlink=4 f_del     rdev=0/0
c--------- ino=254 nlink=1 f_mv      rdev=0/0                        <- whiteout left by renaming a file
-rwxr-x--- f_cap        xattr={'user.overlay.origin': '', 'security.capability': '01000002…'}
-rw------- f_chmod_big  size=8388608 blocks=16384 xattr={'user.overlay.origin': ''}
-rw-r--r-- f_touch_big  size=8388608 blocks=16384 xattr={'user.overlay.origin': ''}
-rw-r--r-- f_same       size=7      xattr={'user.overlay.origin': ''}   <- identical rewrite
-rw-r--r-- nlink=1 hl1  size=5                                          <- hl2 is not in the upper
```

Table 4. Operation and what lands in the upper.

| Operation | Result |
|---|---|
| Create a file | Regular file in the upper |
| Modify a lower file | Whole file copied up, plus `user.overlay.origin` |
| Delete a lower file or directory | Char device `0/0`, mode `c---------`. **Several whiteouts are hardlinks to one inode** (`nlink=4`) |
| Delete, then recreate a directory | Directory with `user.overlay.opaque=y`. After remount only the new file is visible (`['c']`) |
| Create a new directory, then rename it | Also marked opaque, though the lower has nothing to hide |
| Rename a lower file | New file + whiteout at the old name |
| `rename(2)` a lower directory | **`EXDEV`** |
| `mv` (coreutils) a lower directory | OK: on `EXDEV`, `mv` copies recursively, then deletes. The upper holds the whole subtree plus a whiteout at the old name |
| `rename(2)` a directory that exists only in the upper | OK |
| `chmod`, `touch` on an 8 MB file | **The whole 8 MB is copied up** |
| Rewrite identical content | The file still lands in the upper |
| Modify `hl1` (hardlinked to `hl2` in the lower) | **Hardlink broken**: `hl1` has nlink=1 in the upper; reading `hl2` still returns the old `'hl\n'` |
| `security.capability` | Preserved by copy-up. Writing it afresh on the merged view also works |

**Opaque xattr name.** Mounted in a user namespace (podman's or a self-made
one), **with or without** the `userxattr` option, mountinfo shows
`userxattr` and the xattrs are `user.overlay.*`. Kernel 7.0 enables
`userxattr` by itself when `CAP_SYS_ADMIN` in the initial user namespace is
missing. `trusted.overlay.opaque` under real root could not be tested here
(no sudo); per the kernel documentation it is `trusted.overlay.opaque`
**[external]**. [Experiment 06a](06a-root-rerun.md) later confirmed this as
real root. **[inferred]** Layers unpacked in root mode (`trusted.*`) and in
user-namespace mode (`user.*`) cannot be shared, so the tool should **always
mount with `userxattr`, even as real root**, so that `layers/` has a single
format. This needed checking as real root at the time.

**npm and pnpm when the package directory is in the lower** (npm-pnpm test):

```
KQ npm RUN#2 OK  require(pkga)=2.0.0
    renames returning EXDEV: 2
    rename("/app/node_modules/pkga", "/app/node_modules/.pkga-hFRKCpbR") = -1 EXDEV (Cross-device link)
    rename("/app/node_modules/pkga/lib", "/app/node_modules/.pkga-hFRKCpbR/lib") = -1 EXDEV (Cross-device link)
KQ pnpm RUN#2 OK  require(pkga)=2.0.0
    renames returning EXDEV: 0
```

- npm 10.9.2 hits `EXDEV`, falls back to copying, and the result is correct.
- pnpm 9.15.9 does not rename the lower directory: it creates a new
  `.pnpm/pkga@…2.0.0` directory and repoints the symlink.
- A trap found while testing: running pnpm **without chroot**, with the
  overlay mounted at different paths in the two runs, gives
  `ERR_PNPM_UNEXPECTED_STORE`, because pnpm records the store's absolute
  path. Chrooting into the rootfs, as the design does, avoids it.
- Not tried here: yarn, pip, cargo, `apt`/`dpkg` (**[inferred]** dpkg renames
  files, not directories). See [experiment 10](10-exdev-package-managers.md).

### Question 3: options and defaults

Table 5. Option acceptance in a user namespace (options test; podman's user
namespace and a nested one gave the same result).

| Option | Result | mountinfo |
|---|---|---|
| (none) | OK | `redirect_dir=nofollow,uuid=on,userxattr` |
| `metacopy=off`, `index=off`, `index=on`, `xino=off/auto/on`, `nfs_export=off` | OK | unchanged (`index=on` is silently ignored) |
| `redirect_dir=nofollow` | OK | |
| **`redirect_dir=off`** | **EINVAL** | |
| `redirect_dir=on`, `redirect_dir=follow`, `metacopy=on` | EINVAL | |
| `uuid=off` / `uuid=null` | OK | `uuid=off` / `uuid=null`; `user.overlay.uuid` no longer written |
| `volatile` | OK | `fsync=volatile` |
| `userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off,uuid=off` | **OK** | The set to use |

Kernel 7.0 Ubuntu defaults (from `/boot/config-7.0.0-34-generic` and
`/sys/module/overlay/parameters`): `CONFIG_OVERLAY_FS_REDIRECT_DIR` not set,
`REDIRECT_ALWAYS_FOLLOW=y`, `INDEX` not set, `METACOPY` not set,
`XINO_AUTO=y`. That is `redirect_dir=off` (follow), `index=off`,
`metacopy=off`, `xino=auto`, `uuid=on`. In a user namespace the kernel
forces `redirect_dir=nofollow` + `userxattr`.

The kernel logs no reason for the rejections in the `fsopen` log (it is
empty; only `dmesg` would have it, and `dmesg` is blocked). Per the kernel
documentation **[external]**, `redirect_dir=nofollow` means redirects are
neither created nor followed. That is exactly the semantics §3.1 needs, and
it is valid as root too.

Proposed correction to §3.1:
`userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off`.
Even with `uuid=off` the upper still carries `user.overlay.origin` and
`user.overlay.impure`, so the tar exporter must still filter.

**Reverse direction (unpack test).** Writing a layer directly into an upper
directory, in a default container with `unshare -Urm` and in
`--privileged`, gave identical results: `mknod c 0 0` OK;
`setxattr trusted.overlay.opaque=y` FAIL `EPERM`;
`setxattr user.overlay.opaque=y` OK; the merged view hides the whited-out
entry and shows only the new child of the opaque directory. A
`user.overlay.foo` xattr set on a content file is hidden: `listxattr` through
the merged view returns only `user.app`, and after copy-up the upper copy
has `user.overlay.origin` and `user.app`, so `user.overlay.foo` is lost.

### Question 4: lowerdir limits

Table 6. Lower-layer limits by mount API (lowerdir test). Each layer is a
directory on tmpfs; the column is the length of each layer path.

| API | Path length 12 | Path length 40 | Path length 120 |
|---|---|---|---|
| **legacy** `mount(2)`, one data string | ≤300 OK (3,635 bytes); 499+ **ENOENT** (6,023 bytes) | 100 OK (4,035 bytes); 300+ ENOENT | 2 OK; 100+ ENOENT |
| **fsconfig** `"lowerdir"`, one string | 2 OK; 100+ **EINVAL** (1,199 bytes) | same as left | same as left |
| **fsconfig** `"lowerdir+"`, one layer per call | ≤500 OK; 501 **EINVAL** | ≤500 OK; 501 EINVAL | ≤500 OK; 501 EINVAL |

- Hard limit: **500 layers**. The fs_context log says
  `overlay: too many lower directories, limit is 500`.
- Legacy: the data is cut at one page (4,096 bytes). The error is `ENOENT`
  (the truncated path does not exist), **not `EINVAL`**, which is easy to
  misdiagnose.
- `fsconfig(FSCONFIG_SET_STRING, "lowerdir", …)`: the value is at most
  **256 bytes** (`strndup_user(…, 256)` in `fs/fsopen.c`, **[code]**, Linux
  source, commit not recorded). Unusable for long strings.
- `lowerdir+` does not depend on path length. The short-named `l/` directory
  of survey 06 §2 is **needed only for the legacy path**.
- `mount(8)` from util-linux 2.41 (libmount, which uses the new mount API)
  fails with 450–600 layers of paths around 100 characters. ranikust should
  call `fsopen`/`fsconfig(lowerdir+)` itself rather than rely on `mount(8)`.
- Nested overlay: a lower on the container's overlay rootfs works (Table 2).
  The kernel caps stacking depth at 2 (`FILESYSTEM_MAX_STACK_DEPTH`)
  **[code]**. If the container running ranikust already sits on a nested
  overlay, this would break. Not testable: that setup could not be built.

### Question 5: how containerd and BuildKit export an upper directory

Source read at BuildKit `c19b15f57` (2026-10-28), containerd `43aa4ea13`
(2026-10-27) and continuity `555e512ce`. Everything in this subsection is
**[code]**.

**BuildKit has an overlay-specific differ that reads the upper directory
directly.**

- On by default when the snapshotter is `overlayfs` or `stargz`; off with
  `fuse-overlayfs` and `native`. On error it falls back to the walking
  differ (`buildkit@c19b15f57 cache/blobs.go:170-190`,
  `cache/blobs_linux.go:28-35`).
- Used only when the upper is exactly one layer above the lower and the
  mount options are all on a known list (`workdir`, `index=off`,
  `userxattr`, `redirect_dir=`). An unknown option sends it to the walking
  differ (`util/overlay/overlay_linux.go:27-110`).
- `WriteUpperdir` mounts the lower on a temporary directory, mounts an
  "upperdir view" (a read-only overlay with `lowerdir=upperdir:empty`, which
  hides whiteouts), then runs `filepath.Walk(upperdir)`
  (`overlay_linux.go:113-163`).
- `Changes`, for each entry in the upper (`overlay_linux.go:186-273`):
  - A directory with `trusted.overlay.redirect` is an error (`redirect_dir`
    is unsupported). Only `trusted.` is checked, not
    `user.overlay.redirect`.
  - Char device 0/0: skipped if the lower **does not** have that name,
    otherwise a Delete (`overlay_linux.go:277-299`).
  - Present in the lower → Modify, **skipped if `sameDirent`**: same
    mode/uid/gid/rdev, same `security.capability`, same size and mtime.
    Content is compared only when both mtimes have a zero nanosecond part
    (`overlay_linux.go:348-391`). In practice this filters **copied-up
    parent directories**, but **not** files rewritten identically or
    `touch`ed (their mtime changes).
  - Opaque directory (`trusted.overlay.opaque` **or**
    `user.overlay.opaque`): treated as a normal directory if the lower lacks
    it. Otherwise continuity's **walking differ** (`fs.Changes`) runs
    between the lower and the upperdir view for that subtree only
    (`overlay_linux.go:302-322`, `:255-270`). The result is **one
    `.wh.<name>` per hidden child**, not `.wh..wh..opq`.
- The tar is written by containerd's `archive.ChangeWriter`:
  - Delete → an empty regular file `.wh.<name>`, mtime 0
    (`containerd@43aa4ea13 pkg/archive/tar.go:557-580`). Whiteouts never
    reach the hardlink branch, so whiteouts sharing an inode cause no harm.
  - Hardlinks are tracked by inode. Of the xattrs, **only
    `security.capability` is written**; every other xattr is dropped,
    `user.overlay.*` included (`pkg/archive/tar.go:636-665`).

**containerd.**

- The default differ is the walking differ: mount lower and upper and walk
  both trees in parallel (`fs.Changes`); the upper directory is not used
  (`plugins/diff/walking/differ.go:151`, `pkg/archive/tar.go:79-112`).
- Unpacking writes the tar straight into the upper directory, turning
  `.wh..wh..opq` into `trusted.overlay.opaque=y` and `.wh.x` into
  `mknod 0/0`. **This path is skipped in a user namespace** (containerd issue
  #3762), and the tar is applied through the mounted overlay instead
  (`core/diff/apply/apply_linux.go:34-57`,
  `pkg/archive/tar_opts_linux.go:29-50`).
- When unpacking, containerd does not write `trusted.*` xattrs found in the
  tar; it returns `ENOTSUP` (`pkg/archive/tar_unix.go:147-149`).
- continuity has `DiffDirChanges`, which reads an overlay-style diff
  directory directly (`continuity@555e512ce fs/diff.go:151-170`), but
  containerd's walking differ does not use it.

## Findings

1. In a user namespace, kernel 7.0 **turns on `userxattr` by itself** and
   **rejects `redirect_dir=off`**, so the option set in survey 06 §3.1 makes
   the mount fail (Table 5).
2. Overlay whiteouts are **hardlinks to one inode** (`nlink=4`); a tar writer
   that detects hardlinks by inode would write them wrongly (Table 4).
3. The upper carries stray xattrs: `user.overlay.origin`,
   `user.overlay.impure`, `user.overlay.uuid` (listing above).
4. Copy-up **breaks hardlinks**, so the two modes differ **in content**, not
   only in digest (Table 1, Table 4).
5. A default container can run overlay through a user namespace but **cannot
   mount a fresh `proc`**, because `/proc` is masked; the §1 probe order must
   try without a new user namespace first (Table 3).
6. `mount(2)` data longer than one page is truncated and fails with
   **`ENOENT`**; `fsconfig("lowerdir")` is limited to **256 bytes**; only
   `lowerdir+` works for long lists (Table 6).
7. `user.overlay.*` xattrs that are part of the layer content are **hidden by
   overlay and lost on copy-up** (unpack test: `listxattr` through the merged
   view returns only `user.app`). They would need escaping as
   `user.overlay.overlay.*` (kernel ≥ 6.7 **[external]**). Rare in practice.
8. `index=on` and `xino=on` are accepted in a user namespace but have no
   effect and raise no error (Table 5).
9. `rename(2)` of a lower directory returns `EXDEV`; `mv` and npm 10 recover
   by copying and pnpm 9 never renames, so both package managers produce the
   correct result (npm-pnpm test).
10. An upper on the container's overlay rootfs is rejected at every privilege
    level, while tmpfs and ext4 uppers work (Table 2).

## Implications for ranikust

- Mount options: the corrected set
  `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off`
  is the basis of [ADR-0016](../../adr/0016-overlay-mount-options.md), which
  also adds `volatile` and accepts `EXDEV` (see experiments
  [06a](06a-root-rerun.md), [10](10-exdev-package-managers.md) and
  [11](11-root-extras.md)).
- Mode detection ([ADR-0002](../../adr/0002-two-snapshot-modes.md),
  [ADR-0004](../../adr/0004-support-every-privilege-level.md),
  [modes-and-detection](../../design/modes-and-detection.md)): probe without a
  new user namespace first; when a fresh `proc` cannot be mounted, either
  require `unmask=ALL` or `rbind` the existing `/proc` at the cost of PID
  isolation; on Ubuntu hosts, an unprivileged user needs an AppArmor profile.
- Hardlink divergence between modes is a content difference and belongs on
  the accepted-differences list of
  [ADR-0012](../../adr/0012-accept-divergence-between-modes.md).
- Mounting: call `fsopen`/`fsconfig("lowerdir+")` directly, not `mount(8)`,
  and cap a stage at 500 lower layers.
- Tar exporter ([overlay-snapshot](../../design/overlay-snapshot.md)),
  **[inferred]** from the results and the prior art:
  1. Walk the upper directory. For a char device 0/0, look the name up in the
     lower: skip it if absent, write `.wh.<name>` if present. Handle
     whiteouts **before** hardlink detection.
  2. For an opaque directory that also exists in the lower, either write
     `.wh..wh..opq` (compact, per the OCI spec) or do as BuildKit does
     (a whiteout per child). If the lower lacks it, drop the opaque flag.
  3. Drop `trusted.overlay.*` and `user.overlay.*` xattrs; keep
     `security.capability`. BuildKit drops all other `user.*` xattrs; keeping
     them is an open decision (the digest would then differ from BuildKit's).
  4. For entries present in the lower, compare metadata as `sameDirent` does,
     to drop unchanged parent directories. A content hash comparison, as
     survey 06 §3.4 suggests, could be added; BuildKit does not do this.
  5. Check `user.overlay.redirect` as well as `trusted.overlay.redirect`.

## Limitations and open questions

- **Real root in the initial user namespace** (the "outside a container, with
  root" path of survey 06 §1): no sudo was available, so `trusted.overlay.*`
  was never observed, whether `redirect_dir=off` is accepted was unknown, and
  `userxattr` as root was untested. All three were later covered by
  [experiment 06a](06a-root-rerun.md).
- **Docker, containerd/Kubernetes, gVisor, Kata**: not available on the
  machine. Their default seccomp and AppArmor profiles differ from podman's.
- **fuse-overlayfs** as the container's rootfs: podman here uses native
  overlay.
- **`newuidmap` inside the container**: the image lacks it, and it needs a
  setuid binary and `/etc/subuid` in the container.
- **Performance** (copy-up time, export time): not measured; out of scope.
  See [experiment 06](06-overlay-vs-walk.md).
- **pnpm through corepack**: corepack in Node 22.13 failed with a signing-key
  error (`Cannot find matching keyid`), so pnpm 9 was installed with
  `npm i -g`.
- The kernel's `fs/overlayfs/params.c` could not be read because GitHub
  rate-limited the requests. The limits in Questions 3–4 come from
  experiments and kernel error messages.
- Why a nested user namespace can be created in a podman container despite
  the host sysctl remains an inference (Question 1 notes).
- A ranikust container that itself sits on a nested overlay was not tested.

## Reproduction

The PoC lived in `docs-rust/poc/01-overlay/` (archived outside this
repository). Each script runs on the host as an ordinary user, starts
throwaway podman containers, and writes `out/<script>.txt`:

| Script | Test | Output |
|---|---|---|
| `01-quyen.sh` | privilege | `out/01-quyen.txt` |
| `02-proc.sh` | proc | `out/02-proc.txt` |
| `03-ngu-nghia.sh` | semantics | `out/03-ngu-nghia.txt` |
| `04-options.sh` | options | `out/04-options.txt` |
| `05-lowerdir.sh` | lowerdir | `out/05-lowerdir.txt` |
| `06-npm-pnpm.sh` | npm-pnpm (needs network in the container) | `out/06-npm-pnpm.txt` |
| `07-giai-nen-layer.sh` | unpack | `out/07-giai-nen-layer.txt` |

- Images are selected with the `IMG_ID` and `NODE_IMG` environment variables
  (defaults `5d8d6618be1c` and `e99a3c58e292`).
- Helpers run inside the containers: `probe-in.sh` (privilege probes),
  `ngu-nghia.py` (operations and upper dump), `lowerdir.py` (mount API
  calls), `07-giai-nen-layer.py` (unpack).
- `out/00-host-userns.txt` records `unshare -Ur` being blocked on the host.
  `out/06-npm-pnpm.v1-khong-chroot.txt` is the earlier pnpm run without
  chroot that produced `ERR_PNPM_UNEXPECTED_STORE`.
- The real-root re-run used the same scripts; see
  [experiment 06a](06a-root-rerun.md).

## References

1. [Survey chapter 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md), §1–§3, §6.
2. [Survey chapter 00: Summary](../survey/00-summary.md), §2, item 6.
3. [Experiment 06a: Root re-run](06a-root-rerun.md).
4. [Experiment 03: Privilege matrix](03-privilege-matrix.md).
5. [Experiment 10: EXDEV and package managers](10-exdev-package-managers.md).
6. Linux kernel documentation, overlayfs:
   <https://docs.kernel.org/filesystems/overlayfs.html> (accessed
   2026-10-29). **[external]**
7. Linux kernel source: `fs/namespace.c` (`mount_too_revealing`),
   `fs/fsopen.c` (`strndup_user(…, 256)`), `FILESYSTEM_MAX_STACK_DEPTH`;
   commit not recorded. **[code]**
8. BuildKit `c19b15f5775bd222f93b9741ade08fe77c3d3de7`:
   [cache/blobs.go#L170-L190](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/cache/blobs.go#L170-L190),
   [cache/blobs_linux.go#L28-L35](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/cache/blobs_linux.go#L28-L35),
   [util/overlay/overlay_linux.go#L27-L110](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/util/overlay/overlay_linux.go#L27-L110),
   [#L113-L163](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/util/overlay/overlay_linux.go#L113-L163),
   [#L186-L273](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/util/overlay/overlay_linux.go#L186-L273),
   [#L255-L270](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/util/overlay/overlay_linux.go#L255-L270),
   [#L277-L299](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/util/overlay/overlay_linux.go#L277-L299),
   [#L302-L322](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/util/overlay/overlay_linux.go#L302-L322),
   [#L348-L391](https://github.com/moby/buildkit/blob/c19b15f5775bd222f93b9741ade08fe77c3d3de7/util/overlay/overlay_linux.go#L348-L391).
   **[code]**
9. containerd `43aa4ea134f3e50791b8537c48a18303f0fcc775`:
   [pkg/archive/tar.go#L79-L112](https://github.com/containerd/containerd/blob/43aa4ea134f3e50791b8537c48a18303f0fcc775/pkg/archive/tar.go#L79-L112),
   [#L557-L580](https://github.com/containerd/containerd/blob/43aa4ea134f3e50791b8537c48a18303f0fcc775/pkg/archive/tar.go#L557-L580),
   [#L636-L665](https://github.com/containerd/containerd/blob/43aa4ea134f3e50791b8537c48a18303f0fcc775/pkg/archive/tar.go#L636-L665),
   [plugins/diff/walking/differ.go#L151](https://github.com/containerd/containerd/blob/43aa4ea134f3e50791b8537c48a18303f0fcc775/plugins/diff/walking/differ.go#L151),
   [core/diff/apply/apply_linux.go#L34-L57](https://github.com/containerd/containerd/blob/43aa4ea134f3e50791b8537c48a18303f0fcc775/core/diff/apply/apply_linux.go#L34-L57),
   [pkg/archive/tar_opts_linux.go#L29-L50](https://github.com/containerd/containerd/blob/43aa4ea134f3e50791b8537c48a18303f0fcc775/pkg/archive/tar_opts_linux.go#L29-L50),
   [pkg/archive/tar_unix.go#L147-L149](https://github.com/containerd/containerd/blob/43aa4ea134f3e50791b8537c48a18303f0fcc775/pkg/archive/tar_unix.go#L147-L149).
   **[code]**
10. containerd issue #3762 (unpacking into the upper directory in a user
    namespace): <https://github.com/containerd/containerd/issues/3762>.
    **[external]**
11. continuity `555e512ce5c436ac0a05cdd516eff4ae5995ed15`:
    [fs/diff.go#L151-L170](https://github.com/containerd/continuity/blob/555e512ce5c436ac0a05cdd516eff4ae5995ed15/fs/diff.go#L151-L170).
    **[code]**
