# kaniko today: size, building blocks, and what is hard-wired to `/`

| | |
|---|---|
| **Status** | Frozen, 2026-10-29 |
| **Original** | `docs-rust/01-kaniko-hien-tai.md` (Vietnamese, not in this repository) |
| **Superseded by** | [`docs/design/kaniko-mode.md`][d-kaniko] for the parameterization in §3. The rest is background and is not maintained. |

## Summary

- **How much code does a rewrite replace?** About 24 thousand lines of non-test
  Go, 68 feature flags, 71 CLI flags and 201 integration Dockerfiles. The line
  count overstates the work in one direction (metrics, tracing and
  feature-flagged optimizations need no port) and understates it in another
  (kaniko borrows its Dockerfile parser, image library and `.dockerignore`
  matcher, and the Rust equivalents are weaker or missing).
- **Which blocks are hard to port?** The Dockerfile parser and variable
  expansion are hard. Commands, snapshotting, extraction, the executor, registry
  I/O and caching are medium. Build contexts, bake and observability are low.
- **What ties kaniko to `/`?** More than the `RootDir` variable: `RUN` runs
  without chroot or namespaces, executables and users are looked up on the host,
  the ignore list comes from the process's own mountinfo, and leftover processes
  are cleaned up by process group. Each place needs a specific fix in kaniko
  mode.
- **What carries over?** In kaniko mode, kaniko's flow ports almost unchanged
  (snapshot, filesystem, extract, ignore list). The `RUN` sandbox and fd-based
  filesystem access are new, and both are shared with overlay mode.
- **Which kaniko optimizations should Rust adopt from day one?** Parallel
  deterministic compression, streaming tar into the compressor, one compressed
  blob shared by image and cache, parallel context hashing, pipelined
  extraction, stat-based hash caching (kaniko mode only), and parallel zstd.

## 1. Size of what would be rewritten

Evidence: **[code]**, measured on kaniko branch `bench-metrics` at `44aa7ddc0`.

| Metric | Value | How it was counted |
|---|---|---|
| Go excluding tests, `pkg/` + `cmd/` | 24,133 lines | `find pkg cmd -name '*.go' ! -name '*_test.go' \| xargs wc -l` on `bench-metrics` |
| `FF_KANIKO_*` feature flags | 68 | `grep -o 'FF_KANIKO_[A-Z_]*' pkg/config/featureflags.go \| sort -u` |
| CLI flags registered in `root.go` | 71 | `grep -c 'Flags()\.' cmd/executor/cmd/root.go` |
| Integration-test Dockerfiles | 201 | `ls integration/dockerfiles` |
| Largest files | `pkg/executor/build.go` 2,506 lines, `pkg/util/fs_util.go` 1,821 lines | `wc -l` |

The figure of 24 thousand lines **overstates** the work for Rust:

- About 3–4 thousand lines are metrics, tracing and the feature-flagged
  optimizations of the `bench-metrics` branch (kaniko optimization notes,
  docs/12–28, external). The Rust version needs no feature-flag mechanism; it
  only needs the right design from the start. **[inferred]**
- Some code becomes simpler with root: `swapDir` for cache mounts is replaced by
  a bind mount, and `tini` plus `kill(-pgid)` are replaced by a pid namespace
  (§3). The ignore list and `DeleteFilesystem` are still needed, because the
  model is still "one rootfs, wiped between stages".
- In the other direction, kaniko **does not implement** most of what it uses.
  The Dockerfile parser and linter come from BuildKit; image, registry, tarball
  and layout handling come from `go-containerregistry`; `.dockerignore` matching
  comes from `moby/patternmatcher` (kaniko optimization notes,
  docs/01-overview/04-package-map.md §3.4, external). The corresponding Rust
  libraries are weaker or do not exist ([03][s03] §3), so this part has to be
  written.

## 2. Building blocks and porting difficulty

The difficulty column is **[inferred]**.

| Block (Go) | What it does | Difficulty of a Rust rewrite |
|---|---|---|
| `pkg/dockerfile` + BuildKit parser | Parsing, directives (`syntax`, `escape`), heredocs, `ARG`/`ENV` expansion with shell-word rules, stage splitting, `--target`, stage squashing, `ONBUILD` | **High.** No Rust parser has been verified as BuildKit-compatible. Variable expansion (`${a:-b}`, `${a:+b}`, escapes) has many edge cases |
| `pkg/commands` (19 instructions) | `RUN`, `COPY`/`ADD` (`--chown`, `--chmod`, `--from`, `--link`, URLs, auto-extracted tars), metadata (`ENV`, `LABEL`, `USER`, `WORKDIR`, `EXPOSE`, `VOLUME`, `ENTRYPOINT`, `CMD`, `SHELL`, `HEALTHCHECK`, `STOPSIGNAL`, `ONBUILD`, `ARG`) | Medium. Metadata is easy; `COPY` has many edge rules (directory copies, wildcards, symlinks, parent-directory permissions, `.dockerignore`) |
| `pkg/snapshot` + `pkg/filesystem` | Walk + hash, `LayeredMap`, whiteouts, symlink resolution, hints | Medium. Ports almost unchanged for kaniko mode, together with the optimizations of docs/12 and docs/13 (§4). Overlay mode is new code ([06][s06]) |
| `pkg/util/fs_util.go` | Layer extraction (whiteouts, hardlinks, xattrs, securejoin), copy, ownership, timestamps | Medium. Rust has `tar` and `rustix`/`nix`; `openat2(RESOLVE_IN_ROOT)` gives securejoin in the kernel |
| `pkg/executor` | Stage loop, cache key, lookahead, stage saving, push | Medium–high. The cache key must be stable, and kaniko-compatible if that is required ([05][s05] Q3, [ADR-0005][adr5]) |
| `pkg/image`, `pkg/image/remote`, `pkg/creds`, `pkg/mounts` | Pull/push, mirrors, auth (docker config, credential helpers), cross-repository mount | Medium. `oci-client` **[external]** covers basic pull/push; mirrors, credential helpers and streaming upload may have to be written |
| `pkg/cache`, `pkg/warmer` | Registry layer cache, OCI-layout cache | Medium |
| `pkg/buildcontext` | dir, tar, git, https, s3, gcs, azure | Low for dir/tar; git and cloud storage can wait |
| `pkg/bake` | HCL bake files | Low–medium (the `hcl-rs` crate exists) **[external]**, not verified |
| `pkg/tracing`, `pkg/metrics`, `pkg/timing` | OpenTelemetry, metrics | Low; use `tracing` + `opentelemetry` |

## 3. Places hard-wired to `/` that must be parameterized

Evidence: **[code]**, kaniko at `44aa7ddc0`.

In **kaniko mode** ([00][s00] §1) the workdir is the rootfs. ranikust keeps
kaniko's flow (extract → run instructions → walk + hash → wipe the filesystem)
and only replaces `/` with `--workdir`. The table lists every place where kaniko
relies on the process's own `/`, and what the Rust version must do there.

| Tied to `/` | Evidence | If only `RootDir` changes | The Rust version needs |
|---|---|---|---|
| The build rootfs is `/` | `config.RootDir = constants.RootDir = "/"` (`kaniko@44aa7ddc0 pkg/config/init.go:95-97`). 37 uses of `config.RootDir` outside tests | Changing the variable works, but a path join is easy to miss | A single `RootPath` type; every filesystem operation goes through the workdir fd ([02][s02] §4) |
| Refuses to run outside a container unless `--force` is given | `checkContained()` (`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:435-437`, `:236-241`) | — | Outside a container, require a `--workdir` other than `/` instead of `--force` |
| At the end of a stage, delete the rootfs except the ignore list | `DeleteFilesystem` walks `config.RootDir`, then `os.RemoveAll` (`kaniko@44aa7ddc0 pkg/util/fs_util.go:292-324`) | Right scope, but `RemoveAll` crosses mount points inside the workdir | Delete by fd, never follow symlinks, never cross mount points; check the marker file before deleting |
| `RUN` runs without chroot or namespaces | `exec.Command` + `Setpgid` (`kaniko@44aa7ddc0 pkg/commands/run.go:349-356`) | The command runs the **host's** `/bin/sh` and modifies the host filesystem | `chroot` into the workdir in its own mount and pid namespaces, with `/proc`, `/dev`, `/sys` ([02][s02] §3) |
| Executables are found with the kaniko process's `PATH` | Global `os.Setenv("PATH")`, then `exec.LookPath` (`kaniko@44aa7ddc0 pkg/commands/run.go:326-337`) | Finds the host's binary instead | Look up after `chroot`, or look up inside the workdir |
| Users and groups are resolved with `os/user` | `user.Lookup`, `user.LookupGroup` (`kaniko@44aa7ddc0 pkg/util/command_util.go:509-544`) | Reads the **host's** `/etc/passwd`; wrong uid | Parse `<workdir>/etc/passwd` and `group` directly |
| The ignore list comes from the process's own mountinfo | `DetectFilesystemIgnoreList` reads `/proc/self/mountinfo` (`kaniko@44aa7ddc0 pkg/util/fs_util.go:625-665`) | Puts every host mount into the list (harmless but wasteful) | Only take mount points under the workdir |
| `/kaniko` lives inside the rootfs and is ignored | `kaniko@44aa7ddc0 pkg/util/fs_util.go:71-88`, `pkg/config/init.go:23-46` | Still works | State dir outside the workdir ([02][s02] §2) |
| `RUN --mount=type=cache` works by **renaming directories** | `swapDir(cacheDir, m.Target)`, renamed back in a `defer` (`kaniko@44aa7ddc0 pkg/commands/run.go:83-135`, `:563-597`) | Still works | With root, a bind mount inside the `RUN` namespace |
| Grandchild processes are cleaned up by `SIGKILL` to the process group, optionally through `tini` | `kaniko@44aa7ddc0 pkg/commands/run.go:346-348`, `:397-405` | A process that called `setsid` leaves the process group and survives **on the host** | A pid namespace: when its init dies, the kernel kills everything |
| One rootfs shared by all stages, so stages run sequentially | kaniko optimization notes, docs/30 §1 (external) | — | Kaniko mode: stay sequential. Overlay mode: parallel along the DAG ([06][s06] §4) |

**Takeaway:** in kaniko mode, porting kaniko's flow faithfully is the right
call. Most of `snapshot`, `filesystem`, extraction and the ignore list carry
over almost unchanged. What must be written from scratch is the `RUN` sandbox
and the fd-based filesystem layer for the workdir, and both are shared with
overlay mode.

## 4. Lessons from the kaniko optimization work

The optimizations measured in the kaniko optimization notes (docs/12–28,
external, not in this repository), ranked by how much they are worth carrying
into Rust. The "Source" column names notes in that set.

| Optimization | Source | For Rust |
|---|---|---|
| Parallel compression (pgzip), deterministic under `--reproducible` | docs/14, docs/15 | Yes. Compress in parallel by block, keep a single gzip member, and make the output deterministic regardless of CPU count |
| Stream tar straight into the compressor, no intermediate tar file | docs/28 (workload 1: 40.3 → 32.9 s) | Yes, as the default from the start |
| One compressed blob shared by the image and the cache | docs/19 | Yes |
| Parallel context hashing, memoized between the two passes | docs/16 | Yes |
| Pipelined extraction, fewer syscalls while extracting | docs/17, docs/21 | Yes |
| Stat-based hash cache, parallel hashing, memoized `ResolvePaths` | docs/12, docs/13 | Yes, for kaniko mode (walk + hash). Overlay mode does not walk, so it does not need them |
| Keep extracted base tars between stages | docs/22 | Maybe. On a host the state dir persists between builds, so the gain may be larger than in a pod |
| `--compression=zstd` | docs/26 | Yes. Support parallel zstd (the `zstd` crate has a multithreaded mode) to avoid losing parallelism the way kaniko did in docs/26 |

## References

1. kaniko at `44aa7ddc0` (local branch `bench-metrics`; see the
   [research README](../README.md#source-snapshot)). All
   `kaniko@44aa7ddc0 path:line` references above.
2. kaniko optimization notes (external, not in this repository):
   docs/01-overview/04-package-map.md; docs/12 stat-and-parallel-hash; docs/13
   fast-resolve-paths; docs/14 parallel-gzip; docs/15
   reproducible-parallel-gzip; docs/16 context-hash-parallel; docs/17
   pipelined-extract; docs/19 shared-layer-blob; docs/21 fast-extract-syscalls;
   docs/22 reuse-base-rootfs; docs/26 kaniko-zstd; docs/28 stream-tar-compress;
   docs/30 parallel-stages-a.
3. `oci-client` crate, <https://crates.io/crates/oci-client> **[external]**,
   accessed 2026-10-29.
4. `hcl-rs` crate, <https://crates.io/crates/hcl-rs> **[external]**, accessed
   2026-10-29.
5. Survey chapters: [00 Summary][s00], [02 kaniko-mode requirements][s02],
   [03 Rust architecture][s03], [05 Decisions][s05],
   [06 Overlay and parallelism][s06].

[s00]: 00-summary.md
[s02]: 02-kaniko-mode-requirements.md
[s03]: 03-rust-architecture.md
[s05]: 05-decisions.md
[s06]: 06-overlay-and-parallelism.md
[adr5]: ../../adr/0005-own-versioned-cache-key.md
[d-kaniko]: ../../design/kaniko-mode.md
