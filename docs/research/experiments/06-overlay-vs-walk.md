# Experiment 06: Overlay vs walk

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | How much cheaper is taking a layer diff from an overlay upperdir than from an optimized kaniko-style full walk, and what does overlay cost elsewhere? |
| **Answer** | Diffing five steps took 0.51–0.58 s with overlay against 2.3–3.7 s with the walk (4–6× faster). After slower commands are counted, the build saved 0.7–2.1 s, and only with `volatile`: without it one copy-up step took 12.9 s instead of 0.37 s. |
| **Related** | [ADR-0002](../../adr/0002-two-snapshot-modes.md), [ADR-0016](../../adr/0016-overlay-mount-options.md), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md), [overlay snapshot design](../../design/overlay-snapshot.md), [kaniko-mode design](../../design/kaniko-mode.md), experiments [01](01-overlay-semantics.md), [06a](06a-root-rerun.md), [07](07-kaniko-vs-overlay.md), [09](09-kaniko-mode-prototype.md) |
| **Original** | `docs-rust/poc/06-overlay-vs-walk/` (Vietnamese, not in this repository) |

## Summary

- One Go program ran the same five build steps (shaped like the `deps` and
  `final` stages of W1) in two modes: (A) one fresh overlay per step with the
  diff read from the upperdir, and (B) a simulation of kaniko's
  `snapshot.init` + `TakeSnapshotFS` with a parallel hash and a stat cache.
- Overlay diffing was 4–6× faster: 0.51–0.58 s against 2.3–3.7 s for the five
  steps. A small command on a large rootfs cost A 0.006–0.034 s and B
  0.35–1.0 s, because B walks about 255k paths every time **[measured]**.
- Overlay makes the commands themselves slower by 0.2–0.8 s over five steps
  (+7 % to +16 %). The net gain for the whole sequence was 0.7–2.1 s
  **[measured]**.
- Without `volatile`, overlay on ext4 fsyncs every copy-up: one step went from
  0.37 s to 12.9 s and overlay lost to the walk by a wide margin. `volatile`
  (or an upper on tmpfs) is required **[measured]**.
- Both modes produced the same file contents. They differ in parent directory
  entries, mtimes and hardlink structure (1,813 pnpm store files present only
  in the walk diff) **[measured]**.
- The hash function barely matters; parallel walking plus a stat cache is
  enough for kaniko mode. Compared with a well-written walk, overlay gains
  about 1–2 s on this workload, not the 5–10 s it gains over kaniko itself
  **[inferred]**, later measured in [experiment 07](07-kaniko-vs-overlay.md).

## Questions

1. How long does it take to obtain each step's diff by walking an overlay
   upperdir, compared with a kaniko-style full walk with a parallel hash and a
   stat cache?
2. How much slower do commands run on overlay, and what does `volatile`
   change?
3. What is the net effect on command time + diff time + `syncfs`/`umount`?
4. Do the two modes produce the same file sets and diff sizes?
5. How much do the hash function, the number of hash workers and the stat
   cache matter for the walk?
6. How much would lazy export save per intermediate stage?

## Environment

- **Date:** 2026-10-29, 02:50–03:26.
- **Host:** the survey machine (see the [research README](../README.md)):
  32 CPUs, NVMe, Linux 7.0, podman 5.7 rootless with native overlay storage
  (not fuse-overlayfs).
- **Privilege:** every run was a fresh `podman run --rm --privileged`, so the
  process was root inside a user namespace. Overlay was therefore the
  in-userns overlay with `userxattr`; the kernel rejects `redirect_dir=off`
  there and sets `redirect_dir=nofollow` itself.
- **Filesystems:** ext4 on NVMe `/home` (a podman volume) or the container's
  tmpfs. The upper and the rootfs were never on the container's own overlay.
- **Toolchain:** Go 1.26.9 (`golang` image `5d8d6618be1c`), static binary
  (`CGO_ENABLED=0`).
- **Inputs:** base image `node@sha256:bbf39cb3…`
  (`nextjs-kaniko/bench/base-image.txt`) from the local registry; npm packages
  from the `nextjs-kaniko` verdaccio with a warm cache.
- **Load:** the machine was shared and another benchmark ran intermittently.
  Before every run the harness waited until the machine was idle (CPU busy
  < 10 % over 3 s, three times in a row). It had to wait 46 times during the
  session. The 1-minute load average logged before each run ranged from 1.89
  to 10.99 (`results/run.log`).
- **Page cache:** warm in both modes; files had just been written.

## Method

`main.go` implements both modes in Go, so the comparison is between
mechanisms, not languages.

**Mode A, overlay** (`ov`; `ovv` adds the `volatile` option):

- Each step mounts a new overlay with a new `upperdir`. `lowerdir` is the
  previous uppers plus the base.
- Options: `userxattr,index=off,xino=off,metacopy=off`; the kernel adds
  `redirect_dir=nofollow` (see Environment).
- The command runs under `chroot` with `/proc`, `/dev`, `/etc/hosts` and
  `/etc/resolv.conf` bind-mounted, followed by `umount`.
- The diff is a walk of the upperdir: a 0/0 character device becomes a
  whiteout and the `user.overlay.opaque` xattr becomes an opaque directory.

**Mode B, walk** (`<hash>-<workers>[-nostat]`) simulates kaniko's
`snapshot.init` + `TakeSnapshotFS`
(`kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:116`, `:199`) **[code]**:

- The rootfs is a plain directory copied from the base (copy not timed).
- Before the first step, a baseline scan (`init`).
- After each command: `syncfs` (timed separately), a sequential walk to list
  paths, then `lstat` + hash on N goroutines, compared with the previous scan's
  map to find added, modified and deleted files.
- The hash follows kaniko's `util.Hasher`
  (`kaniko@44aa7ddc0 pkg/util/util.go:52`) **[code]**: mode, mtime, uid, gid,
  then file content or symlink target. Variants: `hwh` (HighwayHash, which
  kaniko uses), `xxh` (xxhash64), `sha256`.
- Stat cache as in kaniko optimization notes, docs/12 (external, not in this
  repository): key {dev, ino, size, mtime, ctime, mode, uid, gid}; files
  changed within the last 2 s (the racy window) are not cached. kaniko uses the
  same 2 s window (`kaniko@44aa7ddc0 pkg/util/stat_hash_cache.go:41`)
  **[code]**.

**Export**, measured in both modes on that mode's own diff: tar to
`/dev/null`, and tar + pgzip with 4 threads (1 MB blocks) to `/dev/null`.

**Steps.** Five steps run in sequence, like W1's `deps` + `final` stages
without `COPY --from`:

| Step | Command | Notes |
|---|---|---|
| `npm-i-pnpm` | `npm install -g pnpm@10.34.7` | small command on a small rootfs (~9k paths) |
| `pnpm-install` | copy `package.json`, `pnpm-lock.yaml`, `.npmrc` into `/app`, then `pnpm install --frozen-lockfile` | large command: 242k files; registry is the `nextjs-kaniko` verdaccio |
| `churn-del` | W1 Dockerfile `RUN` (a): delete `test/`, `docs/`, `*.md`, `*.map` | 9,912 whiteouts |
| `churn-edit` | `RUN` (b): `sed -i` on 2,000 `.js` files, `touch` every `.json` | modifies lower files, i.e. copy-up |
| `churn-gen` | `RUN` (c), shortened: create 5,000 small files | |

The three `churn-*` steps are small commands on a large rootfs (~255k paths):
the case kaniko optimization notes, docs/27 §3 row 1 (external, not in this
repository) calls "rescanning the whole filesystem when a command changes
little".

**Measurement sets** (directories `results/<set>/`):

| Set | Filesystem of upper / rootfs | CPU | pnpm |
|---|---|---|---|
| `main-ext4` | ext4 on `/home` (podman volume) | 32 | default: hardlinks from the store into `node_modules` |
| `main-tmpfs` | container tmpfs | 32 | default |
| `copy-ext4` | ext4 | 32 | `package-import-method=copy`: no hardlinks, ~2.5 GB, close to the docs/27 conditions |
| `cpus4-ext4` | ext4 | `--cpus=4`, as in the earlier benchmark | default |

**Rounds and timing.** Three rounds per configuration, configurations
interleaved within each round. Tables report medians; the (min–max) ranges are
in the [appendix](#appendix-full-tables).

**Correctness.** A separate round on ext4 recorded the file lists of A (`ov`)
and B (`hwh-4`) and compared them step by step (`results/cmp-*.txt`). Stat
cache misses were inspected with `OVPOC_DEBUG_MISS=1`.

## Results

All values are **[measured]** medians of 3 rounds unless labelled otherwise.

### Diff time, five steps in total (s)

Excludes `syncfs`, tar and compression. B includes `init`.

| Configuration | `main-ext4` | `main-tmpfs` | `copy-ext4` | `cpus4-ext4` |
|---|---|---|---|---|
| A `ov` | **0.58** (0.55–0.59) | **0.51** (0.50–0.54) | **0.56** (0.56–0.58) | – |
| A `ovv` (volatile) | **0.55** (0.54–0.68) | – | **0.56** (0.56–0.57) | **0.56** (0.55–0.57) |
| B `hwh-32` | 2.52 (2.49–2.54) | 2.51 (2.49–2.51) | 2.28 (2.26–2.41) | – |
| B `xxh-32` | 2.47 (2.45–2.57) | 2.53 (2.53–2.53) | – | – |
| B `sha256-32` | 2.65 (2.57–3.35) | 2.76 (2.68–2.91) | – | – |
| B `hwh-4` | 2.97 (2.86–2.99) | 2.94 (2.92–2.96) | 2.64 (2.64–2.80) | 3.09 (2.98–3.18) |
| B `xxh-4` | 2.80 (2.79–2.93) | – | – | – |
| B `sha256-4` | 3.55 (3.26–3.69) | – | – | – |
| B `hwh-32-nostat` | 3.68 (3.59–3.72) | – | – | – |

### Per step, `main-ext4` (s)

| Step | A `ov` | A `ovv` | B `hwh-4` | B `hwh-32` | B `xxh-32` |
|---|---|---|---|---|---|
| `init` (baseline scan of the base) | – | – | 0.041 | 0.029 | 0.025 |
| `npm-i-pnpm` (small, small rootfs) | 0.003 | 0.003 | 0.037 | 0.031 | 0.027 |
| `pnpm-install` (large) | 0.508 | 0.484 | 0.948 | 0.853 | 0.849 |
| `churn-del` | 0.034 | 0.033 | 1.031 | 0.878 | 0.871 |
| `churn-edit` | 0.029 | 0.022 | 0.434 | 0.365 | 0.361 |
| `churn-gen` | 0.006 | 0.006 | 0.454 | 0.378 | 0.356 |
| overlay `umount`, total of 5 steps | 0.70 | 0.16 | – | – | – |
| `syncfs` after the command, total of 5 steps | 0.03 | 0.56 | 0.62 | 0.60 | 0.66 |

- A only spends time when the upper is large (`pnpm-install`, 256k entries).
  Small steps stay under 0.04 s.
- B spends 0.35–1.0 s on **every** step on the large rootfs, even the one that
  only creates 5,000 files: it walks ~255k paths each time.
- B's `churn-del` is expensive (0.9–1.0 s) because it re-reads 1.9 GB:
  - `pnpm install` runs for only ~2 s, so almost every file it wrote falls
    inside the 2 s racy window of the previous scan;
  - the previous scan therefore cached none of them, and this scan has to hash
    them again.

  `churn-edit` and `churn-gen` still read 344 MB each: the `.json` files that
  were just `touch`ed are in the racy window too. Checked with
  `OVPOC_DEBUG_MISS=1`: ctime 0.6 s before the scan, nlink ≥ 2. kaniko has the
  same mechanism.
- `ov` (without `volatile`) shows almost no `syncfs`: the kernel syncs the
  upper on `umount` (`umount` after `pnpm-install` took 0.44 s against 0.13 s
  for `ovv`).

### Command run time (s): the cost of overlay

| Configuration | `pnpm-install` | `churn-del` | `churn-edit` | Total, 5 commands |
|---|---|---|---|---|
| B `hwh-32`, ext4 | 1.95 | 0.25 | 0.23 | 3.24 |
| A `ov`, ext4 | 2.36 | 0.41 | **12.92** | **16.50** |
| A `ovv`, ext4 | 2.14 | 0.39 | 0.37 | 3.77 |
| B `hwh-32`, tmpfs | 1.84 | 0.24 | 0.18 | 3.11 |
| A `ov`, tmpfs | 1.92 | 0.32 | 0.30 | 3.34 |
| B `hwh-32+copy`, ext4 | 2.84 | 0.31 | 0.24 | 4.30 |
| A `ovv+copy`, ext4 | 3.20 | 0.42 | 0.41 | 4.90 |
| B `hwh-4`, `--cpus=4` | 4.18 | 0.25 | 0.23 | 5.50 |
| A `ovv`, `--cpus=4` | 4.71 | 0.40 | 0.38 | 6.34 |

- **Without `volatile`, overlay on ext4 fsyncs every copy-up.** `churn-edit`
  copies up ~4,000 files and took 12.9 s instead of 0.37 s (13.1 s in the
  `copy-ext4` set).
- **With `volatile`** (or on tmpfs), overlay still makes the commands slower by
  **+0.2 to +0.8 s** over five steps (+7 % to +16 %), mostly in
  `pnpm-install` and the copy-up steps.

### Commands + diff + `syncfs`/`umount` (s)

| | ext4, 32 CPU | ext4, 32 CPU, pnpm copy | ext4, 4 CPU |
|---|---|---|---|
| B (`hwh-32`; at 4 CPU: `hwh-4`) | 3.24 + 2.52 + 0.60 = **6.36** | 4.30 + 2.28 + 0.56 = **7.14** | 5.50 + 3.09 + 0.61 = **9.20** |
| A `ovv`, with `syncfs` | 3.77 + 0.55 + 0.72 = **5.04** | 4.90 + 0.56 + 0.94 = **6.40** | 6.34 + 0.56 + 0.60 + 0.16 ¹ = **7.66** |
| A `ovv`, without `syncfs` | 3.77 + 0.55 + 0.16 = **4.48** | **5.76** | **7.06** |
| Gain of A | **−1.3 to −1.9 s** | **−0.7 to −1.4 s** | **−1.5 to −2.1 s** |

¹ `umount` time taken from `main-ext4`.

In the middle column, A's `syncfs`/`umount` part is `syncfs` 0.64 + `umount`
0.30.

### Diff size and file sets

From `results/cmp-*.txt`: a separate round, ext4, A `ov` against B `hwh-4`.

| Step | A file / dir / whiteout | B file / dir / whiteout | tar A / B (MB) | gz (MB) |
|---|---|---|---|---|
| `npm-i-pnpm` | 1,632 / 267 / 0 | 1,632 / 264 / 0 | 34 / 34 | 11 |
| `pnpm-install` | 242,431 / 14,087 / 0 | 242,431 / 14,085 / 0 | 1,375 / 1,375 (copy: 2,472) | 290 (copy: 575) |
| `churn-del` | 0 / 4,777 / 9,912 | 0 / 2,136 / 9,912 | 7 / 6 | 0 |
| `churn-edit` | 4,009 / 4,375 / 0 | 5,822 / 37 / 0 (copy: 4,009) | 31 / 31 | 5 |
| `churn-gen` | 5,000 / 2 / 0 | 5,000 / 2 / 0 | 5 / 5 | 0 |

Differences between the file sets:

- **Parent directories.** A has extra parent directories: 2,641 in
  `churn-del`, 4,338 in `churn-edit`, and `/usr`, `/tmp`, … in the first two
  steps. An upper always contains the parents of every written path, even
  when the parent itself did not change. These are tar headers only, adding
  ≤ 1 MB.
- **Copy-up did not enlarge the diff on this workload.** No file was
  "rewritten with identical content" in a way B could skip: B's hash includes
  mtime, so a `touch`ed file enters B's diff too.
- **Hardlinks.** The only real difference is in `churn-edit` with default
  pnpm: **B has 1,813 extra files** under `/root/.local/share/pnpm/store/`.
  - The store is hardlinked into `node_modules`. `touch` changes the mtime of
    the shared inode, so B sees both paths change.
  - Overlay (`index=off`) copies up only the written path and breaks the
    hardlink. The store copy keeps its old mtime.
  - `sed -i` creates a new inode, which breaks the hardlink in both modes, so
    it causes no difference.

  The two modes therefore produce images that differ in **mtime and hardlink
  structure**, not in content bytes. With `+copy` (no hardlinks) the two file
  sets are identical (4,009 files).
- **Not deterministic between runs:** the name
  `/root/.npm/_logs/<timestamp>-debug-0.log`.

### Hash function and worker count (B, `main-ext4`, s)

| | 4 workers | 32 workers |
|---|---|---|
| `xxh` | 2.80 | 2.47 |
| `hwh` | 2.97 | 2.52 |
| `sha256` | 3.55 | 2.65 |
| `hwh`, no stat cache | – | 3.68 |

- `xxh` ≈ `hwh`. `sha256` is clearly slower only with 4 workers.
- 32 workers instead of 4: −0.3 to −0.9 s.
- Without the stat cache: +1.2 s (+46 %).
- The rest (~1.5 s) is walk + `lstat` of 255k paths × 4 steps. A faster hash
  cannot remove it.

### Lazy export (cost measured, end-to-end saving not measured)

Time to tar + pgzip (4 threads) the layers to `/dev/null`, total of five
steps:

| | default pnpm | `+copy` |
|---|---|---|
| tar only | 1.3 s | 1.9 s |
| tar + gzip | 2.1 s | 3.2 s |
| of which `pnpm-install` | 1.95–2.03 s | 3.0–3.07 s |

Nothing was written to disk or pushed. This is what lazy export saves for each
intermediate stage of W1's size. In W1, the `deps` stage *is* the final image
(kaniko squashes `final` into `deps`), so **W1 does not benefit from lazy
export**. W3 (kaniko optimization notes, docs/30 §5.4, external, not in this
repository) has three intermediate stages, so the estimate is ~3 × 2–3 s
**[inferred]**; [experiment 08](08-parallel-stages-lazy-export.md) later
measured it.

## Findings

1. Overlay diffing is 4–6× faster than the walk: 0.51–0.58 s against
   2.3–3.7 s for five steps; for a small step on a large rootfs, A takes
   0.006–0.034 s and B 0.35–1.0 s **[measured]** (diff-time tables).
2. The real gain for the whole sequence is smaller, −0.7 to −2.1 s over five
   steps: overlay makes the commands 0.2–0.8 s slower and gives part of the
   gain back **[measured]** (commands + diff table).
3. `volatile` (or an upper on tmpfs) is mandatory: without it `churn-edit` is
   12.5 s slower and overlay **loses** to the walk by a wide margin
   **[measured]** (command-time table).
4. Any hash function will do; a parallel walk plus a stat cache is enough for
   kaniko mode **[measured]** (hash and worker table).
5. The assumption "overlay saves several seconds on W1" holds against real
   kaniko but not against a well-written walk mode **[inferred]**:
   - kaniko optimization notes, docs/27 measured +7.3 s for kaniko's rescans
     and 4.4 s for `scan_full` after `pnpm install` (4 CPU, Docker Desktop);
   - B here spends 2.3–3.1 s on the same work, `pnpm-install` included;
   - possible reasons: kaniko also resolves paths, checks the ignore list,
     writes intermediate tars to disk, and was measured in a VM;
   - so ranikust's overlay mode might gain ~5–10 s over kaniko, but only
     ~1–2 s over ranikust's own walk mode.
   [Experiment 07](07-kaniko-vs-overlay.md) measured this directly on the same
   machine.
6. The gap grows with the number of `RUN`s on a large rootfs: B pays
   ~0.35–0.45 s per small `RUN` with ~255k paths and A almost nothing, so a
   Dockerfile with 20 `RUN`s after the dependency install would gain ~7–9 s
   **[inferred]**.
7. Lazy export (~2 s per layer of `pnpm install` size) and skipping
   intermediate tars matter as much as, or more than, the diff method
   **[inferred]** (lazy export table).

## Implications for ranikust

- Both snapshot modes stay: overlay when the environment permits it, a
  kaniko-style walk otherwise ([ADR-0002](../../adr/0002-two-snapshot-modes.md)).
  The walk mode is worth optimizing because the gap between the two modes is
  small (Finding 5).
- `volatile` is part of the default overlay mount options
  ([ADR-0016](../../adr/0016-overlay-mount-options.md),
  [overlay snapshot design](../../design/overlay-snapshot.md)).
- Kaniko mode uses a parallel walk with a stat cache and a 2 s racy window; the
  choice of hash is secondary ([kaniko-mode design](../../design/kaniko-mode.md)).
- The two modes can differ in mtimes and hardlink structure for identical
  content; this is one of the accepted differences
  ([ADR-0012](../../adr/0012-accept-divergence-between-modes.md)).
- Lazy export of intermediate stages is the default in overlay mode
  ([overlay snapshot design](../../design/overlay-snapshot.md); measured in
  [experiment 08](08-parallel-stages-lazy-export.md)).

## Limitations and open questions

- Three rounds per configuration. The min–max ranges are narrow (see the
  appendix), but no confidence intervals were computed.
- Shared machine. The harness waited for an idle machine before each run, but
  load that appeared during a run cannot be excluded (`results/run.log` records
  the load).
- Warm page cache in both modes: files had just been written. A cold cache was
  not measured.
- Not measured here: xfs and btrfs; overlay as real root (later done in
  [experiment 06a](06a-root-rerun.md)); more than 5 lowerdirs.
- Real kaniko was not run on the same machine, so the "gain over kaniko" in
  Finding 5 is an inference ([experiment 07](07-kaniko-vs-overlay.md) measured
  it later).

Differences from the earlier benchmark conditions (kaniko optimization notes,
docs/27 and docs/30, external, not in this repository):

| | Earlier benchmark | This experiment |
|---|---|---|
| Machine | macOS, Docker Desktop VM, `--cpus=4` | Linux 7.0 native, 32 CPUs (one set with `--cpus=4`) |
| Runtime | Docker | podman 5.7 rootless, `--privileged` (root in a userns), native overlay storage (not fuse-overlayfs) |
| Overlay | (BuildKit: snapshotter not checked) | kernel overlay in a userns: `userxattr`, `redirect_dir=nofollow` forced. Real-root overlay not measured (no sudo) |
| Filesystem | ext4 inside the VM | ext4 on NVMe `/home` (podman volume) and tmpfs. The upper is not on the container's overlay |
| pnpm | no hardlinks, ~2.7 GB, 266k files | hardlinks by default (tar 1.4 GB). The `+copy` set has no hardlinks (tar 2.5 GB, ~266k paths) |
| `pnpm install` | 7.7 s (kaniko) | 1.8–3.2 s (verdaccio warm cache, 32 CPUs), 4.2–4.7 s at 4 CPUs. The 2 s racy window therefore excludes more files from the stat cache than in the earlier benchmark |
| kaniko | real executor | simulated: no path resolution, no mountinfo-based ignore list, no tar written to `/kaniko/layers` |
| Export | written to file + pushed | `/dev/null` only |
| `COPY` | separate layer | merged into the `pnpm-install` layer (3 small files) |

## Reproduction

The PoC sources are archived outside this repository, under
`docs-rust/poc/06-overlay-vs-walk/`:

- `main.go`: the `ovpoc` binary with subcommands `prep` (extract the base
  image), `run` (one measured sequence in mode A or B) and `cmp` (compare two
  file lists).
- `run.sh`: the harness. `build` compiles `bin/ovpoc` in the `golang` image;
  `prep` extracts the W1 base image into the podman volume `ovpoc-work`;
  `matrix <label> <rounds> <ext4|tmpfs> [podman args] -- <cfg>...` runs one
  measurement set, each run in a fresh `podman run --rm --privileged` after the
  idle gate; `cmp` records and compares the A and B file lists; `clean` removes
  the volumes. A configuration is `ov`, `ovv` or `<hash>-<workers>[-nostat]`,
  with an optional `+copy` suffix for `package-import-method=copy`.
- `all.sh`: the full matrix in the order it was run.
- `summarize.py`: produces `results/tables.md`.
- `results/<set>/r<round>-<cfg>.json` (raw numbers), `results/cmp-*.txt`
  (file-set comparison), `results/run.log` (run log with load).

The run needs the `nextjs-kaniko` compose project (`nk-registry`,
`nk-verdaccio`, network `nextjs-kaniko_internal`).

```sh
./all.sh                     # build bin/ovpoc, extract the base into volume ovpoc-work, run the matrix, compare file sets
python3 -I summarize.py > results/tables.md
./run.sh clean               # remove volumes ovpoc-work and ovpoc-gomod
```

## References

1. [Experiment 01: Overlay semantics](01-overlay-semantics.md).
2. [Experiment 06a: Root re-run](06a-root-rerun.md) (this experiment repeated
   as real root).
3. [Experiment 07: kaniko vs overlay](07-kaniko-vs-overlay.md).
4. [Experiment 08: Parallel stages and lazy export](08-parallel-stages-lazy-export.md).
5. Kaniko optimization notes, docs/12 (stat cache), docs/27 (W1 rescans),
   docs/30 §5.4 (W3, lazy export) (external, not in this repository).
6. `kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:116` (`Snapshotter.Init`),
   `:199` (`TakeSnapshotFS`).
7. `kaniko@44aa7ddc0 pkg/util/util.go:52` (`Hasher`).
8. `kaniko@44aa7ddc0 pkg/util/stat_hash_cache.go:41`
   (`StatHashCacheRacyWindow = 2 * time.Second`).

## Appendix: full tables

Generated by `summarize.py` from `results/<set>/`. Medians with (min–max) over
3 rounds.

### `main-ext4`

Rounds per configuration: hwh-32-nostat=3, hwh-32=3, hwh-4=3, ov=3, ovv=3,
sha256-32=3, sha256-4=3, xxh-32=3, xxh-4=3

Diff time (s): overlay = walk of the upperdir; walk = walk + lstat + hash + map
comparison. Excludes `syncfs`, tar and compression.

| configuration | init | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | **total** | total + syncfs |
|---|---|---|---|---|---|---|---|---|
| hwh-32-nostat | 0.030 (0.029–0.034) | 0.035 (0.031–0.035) | 0.848 (0.835–0.893) | 0.922 (0.881–0.931) | 0.917 (0.890–0.945) | 0.914 (0.906–0.922) | **3.68 (3.59–3.72)** | 4.23 (4.17–4.35) |
| hwh-32 | 0.029 (0.029–0.029) | 0.031 (0.031–0.032) | 0.853 (0.834–0.856) | 0.878 (0.848–0.879) | 0.365 (0.363–0.380) | 0.378 (0.350–0.379) | **2.52 (2.49–2.54)** | 3.11 (3.08–3.13) |
| hwh-4 | 0.041 (0.030–0.042) | 0.037 (0.035–0.038) | 0.948 (0.947–0.967) | 1.031 (0.980–1.052) | 0.434 (0.420–0.464) | 0.454 (0.447–0.458) | **2.97 (2.86–2.99)** | 3.57 (3.54–3.59) |
| ov | – | 0.003 (0.003–0.004) | 0.508 (0.474–0.515) | 0.034 (0.033–0.035) | 0.029 (0.025–0.035) | 0.006 (0.005–0.007) | **0.58 (0.55–0.59)** | 0.62 (0.58–0.63) |
| ovv | – | 0.003 (0.003–0.003) | 0.484 (0.466–0.621) | 0.033 (0.031–0.036) | 0.022 (0.022–0.030) | 0.006 (0.004–0.006) | **0.55 (0.54–0.68)** | 1.11 (1.10–1.37) |
| sha256-32 | 0.066 (0.065–0.067) | 0.068 (0.067–0.068) | 0.926 (0.881–1.585) | 0.876 (0.843–0.877) | 0.371 (0.361–0.381) | 0.347 (0.346–0.378) | **2.65 (2.57–3.35)** | 3.28 (3.15–4.02) |
| sha256-4 | 0.070 (0.069–0.071) | 0.075 (0.074–0.078) | 1.209 (1.107–1.271) | 1.256 (1.100–1.300) | 0.470 (0.446–0.472) | 0.464 (0.464–0.499) | **3.55 (3.26–3.69)** | 4.18 (3.95–4.29) |
| xxh-32 | 0.025 (0.025–0.026) | 0.027 (0.027–0.028) | 0.849 (0.843–0.856) | 0.871 (0.860–0.884) | 0.361 (0.330–0.384) | 0.356 (0.351–0.392) | **2.47 (2.45–2.57)** | 3.13 (3.13–3.13) |
| xxh-4 | 0.030 (0.030–0.030) | 0.033 (0.030–0.034) | 0.923 (0.921–0.926) | 0.961 (0.942–1.023) | 0.424 (0.423–0.455) | 0.441 (0.433–0.464) | **2.80 (2.79–2.93)** | 3.49 (3.49–3.50) |

Command run time (s) and `syncfs` after each command (s):

| configuration | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | commands total | syncfs total |
|---|---|---|---|---|---|---|---|
| hwh-32-nostat | 0.76 (0.73–0.79) | 2.02 (1.98–2.12) | 0.27 (0.27–0.34) | 0.22 (0.22–0.23) | 0.05 (0.05–0.05) | 3.42 (3.25–3.42) | 0.58 (0.55–0.63) |
| hwh-32 | 0.76 (0.74–0.77) | 1.95 (1.95–1.97) | 0.25 (0.25–0.26) | 0.23 (0.23–0.23) | 0.05 (0.05–0.05) | 3.24 (3.22–3.28) | 0.60 (0.54–0.62) |
| hwh-4 | 0.76 (0.73–0.78) | 2.00 (1.94–2.01) | 0.26 (0.25–0.26) | 0.22 (0.22–0.23) | 0.05 (0.05–0.05) | 3.26 (3.23–3.31) | 0.62 (0.55–0.71) |
| ov | 0.76 (0.73–0.83) | 2.36 (2.25–2.43) | 0.41 (0.40–0.52) | 12.92 (12.91–13.05) | 0.08 (0.06–0.10) | 16.50 (16.50–16.83) | 0.03 (0.03–0.04) |
| ovv | 0.78 (0.75–0.80) | 2.14 (2.13–2.64) | 0.39 (0.39–0.41) | 0.37 (0.36–0.39) | 0.07 (0.07–0.07) | 3.77 (3.74–4.26) | 0.56 (0.56–0.69) |
| sha256-32 | 0.76 (0.72–0.76) | 1.94 (1.90–1.98) | 0.28 (0.25–0.32) | 0.22 (0.22–0.23) | 0.05 (0.04–0.05) | 3.21 (3.17–3.35) | 0.62 (0.58–0.67) |
| sha256-4 | 0.75 (0.72–0.75) | 1.99 (1.89–2.02) | 0.25 (0.25–0.26) | 0.23 (0.22–0.23) | 0.05 (0.04–0.05) | 3.27 (3.12–3.30) | 0.63 (0.60–0.69) |
| xxh-32 | 0.78 (0.75–0.80) | 1.91 (1.88–1.99) | 0.25 (0.25–0.25) | 0.22 (0.22–0.23) | 0.05 (0.05–0.05) | 3.22 (3.18–3.26) | 0.66 (0.56–0.68) |
| xxh-4 | 0.75 (0.74–0.76) | 1.94 (1.93–1.96) | 0.25 (0.25–0.27) | 0.22 (0.22–0.23) | 0.05 (0.04–0.05) | 3.23 (3.20–3.24) | 0.69 (0.56–0.70) |

Diff size and export cost (medians): file = files + symlinks; tar and gz in MB;
tar s = tar to /dev/null; tgz s = tar + pgzip with 4 threads to /dev/null.

| configuration | step | walked | file | dir | whiteout | tar MB | gz MB | tar s | tgz s | hash read MB | cache hits |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hwh-32-nostat | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 249 | 0 |
| hwh-32-nostat | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.15 (1.12–1.16) | 2.08 (1.97–2.10) | 2432 | 0 |
| hwh-32-nostat | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 2132 | 0 |
| hwh-32-nostat | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.05–0.05) | 2132 | 0 |
| hwh-32-nostat | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.02–0.03) | 0.03 (0.03–0.03) | 2132 | 0 |
| hwh-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 249 | 0 |
| hwh-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.11 (1.10–1.12) | 1.98 (1.95–2.01) | 2432 | 0 |
| hwh-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 1883 | 7404 |
| hwh-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.05–0.05) | 344 | 215499 |
| hwh-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.04 (0.03–0.04) | 344 | 215499 |
| hwh-4 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.04–0.04) | 249 | 0 |
| hwh-4 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.10 (1.09–1.10) | 1.95 (1.94–1.98) | 2432 | 0 |
| hwh-4 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 1883 | 7404 |
| hwh-4 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.04 (0.04–0.05) | 344 | 215499 |
| hwh-4 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |
| ov | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 0 | 0 |
| ov | pnpm-install | 256518 | 242431 | 14087 | 0 | 1375 | 290 | 1.25 (1.24–1.35) | 2.03 (1.99–2.17) | 0 | 0 |
| ov | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.02 (0.02–0.02) | 0 | 0 |
| ov | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.04) | 0.05 (0.05–0.06) | 0 | 0 |
| ov | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.03–0.04) | 0.03 (0.03–0.04) | 0 | 0 |
| ovv | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 0 | 0 |
| ovv | pnpm-install | 256518 | 242431 | 14087 | 0 | 1375 | 290 | 1.21 (1.20–1.37) | 1.97 (1.97–1.99) | 0 | 0 |
| ovv | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.02 (0.02–0.02) | 0 | 0 |
| ovv | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 0 | 0 |
| ovv | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.02–0.03) | 0.03 (0.03–0.03) | 0 | 0 |
| sha256-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 249 | 0 |
| sha256-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.13 (1.10–1.73) | 2.01 (1.93–2.07) | 2432 | 0 |
| sha256-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1883 | 7404 |
| sha256-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 344 | 215499 |
| sha256-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.02) | 0.03 (0.03–0.04) | 344 | 215499 |
| sha256-4 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 249 | 0 |
| sha256-4 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.14 (1.08–1.14) | 2.11 (1.95–2.11) | 2432 | 0 |
| sha256-4 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1883 | 7404 |
| sha256-4 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.04) | 0.05 (0.04–0.05) | 344 | 215499 |
| sha256-4 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |
| xxh-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.03 (0.03–0.04) | 249 | 0 |
| xxh-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.12 (1.11–1.12) | 1.96 (1.95–1.98) | 2432 | 0 |
| xxh-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1883 | 7404 |
| xxh-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 344 | 215499 |
| xxh-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |
| xxh-4 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.03 (0.03–0.04) | 249 | 0 |
| xxh-4 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.09 (1.09–1.11) | 1.94 (1.93–1.99) | 2432 | 0 |
| xxh-4 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 1883 | 7404 |
| xxh-4 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 344 | 215499 |
| xxh-4 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |

### `main-tmpfs`

Rounds per configuration: hwh-32=3, hwh-4=3, ov=3, sha256-32=3, xxh-32=3

Diff time (s): overlay = walk of the upperdir; walk = walk + lstat + hash + map
comparison. Excludes `syncfs`, tar and compression.

| configuration | init | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | **total** | total + syncfs |
|---|---|---|---|---|---|---|---|---|
| hwh-32 | 0.032 (0.031–0.033) | 0.036 (0.036–0.040) | 0.856 (0.854–0.864) | 0.853 (0.844–0.862) | 0.355 (0.344–0.366) | 0.363 (0.362–0.381) | **2.51 (2.49–2.51)** | 2.51 (2.49–2.51) |
| hwh-4 | 0.035 (0.032–0.035) | 0.037 (0.036–0.038) | 0.951 (0.949–0.969) | 1.003 (1.001–1.003) | 0.436 (0.431–0.441) | 0.462 (0.452–0.503) | **2.94 (2.92–2.96)** | 2.94 (2.92–2.96) |
| ov | – | 0.003 (0.003–0.003) | 0.454 (0.444–0.482) | 0.031 (0.029–0.035) | 0.019 (0.018–0.023) | 0.004 (0.003–0.005) | **0.51 (0.50–0.54)** | 0.51 (0.50–0.54) |
| sha256-32 | 0.067 (0.066–0.069) | 0.070 (0.068–0.072) | 0.925 (0.891–1.010) | 0.918 (0.911–0.927) | 0.386 (0.342–0.451) | 0.392 (0.384–0.399) | **2.76 (2.68–2.91)** | 2.76 (2.68–2.91) |
| xxh-32 | 0.030 (0.026–0.033) | 0.030 (0.030–0.032) | 0.848 (0.845–0.869) | 0.889 (0.888–0.897) | 0.351 (0.344–0.360) | 0.369 (0.365–0.384) | **2.53 (2.53–2.53)** | 2.53 (2.53–2.53) |

Command run time (s) and `syncfs` after each command (s):

| configuration | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | commands total | syncfs total |
|---|---|---|---|---|---|---|---|
| hwh-32 | 0.83 (0.81–0.83) | 1.84 (1.81–1.88) | 0.24 (0.23–0.24) | 0.18 (0.18–0.19) | 0.02 (0.02–0.02) | 3.11 (3.07–3.14) | 0.00 (0.00–0.00) |
| hwh-4 | 0.76 (0.76–0.83) | 1.84 (1.80–1.99) | 0.23 (0.22–0.24) | 0.18 (0.18–0.18) | 0.02 (0.02–0.02) | 3.05 (3.03–3.19) | 0.00 (0.00–0.00) |
| ov | 0.77 (0.76–0.78) | 1.92 (1.86–1.94) | 0.32 (0.32–0.34) | 0.30 (0.29–0.31) | 0.03 (0.03–0.03) | 3.34 (3.29–3.37) | 0.00 (0.00–0.00) |
| sha256-32 | 0.76 (0.76–0.76) | 2.00 (1.92–2.03) | 0.24 (0.23–0.26) | 0.19 (0.18–0.19) | 0.02 (0.02–0.02) | 3.20 (3.11–3.25) | 0.00 (0.00–0.00) |
| xxh-32 | 0.77 (0.73–0.77) | 1.97 (1.95–2.00) | 0.24 (0.23–0.24) | 0.18 (0.18–0.18) | 0.02 (0.02–0.02) | 3.16 (3.13–3.22) | 0.00 (0.00–0.00) |

Diff size and export cost (medians): file = files + symlinks; tar and gz in MB;
tar s = tar to /dev/null; tgz s = tar + pgzip with 4 threads to /dev/null.

| configuration | step | walked | file | dir | whiteout | tar MB | gz MB | tar s | tgz s | hash read MB | cache hits |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hwh-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.02) | 0.04 (0.03–0.04) | 249 | 0 |
| hwh-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.15 (1.13–1.16) | 2.08 (2.04–2.09) | 2432 | 0 |
| hwh-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 1896 | 6473 |
| hwh-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 344 | 215499 |
| hwh-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |
| hwh-4 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.03 (0.03–0.04) | 249 | 0 |
| hwh-4 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.12 (1.12–1.18) | 2.07 (2.04–2.07) | 2432 | 0 |
| hwh-4 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1896 | 6369 |
| hwh-4 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.04) | 0.05 (0.04–0.05) | 344 | 215499 |
| hwh-4 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |
| ov | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.03–0.04) | 0 | 0 |
| ov | pnpm-install | 256518 | 242431 | 14087 | 0 | 1375 | 290 | 1.26 (1.19–1.27) | 2.00 (1.99–2.01) | 0 | 0 |
| ov | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.02 (0.02–0.02) | 0 | 0 |
| ov | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 0 | 0 |
| ov | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 0 | 0 |
| sha256-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.04–0.04) | 249 | 0 |
| sha256-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.12 (1.12–1.17) | 2.02 (1.99–2.08) | 2432 | 0 |
| sha256-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1883 | 7404 |
| sha256-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.06) | 0.05 (0.04–0.05) | 344 | 215499 |
| sha256-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |
| xxh-32 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.03 (0.03–0.04) | 249 | 0 |
| xxh-32 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.12 (1.12–1.13) | 2.06 (2.05–2.11) | 2432 | 0 |
| xxh-32 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1883 | 7401 |
| xxh-32 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.05–0.05) | 344 | 215499 |
| xxh-32 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.02–0.03) | 0.03 (0.03–0.03) | 344 | 215499 |

### `copy-ext4`

Rounds per configuration: hwh-32+copy=3, hwh-4+copy=3, ov+copy=3, ovv+copy=3

Diff time (s): overlay = walk of the upperdir; walk = walk + lstat + hash + map
comparison. Excludes `syncfs`, tar and compression.

| configuration | init | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | **total** | total + syncfs |
|---|---|---|---|---|---|---|---|---|
| hwh-32+copy | 0.030 (0.029–0.031) | 0.033 (0.030–0.036) | 0.898 (0.877–0.946) | 0.649 (0.644–0.693) | 0.322 (0.322–0.390) | 0.324 (0.322–0.363) | **2.28 (2.26–2.41)** | 2.88 (2.81–2.93) |
| hwh-4+copy | 0.033 (0.033–0.033) | 0.037 (0.037–0.043) | 1.017 (1.010–1.093) | 0.777 (0.766–0.787) | 0.384 (0.377–0.397) | 0.405 (0.405–0.447) | **2.64 (2.64–2.80)** | 3.25 (3.22–3.33) |
| ov+copy | – | 0.003 (0.003–0.004) | 0.493 (0.488–0.512) | 0.036 (0.034–0.037) | 0.022 (0.022–0.027) | 0.005 (0.005–0.006) | **0.56 (0.56–0.58)** | 0.60 (0.60–0.62) |
| ovv+copy | – | 0.003 (0.003–0.004) | 0.497 (0.495–0.498) | 0.033 (0.033–0.035) | 0.024 (0.023–0.027) | 0.006 (0.005–0.007) | **0.56 (0.56–0.57)** | 1.21 (1.19–1.21) |

Command run time (s) and `syncfs` after each command (s):

| configuration | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | commands total | syncfs total |
|---|---|---|---|---|---|---|---|
| hwh-32+copy | 0.78 (0.76–0.78) | 2.84 (2.62–2.93) | 0.31 (0.31–0.34) | 0.24 (0.24–0.34) | 0.05 (0.05–0.05) | 4.30 (3.97–4.35) | 0.56 (0.53–0.60) |
| hwh-4+copy | 0.77 (0.75–0.78) | 2.74 (2.71–2.86) | 0.32 (0.32–0.34) | 0.23 (0.23–0.24) | 0.05 (0.05–0.05) | 4.13 (4.07–4.24) | 0.58 (0.53–0.62) |
| ov+copy | 0.76 (0.74–0.80) | 3.17 (2.98–3.22) | 0.42 (0.40–0.44) | 13.06 (12.90–23.09) | 0.06 (0.06–0.06) | 17.43 (17.28–27.47) | 0.04 (0.04–0.04) |
| ovv+copy | 0.80 (0.79–0.85) | 3.20 (2.93–3.70) | 0.42 (0.40–0.43) | 0.41 (0.37–0.42) | 0.07 (0.07–0.08) | 4.90 (4.65–5.40) | 0.64 (0.63–0.65) |

Diff size and export cost (medians): file = files + symlinks; tar and gz in MB;
tar s = tar to /dev/null; tgz s = tar + pgzip with 4 threads to /dev/null.

| configuration | step | walked | file | dir | whiteout | tar MB | gz MB | tar s | tgz s | hash read MB | cache hits |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hwh-32+copy | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.02) | 0.04 (0.04–0.04) | 249 | 0 |
| hwh-32+copy | pnpm-install | 265694 | 242431 | 14085 | 0 | 2472 | 575 | 1.65 (1.62–1.83) | 3.00 (2.99–3.21) | 2432 | 0 |
| hwh-32+copy | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 1174 | 99849 |
| hwh-32+copy | churn-edit | 253735 | 4009 | 37 | 0 | 29 | 5 | 0.03 (0.03–0.03) | 0.04 (0.04–0.04) | 26 | 230337 |
| hwh-32+copy | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.03–0.03) | 0.03 (0.03–0.03) | 26 | 230337 |
| hwh-4+copy | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.03 (0.03–0.04) | 249 | 0 |
| hwh-4+copy | pnpm-install | 265694 | 242431 | 14085 | 0 | 2472 | 575 | 1.63 (1.61–1.76) | 3.01 (2.99–3.16) | 2432 | 0 |
| hwh-4+copy | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 1321 | 91194 |
| hwh-4+copy | churn-edit | 253735 | 4009 | 37 | 0 | 29 | 5 | 0.02 (0.02–0.02) | 0.04 (0.04–0.05) | 26 | 230337 |
| hwh-4+copy | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.03–0.03) | 0.03 (0.03–0.03) | 26 | 230337 |
| ov+copy | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.02) | 0.04 (0.03–0.04) | 0 | 0 |
| ov+copy | pnpm-install | 256518 | 242431 | 14087 | 0 | 2472 | 575 | 1.86 (1.86–3.74) | 3.07 (2.98–3.08) | 0 | 0 |
| ov+copy | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.02 (0.02–0.02) | 0 | 0 |
| ov+copy | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.04–0.05) | 0 | 0 |
| ov+copy | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.02–0.03) | 0.03 (0.03–0.03) | 0 | 0 |
| ovv+copy | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.03) | 0.04 (0.04–0.08) | 0 | 0 |
| ovv+copy | pnpm-install | 256518 | 242431 | 14087 | 0 | 2472 | 575 | 1.89 (1.82–1.97) | 3.06 (3.02–3.13) | 0 | 0 |
| ovv+copy | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.02 (0.01–0.02) | 0 | 0 |
| ovv+copy | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.04) | 0.05 (0.04–0.06) | 0 | 0 |
| ovv+copy | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.03 (0.03–0.03) | 0.03 (0.03–0.04) | 0 | 0 |

### `cpus4-ext4`

Rounds per configuration: hwh-4=3, ovv=3

Diff time (s): overlay = walk of the upperdir; walk = walk + lstat + hash + map
comparison. Excludes `syncfs`, tar and compression.

| configuration | init | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | **total** | total + syncfs |
|---|---|---|---|---|---|---|---|---|
| hwh-4 | 0.042 (0.032–0.044) | 0.039 (0.038–0.040) | 1.051 (1.017–1.067) | 0.983 (0.982–1.031) | 0.445 (0.439–0.486) | 0.477 (0.460–0.583) | **3.09 (2.98–3.18)** | 3.68 (3.59–3.79) |
| ovv | – | 0.004 (0.003–0.004) | 0.490 (0.487–0.500) | 0.034 (0.033–0.035) | 0.024 (0.023–0.024) | 0.006 (0.006–0.006) | **0.56 (0.55–0.57)** | 1.15 (0.96–1.24) |

Command run time (s) and `syncfs` after each command (s):

| configuration | npm-i-pnpm | pnpm-install | churn-del | churn-edit | churn-gen | commands total | syncfs total |
|---|---|---|---|---|---|---|---|
| hwh-4 | 0.76 (0.76–0.78) | 4.18 (4.02–4.22) | 0.25 (0.25–0.27) | 0.23 (0.22–0.23) | 0.04 (0.04–0.05) | 5.50 (5.29–5.51) | 0.61 (0.50–0.70) |
| ovv | 0.79 (0.79–0.91) | 4.71 (4.66–4.89) | 0.40 (0.39–0.43) | 0.38 (0.38–0.46) | 0.07 (0.06–0.07) | 6.34 (6.28–6.76) | 0.60 (0.39–0.68) |

Diff size and export cost (medians): file = files + symlinks; tar and gz in MB;
tar s = tar to /dev/null; tgz s = tar + pgzip with 4 threads to /dev/null.

| configuration | step | walked | file | dir | whiteout | tar MB | gz MB | tar s | tgz s | hash read MB | cache hits |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hwh-4 | npm-i-pnpm | 9180 | 1632 | 264 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.04–0.04) | 249 | 0 |
| hwh-4 | pnpm-install | 265694 | 242431 | 14085 | 0 | 1375 | 290 | 1.11 (1.09–1.22) | 2.29 (2.22–2.36) | 2432 | 0 |
| hwh-4 | churn-del | 253735 | 0 | 2136 | 9912 | 6 | 0 | 0.01 (0.01–0.01) | 0.01 (0.01–0.02) | 1831 | 19298 |
| hwh-4 | churn-edit | 253735 | 5822 | 37 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.05–0.05) | 344 | 215499 |
| hwh-4 | churn-gen | 258736 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.02) | 0.03 (0.03–0.03) | 344 | 215499 |
| ovv | npm-i-pnpm | 1899 | 1632 | 267 | 0 | 34 | 11 | 0.01 (0.01–0.01) | 0.04 (0.04–0.04) | 0 | 0 |
| ovv | pnpm-install | 256518 | 242431 | 14087 | 0 | 1375 | 290 | 1.28 (1.21–1.40) | 2.23 (2.22–2.35) | 0 | 0 |
| ovv | churn-del | 14689 | 0 | 4777 | 9912 | 7 | 0 | 0.01 (0.01–0.01) | 0.02 (0.02–0.02) | 0 | 0 |
| ovv | churn-edit | 8384 | 4009 | 4375 | 0 | 31 | 5 | 0.03 (0.03–0.03) | 0.05 (0.05–0.05) | 0 | 0 |
| ovv | churn-gen | 5002 | 5000 | 2 | 0 | 5 | 0 | 0.02 (0.02–0.03) | 0.03 (0.02–0.03) | 0 | 0 |
