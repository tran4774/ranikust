# Glossary

**TL;DR:** One line per term, alphabetical. The Ref column points to where
the term is defined or measured; short forms as in [README.md](README.md#conventions-in-these-files).

## Terms

| Term | Meaning | Ref |
|---|---|---|
| accepted differences | The four ways overlay and kaniko mode may differ: hardlink splits, metadata on hardlinked files, link-count-only changes, unchanged copied-up files | `ADR-0012` |
| ADR | Architecture Decision Record in `docs/adr/`; an accepted body is never edited, only superseded | [`../adr/README.md`](../adr/README.md) |
| bounding set | Capability bounding set; cut to Docker's 14 defaults for every `USER`, or a setuid binary regains dropped capabilities | `design/sandbox.md#per-run-sequence`, `exp-02` |
| BuildKit | Docker's builder (moby/buildkit): the reference for Dockerfile semantics and the upperdir differ, and a comparison oracle | [refs.md](refs.md#upstream-code) |
| `BuildPaths` | `{ root, state }` value, defined in `rnk-store`, passed down instead of kaniko's global `RootDir` and `KanikoDir` | `ARCHITECTURE.md#no-global-state` |
| copy-up | Overlay copies a whole lower file into the upper on its first write or metadata change; splits hardlinks | `exp-01`, `ADR-0012` |
| cross-repo mount | Distribution API blob mount from another repository of the same registry instead of an upload; falls back to uploading | `rnk-registry` |
| diffID vs digest | diffID: sha256 of the uncompressed layer tar (config `rootfs.diff_ids`); digest: sha256 of the stored, usually compressed, blob (manifest) | OCI image-spec |
| Docker tar | `docker save`-style archive written by `--tar-path`; Docker v2s2, so no zstd layers | [kaniko-map.md](kaniko-map.md#flags) |
| Docker v2s2 | Docker image manifest v2, schema 2 media types; what `--tar-path` writes and what kaniko's `--reproducible` switches to | [refs.md](refs.md#specs) |
| EXDEV | `rename(2)` of a directory from a lower layer fails with it under `redirect_dir=nofollow`; accepted, ranikust hints "rerun with `--mode=kaniko`" | `ADR-0016`, `ADR-0023`, `exp-10` |
| fake `/proc` | Static snapshot of 14 `/proc` and `/sys` files per cgroup version, written into a stage root on the `--parallel-stages` path | `design/kaniko-mode.md#fake-proc`, `exp-12a`, `exp-16` |
| flattened content | An image reduced to `(path, type, mode, uid, gid, size, sha256, link, xattrs)` plus its config; what comparisons diff | `docs/testing.md` |
| ignore list | Paths the kaniko-mode walk and deletion skip: mount points under the root, `--ignore-path`, `/proc`, `/sys`, `/dev`, the state directory inside the root | `design/kaniko-mode.md#ignore-list` |
| `Isolation::Direct` | `rnk-sandbox` variant for sequential kaniko mode: `RUN` runs directly on `/`, without namespaces, as in kaniko; `[design]` | `design/sandbox.md#variants-by-environment` |
| `JAVA_TOOL_OPTIONS` | Injected on the `--parallel-stages` path under cgroup limits so the JVM sizes heap and CPUs correctly | `ADR-0019` |
| `--jobs` | Maximum concurrent `RUN`s across stages; defaults to `available_parallelism()` | `ADR-0013` |
| kaniko | The Go image builder ranikust replaces, cited at `44aa7ddc0` | [kaniko-map.md](kaniko-map.md) |
| kaniko mode | Snapshot by walking and hashing the root; the workdir is the root (`/` in a container); sequential by default | `ADR-0002`, `ADR-0014` |
| `LayeredMap` | kaniko's per-layer path → hash map used to diff; ported for kaniko mode | `kaniko@44aa7ddc0 pkg/snapshot/layered_map.go:45` |
| lazy export | A layer is turned into tar, compressed and digested only if pushed, cached, written out or in the target | `design/scheduler.md#lazy-export`, `exp-08` |
| lazy snapshot | kaniko mode does not walk a stage whose layers nobody uses | `design/kaniko-mode.md#lazy-snapshots` |
| lowerdir | Read-only overlay layers, topmost first; at most 500 | `design/overlay-snapshot.md#mounting` |
| `lowerdir+` | `fsconfig` key adding one lower layer (new mount API); probed by a real create, else `mount(2)` with short `l/` names | `design/overlay-snapshot.md#mounting` |
| MADR | Markdown Any Decision Records 4.0, short variant plus an Evidence section: the ADR format | `docs/adr/template.md` |
| manifest list | OCI image index or Docker manifest list: one manifest per platform; the base is picked by platform, never by host | `ADR-0009` |
| marker file | `.ranikust-root`; a non-empty workdir without it is refused | `design/kaniko-mode.md#host-safety-checks` |
| metacopy | Overlay option to copy up only metadata; ranikust mounts with `metacopy=off` | `ADR-0016` |
| `mnt_id` | Mount id from `statx`; deletion stops where it changes (`st_dev` misses binds of the same filesystem) | `exp-09` |
| `--mode` | Selects the snapshot mode: `auto` (probe, default), `overlay` or `kaniko`; a forced mode still runs its checks | `ADR-0023` |
| `MS_MOVE` | Mount move; `mount(root, "/", MS_MOVE)` + `chroot(".")` replaces `pivot_root` where seccomp blocks it | `design/sandbox.md#per-run-sequence` |
| OCI layout | Directory image format (`oci-layout`, `index.json`, `blobs/`) written by `--oci-layout-path`; also a cache target | [refs.md](refs.md#specs) |
| opaque dir | Directory hiding all lower contents: overlay `user.overlay.opaque=y` ↔ OCI `.wh..wh..opq` | `design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer` |
| overlay mode | Each instruction writes into a fresh upperdir, which is the diff; stages run in parallel; needs mount privileges | `ADR-0002`, `design/overlay-snapshot.md` |
| `--parallel-stages` | Opt-in parallel stages in kaniko mode without mounts: a directory per stage, bare `chroot`, fake `/proc`; placeholder name | `ADR-0014` |
| PDEATHSIG | `prctl(PR_SET_PDEATHSIG, SIGKILL)`: the child dies with its parent *thread*, hence the spawner thread | `design/sandbox.md` |
| `pivot_root` | Swaps the root mount for `RUN`; root escapes a bare `chroot` | `exp-02`, `exp-06a` |
| probe | Start-up checks: each crate performs its own privileged steps for real (`rnk-sandbox`, `rnk-snapshot`); `ranikust` runs them in order and picks overlay root, overlay rootless, kaniko, or refuse | `design/modes-and-detection.md#probe-order---modeauto` |
| Q1–Q12 | The survey's decision questions, now `ADR-0003` to `ADR-0014` | `docs/research/survey/05-decisions.md` |
| `redirect_dir` | Overlay directory-rename feature; ranikust uses `nofollow` (`off` is refused in a user namespace), so lower-directory renames give `EXDEV` | `ADR-0016` |
| `rnk-store` | Crate owning the store in the state directory, its locks, clean-up, `prune` and `BuildPaths` | [crates/rnk-store.md](crates/rnk-store.md), `ADR-0021` |
| rootless overlay | Overlay mode inside a user namespace (`userxattr`); Phase 3 | `design/modes-and-detection.md#outcomes` |
| `RootPath` | `rnk-rootfs` handle for paths in a root, resolved with `openat2(RESOLVE_IN_ROOT)` so `..` and symlinks cannot escape | [crates/rnk-rootfs.md](crates/rnk-rootfs.md) |
| `--snapshot-mode` | kaniko's flag, same meaning: what the kaniko-mode walk compares (`full`, `redo`, `time`); ignored with a warning in overlay mode | `ADR-0023` |
| `Snapshotter` | `rnk-snapshot` trait that yields an instruction's diff; overlay and kaniko implementations; signature `[open: Q-024]` until Phase 1 task 9 | `ARCHITECTURE.md#rnk-snapshot` |
| spawner thread | The one thread that forks every `RUN` child, alive for the whole build | `design/sandbox.md#process-set-up-once-per-build` |
| stage squashing | kaniko folds a base stage into its only child; ranikust does it only in sequential kaniko mode | [kaniko-map.md](kaniko-map.md#behaviour-to-match) |
| `StageRootfs` | `rnk-snapshot` trait for a stage's root: a shared `/` runs alone, a private directory may run in parallel | `design/kaniko-mode.md#variants` |
| stat cache | kaniko-mode hash cache keyed on `{dev, ino, size, mtime, ctime, mode, uid, gid}`, with a 2 s racy window | `design/kaniko-mode.md#walk-hash-and-diff` |
| state dir | `--state-dir`, ranikust's `/kaniko`, one for every mode: the store, `builds/<id>`, stub journal, `deps/`, stage images. Default `<workdir>/.ranikust` (overlay), `/ranikust/state` (kaniko, on the ignore list) | `ADR-0022`, [overview.md](overview.md#where-state-lives) |
| store | Persistent, content-addressed `blobs/`, `layers/`, `caches/` in the state directory, shared by builds, owned by `rnk-store`; per-digest `flock`, atomic rename | `ADR-0006`, `ADR-0007`, `ADR-0021` |
| stub mountpoint | Empty file or directory created in a root as a bind target (`/etc/resolv.conf`); journaled first, removed before the diff | `design/sandbox.md#stub-mount-points` |
| T0–T3 | Test tiers: no privileges, user namespaces, root in a VM, CI | [workflow.md](workflow.md#test-tiers) |
| upperdir | Writable overlay layer; one per instruction (`diff-k`), which is that layer's diff | `design/overlay-snapshot.md#one-layer-per-instruction` |
| `userxattr` | Overlay keeps its metadata in `user.overlay.*` xattrs; needed in a user namespace, used everywhere so `layers/` has one format | `ADR-0016` |
| `volatile` | Overlay option that skips fsync on copy-up; required for speed; leaves a dirty marker, so an upper/work pair is never remounted | `design/overlay-snapshot.md#the-volatile-lifecycle`, `exp-06` |
| W1, W2, W3 | `nextjs-kaniko` workloads: W1 Next.js app (`app/`), W2 cache paths (`app-cache/`), W3 three independent `pnpm install` stages (`app-par/`) | `docs/research/README.md#workloads` |
| whiteout | Deletion marker: overlay character device 0/0 ↔ OCI `.wh.<name>` | [refs.md](refs.md#specs) |
| work dir (overlay) | Overlay's own scratch directory (`workdir=`, `work-k`), not `--workdir` | `design/overlay-snapshot.md#state-directory-layout` |
| workdir | `--workdir`, the build area: holds the default state directory in overlay mode; is the root itself in kaniko mode, locked exclusively | [overview.md](overview.md#where-state-lives) |
