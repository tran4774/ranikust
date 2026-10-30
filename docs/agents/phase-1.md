# Phase 1: MVP, overlay mode as root

**TL;DR:** Build real Dockerfiles in overlay mode as root, with parallel
stages, lazy export and all three outputs, measured against kaniko and
BuildKit. Start with the foundations; O1–O5 are resolved (ADR-0021 to
ADR-0024, ADR-0030).

Source: survey 04 §2–§3 and survey 00 §4 (`docs/research/survey/`), an
internal milestone of 8–12 weeks for one person `[inferred]`. Tick items here
as they land.

## Scope

Status `[ ]` open, `[~]` in progress, `[x]` done; owner = the crate holding
the code.

| | Item | Owner |
|---|---|---|
| [x] | OCI and Docker v2s2 manifests, index, config, media types; config edits of the metadata instructions, history and `diff_ids` appends, docker-save `manifest.json` and OCI layout types (Q-007; config edits, history and empty fields follow BuildKit, `ADR-0032`, code to follow) | `rnk-oci` |
| [x] | Dockerfile front end: lexer, directives, heredocs, `ARG`/`ENV` expansion, instructions, stage DAG; BuildKit goldens on kaniko's 200 Dockerfiles | `rnk-dockerfile` |
| [x] | `.dockerignore` with exact `patternmatcher` semantics | `rnk-ignore` |
| [~] | `RootPath` (`openat2` + `RESOLVE_IN_ROOT`); extraction of a decompressed tar into the overlay whiteout form; copy, ownership, raw modes, xattrs; `USER`/`--chown` name lookup in the root; safe deletion (done; the workdir checks wait for Q-011) | `rnk-rootfs` |
| [x] | Deterministic tar (the writer sorts entries and adds missing parent headers), pigz-style parallel gzip, parallel zstd as independent 1 MiB frames, gzip/zstd decoder for pulled blobs, streaming digests | `rnk-layer` |
| [x] | Pulls from public registries and with docker `config.json` auth; rustls with `ring`, trusting host CAs plus webpki roots (`ADR-0024`); `--registry-certificate` (the client; the flags come with task 8) | `rnk-registry` |
| [x] | Persistent store in `--state-dir`: `blobs/`, `layers/`, `caches/`, `tmp/`, per-digest `flock`, atomic rename, start-up cleanup; `BuildPaths` (`ADR-0006`, `ADR-0021`, `ADR-0022`); T0 across threads, processes and `SIGKILL` (`ranikust` still has to open and close it) | `rnk-store` |
| [~] | Process set-up: flags (`--mode`, `ADR-0023`) and validation into `BuildConfig`; private mount namespace (refused in a plain `chroot`), the store opened before the probes (marker, start-up clean-up; Q-111), the probes in order, the outcome, spawner thread, all while single-threaded (done; T0, T1 `just smoke`, T2 `just test-vm smoke`); left: workdir checks [open: Q-011], workdir `flock` in kaniko mode | `ranikust` |
| [~] | Overlay-root probe (steps 1, 3, 4 of `design/modes-and-detection.md`): namespaces, `proc`, `pivot_root`, `sysfs`; overlay mount and `lowerdir+`; refusal with the diagnosis `[inferred]` (the sandbox's step 3 is done: the real sequence, `pivot_root` else `MS_MOVE`; the overlay steps 1 and 4 are done: `rnk_snapshot::probe` mounts with `lowerdir+`, else `mount(2)`, and checks a write through it; `ranikust` runs both and refuses with guidance; left: the user-namespace steps, Phase 3) | `rnk-sandbox`, `rnk-snapshot`; run by `ranikust` |
| [~] | `RUN` sandbox: namespaces, mounts, `pivot_root`/`MS_MOVE`, capability drop, `USER` from the root, stubs and journal, PDEATHSIG (done; runs in rootless podman, `just smoke`, and as real root in the VM, `just test-vm`, on kernels 5.15 and 6.8: `pivot_root` on the host and in `docker run --privileged`, `MS_MOVE` under Docker's seccomp profile) | `rnk-sandbox` |
| [~] | `RUN --mount=type=cache\|secret`; `--network=none` (done, as BuildKit (ADR-0032): cache mounts leased from the store with `sharing=`, secrets as a private `0400` file or `env=`, `--secret` in kaniko's and buildctl's syntax, `RUN --network=none` and `--network=none`, `--allow network.host`; the e2e build checks loopback only, the secret file's mode and owner, and that the secret reaches no blob; left: the acceptance check that a cache persists across builds) | `rnk-sandbox`, `rnk-builder` |
| [x] | Overlay stage roots: `StageRootfs`/`Snapshotter` (interim signature, [open: Q-024]), `fsconfig(lowerdir+)` with `mount(2)` fallback, `ADR-0016` options, one upper per instruction, root metadata from the lower; base-layer extraction into the store (`rnk-layer` decoder → `rnk-rootfs::extract_overlay`) (done, T0 and T2 `just test-vm unit`; no merging near 500 lowers, [open: Q-026]) | `rnk-snapshot` |
| [x] | Upperdir → OCI layer exporter, `WriteUpperdir`-style: whiteouts first, raw modes, overlay xattrs stripped; entries fed to the `rnk-layer` writer, which sorts them and adds parent headers (done, T0; `blocks_read` not recorded yet) | `rnk-snapshot` → `rnk-layer` |
| [~] | Executors: `FROM` (also `<stage>`), `ARG` (also before `FROM`), `ENV`, `RUN` (shell, exec, heredoc), local `COPY`/`ADD` (`--chown`, `--chmod`, `--from`), `WORKDIR`, `USER`, `LABEL`, `EXPOSE`, `VOLUME`, `ENTRYPOINT`, `CMD`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK` (done: `build_stage`, T0 on a fake snapshotter, T1 `just smoke` and T2 `just test-vm smoke` build `tests/smoke/e2e`; left: config goldens against kaniko and BuildKit, W1–W3) | `rnk-builder` |
| [x] | Local directory context; locating the ignore file (`<Dockerfile>.dockerignore` first); the context walk as BuildKit's filtered walk (`ADR-0027`) | `rnk-context` |
| [x] | Lazy export, config updates, image assembly; canonical JSON with sorted keys (`LayerDemand`, `LayerExporter` over the snapshot's diff, `assemble`; a layer is encoded right after its instruction, not overlapped with the next one yet) | `rnk-builder`, `rnk-oci` |
| [~] | Outputs: parallel push with cross-repository mount and upload fallback, Docker tar, OCI layout; `--reproducible` (done and called by `ranikust build`; the OCI layout and Docker tar load in podman and Docker; one digest with `--jobs 1` and the default under `--reproducible`; left: a push to a real registry in an end-to-end run; OCI by default and clamped times under `SOURCE_DATE_EPOCH`, `ADR-0032`) | `rnk-builder`, `rnk-registry`, `rnk-layer` |
| [~] | Stage DAG: up-front pulls, `--jobs` (default `available_parallelism()`, `ADR-0013`), memory limit, cancel on first failure, per-stage log prefix (done; `ranikust`'s runner builds each stage on a thread of its own and takes a `--jobs` permit per `RUN`; left: the W3 digest test, the memory thresholds [open: Q-030]) | `rnk-sched` |
| [~] | Common kaniko flags and the subcommands (`ADR-0011`): `build` and `version`, accepted flags parsed and validated, the others refused with a reason (done); final list [open: Q-023], `--cache` [open: Q-021], `--secret` and `--allow` (ADR-0032), `push`/`login` [open: Q-048] | `ranikust` |
| [~] | aarch64 in CI: cargo-zigbuild cross-build of the static binary, T0 tests natively on GitHub's `ubuntu-24.04-arm` runner, or under qemu-user 9.1+ on x86_64 (`ADR-0009`, `just test-aarch64`); the release binary is static but not static-pie [open: Q-140] | CI |
| [~] | T2 VM harness `just test-vm` (`docs/testing.md`): `unit` and `smoke` suites on Ubuntu 22.04 and 24.04 (done); cgroup v1, kernels before 5.11, the host-safety attack suite and a CI job with KVM to come | tests |
| [~] | `nextjs-kaniko` harness hooked up: `bench/run-fair.sh`, `digest-check/` (separate project, e.g. `../nextjs-kaniko`) (the `e2e` VM suite, `just test-vm e2e`: W1–W3 built by kaniko and BuildKit as `run-fair.sh` does, flattened-image and config comparison, `check-blobs.py`, metrics side by side; the `ranikust` leg waits for `ranikust build` and a way to reach a plain-HTTP registry, (Q-200)) | tests |
| [~] | Build metrics (`ADR-0025`): subscriber, metrics layer, JSON report on every exit path, `RANIKUST_METRICS_FILE`, `--metrics-file`, `meta.mode` and `meta.options` (done); every crate's spans per `design/metrics.md` with its own task | `rnk-metrics`, `ranikust`; each crate for its spans |
| [x] | Build toolchain: cargo-zigbuild with zig in the `justfile` and CI for the musl x86_64 and aarch64 builds (`ADR-0020`, Q-052); zig compiles every C dependency, rustc links x86_64 with `cc` to keep static-pie; mimalloc is the binary's allocator | build, CI, `ranikust` |
| [x] | Start-up check for Linux 5.8 (`openat2` with `RESOLVE_IN_ROOT`, `statx` mount id), with a clear refusal (`ADR-0028`) | `ranikust`; probe in `rnk-rootfs` |
| [x] | Base-layer extraction in the overlay form: a hardlink into a lower layer is copied from the lower layers as a regular file (Q-055; `rnk_rootfs::extract_overlay`, called by `rnk_snapshot`'s `base_layer`; keyed by chain ID, as BuildKit, ADR-0032) | `rnk-rootfs`, `rnk-snapshot` |

## Task order

Two mostly independent tracks (survey 04 §3): front end
(`rnk-dockerfile`, `rnk-ignore`, `rnk-context`, `rnk-builder`) and low level
(`rnk-rootfs`, `rnk-sandbox`, `rnk-snapshot`, `rnk-layer`, `rnk-registry`,
`rnk-store`).

| # | Task | Needs | Blocked by |
|---|---|---|---|
| 0 | `rnk-metrics` and the binary's wiring (`ADR-0025`); done | | |
| 1 | `rnk-oci` types (done) | | |
| 2 | `rnk-dockerfile` parser with BuildKit goldens | | |
| 3 | `rnk-ignore` | | |
| 4 | `rnk-rootfs`: `RootPath` → extraction → copy, ownership, xattrs → safe deletion (done but `check_workdir`, Q-011) | | |
| 5 | `rnk-layer`: tar → gzip → digests → zstd (done) | 5a | |
| 5a | cargo-zigbuild toolchain in the `justfile` and CI (`ADR-0020`, Q-052); mimalloc (done) | | |
| 6 | `rnk-registry`: auth, TLS (`ADR-0024`), streamed pull (done; also pushes, mounts and retries for task 11; `ring` needs 5a for the musl targets) | 1 | |
| 7 | `rnk-store`: layout, locks, publication, start-up clean-up (`ADR-0021`, `ADR-0022`) (done; `prune` is Phase 5) | 4, 5, 6 | |
| 8 | `ranikust` set-up and flags (`ADR-0023`), the Linux 5.8 check (`ADR-0028`); `rnk-sandbox` sandbox and its probes; `ranikust` runs them, opens the store and picks the outcome (done) | 4 | |
| 9 | `rnk-snapshot` overlay roots, overlay probes, extraction (hardlinks into lower layers copied, Q-055) and exporter; design the `Snapshotter` signature (done; interim signature, Q-024) | 4, 5, 7 | |
| 10 | `rnk-context` local directory (done); `COPY` sources are resolved in 11 (Q-080, `ADR-0032`) | 3 | |
| 11 | `rnk-builder`: executors, lazy export, assembly, outputs and push (done; config goldens and W1–W3 left) | 1, 2, 6, 8, 9, 10 | |
| 12 | `rnk-sched`: DAG, `--jobs`, cancellation (done behind the `StageRunner` trait, tested with fakes; the builder's runner comes with 11) | 2, 11 | |
| 13 | CLI end to end, outputs, acceptance runs (`ranikust build` builds end to end; the acceptance runs are left) | all | |

Any time: the VM harness (before 8 and 9 can be
tested as root).

Every task instruments its crate as its card's `## Metrics` and
[design/metrics.md](../design/metrics.md#spans-per-crate) list, tested
with `rnk_metrics::Capture`; a task is not done without it.

## Acceptance criteria

- [x] Builds W1 (`nextjs-kaniko/app`), W2 cold (`nextjs-kaniko/app-cache`;
      it needs `COPY --from=<image>`) and W3 (`nextjs-kaniko/app-par`).
      Built by `just test-vm e2e` on noble, one round (2026-10-30); the W3
      digest across `--jobs` is to be compared by content without the
      workload's `du` files ([Q-187](../open-questions.md#q-187-how-does-the-w3-digest-check-treat-the-workloads-du-output)).
- [ ] `--jobs 1`, `2` and default measured on W3 with `run-fair.sh`
      (`exp-08` at 32 CPUs, lazy: 8.44, 6.52, 6.02 s); peak memory recorded;
      with `--reproducible`, `--jobs 1` and default give identical digests.
- [ ] Flattened content `(path, type, mode, uid, gid, size, sha256, link,
      xattrs)` and config compared with kaniko and BuildKit: only `ADR-0012`
      differences, plus W1's 1,654 non-deterministic files (`exp-07` §3).
- [x] Push to a loopback and a real network registry; `--tar-path` loads in
      podman or docker; `--oci-layout-path` is a valid layout. `just
      test-vm push` on noble (2026-10-30): one build as real root pushed to
      the VM's loopback registry (plain HTTP, anonymous) and over the
      network to the host's test registries (`tests/registry`): HTTP with
      `--insecure-registry`, HTTPS with a self-signed CA through
      `--registry-certificate` and basic auth from a `config.json`. All
      three manifests read back with the printed digest; Docker ran the
      pulled image, the tarball and the layout, whose blobs all hash to
      their names.
- [ ] gzip and zstd run in parallel, tar streamed into the compressor, and
      give the same bytes at 1 and N threads.
- [ ] `--reproducible`: two builds at different times, same digest.
- [ ] `RUN --mount=type=cache` persists across builds; `type=secret` never
      reaches a layer, the config, a log or the environment.
- [ ] `--network=none`: `RUN` sees only loopback.
- [ ] Common kaniko flags: list fixed ([kaniko-map.md](kaniko-map.md#flags)),
      each accepted flag tested for its meaning, mapping in the book.
- [x] aarch64 in CI: `just check-aarch64` plus the static build and the
      unit tests of `ADR-0009` (`just test-aarch64`), natively on the
      `ubuntu-24.04-arm` runner with nothing skipped; green in CI run
      41056969633 (2026-10-30). The binary is not static-pie yet [open: Q-140].
- [ ] W1 and W3 metrics reports compared with kaniko's by the bench harness
      ([phase map](../design/metrics.md#comparing-with-kaniko)); with
      `--reproducible`, the same digests with metrics on and off.

## Deferred

| Phase | What |
|---|---|
| 2 | kaniko mode: walk and hash, stat cache, `LayeredMap`, safe filesystem deletion, per-stage directories, `--parallel-stages`, fake `/proc`, `JAVA_TOOL_OPTIONS` |
| 3 | Rootless overlay, the full probe and refusal guidance, AppArmor sample profile, privilege matrix, `--seccomp`, `--userns` |
| 4 | `rnk-cache`: registry layer cache with own versioned keys, lookahead, skipping cached stages, W2 warm and partial |
| 5 | `ADD` URL/git/tar, `ADD --checksum`/`--keep-git-dir`/`--unpack`, `RUN --mount=type=bind\|tmpfs\|ssh`, `RUN --mount=type=cache,from=`, `ONBUILD`, `COPY --link`/`--parents`/`--exclude`, mirrors, credential helpers, tar/git/HTTPS contexts, `prune` |
| 6 | 200 integration Dockerfiles in every mode, fuzzing, the host-safety attack suite, user docs, packaging |
| never | bake, the warmer, cloud contexts, multi-platform builds (`ADR-0008`) |

## Open questions blocking Phase 1

- **O1 Store owner.** Resolved: a new crate, `rnk-store`, owns the store,
  its locks, clean-up, `prune` and `BuildPaths`
  ([ADR-0021](../adr/0021-rnk-store-crate.md)). Task 7 is unblocked.
- **O2 `--snapshot-mode` clash.** Resolved: `--mode=auto|overlay|kaniko`
  selects the mode; `--snapshot-mode=full|redo|time` keeps kaniko's meaning
  and is ignored with a warning in overlay mode
  ([ADR-0023](../adr/0023-mode-flag.md)). Task 8 is unblocked.
- **O3 Store location.** Resolved: one `--state-dir` for every mode, by
  default `<workdir>/.ranikust` (overlay) or `/ranikust/state` (kaniko);
  only kaniko mode locks the workdir
  ([ADR-0022](../adr/0022-common-state-dir.md)). Task 7 is unblocked.
- **O4 TLS stack.** Resolved: rustls with `ring`; the trust store is the
  host's CAs plus the embedded webpki roots
  ([ADR-0024](../adr/0024-tls-rustls-ring.md)). Task 6 is unblocked.
- **O5 Dependency picks left open in survey 03 §3.** Resolved for Phase 1:
  `rustix`, `sha2`, gzip by hand, `reqwest`, no `docker_credential`, `clap`
  ([ADR-0030](../adr/0030-phase-1-dependency-picks.md), Q-012).
  Still open, Phase 2 picks (`highway` or `xxhash-rust`, `jwalk` or `rayon` +
  `getdents64`):
  [Q-038](../open-questions.md#q-038-which-crates-fill-the-phase-2-dependency-picks).

Also open, not blocking a start:

- the final `--parallel-stages` name:
  [Q-019](../open-questions.md#q-019-what-is-the-final-name-of-the-parallel-stages-flag);
- `--platform` versus kaniko's `--custom-platform`:
  [Q-020](../open-questions.md#q-020-is-custom-platform-accepted-as-an-alias-of-platform);
- what `--cache`/`--cache-repo` do before Phase 4:
  [Q-021](../open-questions.md#q-021-what-do-the-cache-flags-do-before-phase-4);
- the memory-limit algorithm (`design/scheduler.md#limits`):
  [Q-030](../open-questions.md#q-030-what-are-the-memory-limit-algorithm-and-thresholds).

Settled by following BuildKit
([ADR-0032](../adr/0032-observable-behaviour-follows-buildkit.md)): the
config, history, `COPY`, `RUN --mount` and registry questions; the code
changes land in follow-up branches. Escalated to the maintainer under that
rule: `RUN --network=none` without `CAP_NET_ADMIN` (Q-183, answered in
[ADR-0033](../adr/0033-network-none-loopback-through-a-user-namespace.md):
refused, or with `--allow network.none-userns` a network namespace in a
user namespace under seccomp), remote repository contexts (Q-045, Phase 5,
since decided in [ADR-0035](../adr/0035-git-contexts-built-in-or-external-git.md))
and filtering unchanged copied-up files (Q-049, settled by ADR-0034:
BuildKit's full rule).

Every open question, in task order, is in
[open-questions.md](../open-questions.md#open).
