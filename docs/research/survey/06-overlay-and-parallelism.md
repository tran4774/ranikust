# Two snapshot modes (overlay and kaniko-style) and parallel stages

| | |
|---|---|
| **Status** | Frozen, 2026-10-29 |
| **Original** | `docs-rust/06-overlay-va-song-song.md` (Vietnamese, not in this repository) |
| **Superseded by** | [`docs/design/modes-and-detection.md`][d-modes], [`docs/design/overlay-snapshot.md`][d-overlay], [`docs/design/scheduler.md`][d-sched], [`docs/design/kaniko-mode.md`][d-kaniko], [`docs/design/cache-and-store.md`][d-cache]; [ADR-0002][adr2], [ADR-0012][adr12], [ADR-0014][adr14], [ADR-0016][adr16]–[ADR-0019][adr19] |

This chapter answers a follow-up requirement: use overlayfs when privileges
allow, fall back to kaniko's approach when they do not, and run stages in
parallel as BuildKit does. **fanotify is not used.** It replaces the earlier
"reducing snapshot cost" and "parallel stages" material in [02][s02].

## Summary

- **Which mode runs where?** Overlay (rootful or rootless) where the process can
  mount; kaniko mode where it is root but cannot mount, which is **most
  locked-down CI pods**; no mode for a regular user on Ubuntu ≥ 24.04 or in a
  PSS restricted pod. Detection actually performs each step instead of checking
  versions. ranikust's differentiator is one binary that covers both sides, not
  "overlay everywhere".
- **How does an overlay snapshot work?** Each instruction mounts a new upper
  directory over the previous diffs and the base layers, with
  `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile`.
  The upper directory is the diff; only it is walked, against the lowers, to
  convert overlay whiteouts into OCI ones.
- **How much faster is overlay?** Taking the diff is 4.5–5.5 times faster than
  an optimized walk, but commands run 7–16% slower on overlay, so a W1 build
  gains 18–23% (1.5–1.7 s) **[measured]**. `volatile` is mandatory. Exporting a
  layer costs about 2 s in both modes, so **lazy export** matters as much as the
  diff method.
- **Do parallel stages help?** On native Linux, yes: overlay with lazy export
  and the default `--jobs` builds W3 in 6.02 s at 32 CPUs against 8.44 s with
  `--jobs 1` **[measured]**. The real constraint is memory, so the scheduler
  must also limit by available RAM.
- **Can kaniko mode run stages in parallel?** Yes, with one rootfs directory per
  stage: 41–44% faster on W3 **[measured]**. Without mounts, `RUN` then has no
  `/proc`, so this is opt-in (`--parallel-stages`). A fake `/proc` of static
  files fixes Node tools and `next build`; Java needs `JAVA_TOOL_OPTIONS` under
  memory limits.
- **Are the modes interchangeable?** The cache key does not depend on the mode,
  but images can differ in metadata, hardlink structure and, for in-place
  writes, content.

## 1. Choosing a mode

The table was checked against the PoCs ([experiment 01][e01],
[experiment 02][e02]) and desk research ([experiment 03][e03], which has the
full table per CI system and cluster). In the confidence column, **High** means
official documentation or source code and **Medium** means secondary sources or
reasoning from the mechanism (both **[external]**, from experiment 03);
**Measured** means run for real **[measured]**.

| Environment | Mode | Notes | Confidence |
|---|---|---|---|
| Host, running as root/sudo (including GitHub Actions through `sudo`, GitLab.com SaaS) | **overlay** | Every step works without a fallback: new `proc`, `sysfs`, `mknod`, `pivot_root` | Measured ([experiment 06a][e06a]) |
| `--privileged` container; or podman with `--cap-add SYS_ADMIN` | **overlay** | The workdir must be a volume or tmpfs. An upper on the container's overlay rootfs gives `EINVAL`. **Docker `--cap-add SYS_ADMIN` is not enough:** AppArmor `docker-default` returns `EACCES` for mount, so it falls back to kaniko mode (measured, [experiment 13][e13]) | Measured (podman; real Docker `--privileged`, experiment 13) |
| Default rootless podman; Docker with `seccomp=unconfined,apparmor=unconfined` | **rootless overlay** (`userxattr`) | A new `proc` gets `EPERM` because `/proc` is masked, so `/proc` is rbind-mounted (`RUN` sees the container's processes). With `unmask=ALL` there is a private pid namespace | Measured (podman; real Docker 29.1 in a VM, experiment 13) |
| Debian, Fedora, RHEL 8+, Ubuntu ≤ 22.04 host, regular user | **rootless overlay** | With `newuidmap` + `/etc/subuid` the full uid range is available. Without them only 1 uid is mapped, and images with several uids fail at `chown` | Medium |
| Default Docker, GitHub Actions `container:` jobs, Kubernetes pods on Ubuntu/COS nodes, GKE Autopilot, Bottlerocket, GitLab Kubernetes executor | **kaniko** | Docker's seccomp blocks **even** `CLONE_NEWUSER`; AppArmor `docker-default`/`cri-containerd` has `deny mount` | High; measured (real Docker 29.1, cgroup v2 and v1, experiment 13) |
| Ubuntu ≥ 24.04 host, regular user, binary downloaded to `~/bin` | **no mode** | AppArmor allows creating a user namespace but strips all its capabilities. Kaniko mode needs root. Installing the binary in `/usr/local/bin` with a 3-line profile (`userns,` + `flags=(unconfined)`, bound to the path) gives full **rootless overlay**, including a new `proc` ([experiment 11][e11]) | High; measured |
| PSS restricted pod (`runAsNonRoot`), OpenShift `restricted-v2` | **no mode** | No user namespace, not root | Medium |

So **most locked-down CI pods land in kaniko mode**, and overlay is available
only where BuildKit or buildah already run. ranikust's differentiator is **one
binary that works on both sides**, not "overlay everywhere". User documentation
should not suggest otherwise.

Detection with `--snapshot-mode=auto` (the default). Every step must **actually
be performed**, because many probes "succeed" at the first step and then fail at
a later one:

1. Try **without a user namespace**: `unshare(CLONE_NEWNS)`, mount a small
   overlay inside the workdir, then mount `proc` in a new pid namespace. With
   `CAP_SYS_ADMIN`, this is the only way to mount a new `proc` when `/proc` is
   masked.
2. If that fails, try **a user namespace**:
   - `unshare(CLONE_NEWUSER | CLONE_NEWNS)`, then **write `uid_map`** (several
     ranges when root in a container);
   - mount overlay with `userxattr`;
   - mount a new `proc`; on failure, try rbind `/proc`.

   On Ubuntu 24.04+, `unshare` succeeds but writing the map or mounting fails.
   The probe must get that far.
3. Also try `pivot_root` (Docker's seccomp blocks it; use MS_MOVE) and `sysfs`
   (fallback: rbind), so the sandbox knows which path to take.
4. If no overlay path works:
   - if root: **kaniko**;
   - otherwise **stop** and print guidance: run with sudo; install an AppArmor
     profile for the binary's path (or install from a `.deb` that ships the
     profile, as podman does); or run in a container. Read
     `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` to give the right
     hint.
5. Log the reason clearly. The mode can be forced with
   `--snapshot-mode=overlay|kaniko`. Detection only tries the mounts and never
   compares kernel versions: Ubuntu and Debian carried the overlay-in-userns
   patch before 5.11, while SELinux requires 5.13.

In overlay mode, **containers and hosts run the same code path**: `RUN` always
does `pivot_root` into the stage rootfs. There are therefore only two code
paths: overlay + sandbox, and kaniko (ported almost unchanged).

## 2. The workdir in overlay mode

In kaniko mode the workdir is the rootfs ([02][s02]). In overlay mode each stage
needs its own rootfs to run in parallel, so **the workdir becomes the root that
holds the rootfs directories**: each stage's rootfs is an overlay mount point
inside the workdir. All data stays under the workdir; the tool writes nothing
outside it.

```
<workdir>/
├── .ranikust-root                   marker file
├── blobs/sha256/<digest>            downloaded compressed blobs (base, cache)
├── layers/<id>/                     extracted layers, immutable, whiteouts in overlay format
├── l/<short>  → ../layers/<id>      short-name symlinks; only needed for the old mount(2) path, not with fsconfig(lowerdir+) (§3.4)
├── caches/<sha256(id)>/             RUN --mount=type=cache
└── builds/<build-id>/
    ├── context/
    └── stages/<n>/
        ├── diff-<k>/                upper of instruction k = diff of layer k
        ├── work-<k>/
        └── rootfs/                  overlay mount point of stage n
```

`layers/` is content-addressed and shared between stages and between builds, so
a shared base is downloaded and extracted **only once**. A per-digest lock keeps
two parallel stages from extracting the same layer.

## 3. Overlay snapshots

### 3.1 One layer per instruction

1. `lowerdir = diff-(k-1):…:diff-1:<base layers>`, top to bottom. Nothing is
   copied.
2. Create fresh `diff-k` and `work-k`. Mount overlay on `rootfs/` with
   `upperdir=diff-k` and these options (verified in a user namespace, kernel
   7.0) **[measured]**:

   `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile`

   - **The kernel rejects `redirect_dir=off` (`EINVAL`) in a user namespace.**
     `nofollow` gives the semantics needed: redirects are neither created nor
     followed.
   - **Always use `userxattr`, even as real root**, so that `layers/` has a
     single format (`user.overlay.*`). In a user namespace the kernel enables
     `userxattr` even when it is not passed. Verified as real root: the full
     option set mounts. Root without `userxattr` accepts `redirect_dir=off` and
     uses `trusted.overlay.*`. With `userxattr`, `redirect_dir=off` still gives
     `EINVAL`, even as root.
   - The cost of `userxattr`: `user.overlay.*` xattrs already present in layer
     content are hidden; with `trusted` they are kept. This case is rare, so a
     single format still wins.
   - **`volatile` is required for performance.** Without it every copy-up is
     fsynced: a step that modifies 2,000 `.js` files and `touch`es every `.json`
     takes 12.9 s instead of 0.37 s ([experiment 06][e06]) **[measured]**.
     Losing crash safety does not matter, because `builds/<id>` is deleted after
     a crash anyway.
   - **A `volatile` mount leaves `work/work/incompat/volatile` behind.**
     Remounting the same upper/work pair gives `EINVAL`; using the old upper as
     a lower still works. So every layer cut uses a fresh `work-k`, or deletes
     that directory before remounting (verified on ext4, xfs and btrfs:
     [experiment 11][e11]) **[measured]**.
   - **The root directory of `diff-k` must carry the metadata of the lower root
     directory** (mode, uid, gid, xattrs). If it is created with `mkdir`, the
     image's `/` changes, for example from 555 to 755 ([experiment 09][e09])
     **[measured]**.
   - Mount with `fsopen`/`fsconfig(lowerdir+)`/`fsmount`, not with `mount(2)` or
     `mount(8)` (§3.4).
3. Run the instruction:
   - `RUN`: `pivot_root` into `rootfs/` in its own namespaces ([02][s02] §3);
   - `COPY`/`ADD`: write through `rootfs/`, so symlinks at the destination
     resolve exactly as in kaniko.
4. `umount`. `diff-k` is the diff: there is no walk of **the whole rootfs** and
   no `snapshot.init`. But `diff-k` **is still walked**, and each entry is
   checked against the lowers, as BuildKit's `WriteUpperdir` does (§3.3): drop
   whiteouts for names absent from the lowers, and drop parent directories that
   were copied up without changes.
5. Metadata instructions (`ENV`, `LABEL` and so on) mount nothing and produce no
   layer.

One mount can be kept across several consecutive instructions and remounted only
when a layer must be cut. That is a later optimization.

### 3.2 Create layers only when needed (lazy export)

BuildKit's approach: the diff already sits on disk, and `diff-k` is turned into
tar + compression + digest only when the layer is **actually used**. A layer is
used when:

- its stage is the pushed stage or the `--target`;
- `--cache` is on, so the layer must be pushed to the cache repository;
- a tar or OCI layout of that stage must be written.

A stage used only through `COPY --from` or `FROM <stage>` **never needs a tar or
compression**. This is exactly the hypothesis of kaniko optimization notes
docs/30 §5.4 (external). kaniko cannot do this because it always snapshots. The
upper bound of the gain measured on W3 was about 13.6 s out of 42.1 s
**[measured]**. With overlay, the hypothesis becomes the default.

### 3.3 Converting a diff into an OCI layer

Upperdir behaviour was verified in [experiment 01][e01], which also read how
BuildKit does it (`util/overlay/overlay_linux.go`) **[measured]**.

| In `diff-k` | In the OCI tar |
|---|---|
| char device 0/0 (an overlay whiteout) | If a lower has that name: `.wh.<name>`. If not: dropped. **Several whiteouts share one inode** (`nlink=4`), so whiteouts must be handled **before** detecting hardlinks by inode |
| directory with `user.overlay.opaque=y` | If a lower has that directory: the directory + `.wh..wh..opq`. If not: a plain directory. Overlay also marks a directory opaque when it is created and then renamed |
| directory present in a lower with identical metadata (a parent directory that was copied up) | dropped, like BuildKit's `sameDirent` |
| xattrs `user.overlay.*` (`origin`, `impure`, `uuid`) and `trusted.overlay.*` | **dropped**. `security.capability` and other xattrs are kept |
| `user.overlay.redirect` | error (must not occur, because of `redirect_dir=nofollow`) |
| files, symlinks, hardlinks within the same upper | kept as they are |

In the other direction, when extracting a base or cache layer into `layers/<id>`
(verified in a user namespace) **[measured]**: `.wh.x` → `mknod c 0 0`,
`.wh..wh..opq` → `user.overlay.opaque=y`. `user.overlay.*` xattrs already
present in layer content are hidden by overlay and need escaping as
`user.overlay.overlay.*` (kernel ≥ 6.7). This case is rare.

The tar is streamed straight into the parallel compressor (kaniko optimization
notes, docs/28, external).

### 3.4 Overlay constraints

| Constraint | Handling |
|---|---|
| Upper/work cannot live on overlayfs. In a container, the container's rootfs is usually overlay | Require the workdir on a volume (ext4/xfs/btrfs) or tmpfs. Detection (§1) catches this and switches to kaniko mode |
| Lowerdir limits (measured): at most **500 layers**. `mount(2)` truncates the option string at 4,096 bytes and returns **`ENOENT`** (easy to misdiagnose). `fsconfig("lowerdir", string)` is limited to 256 bytes. `mount(8)` breaks at about 450 layers | Use `fsconfig(lowerdir+)` per layer (kernel ≥ 6.8): independent of path length. Only older kernels need `l/` and `mount(2)`. Measured on kernel 5.15 ([experiment 13][e13]): `lowerdir+` returns `EINVAL`, while ranikust's option set through `mount(2)` still works. Near 500 layers, merge old diffs into one temporary layer |
| Copy-up: a file rewritten with unchanged content, or with only mtime/permissions changed, still enters the layer (verified: after `chmod` on an 8 MB file the upper holds all 8 MB). kaniko compares hashes and drops such files | Accepted; BuildKit does the same (BuildKit filters only parent directories). Optionally compare hashes with the lower at export to filter unchanged files |
| **Copy-up breaks hardlinks.** Writing in place to `a` (hardlinked to `b` in a lower) changes only `a`; `b` keeps the old content ([experiment 01][e01]). In kaniko mode, `a` and `b` both change. On W1 ([experiment 06][e06]): `touch` on every `.json` in `node_modules` gave kaniko mode 1,813 extra files in the pnpm store (same inode, new mtime). `sed -i` creates a new inode and causes no difference | **The two modes can differ in metadata, in hardlink structure and, for in-place writes, in content**, not just in digest. Document it for users. Cross-mode content tests need a list of accepted differences ([ADR-0012][adr12]) |
| `rename(2)` of a lower directory returns `EXDEV` | Verified with npm, pnpm, apt/dpkg, pip, yarn 1 and 4, and cargo: all results correct ([experiment 10][e10]). npm and pip hit `EXDEV` and fall back to copying; the other tools never rename lower directories. **Accepted (2026-10-29):** `redirect_dir` stays off; programs that do not handle `EXDEV` fail in overlay mode. On an `EXDEV` failure the tool suggests re-running with `--snapshot-mode=kaniko` ([ADR-0016][adr16]) |
| Overlay nests at most 2 deep (`FILESYSTEM_MAX_STACK_DEPTH`; measured: depth 3 fails, an upper on overlay fails). A lower on the container's overlay rootfs works | Containers that already run on nested overlay break. Detection (§1) catches it |
| Upper filesystem | Verified on ext4, xfs, btrfs and tmpfs: identical behaviour ([experiment 11][e11]). NFS and network filesystems cannot hold the upper |
| `trusted.*` xattrs need `CAP_SYS_ADMIN` in the initial user namespace | In user-namespace mode, mount with `userxattr` |
| Deleting a stage rootfs | Just `umount` and delete `diff-*`. No walk-and-delete of the rootfs as in `DeleteFilesystem` |

### 3.5 Measurements: overlay vs walk ([experiment 06][e06])

W1 with a `node` base and 5 `RUN` instructions (`npm i -g pnpm`, `pnpm install`
of 242k files, deleting 9,912 files, modifying and `touch`ing files in a lower,
creating 5,000 files). The table shows overlay in a user namespace on ext4,
median of 3 rounds **[measured]**. The re-run as real root
([experiment 06a][e06a]) points the same way: ext4 total 6.50 → 5.26 s (−19%),
tmpfs 5.43 → 3.70 s (−32%), 4 CPUs 7.89 → 6.49 s (−18%); without `volatile` the
copy-up step still took 13 s. The walk is an **already optimized** Go
implementation: parallel, with a stat-based cache, HighwayHash.

| | overlay `volatile` | walk, 32 workers | walk, 4 workers, `--cpus=4` | overlay, `--cpus=4` |
|---|---|---|---|---|
| Taking the diff, total over 5 steps | **0.55 s** | 2.52 s | 3.09 s | 0.56 s |
| Running the instructions | 3.77 s | 3.24 s | 5.50 s | 6.34 s |
| Total (instructions + diff + sync) | **4.88 s** | 6.35 s | 9.18 s | **7.49 s** |

- Taking the diff is **4.5–5.5 times** faster. The walk costs 0.35–1.0 s at
  **every** step because it visits about 255k entries; overlay only spends time
  on the step that creates many files.
- Overlay makes **the instructions themselves 7–16% slower** (copy-up, lookups
  through many layers).
- A whole build gains **18–23%**, i.e. 1.5–1.7 s on W1. The gain grows with the
  number of `RUN` instructions and the size of the rootfs.
- Full W1 builds on the same machine ([experiment 07][e07]):
  - optimized kaniko 15.6 s, Go walk 11.3 s, overlay 9.5–10.0 s at 32 CPUs;
  - at 4 CPUs, 18.3 / 14.8 / 11.2–12.2 s respectively.

  kaniko loses mainly on path resolution (2.5 s) and its sequential walk (3.2
  s).
- **Requirements for the exporter, found by comparing images**
  ([experiment 07][e07]):
  - a layer must carry headers for every parent directory, or the mode of that
    directory from a lower layer is masked;
  - setting modes must preserve the sticky, setuid and setgid bits.
- **Exporting (tar + gzip) the `pnpm install` layer takes about 2 s in both
  modes.** Lazy export (§3.2) therefore matters as much as how the diff is
  taken. W1 does not benefit from lazy export because its `deps` stage is the
  final image; W3 does.
- The hash algorithm barely matters (xxh ≈ hwh; sha256 is 5–20% slower).
  Dropping the stat-based cache costs another 46%.

## 4. Parallel stages, as in BuildKit

### 4.1 Dependency graph

Edges of the stage DAG:

- `FROM <stage>`;
- `COPY --from=<stage>`, `ADD --from` (if present);
- `RUN --mount=type=bind|cache,from=<stage>`;
- `ONBUILD` of a stage used as a base.

Stages not on the path to `--target` are dropped, as in `MakeKanikoStages`.
**There is no need to squash a parent stage into its child**
(`kaniko@44aa7ddc0 pkg/dockerfile/dockerfile.go:393-406`): in overlay mode,
`FROM <stage>` just reuses the parent stage's lowerdir chain, which is almost
free.

### 4.2 Scheduler

- Each stage is an async task that waits for the stages it depends on. Steps
  that do not touch the rootfs (resolving and fetching bases, cache lookups)
  start for every stage **right away**, in parallel.
- `--jobs N` limits the number of concurrent `RUN` instructions; the default is
  the number of usable cores (`available_parallelism`, Q11, [ADR-0013][adr13]).
  Network fetches and compression have their own limits.
- When one stage fails, the others are cancelled: their pid namespaces are
  killed, unmounted and cleaned up. The exit code comes from the first error, as
  in BuildKit.
- Log lines are prefixed `[stage-name k/n]`. An option groups logs by stage for
  easier reading in CI.
- The output does not depend on execution order: layers, config and manifest
  depend only on content.
- `COPY --from=<stage>` reads straight from the source stage's final rootfs,
  mounted read-only. There is no need to save files into `deps/` and then wipe
  the filesystem as kaniko does
  (`kaniko@44aa7ddc0 pkg/executor/build.go:1941-1969`).
- Later: when two stages have the same lower chain and the same cache key for an
  instruction, run it only once. BuildKit does this at the vertex level (kaniko
  optimization notes, docs/27 §2.1, external: `deps` and `assets` share the
  `FROM` vertex).

### 4.3 Parallel is not automatically faster: it must be measured

Kaniko optimization notes docs/30 §5 (external) measured workload 3
(`nextjs-kaniko/app-par`, 3 independent `pnpm install` stages) **[measured]**:

| Configuration | W3 without cache, 4 CPUs | W3 without cache, 8 CPUs |
|---|---|---|
| BuildKit parallel | 34.6 s | 31.1 s |
| BuildKit `max-parallelism=1` | **29.2 s** | **26.4 s** |
| Difference (parallel − sequential) | **+3.5 s** [+1.3; +12.4] | **+6.1 s** [+3.2; +13.6] |

On that workload, **parallelism made BuildKit slower**: the three `pnpm install`
steps competed for CPU, disk and a shared npm registry. kaniko was 12.4 s slower
than sequential BuildKit, mainly because of snapshotting (docs/30 §5.3 item 3),
not because it lacks parallelism.

**Re-measured on the development machine (native Linux, 32 CPUs), 4 rounds
([experiment 08][e08]) [measured]:**

| W3, wall time | 32 CPUs | `--cpus=4` |
|---|---|---|
| optimized kaniko | 19.91 s | 22.08 s |
| BuildKit parallel / sequential | 9.37 / 12.28 s | 13.98 / 15.37 s |
| Go walk / Go walk skipping intermediate-stage snapshots | 14.32 / 9.58 s | 17.38 / 12.09 s |
| overlay lazy, `--jobs` 1 / 2 / default | 8.44 / 6.52 / **6.02 s** | 11.35 / 10.63 / **10.73 s** |
| overlay exporting every layer, `--jobs` 1 / default | 9.65 / 12.69 s | 13.06 / 12.99 s |

- Here parallel is **faster** than sequential, BuildKit included. The docs/30
  result came from the OrbStack environment.
- Lazy export saves 1.3–2.3 s.
- In walk mode, skipping snapshots of stages used only through `COPY --from`
  saves 4.7–5.3 s (30–33%). This verifies the docs/30 §5.4 hypothesis by
  simulation.
- Exception: parallel **with every layer exported** is 3 s slower at 32 CPUs.
  RAM climbs to 6.4–7.6 GB against an 8 GB limit, so export has to read back
  from disk.

Design consequences:

- The biggest gain of overlay mode is **dropping the snapshot walk and not
  exporting unused layers** (§3.2, §3.5), not parallelism.
- Stage-level parallelism helps (measured in experiment 08). The real constraint
  is **memory**: the scheduler should limit concurrent stages by available RAM
  (cgroup `memory.max` or `MemAvailable`), not only by `--jobs`.
- `--jobs` defaults to the core count (decided, Q11), and experiment 08 supports
  that: no lazy configuration was slower than `--jobs 1`. W3 has only 3 stages,
  so it cannot distinguish "number of cores" from "number of stages".
- Parallelism clearly helps when stages use different bases over a real network
  (overlapping downloads), or when stages are bound by network I/O rather than
  CPU. This has not been measured.

## 5. Kaniko mode (without mount privileges)

### 5.1 Baseline

The baseline design is [02][s02] §2–§4: the workdir is the rootfs, walk + hash
with all the optimizations of kaniko optimization notes docs/12–28, and parallel
prefetching (option A of docs/30: parallelize the non-`RUN` work of independent
stages). Per §3.5, a parallel walk and a stat-based cache are mandatory.

### 5.2 Parallel stages in kaniko mode

**[Experiment 12][e12], [measured].** Each stage can have its own rootfs
directory under the workdir and run along the DAG as in overlay mode. The images
are identical to the sequential ones. On W3 at 32 CPUs, parallel is 41–44%
faster than sequential (14.04 → 7.99 s); with lazy export it drops to 5.38 s,
faster even than overlay lazy (6.29 s). W1 barely benefits. How `RUN` runs when
the rootfs is a subdirectory depends on privileges:

| Privileges | `RUN` in a subdirectory | `/proc`, `/dev` | Proposal **[inferred]** |
|---|---|---|---|
| Root, can mount but cannot use overlay (workdir on NFS/FUSE, upper on the container's overlay, kernel without overlay) | Full sandbox as in overlay mode: bind `/proc` and `/dev`, `pivot_root` | Yes | **Parallel by default** |
| Root, cannot mount (default Docker, ordinary Kubernetes pod) | Bare `chroot` (needs `CAP_SYS_CHROOT`, which is in the default capability set) | **No `/proc`.** `/dev` is available if `mknod` works: on rootful Docker it does (measured on real Docker 29.1, every `docker run` variant, [experiment 13][e13]); on rootless podman it does not | **Sequential on `/` by default**, as in kaniko, with a full `/proc` and `/dev` (decided: Q12, [ADR-0014][adr14]). The `--parallel-stages` flag turns on parallel stages |
| Same as above | proot (ptrace; AppArmor `docker-default` does not block it, experiment 13) | Yes | Not in 1.0: +26–56% wall time, apt hangs without `APT::Sandbox::User=root`, and it is a third binary under GPL-2.0 ([ADR-0018][adr18]) |

### 5.3 Requirements when the rootfs is a subdirectory

- Build the rootfs by extraction or `cp -a` (on xfs/btrfs, `cp -a` reflinks
  automatically, so each stage costs only a few MB). **Never use a hardlink
  farm**: `>>`, `chmod`, `chown` or `touch` in one stage would also modify the
  store and the other stages ([ADR-0017][adr17]).
- Restore the parent-directory metadata (the mtime of `/etc`) for everything the
  tool places in the rootfs, such as hosts and resolv.conf. Otherwise `/etc`
  leaks into every layer.
- Under a bare `chroot`, commands that read `/proc` can **return wrong results
  without an error**: `ps` is empty, `os.cpus().length == 0`. The tool must log
  a warning.
- A bare `chroot` cannot stop a malicious `RUN` from reading or writing another
  stage's rootfs (measured). One stage's secrets leak to the others.
- The scheduler limits by memory, page cache included: a parallel walk of W3
  needs about 8.2 GB.
- With `--parallel-stages`, log at build start that `RUN` has no `/proc`.

### 5.4 A fake `/proc`

**Some `/proc` files can be copied into `rootfs/proc`** (a "fake `/proc`",
[experiment 12a][e12a], **[measured]**). It is a static snapshot of 13 files,
put in place before each `RUN` and removed afterwards, at a cost of 11–27 ms:

- `/proc/{stat,cpuinfo,meminfo}`
- `/proc/self/{cgroup,mountinfo,stat}`
- `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max,memory.high}`
- `/sys/devices/system/cpu/{online,possible,present}`

**What it fixes:**

- `next build` fails immediately without `/proc/self/stat`. With the fake
  `/proc` it completes in 10.88 s, against 10.95 s with a real `/proc`.
- Tools that split work by `os.cpus()`, when `os.cpus()` returns 0: jest 29.3 is
  9.5 times slower, terser-webpack-plugin 5.3.10 is 3.5 times slower.
- Tools that ignore the cgroup quota under `--cpus=4`: jest uses 20 workers
  instead of 3; webpack is 51% slower; the V8 heap exceeds `--memory`.

**What it does not fix:**

- `/dev/fd`: `<(...)`, `/dev/stdin`.
- `/proc/self/exe`: **Java does not start**, and neither does any binary that
  relies on `$ORIGIN`.
- `ps` and `/proc/<pid>`.
- Live values: the content is a snapshot and is never updated.

**The file list depends on the cgroup version** (measured on real Docker,
[experiment 13][e13]). The set above is for cgroup v2. Under cgroup v1 the v2
set has no effect; the tool must use
`/sys/fs/cgroup/cpu,cpuacct/cpu.cfs_{quota,period}_us`,
`/sys/fs/cgroup/memory/memory.{limit,soft_limit}_in_bytes` and the v1 versions
of `/proc/self/{cgroup,mountinfo}`. The tool must therefore derive the list from
`/proc/self/{cgroup,mountinfo}` at run time, not hard-code it.

### 5.5 Results with real builds

All results in this section are **[measured]**.

- **`next build` of the W1 app** ([experiment 14][e14]): without `/proc` it
  fails after 0.5 s. With the fake `/proc` it runs correctly, and the `.next`
  output is identical to the run with a real `/proc` (2,032 files, after
  removing the random build ID). The worker count is the same too.
- **Java depends on how the image installs the JDK** (experiment 14). Temurin
  images (`maven:*-eclipse-temurin`, `gradle:*-jdk*`) work, even without
  `/proc`, because the image has already run `ldconfig` for the JDK's libraries.
  A JDK installed with apt (Debian) or apk (Alpine) does not start, and the fake
  `/proc` does not fix it.
- **Does the JVM honour `--cpus`/`--memory`?** (real Docker,
  [experiment 16][e16]):
  - Newer JDKs (21.0.12+, 17.0.20+) detect cgroups through the filesystem type
    of `/sys/fs/cgroup`. Under cgroup v2 the fake files fail that check, so the
    JVM ignores the limits. Under cgroup v1 the fake `/proc` fixes it.
  - Older JDKs (21.0.8, 17.0.12) read `/proc/cgroups`, so the fake `/proc` fixes
    them under both v1 and v2.
- **TypeScript 7 (the Go port) fails at start-up**, because it reads
  `/proc/self/exe` (experiment 14). tsc 5, esbuild and vite work.
- **The final file set: 14 files per cgroup version** (experiment 16):
  - **Common:** `/proc/{stat,cpuinfo,meminfo,cgroups}`,
    `/proc/self/{cgroup,mountinfo,stat}`,
    `/sys/devices/system/cpu/{online,possible,present}`.
  - **v2:** `cgroup.controllers`, `cpu.max`, `memory.max`, `memory.high`.
  - **v1:** `cpu.cfs_{quota,period}_us`, `memory.{limit,soft_limit}_in_bytes`.
  - If any file of the set is missing, Node loses the quota.
  - The directory of the cgroup files is derived from
    `/proc/self/{cgroup,mountinfo}` at run time.
  - No cpuset file is needed: the CPU mask still applies through `chroot`.
  - **[inferred]** Kubernetes with cgroup v1 may also need `memory.stat`.
- **Java under a memory limit** ([experiment 15][e15], a project of 3,040
  classes):
  - **Real OOM.** At `--cpus=4 --memory=2g`: with a real `/proc`, maven and
    gradle both pass; without `/proc` or with the fake `/proc`, they **fail 100%
    of the time**. Maven is OOM-killed, gradle dies out of heap.
  - Cause: the JVM sees 32 CPUs, so it starts 32 forks or workers, and picks a
    7.6 GB heap.
  - **Proposal:** when running in parallel, under a cgroup limit, in a stage
    that has a JVM, the tool adds this to the `RUN` environment
    ([ADR-0019][adr19]):

    ```
    JAVA_TOOL_OPTIONS=-XX:+IgnoreUnrecognizedVMOptions -XX:ActiveProcessorCount=N -XX:MaxRAM=<limit> <the user's value>
    ```

    - At 2 GB this fixed 16/16 runs.
    - Below about 1.75 GB gradle still hits OOM, because the JVM picks G1
      instead of SerialGC. A GC flag cannot be injected: the JVM stops if the
      user has already chosen a GC. Only a warning is printed.
    - Side effect: every JVM prints an extra `Picked up JAVA_TOOL_OPTIONS` line,
      which breaks scripts that read the first line of `java -version`. The
      image config is not affected.
- **Toolchains that download another JDK** (gradle foojay, maven-toolchains): a
  downloaded JDK 17 loads Temurin 21's `libjli.so` from the ld cache by mistake,
  and the build fails (`NoSuchMethodError`). The reverse (JDK 17 image, JDK 21
  toolchain) runs but uses the wrong launcher library, **without an error**.
- **Next 16.4 and Next 15.5 `--turbopack`:** without `/proc` they fail; with the
  fake `/proc` 12/12 runs are correct and the output is identical. Turbopack's
  native binary reads only files from the 14-file set.
- **Several stages at once, each with its own fake `/proc`**
  (`ovbuild -fakeproc`, 1 ms per `RUN`): three stages running `next build`, jest
  and webpack in parallel. No snapshot mix-up between stages (27/27). No fake
  file leaked into a layer. The image is identical to the sequential one. 22%
  faster; peak RAM 7.2 against 6.2 GB.
- Without `/proc/self/fd`, the JDK closes every fd up to the 524,288 limit each
  time it spawns a process: 5.2 million `close` calls against 3.3 thousand.
  Without strace this costs almost nothing.
- **Do not fake `/proc/self/exe`, and do not set
  `LD_ORIGIN_PATH`/`LD_LIBRARY_PATH`**: each of these gives wrong results
  without an error (measured).
- Warn before the build if a stage has `bin/java` but `libjli.so` is not in the
  loader cache.

Never fake `/proc/self/{maps,exe,fd,status}` or `/proc/sys`. The snapshot walk
always skips `/proc`, `/sys` and `/dev`. **[inferred]** If a fake `/proc` is
implemented, enable it automatically with `--parallel-stages`.

### 5.6 Shared code and the `StageRootfs` trait

The hash is HighwayHash or xxh3; the choice does not matter. Because this is the
mode of most CI pods (§1), its speed determines the experience of most users. It
shares the parser, registry, layer/compression, cache and builder crates with
overlay mode. Only the `Snapshotter` and `StageRootfs` traits differ:

```rust
trait StageRootfs {            // overlay: overlay mount; kaniko: shared `/` or one directory per stage
    fn path(&self) -> &RootPath;
    fn begin_layer(&mut self) -> Result<()>;
    fn end_layer(&mut self) -> Result<LayerDiff>;   // overlay: diff-k; kaniko: walk + hash
    fn release(self) -> Result<()>;                 // overlay: umount; kaniko: wipe the filesystem or delete the directory
}
```

The scheduler reads the capabilities of the `StageRootfs`. When the rootfs is
`/`, only one stage may touch the rootfs at a time. When each stage has its own
directory, stages run along the DAG as in overlay mode.

## 6. Cache keys across the two modes

The cache key is computed from the instruction, the context and the chain of
keys of the previous instructions; it does not depend on the snapshot mode. A
cache layer produced by one mode can be used by the other. The modes differ only
in "unchanged" files from copy-up and in split hardlinks (§3.4).

## Open questions

- Whether parallel stages help when stages use different bases over a real
  network, or are bound by network I/O, has not been measured (§4.3).
- W3 has only 3 stages, so it cannot distinguish "number of cores" from "number
  of stages" as the `--jobs` default (§4.3).
- Whether Kubernetes with cgroup v1 needs `memory.stat` in the fake `/proc` is
  **[inferred]** and untested (§5.5).
- Whether the fake `/proc` is enabled automatically with `--parallel-stages` is
  undecided (§5.5).

## References

1. Experiments: [01][e01], [02][e02], [03][e03], [06][e06], [06a][e06a],
   [07][e07], [08][e08], [09][e09], [10][e10], [11][e11], [12][e12],
   [12a][e12a], [13][e13], [14][e14], [15][e15], [16][e16].
2. kaniko at `44aa7ddc0`: `pkg/dockerfile/dockerfile.go:393-406`,
   `pkg/executor/build.go:1941-1969`.
3. kaniko optimization notes (external, not in this repository): docs/12–28
   (optimizations), docs/27 workload1-gap (§2.1), docs/28 stream-tar-compress,
   docs/30 parallel-stages-a (§5, §5.3, §5.4).
4. BuildKit `util/overlay/overlay_linux.go` (`WriteUpperdir`, `sameDirent`),
   <https://github.com/moby/buildkit> **[external]**, accessed 2026-10-29.
5. Survey chapters: [02 kaniko-mode requirements][s02], [05 Decisions][s05].

[s02]: 02-kaniko-mode-requirements.md
[s05]: 05-decisions.md
[e01]: ../experiments/01-overlay-semantics.md
[e02]: ../experiments/02-run-sandbox.md
[e03]: ../experiments/03-privilege-matrix.md
[e06]: ../experiments/06-overlay-vs-walk.md
[e06a]: ../experiments/06a-root-rerun.md
[e07]: ../experiments/07-kaniko-vs-overlay.md
[e08]: ../experiments/08-parallel-stages-lazy-export.md
[e09]: ../experiments/09-kaniko-mode-prototype.md
[e10]: ../experiments/10-exdev-package-managers.md
[e11]: ../experiments/11-root-extras.md
[e12]: ../experiments/12-kaniko-mode-parallel.md
[e12a]: ../experiments/12a-fake-proc.md
[e13]: ../experiments/13-docker-rootful-vm.md
[e14]: ../experiments/14-real-builds-without-proc.md
[e15]: ../experiments/15-java-next-concurrency.md
[e16]: ../experiments/16-cgroup-v1-cpuset.md
[adr2]: ../../adr/0002-two-snapshot-modes.md
[adr12]: ../../adr/0012-accept-divergence-between-modes.md
[adr13]: ../../adr/0013-jobs-default-available-parallelism.md
[adr14]: ../../adr/0014-kaniko-mode-sequential-by-default.md
[adr16]: ../../adr/0016-overlay-mount-options.md
[adr17]: ../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md
[adr18]: ../../adr/0018-no-proot-in-1-0.md
[adr19]: ../../adr/0019-java-tool-options-on-parallel-path.md
[d-modes]: ../../design/modes-and-detection.md
[d-overlay]: ../../design/overlay-snapshot.md
[d-sched]: ../../design/scheduler.md
[d-kaniko]: ../../design/kaniko-mode.md
[d-cache]: ../../design/cache-and-store.md
