# Survey summary: rewriting kaniko in Rust as one binary that also runs outside containers

| | |
|---|---|
| **Status** | Frozen, 2026-10-29 |
| **Original** | `docs-rust/README.md` (Vietnamese, not in this repository) |
| **Superseded by** | [`ARCHITECTURE.md`][arch] and the ADRs in [`docs/adr/`](../../adr/), starting with [ADR-0002][adr2] |

This was a survey only: when it was written, the ranikust repository contained
no code (only `.git`). Its sources were the kaniko source tree (branch
`bench-metrics`, HEAD `44aa7ddc0`) and the kaniko optimization notes docs/01–30
(external, not in this repository). Every kaniko `path:line` reference is
relative to the kaniko root and cited as `kaniko@44aa7ddc0 path:line`. Evidence
labels follow [`TEMPLATE.md`](../TEMPLATE.md); **[external]** facts should have
their versions re-checked before use.

## Summary

- **Is it feasible?** Yes. Rewriting in Rust is not the hard part; getting
  Dockerfile semantics and the image format right is.
- **What model was chosen?** `--workdir` holds the build's rootfs, and the tool
  never writes outside it. Two snapshot modes are selected at run time from the
  available privileges: **overlay** where mounting is possible, **kaniko mode**
  (walk + hash) where it is not ([ADR-0002][adr2]).
- **How much faster is overlay?** Less than first assumed: a full W1 build takes
  9.5–10.0 s against 15.6 s for optimized kaniko and 11.3 s for an optimized Go
  walk, at 32 CPUs **[measured]**. A well-written walk recovers most of the gap,
  so kaniko mode will not be much slower.
- **Do parallel stages help?** Yes, together with lazy export. At 32 CPUs, W3
  builds in 6.0 s with overlay ([experiment 08][e08]) and in 5.38 s with
  parallel kaniko mode ([experiment 12][e12]) **[measured]**. Memory, not CPU,
  is the real limit. In kaniko mode without mounts, parallel stages lose
  `/proc`, so they are opt-in.
- **What are the main risks?** `RUN` as real root on the host isolated only by
  namespaces, images that differ between modes, three code paths and a privilege
  test matrix to maintain, and most locked-down CI pods landing in kaniko mode.
- **How long?** An internal MVP in about 9–14 weeks and a 1.0 in about 7–10
  months, for one person **[inferred]**.

## 1. The chosen model

`--workdir` holds the build's rootfs. The tool never writes outside the workdir
(and the state dir), except to the output destination. There are two snapshot
modes, selected automatically from the privileges available at run time
([06][s06] §1):

| Environment (examples) | Mode | Rootfs | Snapshot | Stages |
|---|---|---|---|---|
| Host as root/sudo; privileged container | **overlay** | One overlay mount per stage inside the workdir (a volume or tmpfs, not on the container's overlay) | Walk only the upperdir (the diff), never the rootfs | **Parallel** along the DAG, as in BuildKit |
| Default rootless podman; Docker with relaxed seccomp/AppArmor; regular user on Debian/Fedora/RHEL | **rootless overlay** (`userxattr`) | As above. `/proc` is rbind-mounted if a new `proc` cannot be mounted | As above | Parallel |
| Default Docker, ordinary Kubernetes pods, GKE Autopilot, Bottlerocket: **most locked-down CI** | **kaniko** | The workdir is the rootfs, `/` by default | Parallel walk + hash, stat cache, the optimizations of kaniko optimization notes docs/12–28 | **Sequential** by default, with only prefetching in parallel. The `--parallel-stages` flag turns on parallel stages (one directory per stage + bare `chroot`; `RUN` has no `/proc`) ([experiment 12][e12], Q12, [ADR-0014][adr14]) |
| Regular user on Ubuntu ≥ 24.04; PSS restricted pod | **cannot run** | Stop and print guidance: sudo, an AppArmor profile, or run inside a container | | |

fanotify is not used. The full table per CI system and cluster is in
[experiment 03][e03].

## 2. Conclusions

**Feasible. Rewriting in Rust is not the hard part. The hard part is getting
Dockerfile semantics and the image format right.**

1. **Rust has libraries for every block** except the Dockerfile parser and
   `.dockerignore`. `parse-dockerfile` was tried: it fails on heredocs in the
   middle of an instruction and does not expand variables. The parser is
   therefore **written in-house** (about 3,000–3,300 lines) and `patternmatcher`
   is ported (about 350–400 lines) ([03][s03] §3, [experiment 04][e04],
   [ADR-0015][adr15]).
2. **A single static binary is feasible and was built** **[measured]**: a 5.1 MB
   static-pie that runs in an empty rootfs and cross-builds for `aarch64`. No
   `runc`, daemon or `tini` is needed. Two things are mandatory:
   - `mimalloc`, because musl's allocator is 250–300 times slower under
     multithreading;
   - a hand-built CA configuration for rustls.

   A hand-written pigz-style parallel gzip reaches 2.2 GB/s and is
   deterministic. Multithreaded zstd is deterministic only when fed in
   fixed-size blocks ([03][s03] §1, [experiment 05][e05], [ADR-0020][adr20]).
3. **Overlay mode is faster, but by less than first assumed** ([06][s06] §3.5,
   [experiment 06][e06]). Measured on W1 with 5 `RUN` instructions
   **[measured]**:
   - Taking the diff is **4.5–5.5 times** faster than an optimized walk
     (parallel, with a stat cache).
   - But overlay makes the instructions themselves 7–16% slower, so the whole
     build is only **18–23%** faster (1.5–1.7 s).
   - **`volatile` mounts are mandatory.** Without them overlay fsyncs on every
     copy-up and is far slower than the walk.
   - **Full W1 builds on the same machine, including pull and push**
     ([experiment 07][e07]):

     | Builder | 32 CPUs | 4 CPUs |
     |---|---|---|
     | upstream kaniko | 35.6 s | 35.7 s |
     | optimized kaniko (docs/12–28) | 15.6 s | 18.3 s |
     | optimized Go walk | 11.3 s | 14.8 s |
     | overlay | 9.5–10.0 s | 11.2–12.2 s |

     Overlay beats optimized kaniko by 5.5–6.7 s (33–39%). But **a well-written
     walk recovers most of that gap**, so ranikust's kaniko mode will not be
     much slower than overlay.
   - Lazy export is worth as much as the diff method, because tar + gzip of a
     1.4 GB layer costs about 2 s in both modes ([06][s06] §3.2).
4. **Parallel stages plus lazy export pay off**, and a `--jobs` default equal to
   the core count is sound ([experiment 08][e08], W3) **[measured]**:

   | Configuration | 32 CPUs | 4 CPUs |
   |---|---|---|
   | optimized kaniko | 19.9 s | 22.1 s |
   | BuildKit parallel | 9.4 s | 14.0 s |
   | overlay lazy, `--jobs` 1 | 8.4 s | 11.4 s |
   | overlay lazy, `--jobs` default | **6.0 s** | **10.7 s** |

   - On this machine parallel was never slower than sequential, BuildKit
     included. The "parallel is slower" result of kaniko optimization notes
     docs/30 (measured on OrbStack) did not reproduce.
   - The real constraint is **memory**: running in parallel raises peak RAM from
     about 4.3 GB to about 7 GB. When every layer is exported, export has to
     read back from disk and slows down. Parallelism must be limited by memory,
     not only by CPU ([06][s06] §4.3).
   - **Kaniko mode can run in parallel too** ([experiment 12][e12]): one rootfs
     directory per stage under the workdir, no mounts needed. The resulting
     image is identical to the sequential one.

     | W3, kaniko mode | 32 CPUs | 4 CPUs |
     |---|---|---|
     | sequential | 14.04 s | 17.14 s |
     | parallel, bare `chroot` | 7.99 s | 13.74 s |
     | parallel, bare `chroot`, lazy | **5.38 s** | **10.02 s** |
     | overlay lazy (reference) | 6.29 s | 10.64 s |

     Parallelism helps the walk mode more than the overlay mode, because the
     extraction, walk and tar of different stages overlap.
     - Where mounting is possible, the full sandbox is used at no cost.
     - Where it is not (default Docker), `RUN` runs under a bare `chroot` with
       **no `/proc`**. Some commands then return wrong results without an error.
       Decided: sequential on `/` by default, as in kaniko, with the
       `--parallel-stages` flag to turn on parallel stages ([05][s05] Q12,
       [ADR-0014][adr14]).
     - proot restores `/proc`, but is 26–56% slower and is a third binary under
       the GPL. It is not in 1.0 ([ADR-0018][adr18]).
5. **In kaniko mode, every place hard-wired to `/` must be parameterized**
   ([01][s01] §3, [02][s02]). The `RUN` sandbox (chroot + namespaces) and the
   guard against writes outside the workdir are shared by both modes.
6. **Main risks:**
   - `RUN` runs as real root on the host, contained only by namespaces. The user
     decides whether to build untrusted code; the tool offers `--seccomp` and
     `--userns` for tighter isolation ([ADR-0003][adr3]).
   - **The two modes can produce different images, not just different digests.**
     Overlay copy-up splits hardlinks. BuildKit behaves like overlay ([06][s06]
     §3.4, [ADR-0012][adr12]).
   - Three paths (rootful overlay, rootless overlay, kaniko) and a
     privilege-level test matrix must be maintained.
   - **Most locked-down CI pods land in kaniko mode**, so kaniko-mode speed
     determines the experience of most users. ranikust's differentiator is **one
     binary that covers both sides** (kaniko never uses overlay; buildah and
     BuildKit stop when no user namespace is available), not "overlay
     everywhere".
   - The `RUN` sandbox ran in the PoC, but has many traps to handle: `unshare`
     only while the process is single-threaded, `PR_SET_PDEATHSIG`, `pivot_root`
     instead of `chroot` (a bare `chroot` was escaped), dropping the bounding
     set, stub mountpoints, `resolv.conf` being a symlink ([02][s02] §3).

   Details in [04][s04] §1.

The decisions taken (an open-source tool, not tied to any environment, runs
everywhere, `x86_64` only but keeping the way open for `aarch64`, and so on) are
in [05][s05].

Rough estimates **[inferred]**:

- Internal MVP (rootful overlay, parallel stages, no registry cache yet):
  **about 9–14 weeks for one person** who already knows Rust and Linux.
- 1.0 (all three paths, rootful overlay / rootless overlay / kaniko, plus cache
  and kaniko's commonly used features): **about 7–10 months**.

The PoCs raised the estimate slightly: the parser must be written in-house, and
the sandbox and privilege probing gained many concrete tasks. No finding changed
the "feasible" conclusion.

Details in [04][s04] §3.

## 3. Chapters and experiments

| Report | Content |
|---|---|
| [01 kaniko today][s01] | kaniko's building blocks, size (lines of code, flags, feature flags), and the places **hard-wired to `/`** that must be parameterized |
| [02 kaniko-mode requirements][s02] | Kaniko mode (workdir = rootfs), the `RUN` sandbox with chroot + namespaces, refusing writes and deletes outside the workdir. The last two parts are shared by both modes |
| [03 Rust architecture][s03] | The proposed ranikust architecture: Go packages mapped to Rust crates, library choices, what must be written from scratch |
| [04 Risks and roadmap][s04] | Risks, MVP scope, phased roadmap, effort estimates, verification plan |
| [05 Decisions][s05] | The decisions taken (Q1–Q12) and their consequences: optional isolation, runs everywhere, persistent store, a way open for `aarch64`, `--jobs` by core count |
| [06 Overlay and parallelism][s06] | Mode selection by privilege, overlay snapshots, lazy layer export, overlay constraints, overlay vs walk measurements, the DAG and parallel stage scheduling, the parallel measurements of docs/30 |
| [Experiment 01][e01] | Overlay by privilege level, mount options, upperdir behaviour (whiteouts, opaque directories, copy-up, hardlinks), lowerdir limits, how BuildKit/containerd read the upperdir |
| [Experiment 02][e02] | The `RUN` sandbox on alpine/debian/node: mount order, minimum privileges, chroot escape, capability dropping, stubs, overhead |
| [Experiment 03][e03] | Which environments (distributions, Docker, Podman, Kubernetes, CI) land in which mode; comparison with buildah, BuildKit, kaniko |
| [Experiment 04][e04] | `parse-dockerfile` and the `ignore` crate against BuildKit on kaniko's 201 Dockerfiles |
| [Experiment 05][e05] | Static binary, aarch64, allocator, deterministic parallel gzip/zstd, `oci-client`, `tar`, `available_parallelism` |
| [Experiment 06][e06] | Diff time with overlay vs walk + hash on W1 |
| [Experiment 06a][e06a] | Experiments 01, 02 and 06 re-run as real root on the host, and the results |
| [Experiment 07][e07] | Full W1 builds: upstream kaniko, optimized kaniko, Go walk, overlay; checks that the images match |
| [Experiment 08][e08] | W3: `--jobs` 1/2/default, lazy export vs exporting every layer, compared with BuildKit and kaniko |
| [Experiment 09][e09] | Kaniko-mode PoC in Rust: walk + diff, ignore list from mountinfo, safe filesystem deletion, walk timing |
| [Experiment 10][e10] | apt/dpkg, pip, yarn, cargo on overlay when renaming a lower directory fails with `EXDEV` |
| [Experiment 11][e11] | AppArmor profile for regular users; overlay on ext4/xfs/btrfs; the `volatile` lifecycle; nested overlay |
| [Experiment 12][e12] | Parallel stages in kaniko mode: one rootfs directory per stage; bare `chroot`, proot, seccomp user-notify, fakechroot; `RUN` compatibility; fake `/proc` (jest, terser, `next build`, Java); building the rootfs; isolation; W1/W3 timings |
| [Experiment 13][e13] | Real rootful Docker 29.1 in a VM (Ubuntu 22.04, kernel 5.15), cgroup v2 and v1: mode table, AppArmor, `mknod`, proot, the Q12 case, fake `/proc` by cgroup version, overlay on kernel 5.15 |
| [Experiment 14][e14] | Real builds under bare `chroot` with a fake `/proc`: `next build` of the W1 app, maven/gradle with Temurin, Debian and Alpine JDKs, patching `$ORIGIN`, the JVM under cgroup limits, tsc/esbuild/vite/TypeScript 7 |
| [Experiment 15][e15] | Large Java builds under a memory limit (OOM, `JAVA_TOOL_OPTIONS`), toolchains that download their own JDK, Next 16 and Turbopack, several stages at once each with its own fake `/proc` |
| [Experiment 16][e16] | Real Docker in a VM, cgroup v2 and v1: JVM cgroup limits per JDK version; jest, webpack, `next build` of W1 with a fake `/proc`; parallel W3 under v1; cpuset; the final fake `/proc` file set per cgroup version |

## 4. What was verified and what was not

On 2026-10-29, 16 PoCs were run, plus web research. The environments were:

- the development machine: kernel 7.0, 32 CPUs, rootless podman;
- real root through pkexec;
- rootful Docker 29.1 in an Ubuntu 22.04 VM (kernel 5.15), cgroup v2 and v1.

The PoC code is archived outside this repository and **is not part of
ranikust**. The survey stops here; the items under "Not verified" are left for
the implementation.

**Verified [measured]:**

- overlay at each podman privilege level;
- overlay mount options, upperdir behaviour, lowerdir limits;
- the `RUN` sandbox sequence on three distributions;
- minimum privileges, including under Docker's seccomp profile;
- the parser and `.dockerignore` against BuildKit;
- the static binary and the `aarch64` cross-build;
- the allocator;
- parallel compression;
- `oci-client` and the `tar` crate;
- `available_parallelism`;
- overlay vs walk performance;
- **experiments 01, 02 and 06 re-run as real root on the host** through pkexec
  ([experiment 06a][e06a]). The results matched the rootless runs, with two
  additions:
  - a bare `chroot` escapes all the way to the host's real `/`;
  - the common overlay option set also works as root.
- **kaniko mode** (Rust PoC, [experiment 09][e09]):
  - the diff is correct, and no container mount leaks into a layer;
  - walking 269k files takes about 0.1 s with a stat cache;
  - finding: safe filesystem deletion must compare `mnt_id`, not `st_dev`;
- `EXDEV` with apt/dpkg, pip, yarn, cargo: all results correct;
- an AppArmor profile lets a regular user on Ubuntu 24.04+ run rootless overlay;
- overlay on ext4, xfs, btrfs; the `volatile` lifecycle; the nesting limit of
  depth 2;
- **real builds on the parallel path** ([experiment 14][e14]): `next build` of
  the W1 app gives identical output with a fake `/proc`; Java works with Temurin
  images and fails with JDKs installed from apt/apk; TypeScript 7 fails;
- **Java, Next 16 and several stages at once** ([experiment 15][e15],
  [experiment 16][e16]): large Java builds hit OOM without `/proc`, and
  injecting `JAVA_TOOL_OPTIONS` fixes it from about 1.75 GB; toolchains that
  download a different JDK fail; Next 16/Turbopack runs correctly with a fake
  `/proc`; three stages running at once, each with a fake `/proc`, produce a
  correct image with no fake file leaked; on cgroup v2 a fake `/proc` cannot fix
  newer JVMs, on v1 it can;
- **real rootful Docker** ([experiment 13][e13]): the mode table holds under
  both cgroup v2 and v1; `--cap-add SYS_ADMIN` is still blocked from mounting by
  AppArmor; `mknod` works in every variant; kernel 5.15 has no `lowerdir+`;
- **parallel stages in kaniko mode** ([experiment 12][e12]): a bare `chroot`
  runs apk, apt, npm and pnpm in a container emulating default Docker; W3 is
  41–44% faster than sequential; proot is 26–56% slower; a hardlink farm
  corrupts the store.

**Not verified** (left for the implementation, with the phase in which each
should be checked, per [04][s04] §3):

*Environments*

| Item | Why it is still open | Phase |
|---|---|---|
| **Real Kubernetes** | Excluded from the survey scope by the maintainer. Covers containerd, AppArmor `cri-containerd`, `procMount`, `hostUsers: false`, PSS. Kubernetes on cgroup v1 with the memory limit set on a parent cgroup may also need `memory.stat` in the fake `/proc` **[inferred]**. Only desk research so far | 3 |
| Kernels other than 7.0 and 5.15 | 5.10, 6.1, RHEL 8/9. SELinux (Fedora/RHEL) with overlay in a user namespace is untested | 3 |
| Rootless Docker, real CI runners | dockerd-rootless; real GitHub Actions and GitLab runners (so far only inferred from the privilege table) | 3 |
| fuse-overlayfs, NFS, LXCFS, `--cgroupns=host` | Never run | 3 |
| `aarch64` on real hardware | Only cross-built and run under qemu-user | 6 |

*Performance*

| Item | Why it is still open | Phase |
|---|---|---|
| A real network registry, W2 (cache) | The registry ran on loopback, so pull/push cost almost nothing. W2 was not measured | 1, 4 |
| More stages than cores; a memory-aware scheduler | W3 has only 3 stages. Limiting parallelism by RAM is only a proposal; it has not been built | 1 |
| Overlay export varies between 1.8 and 6.6 s | Suspected page-cache reclaim, not confirmed | 1 |
| Long-running `RUN` with a fake `/proc` | The fake `/proc` is a static snapshot; the effect of stale values on a long-running `RUN` was not measured | 2 |

*Correctness and features*

| Item | Why it is still open | Phase |
|---|---|---|
| **Dockerfile semantics** | The parser is not written yet. The 201 integration Dockerfiles have not been run, and image content has not been compared with kaniko | 1, 6 |
| A complete builder | `ovbuild` is only a PoC: no cache, `.dockerignore`, `USER`, `ADD`, `RUN --mount` | 1 |
| Other tools on the `--parallel-stages` path | cargo/go builds, conda, Python on Debian/musl, and binaries that use `$ORIGIN` are untested. It is unknown which 21.0.x/17.0.x JDK release changed cgroup detection. Java below about 1.75 GB has no fix yet | 2 |

*Safety*

| Item | Why it is still open | Phase |
|---|---|---|
| **Host-safety test suite** | Each mechanism was proven on its own. The attack suite has not been run: path traversal in layers, symlinks/hardlinks pointing outside, fork bombs, `--workdir=/` | 6 |
| TOCTOU, shared store | Races between filesystem deletion and extraction; several builds sharing one store (per-digest locks, crashes half-way) | 1, 6 |
| `--seccomp`, `--userns` | Designed only, not run end to end | 3 |

*Estimates:* every effort figure (7–10 months to 1.0) is still **[inferred]**.

## Open questions

Everything under "Not verified" in §4 remains open, with the phase where it
should be checked.

## References

1. Survey chapters: [01][s01], [02][s02], [03][s03], [04][s04], [05][s05],
   [06][s06].
2. Experiments: [01][e01] to [16][e16], including [06a][e06a]; see §3.
3. kaniko at `44aa7ddc0` (local branch `bench-metrics`; see the
   [research README](../README.md#source-snapshot)).
4. kaniko optimization notes (external, not in this repository): docs/01–30, in
   particular docs/12–28 (optimizations) and docs/30 parallel-stages-a.

[arch]: ../../../ARCHITECTURE.md
[s01]: 01-kaniko-today.md
[s02]: 02-kaniko-mode-requirements.md
[s03]: 03-rust-architecture.md
[s04]: 04-risks-and-roadmap.md
[s05]: 05-decisions.md
[s06]: 06-overlay-and-parallelism.md
[e01]: ../experiments/01-overlay-semantics.md
[e02]: ../experiments/02-run-sandbox.md
[e03]: ../experiments/03-privilege-matrix.md
[e04]: ../experiments/04-dockerfile-parser.md
[e05]: ../experiments/05-rust-ecosystem.md
[e06]: ../experiments/06-overlay-vs-walk.md
[e06a]: ../experiments/06a-root-rerun.md
[e07]: ../experiments/07-kaniko-vs-overlay.md
[e08]: ../experiments/08-parallel-stages-lazy-export.md
[e09]: ../experiments/09-kaniko-mode-prototype.md
[e10]: ../experiments/10-exdev-package-managers.md
[e11]: ../experiments/11-root-extras.md
[e12]: ../experiments/12-kaniko-mode-parallel.md
[e13]: ../experiments/13-docker-rootful-vm.md
[e14]: ../experiments/14-real-builds-without-proc.md
[e15]: ../experiments/15-java-next-concurrency.md
[e16]: ../experiments/16-cgroup-v1-cpuset.md
[adr2]: ../../adr/0002-two-snapshot-modes.md
[adr3]: ../../adr/0003-dockerfile-trust-and-default-isolation.md
[adr12]: ../../adr/0012-accept-divergence-between-modes.md
[adr14]: ../../adr/0014-kaniko-mode-sequential-by-default.md
[adr15]: ../../adr/0015-hand-written-dockerfile-parser.md
[adr18]: ../../adr/0018-no-proot-in-1-0.md
[adr20]: ../../adr/0020-static-musl-via-cargo-zigbuild.md
