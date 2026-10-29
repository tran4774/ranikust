# Experiment 09: kaniko-mode prototype

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Can a Rust prototype of kaniko mode walk and diff a rootfs correctly, keep container mounts out of its layers, and delete a build root safely, all without `CAP_SYS_ADMIN`? |
| **Answer** | Yes. The walk + diff matched the real tree exactly, no mount leaked into a diff, and fd-based deletion kept every mount intact; comparing `st_dev` alone is not enough (it deletes into same-filesystem bind mounts). With a stat cache, walking 269k entries took 0.10–0.15 s. |
| **Related** | [ADR-0002](../../adr/0002-two-snapshot-modes.md), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md), [kaniko-mode design](../../design/kaniko-mode.md), [overlay snapshot design](../../design/overlay-snapshot.md), [survey 02](../survey/02-kaniko-mode-requirements.md), experiments [03](03-privilege-matrix.md), [06](06-overlay-vs-walk.md), [07](07-kaniko-vs-overlay.md) |
| **Original** | `docs-rust/poc/09-kaniko-mode/` (Vietnamese, not in this repository) |

## Summary

- A Rust prototype (`kmode`, about 1,400 lines) implemented kaniko mode's
  walk + diff, a mountinfo-based ignore list and safe deletion of a build
  root, and was tested inside a **default, unprivileged podman container**:
  the environment most CI pods land in ([experiment 03](03-privilege-matrix.md)).
  It checks [survey 02](../survey/02-kaniko-mode-requirements.md) §2–§4 and
  [survey 06](../survey/06-overlay-and-parallelism.md) §5.
- **Walk + diff is correct:** applying the diffs to the base reproduced the
  real tree exactly, mtimes included, and no path under a container mount
  leaked into a diff. Against overlay mode, the only differences were
  explained ones, plus a new finding: overlay changes `/` from mode 555 to 755
  unless the upperdir root copies the lower root's metadata **[measured]**.
- **Safe deletion:** fd-based recursive deletion removed everything except
  mount points and their parents, past `PATH_MAX`, without following
  symlinks. Comparing `st_dev` alone **deleted the contents of a
  same-filesystem bind mount**; `stx_mnt_id` or `STATX_ATTR_MOUNT_ROOT` is
  required **[measured]**.
- **Host checks:** refusing system directories needs (dev, ino) comparison as
  well as strings (`//` slipped through a string-only check); marker file and
  `flock` checks behaved as intended **[measured]**.
- **Cost:** with a stat cache, walking a 269k-entry node rootfs took
  0.10–0.15 s; 8 workers were faster than 32. The single-threaded map
  comparison (0.16–0.18 s) was slower than the cached walk itself and should
  move into the walk **[measured]**.

## Questions

1. Does a parallel walk + kaniko-style diff reproduce the real tree after
   each `RUN`, and how does it compare with overlay mode's diff?
2. Does a mountinfo-based ignore list keep container mounts (volumes, tmpfs,
   `/etc/hosts`, `/proc`, `/dev`, `/sys`) out of the diff?
3. Can the build root be emptied safely: without crossing mounts, without
   following symlinks, beyond `PATH_MAX`?
4. Which checks are needed before treating a host directory as a workdir?
5. What does the walk cost on a realistic rootfs (node + W1 dependencies), and
   how does it scale with workers?

## Environment

- **Date:** 2026-10-29.
- **Host:** kernel 7.0.0-34, 32 CPUs, NVMe, podman 5.7 rootless.
- **Privilege:** every test ran in a **default podman container (not
  privileged)** as the container's root: `CapEff=0x800405fb` (no
  `CAP_SYS_ADMIN`), `Seccomp=2`. Only the overlay-mode reference ran with
  `--privileged`.
- **A.1 container:** `debian:latest` with `-v` mounts (bin, ops, out, data,
  `/opt/vol`, scripts) and `--tmpfs` (`/scratch`, `/ovl`). The ignore list read
  from mountinfo had **40 entries**: `/proc` and 13 submounts; `/dev` and 9
  submounts; `/sys` and 3 submounts; `/etc/{hosts,hostname,resolv.conf}`,
  `/run/.containerenv`; and the volumes and tmpfs mounts.
- **A.3 container:** `node:22-bookworm-slim`, default, not privileged, after
  `pnpm install --frozen-lockfile` of `nextjs-kaniko/app` (W1) from the public
  npmjs registry (13 s, `taskset` 4 CPUs; **not** through `nk-verdaccio`).
  `/` was podman's overlay on ext4, about **269k entries** (156k in `/app`,
  100k in the pnpm store). The store is hardlinked into `node_modules`, so the
  data hashed totals **2,548 MB**. Page cache warm (a rootless user cannot
  `drop_caches`). CPU busy 1–3 % before each measurement; load average 1.09 at
  the start (`out/a3-bench.txt`).
- **Toolchain:** Rust with the crates `libc`, `rayon`, `xxhash-rust` and
  `mimalloc`; static musl binary of 1.0 MB, built in `rust:1-alpine`.

## Method

### Language

The prototype is in **Rust**: fast enough to write, and reusable for phase 0.
Every syscall the design needs is directly callable: `statx` (for
`stx_mnt_id` and `STATX_ATTR_MOUNT_ROOT`), `openat2`
(`RESOLVE_BENEATH|NO_SYMLINKS|NO_XDEV`), `unlinkat`, `flock`.

### How `kmode` works

- **Ignore list:** read `/proc/self/mountinfo`, take every mount point
  **below** the root (not the root itself), and add `--ignore` entries. An
  entry is skipped when its path **equals** a list entry; the walk never
  descends into it, so no prefix matching is needed.
- **Parallel walk:** `rayon::scope`, one task per directory. Directories are
  opened with `openat2(root_fd, relative path,
  RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS|RESOLVE_NO_XDEV)`.
  Every entry gets `statx(dirfd, name, AT_SYMLINK_NOFOLLOW|AT_NO_AUTOMOUNT)`.
  An entry whose `stx_mnt_id` differs from the root's is skipped and counted
  as `skip_mnt` (a second line of defence after mountinfo).
- **Hash:** xxh3 of the file content, read through the fd, plus a hash of the
  xattrs (excluding `*.overlay.*`).
- **Stat cache:** key {dev, ino, size, mtime, ctime, mode, uid, gid}, as in
  kaniko optimization notes, docs/12 (external, not in this repository). A
  cached hash is reused only when `max(mtime, ctime) < scan start − 2 s` (the
  racy window).
- **kaniko-style diff:** an entry is added or modified when any of {kind,
  mode, uid, gid, size, mtime, rdev, symlink target, content hash, xattr hash}
  changes. Whiteouts are recorded only at the highest level, i.e. while the
  parent is still a directory.
- **Deletion** (`delete`): recursive and fd-based.
  `openat(O_DIRECTORY|O_NOFOLLOW)`, then compare `ino`/`mnt_id` between `statx` and the opened fd to defeat
  swaps. Removal with `unlinkat`. An entry is kept when it is in the ignore
  list, when its `mnt_id` differs from the root's, or when it has
  `STATX_ATTR_MOUNT_ROOT`. Symlinks are only unlinked, never followed.
- **Host checks** (`--host`):
  1. `canonicalize`.
  2. Refuse `/`, and refuse a workdir that equals or is an ancestor of any of
     30 system directories. Compare **both the string and (dev, ino)** of the
     directory itself and of every ancestor.
  3. Open the workdir with `openat2(RESOLVE_NO_SYMLINKS)`, then
     `flock(LOCK_EX|LOCK_NB)`.
  4. A non-empty workdir without `.ranikust-root` as a regular file is
     refused. An empty workdir gets the marker.

### Tests

- **A.1, walk + diff against overlay mode.** Image `debian:latest`, two `RUN`
  scripts (`ops/step1.sh`, `ops/step2.sh`). Together they: create files and
  directories; modify, delete and rename files and directories from the image
  and from `RUN#1`; `chmod` (including setuid), `chown`, `touch -d`, rewrite
  with identical content; hardlinks within a layer, in-place writes to a
  hardlink, a hardlink to an image file; absolute, relative and directory
  symlinks and a changed target; a directory replaced by a file; delete and
  recreate a directory; a fifo. They also **write into the container's
  mounts**: `/etc/hosts`, volume `/data`, nested volume `/opt/vol`,
  `--tmpfs /scratch`, `/dev/shm`. Correctness check: `apply(base, diff)`
  compared with the real tree (`kmode check`), and the kaniko-mode tree
  compared with the overlay-mode tree (`upper-diff` turns an upperdir into a
  changeset, like BuildKit's `WriteUpperdir`).
- **A.2, safe deletion.** Cases D1–D3 below.
- **A.3, walk cost.** `kmode bench` on the node rootfs with 4, 8 and 32
  workers pinned with `taskset`, 2–3 rounds each; and `kmode steps`, a
  simulated build: `RUN#1` `touch`es 1 file and deletes 1 package, `RUN#2`
  creates 1,000 files.

## Results

All results are **[measured]** (output in `out/`) unless labelled otherwise.

### A.1: walk + diff against overlay mode

| Check | Result |
|---|---|
| Kaniko-mode diff contains paths under a mount (`/data`, `/opt/vol`, `/etc/hosts`, `/dev`, `/proc`, …) | **Correct: 0 lines** in both steps, no `leak*` file. `skip_mnt=0`: mountinfo caught everything and the `mnt_id` layer was never needed |
| `apply(base, diff) == real tree` (file mtimes included), kaniko mode, after RUN#1 and RUN#2 | **Correct**, 0 differences |
| Same, overlay mode (`upper-diff`: whiteout, opaque, `sameDirent`) | **Correct**, 0 differences |
| Tree after RUN#2, kaniko against overlay (mtimes and ignore-list paths excluded) | **3 differences, all explained** (next table) |
| Paths in the diff | RUN#1: 16/17 shared. RUN#2: 27/28 adds shared, 6/6 whiteouts shared, overlay has 1 extra opaque |
| Time (rootfs of 5.4k entries, 4 workers) | base walk 0.013 s; walk after each step 0.004 s (4,406 cache hits) |

Differences between the two modes:

| Path | Difference | Explanation |
|---|---|---|
| `/` | mode 555 (kaniko, as in the image) / 755 (overlay) | **New finding.** The upperdir's root is created by `mkdir` with mode 755. Overlay takes the root directory's metadata from the upper, so `/` of the merged rootfs becomes 755 and the RUN#1 diff gains an entry for `/`. **Overlay mode must copy mode/uid/gid/mtime/xattrs of the top lower layer's root into the upperdir before mounting** |
| `/opt/hl2` | content | Known ([survey 06](../survey/06-overlay-and-parallelism.md) §3.4): an in-place write to `hl1` changes `hl2` too in kaniko mode, while overlay breaks the hardlink |
| `/etc/passwd`, only in the overlay diff | `ln /etc/passwd /opt/hl_passwd` makes overlay copy up `passwd` | kaniko does not put `passwd` into the layer (content unchanged; only `nlink`/`ctime` changed). The kaniko-mode layer therefore contains `hl_passwd` with its **hardlink target in a lower layer**, so the tar writer must write it as a regular file **[inferred]** |
| `/var/lib/apt/lists` | overlay records an opaque directory, kaniko records nothing extra | The directory is empty in the image, so both give the same result |
| `/run/mount` | only in the `--privileged` container | Caused by the environment, not by the snapshot mode |

### A.2: safe deletion

| Case | Setup | Result |
|---|---|---|
| D1: workdir = `/` in a default container | Canaries in volume `/data` and `--tmpfs /scratch`. Absolute symlinks to `/data`, `/k`, `/scratch`; relative symlink `../../../../..`; `/proc/self/root`. Directory with mode 000, a fifo. A tree 3,004 levels deep with a 6,021-byte path (> `PATH_MAX`) | **Correct.** Deleted 4,714 files and 3,742 directories in 0.094 s, 0 errors. Only mount points remained (`/proc /dev /sys /data /scratch /k`, `/etc/{hosts,hostname,resolv.conf}`, `/run/.containerenv`) and their parents (`/etc`, `/run`). Canaries in the volume and the tmpfs intact |
| D2: workdir `/w` is a bind mount of a host directory. It contains `/w/inner` (a bind mount on the **same filesystem**, same `st_dev=50`) and `/w/tmpin` (tmpfs) | Symlinks to `/outside`, `/`, `../outside`, `../../../../outside`, `/w/inner`; a hardlink | **Correct.** Deleted 8 files and 3 directories. Kept the marker, `/w/inner` and `/w/tmpin`. Canaries in `inner`, `outside` and the tmpfs intact |
| D2b: as D2, but comparing **only `st_dev`** (`--stdev-only --no-mountinfo`) | | **Wrong, as predicted: deleted the whole content of the same-filesystem bind mount** (`inner-src` on the host left empty). `rmdir /w/inner` → `EBUSY`. The tmpfs survived because its `st_dev` differs |
| D2c: no mountinfo, only `stx_mnt_id` + `STATX_ATTR_MOUNT_ROOT` | | **Correct.** Kept both `/w/inner` and `/w/tmpin` |
| D3: host checks | `/`, `//`, `/usr`, `/usr/`, `/etc`, `/home`, `/var`, `/var/lib`, `/usr/../`, `/opt`, `/tmp`, `/root`, `/usr/local`, symlink `/lnk_usr → /usr` | **Correct: all 14 refused.** `//` was caught only by the (dev, ino) comparison; the first version compared strings only and let `//` through (fixed) |
| D3 | non-empty directory without a marker | refused; the user's files intact |
| D3 | empty directory → marker created; next run with the marker → contents deleted, marker kept | OK |
| D3 | marker is a symlink to `/etc/passwd` | refused |
| D3 | another process holds the `flock` | refused (`EWOULDBLOCK`); runs once the lock is released |

Consequences for [survey 02](../survey/02-kaniko-mode-requirements.md) §4:

- **The table in §4 that says "compare `st_dev`" is not sufficient.** A bind
  mount on the same filesystem has the same `st_dev`, so a
  `DeleteFilesystem` built that way deletes into the bind mount (D2b). Use
  `stx_mnt_id` (kernel ≥ 5.8) or `STATX_ATTR_MOUNT_ROOT`, plus mountinfo.
  (kaniko's own function: `kaniko@44aa7ddc0 pkg/util/fs_util.go:292`
  **[code]**.)
- Comparing path strings is not enough to refuse system directories; (dev,
  ino) is needed, which would also catch a bind mount of `/` placed elsewhere
  **[inferred]** (a bind of `/` could not be set up to test it).
- A workdir that contains only mount points (for example a pre-mounted cache)
  and no marker is refused (D2). Proposed: treat mount points as "empty" when
  checking for the marker **[inferred]**.
- fd-based deletion does not depend on `PATH_MAX` (D1). The number of open fds
  equals the tree depth; the tool raises `RLIMIT_NOFILE` to the hard limit at
  start-up.

### A.3: walk cost on a node rootfs

Seconds; ranges over the rounds shown.

| Workers (`taskset`) | Cold: hash every file | With stat cache (nothing changed) | No cache | Stat only, no hash |
|---|---|---|---|---|
| 4 (CPUs 0–3), 3 rounds | 0.298–0.303 | **0.152–0.154** | 0.288–0.298 | 0.140–0.141 |
| 8 (CPUs 0–7), 3 rounds | 0.172–0.198 | **0.101–0.107** | 0.173–0.182 | 0.097–0.103 |
| 32 (CPUs 0–31), 2 rounds | 0.267–0.286 | 0.109–0.113 | 0.255–0.256 | 0.098–0.099 |
| 4, node base only (9.2k entries, 262 MB) | 0.013–0.018 | 0.004 | 0.014 | 0.003–0.004 |

Simulated build (`kmode steps`): RUN#1 `touch`es 1 file and deletes 1
package; RUN#2 creates 1,000 files. The diff was correct: the package's
whiteout, `package.json`, the parent directories, `gen-*/f1..f1000`.

| Workers | Base walk (cold) | Walk RUN#1 / RUN#2 | Map comparison into a diff (1 thread) |
|---|---|---|---|
| 4 | 0.286 | 0.148 / 0.153 | 0.168 / 0.167 |
| 8 | 0.177 | 0.107 / 0.102 | 0.164 / 0.160 |

Observations:

- The stat cache brings the walk down to **0.10–0.15 s** for 269k entries.
  What remains is close to "stat only", i.e. the cost of `getdents` + `statx`
  on overlay.
- Hashing 2.5 GB cold adds only 0.07–0.15 s, because the page cache is warm
  and xxh3 is fast. With a cold cache, disk I/O would dominate **[inferred]**
  (not measured).
- **8 workers were fastest; 32 workers were slower than 8** (cold 0.27 against
  0.17 s; cached 0.11 against 0.10 s). The worker count should not default to
  the core count; capping it at about 8 is suggested **[inferred]** (needs
  measuring on other machines).
- **The single-threaded map comparison (0.16–0.18 s) is slower than the
  cached walk itself.** Proposed: compare during the walk. The walker already
  looks up the old entry to use the cache; it only has to mark it "seen", and
  the deleted entries are the old ones left unmarked **[inferred]**.
- The racy window works but has a cost. Deleting a pnpm package changes the
  `ctime` of the hardlinked store files (`nlink` drops), so 430–6,021 files
  were hashed again. In RUN#2 they were still inside the 2 s window and were
  hashed once more (1,430 and 6,592 files).
- `maxrss` was 0.8–1.0 GB while `bench` held 4 maps of 269k entries (about
  200 MB per map: two `String`s per entry plus the `HashMap`). The real
  implementation needs a more compact structure, for example a path arena
  **[inferred]**.
- Compared with experiment 06's Go walk (0.35–1.0 s per step, 32 workers,
  ext4, about 255k entries), the Rust version is clearly faster. **The data
  and conditions differ, so this is only indicative.** A likely reason is that
  the Go version lists paths sequentially before hashing in parallel, while
  this one walks directories in parallel and calls `statx` per dirfd
  **[inferred]**.

## Findings

1. **Overlay mode: the upperdir root must carry the lower root's metadata.**
   Otherwise the image has `/` with mode 755 instead of 555 and the first
   layer gains an entry for `/` **[measured]** (A.1). This was missing from
   [survey 06](../survey/06-overlay-and-parallelism.md) §3.1.
2. **Build-root deletion must use `mnt_id`/`STATX_ATTR_MOUNT_ROOT`, not
   `st_dev`** **[measured]** (A.2, D2b). This corrects
   [survey 02](../survey/02-kaniko-mode-requirements.md) §4.
3. **The walk must not open directories by long paths.** On the 3,004-level
   tree, `openat2` returned `ENAMETOOLONG` beyond 4,096 bytes; the walk
   reported 1 error and skipped everything below, so the manifest had only
   2,044 entries under `/deep` for a tree of 3,004 directories plus 1 file
   **[measured]**. Proposed: open by `(parent fd, name)`, holding parent fds
   in an `Arc` with a cap on open fds, or reopen from the nearest ancestor when
   the path is too long **[inferred]**. Deletion is already fd-based and was
   not affected (D1). Go's `os.Lstat` is also limited by `PATH_MAX`, so kaniko
   probably fails in this case too **[inferred]** (not checked).
4. **A state directory inside the rootfs leaks into the diff.** The first A.3
   run wrote its manifests to `/tmp/steps-*` inside the rootfs, and those files
   appeared in the diff; adding `--ignore` fixed it **[measured]**. This
   confirms [survey 02](../survey/02-kaniko-mode-requirements.md) §2: the state
   directory must be outside the rootfs, or in the ignore list like
   `/kaniko`.
5. **Hardlinks to lower-layer files** (`ln /etc/passwd …`): a kaniko-mode
   layer contains a link whose target is not in the layer, and the tar writer
   must handle it **[inferred]**; to be checked when export is implemented.
6. Mountinfo in default podman is enough to keep every mount out
   (`skip_mnt=0`) **[measured]**. The `mnt_id` check is still worth keeping as
   a second line of defence: it is cheap (one field of the `statx` that runs
   anyway), and D2c showed it is sufficient on its own **[measured]**.
7. With a stat cache, the walk of a 269k-entry rootfs costs 0.10–0.15 s, and 8
   workers beat 32 **[measured]** (A.3).

## Implications for ranikust

- Kaniko mode walks with `openat2`/`statx` relative to directory fds, ignores
  every mount from mountinfo and double-checks `stx_mnt_id`, and keeps its
  state directory outside the rootfs or in the ignore list
  ([kaniko-mode design](../../design/kaniko-mode.md)).
- Emptying a build root is fd-based and never crosses a mount: `mnt_id`/
  `STATX_ATTR_MOUNT_ROOT`, not `st_dev`; host workdirs need the (dev, ino)
  system-directory check, the `.ranikust-root` marker and `flock`
  ([kaniko-mode design](../../design/kaniko-mode.md)).
- Overlay mode copies the lower root's metadata onto the upperdir root before
  mounting ([overlay snapshot design](../../design/overlay-snapshot.md)).
- Both modes are kept ([ADR-0002](../../adr/0002-two-snapshot-modes.md));
  hardlink-related differences between them (`/opt/hl2`, `/etc/passwd`) are
  of the kind [ADR-0012](../../adr/0012-accept-divergence-between-modes.md)
  accepts.
- Proposals still to be designed and measured: comparing during the walk
  instead of a separate map pass, a compact path structure, and a worker cap
  of about 8.

## Limitations and open questions

- Docker defaults and real Kubernetes pods were not tested: Docker's seccomp
  profile could block `openat2` or `statx` on older versions. Docker ≥ 20.10
  allows both **[external]** (no source cited in the original; not verified
  here).
- Cold page cache (after `drop_caches`) and slow disks: a rootless user cannot
  drop the page cache.
- `delete` performance on a large rootfs: measured only on a debian rootfs
  plus the deep tree, about 8.5k entries in total (0.094 s).
- A real race, where another process swaps a directory for a symlink during
  deletion: the code checks `ino`/`mnt_id` after `openat(O_NOFOLLOW)`, but the
  race was not reproduced.
- A bind mount of `/` inside the workdir: could not be set up in a default
  container.

## Reproduction

The PoC sources are archived outside this repository, under
`docs-rust/poc/09-kaniko-mode/`:

| File | Role |
|---|---|
| `src/main.rs` | The `kmode` binary. Subcommands: `ignore`, `walk`, `steps` (run `RUN` commands, then walk + diff as kaniko does), `upper-diff` (turn an overlay upperdir into a changeset, like BuildKit's `WriteUpperdir`), `check` (apply the diff to the base and compare with the real tree), `cmp`, `cmpdiff`, `delete`, `hold-lock`, `tree`, `bench` |
| `build.sh` | Builds `bin/kmode` (static musl) in `rust:1-alpine`, caching crates in the volume `rk-cargo` |
| `ops/step1.sh`, `ops/step2.sh` | The two `RUN` scripts of A.1 |
| `scripts/a1-diff.sh`, `scripts/overlay-ref.sh` | A.1: kaniko mode against overlay mode → `out/a1-diff.txt`, `out/a1/` |
| `scripts/a2-delete.sh` | A.2: safe deletion → `out/a2-delete.txt` |
| `scripts/a3-prep.sh`, `scripts/a3-bench.sh` | A.3: build the node rootfs + `pnpm install`, measure the walk → `out/a3-bench.txt` |

```sh
./build.sh && ./scripts/a1-diff.sh && ./scripts/a2-delete.sh && ./scripts/a3-prep.sh && ./scripts/a3-bench.sh
podman rm -f rk9-nodebench
```

## References

1. [Survey 02: kaniko-mode requirements](../survey/02-kaniko-mode-requirements.md)
   §2–§4.
2. [Survey 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md)
   §3.1, §3.4, §5.
3. [Experiment 03: Privilege matrix](03-privilege-matrix.md).
4. [Experiment 06: Overlay vs walk](06-overlay-vs-walk.md).
5. Kaniko optimization notes, docs/12 (stat cache key) (external, not in this
   repository).
6. `kaniko@44aa7ddc0 pkg/util/fs_util.go:292` (`DeleteFilesystem`).
