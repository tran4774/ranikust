# Decisions

| | |
|---|---|
| **Status** | Frozen, 2026-10-29 |
| **Original** | `docs-rust/05-cau-hoi-mo.md` (Vietnamese, not in this repository) |
| **Superseded by** | [ADR-0003][adr3] … [ADR-0020][adr20] in [`docs/adr/`](../../adr/) |

The decisions were made on 2026-10-29. The overall direction: ranikust is **an
open-source tool for end users**, not tied to any particular environment, and it
must **cover as many cases as possible**. The two-mode model itself is recorded
in [ADR-0002][adr2].

## Summary

- **Twelve survey questions (Q1–Q12) were decided by the maintainer** and are
  now recorded as ADR-0003 to ADR-0014. Six further decisions followed directly
  from the experiments and are recorded as ADR-0015 to ADR-0020.
- **Isolation and trust:** the user decides whether a Dockerfile is trusted; the
  default isolation matches `docker build`, with `--seccomp` and `--userns` as
  opt-in hardening (Q1). `RUN` shares the host network, with `--network=none`
  (Q8).
- **Where it runs:** every privilege level, every kind of machine, no
  environment assumed (Q2, Q5). Environments where no mode can run are accepted
  as a known limitation. Concurrent builds share a persistent store (Q4).
- **Compatibility:** ranikust has its own versioned cache key (Q3), accepts
  kaniko's common flags (Q9), and accepts that overlay and kaniko mode can
  produce different content and digests (Q10).
- **Parallelism:** `--jobs` defaults to `available_parallelism()` (Q11). Kaniko
  mode stays sequential on `/` by default; `--parallel-stages` opts in (Q12).
- **Scope:** no bake, warmer or cloud contexts (Q6); `x86_64` releases, with
  `aarch64` kept buildable (Q7).

## 1. Decision index

| # | Decision | Origin | ADR |
|---|---|---|---|
| Q1 | The user decides whether a Dockerfile is trusted; default isolation ≈ `docker build`; `--seccomp` and `--userns` are opt-in | Survey question | [ADR-0003][adr3] |
| Q2 | No environment is assumed: detect the privilege level, fall back, or refuse with guidance; environments with no usable mode are accepted | Survey question | [ADR-0004][adr4] |
| Q3 | Own cache key with a version field; no cache sharing with kaniko | Survey question | [ADR-0005][adr5] |
| Q4 | The store persists across builds, with `prune`, digest locks and atomic rename | Survey question | [ADR-0006][adr6] |
| Q5 | Runs anywhere; concurrent builds share a store; no systemd or cgroup dependency | Survey question | [ADR-0007][adr7] |
| Q6 | No bake, no warmer, no s3/gcs/azure contexts | Survey question | [ADR-0008][adr8] |
| Q7 | `x86_64` only for releases, with `aarch64` kept buildable | Survey question | [ADR-0009][adr9] |
| Q8 | `RUN` shares the host network; `--network=none` is available | Survey question | [ADR-0010][adr10] |
| Q9 | Accept kaniko's common flags | Survey question | [ADR-0011][adr11] |
| Q10 | Content and digests may differ between overlay and kaniko mode; tests use an accepted-differences list | Survey question | [ADR-0012][adr12] |
| Q11 | `--jobs` defaults to `available_parallelism()` | Survey question | [ADR-0013][adr13] |
| Q12 | Kaniko mode is sequential on `/` by default; `--parallel-stages` opts in to parallel stages | Survey question | [ADR-0014][adr14] |
| — | Write our own Dockerfile parser (a port of BuildKit's) and `.dockerignore` matcher | [Experiment 04][e04] | [ADR-0015][adr15] |
| — | Mount overlay with `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile`; accept `EXDEV` | Experiments [01][e01], [06][e06], [06a][e06a], [10][e10], [11][e11], [13][e13] | [ADR-0016][adr16] |
| — | Build per-stage rootfs directories by `cp -a`/reflink, never by hardlink farms | [Experiment 12][e12] | [ADR-0017][adr17] |
| — | No proot in 1.0 | Experiments [12][e12], [13][e13] | [ADR-0018][adr18] |
| — | Inject `JAVA_TOOL_OPTIONS` on the parallel path under cgroup limits | Experiments [15][e15], [16][e16] | [ADR-0019][adr19] |
| — | Ship a static musl binary; cross-build with `cargo-zigbuild` | [Experiment 05][e05] | [ADR-0020][adr20] |

## 2. Survey decisions Q1–Q12

### Q1. Is the Dockerfile trusted?

**Decision:** the user decides. The tool is open source.

**Consequences:** by default, isolation is equivalent to `docker build`:
namespaces, capabilities reduced to Docker's default set, `pivot_root`. Opt-in
hardening: `--seccomp=default|unconfined|<file>` and `--userns` (uid 0 in the
build = a regular uid on the host). The documentation must state the isolation
level of each mode, especially kaniko mode: running in an unprivileged
container, it has no namespaces.

### Q2. What privileges does the pod or environment have?

**Decision:** no environment is assumed. ranikust must run at every privilege
level.

**Consequences:** 1.0 must ship all three paths: rootful overlay, overlay in a
user namespace (rootless), and kaniko mode. Privilege probing ([06][s06] §1) is
a core component, with tests for each privilege level. The PoCs showed that some
environments **cannot run any mode** (a regular user on Ubuntu 24.04+, a PSS
restricted pod): the tool must stop and print guidance. The maintainer accepted
this limitation on 2026-10-29. "Everywhere" means everywhere kaniko or
BuildKit/buildah can run.

### Q3. Share the registry cache with kaniko?

**Decision:** no.

**Consequences:** ranikust has its own cache key, with a version field so the
algorithm can change later without touching old cache entries.

### Q4. Does the store persist between builds?

**Decision:** yes.

**Consequences:** blobs, extracted layers and cache mounts are kept. This
requires `ranikust prune` (by size and age); per-digest locks when several
builds share a store; and atomic writes (extract into a temporary directory,
then `rename`) so that a crash never leaves a broken layer.

### Q5. Where does it run?

**Decision:** **everywhere**: developer machines, long-lived runners, single-use
VMs, pods.

**Consequences:** several builds run concurrently on one machine and share a
store: each build has its own `builds/<id>`, and the store locks by digest. No
dependency on systemd, the cgroup version or the distribution. cgroups are
optional.

### Q6. `bake`, the warmer, s3/gcs/azure contexts?

**Decision:** **not needed yet**.

**Consequences:** removed from the roadmap. `rk-context` keeps an open trait for
new sources.

### Q7. CPU architecture

**Decision:** **`x86_64` only, but keep the way open for `aarch64`.**

**Consequences:** see §4.

### Q8. Networking for `RUN`

**Decision:** **as proposed:** share the host network, with `--network=none`.

**Consequences:** no veth or NAT setup.

### Q9. kaniko flag compatibility

**Decision:** **as proposed:** accept kaniko's common flags.

**Consequences:** the exact flag list is settled in phase 1, based on kaniko's
README and sample pipelines.

### Q10. Different digests between overlay and kaniko mode

**Decision:** **as proposed:** accepted.

**Consequences:** the PoCs corrected one assumption: the flattened content is
**not** always identical. Overlay copy-up splits hardlinks, so an instruction
that writes to a hardlinked file gives different results in the two modes
([06][s06] §3.4). Overlay behaves like BuildKit. Tests compare flattened content
between the two modes against a list of accepted differences, and compare
digests within one mode.

### Q11. Default `--jobs`

**Decision:** **the number of CPU cores.**

**Consequences:** see §5.

### Q12. Parallel stages in kaniko mode when mounting is impossible

**Question:** in kaniko mode **without mount privileges** (default Docker,
Kubernetes pods), should stages run in parallel, each in its own directory under
a bare `chroot`?

**Decision:** **sequential by default**, as in kaniko: the rootfs is `/`, with a
full `/proc` and `/dev`. The `--parallel-stages` flag (name settled in phase 1
together with Q9) turns on parallel stages: one directory per stage + bare
`chroot`, and `RUN` has no `/proc`. On 2026-10-29 parallel was first chosen as
the default, then reverted because of the `/proc` risk. When mounting is
possible (root, but overlay is unusable), stages run in parallel with the full
sandbox, at no cost.

**Consequences:** [experiment 12][e12]: parallel is 41–44% faster than
sequential on W3, with identical images **[measured]**. In exchange, `RUN` has
no `/proc`, and some commands return wrong results without an error (`ps` is
empty, `os.cpus()` is 0). `/dev` is available on rootful Docker thanks to
`mknod`. With the flag, the tool logs clearly at build start that `RUN` has no
`/proc`, and the user documentation says so. A static snapshot of 13 files from
`/proc` and `/sys` can be copied into the rootfs ([experiment 12a][e12a]). That
fixes `next build`, the CPU count and cgroup quotas; it does not fix `/dev/fd`,
`ps`, JDKs installed from apt/apk, or TypeScript 7 (`/proc/self/exe`). Temurin
images work. Large Java builds hit OOM under a memory limit unless the tool
injects `JAVA_TOOL_OPTIONS` ([06][s06] §5, experiments [14][e14]–[16][e16]).
proot is not in 1.0 ([06][s06] §5).

## 3. Decisions derived from the experiments

These decisions were not survey questions. Each follows from experiment results
already described in the survey chapters.

- **Hand-written Dockerfile parser ([ADR-0015][adr15]).** `parse-dockerfile`
  0.1.8 fails on heredocs in the middle of an instruction and does not expand
  variables; no crate ports `moby/patternmatcher`. ranikust ports BuildKit's
  parser (about 3,000–3,300 lines) and patternmatcher (about 350–400 lines).
  Evidence: [experiment 04][e04], [03][s03] §3.
- **Overlay mount options ([ADR-0016][adr16]).**
  `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile`,
  the same set as real root and in a user namespace. `redirect_dir=off` is
  rejected in a user namespace; `volatile` is required for performance; programs
  that do not handle `EXDEV` fail in overlay mode, and the tool suggests
  `--snapshot-mode=kaniko`. Evidence: experiments [01][e01], [06][e06],
  [06a][e06a], [10][e10], [11][e11], [13][e13]; [06][s06] §3.1 and §3.4.
- **Per-stage rootfs by copy, not hardlinks ([ADR-0017][adr17]).** Build each
  stage's rootfs by extraction or `cp -a` (which reflinks on xfs/btrfs); a
  hardlink farm lets `>>`, `chmod`, `chown` or `touch` in one stage modify the
  store and other stages. Evidence: [experiment 12][e12]; [06][s06] §5.
- **No proot in 1.0 ([ADR-0018][adr18]).** proot restores `/proc` but costs
  +26–56% wall time, hangs apt without `APT::Sandbox::User=root`, and is a third
  binary under GPL-2.0. Evidence: experiments [12][e12], [13][e13]; [06][s06]
  §5.
- **`JAVA_TOOL_OPTIONS` on the parallel path ([ADR-0019][adr19]).** Under a
  cgroup limit without a real `/proc`, the JVM sees 32 CPUs and picks a 7.6 GB
  heap, and large builds hit OOM (`--cpus=4 --memory=2g`). Injecting
  `-XX:+IgnoreUnrecognizedVMOptions -XX:ActiveProcessorCount=N -XX:MaxRAM=<limit>`
  fixed 16/16 runs at 2 GB. Evidence: experiments [15][e15], [16][e16];
  [06][s06] §5.
- **Static musl binary via `cargo-zigbuild` ([ADR-0020][adr20]).** A static-pie
  musl binary of 5.1 MB runs in an empty rootfs; `cargo-zigbuild` 0.24 + zig
  0.15 cross-build `aarch64`, which runs under `qemu-aarch64-static`. Evidence:
  [experiment 05][e05]; [03][s03] §1.

## 4. Keeping the way open for `aarch64` (Q7)

- No architecture-specific code. Crates that use assembly (`sha2`, zlib) must
  have a pure-Rust fallback.
- Syscall numbers come only from `rustix`/`libc`, never hard-coded. Seccomp
  profiles (Q1) are generated per architecture.
- The image architecture comes from the manifest list and the config, not from
  the host: pick the right `platform` when pulling the base, and write
  `architecture`/`variant` into the config. `--platform` is prepared, but the
  first release only accepts `linux/amd64`.
- CI cross-builds `aarch64-unknown-linux-musl` from the start (build, and run
  unit tests under qemu-user), so that nobody accidentally introduces x86-only
  code. Verified ([experiment 05][e05]) **[measured]**: `cargo-zigbuild` 0.24 +
  zig 0.15 build a static binary that runs correctly under
  `qemu-aarch64-static`.
- Multi-platform builds (running arm64 `RUN` on x86 through `binfmt_misc`) are
  **not** on the roadmap.

## 5. `--jobs` defaults to the core count (Q11)

- The default is `std::thread::available_parallelism()`. Verified with Rust 1.99
  ([experiment 05][e05]) **[measured]**: it accounts for both the cgroup quota
  and CPU affinity. `--cpus=2` → 2, `--cpus=2.5` → 2, `--cpus=0.5` → 1, affinity
  0-3 → 4, `--cpus=2` + affinity 0-3 → 2.
- Kaniko mode with `/` as the rootfs runs sequentially and ignores `--jobs`.
  With a separate rootfs directory per stage it uses `--jobs` as overlay mode
  does ([06][s06] §5, Q12).
- [Experiment 08][e08] (W3) showed that the default `--jobs` is not slower than
  `--jobs 1`; the real constraint is memory **[measured]**. W2 and workloads
  with more stages are still to be measured. The results will feed a tuning
  guide for users, not a change of the default.

## Open questions

- The exact kaniko flag list, and the final name of `--parallel-stages`, are
  settled in phase 1 (Q9, Q12).
- W2 and workloads with more stages than W3 are still to be measured for the
  `--jobs` tuning guide (§5).

## References

1. Experiments: [01][e01], [04][e04], [05][e05], [06][e06], [06a][e06a],
   [08][e08], [10][e10], [11][e11], [12][e12], [12a][e12a], [13][e13],
   [14][e14], [15][e15], [16][e16].
2. Survey chapters: [03 Rust architecture][s03],
   [06 Overlay and parallelism][s06].
3. ADRs: [ADR-0002][adr2] to [ADR-0020][adr20].

[s03]: 03-rust-architecture.md
[s06]: 06-overlay-and-parallelism.md
[e01]: ../experiments/01-overlay-semantics.md
[e04]: ../experiments/04-dockerfile-parser.md
[e05]: ../experiments/05-rust-ecosystem.md
[e06]: ../experiments/06-overlay-vs-walk.md
[e06a]: ../experiments/06a-root-rerun.md
[e08]: ../experiments/08-parallel-stages-lazy-export.md
[e10]: ../experiments/10-exdev-package-managers.md
[e11]: ../experiments/11-root-extras.md
[e12]: ../experiments/12-kaniko-mode-parallel.md
[e12a]: ../experiments/12a-fake-proc.md
[e13]: ../experiments/13-docker-rootful-vm.md
[e14]: ../experiments/14-real-builds-without-proc.md
[e15]: ../experiments/15-java-next-concurrency.md
[e16]: ../experiments/16-cgroup-v1-cpuset.md
[adr2]: ../../adr/0002-two-snapshot-modes.md
[adr3]: ../../adr/0003-dockerfile-trust-and-default-isolation.md
[adr4]: ../../adr/0004-support-every-privilege-level.md
[adr5]: ../../adr/0005-own-versioned-cache-key.md
[adr6]: ../../adr/0006-persistent-store.md
[adr7]: ../../adr/0007-run-anywhere-shared-store.md
[adr8]: ../../adr/0008-out-of-scope-bake-warmer-cloud-contexts.md
[adr9]: ../../adr/0009-x86-64-first-aarch64-ready.md
[adr10]: ../../adr/0010-run-network-host-or-none.md
[adr11]: ../../adr/0011-accept-common-kaniko-flags.md
[adr12]: ../../adr/0012-accept-divergence-between-modes.md
[adr13]: ../../adr/0013-jobs-default-available-parallelism.md
[adr14]: ../../adr/0014-kaniko-mode-sequential-by-default.md
[adr15]: ../../adr/0015-hand-written-dockerfile-parser.md
[adr16]: ../../adr/0016-overlay-mount-options.md
[adr17]: ../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md
[adr18]: ../../adr/0018-no-proot-in-1-0.md
[adr19]: ../../adr/0019-java-tool-options-on-parallel-path.md
[adr20]: ../../adr/0020-static-musl-via-cargo-zigbuild.md
