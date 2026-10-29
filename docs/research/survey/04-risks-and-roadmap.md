# Risks, roadmap, estimates and verification plan

| | |
|---|---|
| **Status** | Frozen, 2026-10-29 |
| **Original** | `docs-rust/04-rui-ro-va-uoc-luong.md` (Vietnamese, not in this repository) |
| **Superseded by** | — (the risk register and roadmap are not maintained here; decisions they depend on live in [`docs/adr/`](../../adr/)) |

## Summary

- **What are the main risks?** Host security when `RUN` runs as real root (R1),
  wiping host data through a mistyped `--workdir` (R2), Dockerfile semantics
  drifting from BuildKit/kaniko (R3), and images that differ between the two
  modes (R4). Thirteen risks are tracked, R1–R13.
- **Who benefits from overlay?** Not most locked-down CI: such pods land in
  kaniko mode (R12), so kaniko mode must be fast. Parallel stages without mounts
  lose `/proc` (R13) and are therefore opt-in.
- **What is in scope?** An internal MVP (rootful overlay, parallel stages,
  persistent store, lazy export) to validate the design, then a 1.0 that adds
  kaniko mode, rootless overlay, privilege probing, registry cache and the
  remaining kaniko features. Bake, the warmer, cloud contexts and multi-platform
  builds are out.
- **How long?** About 30–43 weeks (7–10 months) for one person to 1.0, roughly
  40% less with two **[inferred]**.
- **How is it verified?** Parser golden tests, flattened image-content
  comparison against kaniko and between modes, a privilege matrix, the fair
  benchmark harness, and targeted host-safety tests.

## 1. Risks

| # | Risk | Severity | Mitigation |
|---|---|---|---|
| R1 | **Host security.** Outside a container, `RUN` runs as real uid 0, separated only by chroot + namespaces. A malicious Dockerfile can use the capabilities it keeps (for example `CAP_SYS_ADMIN` to remount, `CAP_SYS_CHROOT` to escape a chroot) or a kernel bug to reach the host | **High** when building untrusted code. The user decides (Q1, [ADR-0003][adr3]) | Document the isolation level of each mode. Offer `--seccomp` and `--userns`. **`pivot_root` is mandatory** (fallback MS_MOVE + chroot); [experiment 02][e02] escaped a bare `chroot`. **Dropping the bounding set** to Docker's default capabilities for every `USER` is mandatory; experiment 02 showed that otherwise `RUN` can `umount` a bind and write into the rootfs. `PR_SET_PDEATHSIG` so no process is left behind. Optional seccomp. Later phase: user namespaces |
| R2 | **Deleting host data by mistake.** The model wipes the whole workdir after each stage. A mistyped `--workdir` loses data | **High** | The checks in [02][s02] §4: refuse `/` and system directories, require a marker file, delete by fd, never follow symlinks, never cross mount points. Dedicated tests for these cases |
| R3 | **Dockerfile semantics drift** from BuildKit/kaniko: variable expansion, `COPY` rules, `.dockerignore`, heredocs, `ONBUILD` | High | Re-run kaniko's 201 integration Dockerfiles. Compare image **content** (file list, mode, uid, xattrs, contents) with kaniko, not just the digest |
| R4 | **The two modes produce different images.** (a) Overlay puts rewritten-but-unchanged files into the layer (copy-up); kaniko compares hashes and drops them. (b) **Copy-up breaks hardlinks**: writing to one name changes only that name in overlay, but every name sharing the inode in kaniko. Measured on W1 (pnpm hardlinks its store into `node_modules`): 1,813 files differ **[measured]**. Metadata differs too, and for in-place writes so do contents | Medium | Accepted; BuildKit behaves like overlay. Documented. A flattened-content comparison with a list of accepted differences (hardlinks, parent directories). Optional filtering of unchanged files at export ([06][s06] §3.4, Q10, [ADR-0012][adr12]) |
| R5 | **Overlay is unavailable where it seems available:** the workdir is on the container's overlay; Docker's seccomp blocks even `CLONE_NEWUSER`; AppArmor `deny mount`; a masked `/proc` prevents mounting a new `proc` inside a user namespace; Ubuntu 24.04 allows creating a user namespace but strips its capabilities; the 500-lowerdir limit | Medium | Probe privileges by **actually performing** each step (write the map, mount overlay, mount proc, `pivot_root`) in the order of [06][s06] §1. Fallbacks: rbind `/proc`, rbind `/sys`, MS_MOVE. `fsconfig(lowerdir+)` |
| R6 | **Parallel stages use a lot of memory.** Kaniko optimization notes docs/30 §5 (external) found parallel slower on OrbStack, while [experiment 08][e08] on native Linux found it faster. Peak RAM rises from about 4.3 to about 7 GB; close to the limit, export slows sharply **[measured]** | Medium | Default to the core count (Q11, [ADR-0013][adr13]). The scheduler also limits by free memory. Lazy export reduces memory pressure. A tuning guide for `--jobs` |
| R7 | **Two snapshot paths to maintain**, doubling the test effort | Medium | One shared `StageRootfs` trait, and the same content test suite run against both modes. Kaniko mode can come after the MVP |
| R8 | Cache key incompatible with kaniko | Low, unless the cache must be shared | Decided in Q3 ([ADR-0005][adr5]) |
| R9 | Rust crates lack features (streaming registry upload, tar xattrs/PAX) | Medium | Budget time to write `rk-registry` and tar headers in-house |
| R10 | A build process that dies half-way leaves a half-built workdir | Low | Mounts live in a private mount namespace and disappear on their own. `PR_SET_PDEATHSIG` kills any `RUN` still running. **Stub mountpoints** (empty files created on the rootfs as bind targets) do not disappear on their own: write the list to the state dir before creating them, clean up at startup. The next build deletes old `builds/<id>` (overlay mode: mounts are `volatile`, so the upper cannot be trusted after a crash) or wipes the workdir contents (kaniko mode) |
| R11 | **No mode can run.** A regular user on Ubuntu ≥ 24.04 (AppArmor strips user-namespace capabilities), a PSS restricted pod (`runAsNonRoot` + seccomp `RuntimeDefault`), OpenShift `restricted-v2`. Kaniko mode needs root | Medium: Ubuntu 24.04 is a common developer machine | Stop early with specific guidance: sudo; an AppArmor profile for the binary (ship a sample file, and a `.deb` package with the profile); run in a container. Document it as a known limitation, as for kaniko |
| R12 | **Most CI pods land in kaniko mode** ([experiment 03][e03]), so most users do not see the overlay benefit | Medium (product positioning) | Kaniko mode must be fast (parallel walk, stat cache, the optimizations of kaniko optimization notes docs/12–28). Do not market "overlay everywhere"; the differentiator is one binary that covers both sides. [Experiment 12][e12]: kaniko mode can also run stages in parallel (one directory per stage, bare `chroot`), 41–44% faster than sequential on W3 **[measured]** |
| R13 | **Parallel stages in kaniko mode without mounts leave `RUN` with no `/proc`** (`/dev` is available on rootful Docker thanks to `mknod`, not on rootless podman). Some commands return wrong results without an error: `ps` is empty, `os.cpus().length == 0`. Some fail outright: `next build`, Java ([experiment 12a][e12a]). A malicious `RUN` can read and write another stage's rootfs ([experiment 12][e12]) | Low (parallel only with the `--parallel-stages` flag, Q12, [ADR-0014][adr14]) | Sequential on `/` by default. With the flag: log a warning at build start that `RUN` has no `/proc`, document it for users, `mknod` `/dev` where possible, restore the metadata of `/etc`, never use a hardlink farm. A fake `/proc` of 13 static files ([experiment 12a][e12a]) is possible. It fixes `next build` and the CPU count. Still broken: JDKs installed from apt/apk, TypeScript 7, toolchains that download another JDK, `/dev/fd`, `ps`. The JVM ignores limits, so large Java builds hit OOM under a memory limit; injecting `JAVA_TOOL_OPTIONS` fixes it from about 1.75 GB upwards ([experiments 14][e14], [15][e15], [16][e16]; [ADR-0019][adr19]). No proot in 1.0 (a third binary, GPL-2.0, +26–56%; [ADR-0018][adr18]) |

## 2. MVP and 1.0 scope

Per the decisions in [05][s05], 1.0 must run at every privilege level (Q2, Q5;
[ADR-0004][adr4], [ADR-0007][adr7]). All three snapshot paths are therefore in
1.0, and the MVP is only an internal milestone to measure and validate the
design.

**MVP (internal milestone):**

- **rootful overlay mode** (on a host, and in a container that may mount); a
  store that persists across builds, with locking (Q4, [ADR-0006][adr6]); lazy
  layer export;
- a stage DAG run in parallel with `--jobs` defaulting to the core count (Q11);
- a local directory as the context, `.dockerignore`;
- Dockerfile: `FROM` (including `FROM <stage>`), `ARG` (including before
  `FROM`), `ENV`, `RUN` (shell form, exec form, heredoc), local `COPY`/`ADD`
  (`--chown`, `--chmod`, `--from`), `WORKDIR`, `USER`, `LABEL`, `EXPOSE`,
  `VOLUME`, `ENTRYPOINT`, `CMD`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`;
- pulls from public registries and from registries authenticated through docker
  `config.json`;
- outputs: registry push, Docker tar, OCI layout; parallel gzip and zstd, tar
  streamed straight into the compressor; `--reproducible`;
- `RUN --mount=type=cache` and `type=secret`; `--network=none`;
- kaniko's common flags (Q9, [ADR-0011][adr11]);
- `aarch64` cross-build in CI (Q7, [ADR-0009][adr9]).

**1.0 adds:**

- kaniko mode, overlay in a user namespace (rootless), and privilege probing
  that picks the mode;
- a registry layer cache with its own versioned key (Q3);
- `ADD <url>`/git/tar, `RUN --mount=type=bind`, `ONBUILD`,
  `COPY --link`/`--parents`/`--exclude`, registry mirrors, credential helpers,
  tar/git/https contexts;
- the isolation options `--seccomp` and `--userns` (Q1);
- `ranikust prune`.

**Out of scope** (Q6, [ADR-0008][adr8]): bake, the warmer, s3/gcs/azure
contexts, multi-platform builds.

## 3. Roadmap and estimates

All durations are **[inferred]**, for one person working full time who already
knows Rust and Linux.

| Phase | Content | Duration |
|---|---|---|
| 0. Spike | Mostly done by the survey PoCs (mostly written in Go). Remaining: rebuild the sandbox + overlay in Rust (single-threaded fork, PDEATHSIG, `fsconfig(lowerdir+)`), and **re-run experiments 01, 02 and 06 as real root on the host** (since done: [experiment 06a][e06a], see §4 item 7) | 1–2 weeks |
| 1. MVP (rootful overlay) | The MVP scope in §2. In-house parser (3,000–3,300 lines), pigz-style gzip, `oci-client` wrapper, `WriteUpperdir`-style upperdir exporter, stubs. Builds W1, W2 (cold) and W3 of `nextjs-kaniko`. Measure `--jobs` 1/2/default | 8–12 weeks |
| 2. Kaniko mode | Port the walk + hash snapshot with the optimizations of kaniko optimization notes docs/12 and docs/13; workdir = rootfs, safe `DeleteFilesystem`; a separate rootfs directory per stage, run along the DAG (full sandbox when mounting is possible, bare `chroot` otherwise; [experiment 12][e12]); the same content test suite for all modes | 5–6 weeks |
| 3. Rootless + privilege probing | Overlay in a user namespace (multi-range maps, `newuidmap` on a host); fallbacks rbind `/proc`, `/sys`, MS_MOVE; probing per [06][s06] §1, with a "no mode" outcome plus guidance and a sample AppArmor profile; a test matrix (host root, podman/Docker privileged, `CAP_SYS_ADMIN`, rootless podman, default Docker, Ubuntu 24.04 regular user, Kubernetes pod); `--seccomp`, `--userns` | 5–7 weeks |
| 4. Cache | Versioned cache key, cache lookup/push via registry, lookahead, skipping stages that fully hit the cache | 3–4 weeks |
| 5. Feature completion | `ADD` URL/git/tar, bind mounts (including `from=<stage>`), `ONBUILD`, `COPY --link`/`--parents`/`--exclude`, registry mirrors, credential helpers, tar/git/https contexts, `prune` | 4–6 weeks |
| 6. Hardening and release | All 201 integration Dockerfiles in every mode; fuzzing of the parser and extraction; host-safety tests; user documentation (isolation level of each mode, `--jobs` tuning guide); release packaging | 4–6 weeks |

**Total to 1.0: about 30–43 weeks (7–10 months) for one person.** Two people
could cut that by about 40%: parser/context/builder/cache and
rootfs/sandbox/overlay/layer/registry are two fairly independent tracks.

How the PoCs changed the estimate **[inferred]**:

- Phase 0 got shorter, because most of the technical risk has been checked.
- Phases 1 and 3 got longer, because the work list is more concrete: the parser
  must be written in-house; the sandbox gained stubs, PDEATHSIG, capability
  dropping and fallbacks; probing gained a "refuse" outcome.
- Phase 2 gained about 1 week for a separate rootfs directory per stage
  ([experiment 12][e12]).
- No finding changed the "feasible" conclusion.

Compared with the previous estimate (5–7 months), the increase comes from phase
3: rootless support and the privilege test matrix follow from Q2/Q5. The
decrease comes from dropping bake, the warmer and cloud contexts (Q6).

## 4. Verification plan

1. **Parser golden tests:** for each Dockerfile in
   `kaniko/integration/dockerfiles/`, compare the expanded instruction list with
   the output of BuildKit's parser. The golden files can be generated by a small
   Go program that calls `instructions.Parse`.
2. **Image content comparison** between ranikust and kaniko for each Dockerfile.
   Kaniko mode should produce the same layers as kaniko, except for known
   differences such as timestamps. Overlay mode is compared on flattened
   content. Flatten each image into a list of
   `(path, type, mode, uid, gid, size, sha256, linkname, xattr)` and diff; also
   diff the config (`Env`, `Entrypoint`, `Cmd`, `WorkingDir`, `User`, `Labels`
   and so on). This can build on `nextjs-kaniko/digest-check/`.
3. **Equivalent results between modes:** for the same Dockerfile, overlay mode
   and kaniko mode must produce the same **flattened content**, except for the
   list of known differences (split hardlinks, unchanged copied-up files; R4).
   Layer digests may differ. Parallel (`--jobs` default) and sequential
   (`--jobs 1`) runs must produce **the same digest**.
4. **Privilege matrix:** run the same Dockerfile set at every privilege level
   listed in phase 3. Check that the tool picks the right mode and that the
   flattened content matches.
5. **Performance:** use the same harness, `nextjs-kaniko/bench/run-fair.sh`
   (interleaved, 5 rounds, timing method of kaniko optimization notes docs/28
   §4), with these configurations: kaniko `k-stream`, tuned BuildKit, ranikust
   overlay (`--jobs` 1 and default), ranikust kaniko mode. Workload 1, workload
   2 (cold/warm/partial), workload 3 (`app-par`).
6. **Host safety:** targeted tests:
   - a layer containing `../etc/passwd`, a symlink pointing outside, a hardlink
     to a host file;
   - a Dockerfile that runs `mount`, `setsid sleep 1000 &`, a fork bomb (with a
     cgroup limit), or writes to `/etc/resolv.conf`;
   - `--workdir` pointing at `/`, at `/home`, or at a directory without the
     marker file.

   After each test, check that the host is unchanged and that no mount or
   process is left behind.
7. **PoCs at several privilege levels.** Experiments 01, 02 and 06 were re-run
   as real root on the host ([experiment 06a][e06a]). Real rootful Docker was
   run (29.1, in an Ubuntu 22.04 VM, cgroup v2 and v1, [experiment 13][e13]). A
   real Kubernetes cluster is still missing.

## Open questions

- A real Kubernetes cluster has not been tested (§4 item 7); see the "not
  verified" tables in [00][s00] §4.
- The memory-aware scheduler limit (R6) is a proposal; it has not been built.

## References

1. Experiments: [02][e02], [03][e03], [06a][e06a], [08][e08], [12][e12],
   [12a][e12a], [13][e13], [14][e14], [15][e15], [16][e16].
2. kaniko optimization notes (external, not in this repository): docs/12
   stat-and-parallel-hash, docs/13 fast-resolve-paths, docs/28
   stream-tar-compress (§4 timing method), docs/30 parallel-stages-a (§5).
3. Benchmark workloads and harness from the separate kaniko optimization
   project: `nextjs-kaniko/app`, `nextjs-kaniko/app-par`,
   `nextjs-kaniko/bench/run-fair.sh`, `nextjs-kaniko/digest-check/` (see the
   [research README](../README.md#workloads)).
4. Survey chapters: [00 Summary][s00], [02 kaniko-mode requirements][s02],
   [05 Decisions][s05], [06 Overlay and parallelism][s06].
5. ADRs: [0003][adr3]–[0014][adr14], [0018][adr18], [0019][adr19].

[s00]: 00-summary.md
[s02]: 02-kaniko-mode-requirements.md
[s05]: 05-decisions.md
[s06]: 06-overlay-and-parallelism.md
[e02]: ../experiments/02-run-sandbox.md
[e03]: ../experiments/03-privilege-matrix.md
[e06a]: ../experiments/06a-root-rerun.md
[e08]: ../experiments/08-parallel-stages-lazy-export.md
[e12]: ../experiments/12-kaniko-mode-parallel.md
[e12a]: ../experiments/12a-fake-proc.md
[e13]: ../experiments/13-docker-rootful-vm.md
[e14]: ../experiments/14-real-builds-without-proc.md
[e15]: ../experiments/15-java-next-concurrency.md
[e16]: ../experiments/16-cgroup-v1-cpuset.md
[adr3]: ../../adr/0003-dockerfile-trust-and-default-isolation.md
[adr4]: ../../adr/0004-support-every-privilege-level.md
[adr5]: ../../adr/0005-own-versioned-cache-key.md
[adr6]: ../../adr/0006-persistent-store.md
[adr7]: ../../adr/0007-run-anywhere-shared-store.md
[adr8]: ../../adr/0008-out-of-scope-bake-warmer-cloud-contexts.md
[adr9]: ../../adr/0009-x86-64-first-aarch64-ready.md
[adr11]: ../../adr/0011-accept-common-kaniko-flags.md
[adr12]: ../../adr/0012-accept-divergence-between-modes.md
[adr13]: ../../adr/0013-jobs-default-available-parallelism.md
[adr14]: ../../adr/0014-kaniko-mode-sequential-by-default.md
[adr18]: ../../adr/0018-no-proot-in-1-0.md
[adr19]: ../../adr/0019-java-tool-options-on-parallel-path.md
