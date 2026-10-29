# Phase 1: MVP, overlay mode as root

**TL;DR:** Build real Dockerfiles in overlay mode as root, with parallel
stages, lazy export and all three outputs, measured against kaniko and
BuildKit. Start with the foundations; O1–O4 are resolved (ADR-0021 to
ADR-0024), O5 (dependency picks) is still open.

Source: survey 04 §2–§3 and survey 00 §4 (`docs/research/survey/`), an
internal milestone of 8–12 weeks for one person `[inferred]`. Tick items here
as they land.

## Scope

Status `[ ]` open, `[~]` in progress, `[x]` done; owner = the crate holding
the code.

| | Item | Owner |
|---|---|---|
| [ ] | OCI and Docker v2s2 manifests, index, config, media types | `rnk-oci` |
| [ ] | Dockerfile front end: lexer, directives, heredocs, `ARG`/`ENV` expansion, instructions, stage DAG; BuildKit goldens on kaniko's 200 Dockerfiles | `rnk-dockerfile` |
| [ ] | `.dockerignore` with exact `patternmatcher` semantics | `rnk-ignore` |
| [ ] | `RootPath` (`openat2` + `RESOLVE_IN_ROOT`); extraction of a decompressed tar into the overlay whiteout form; copy, ownership, raw modes, xattrs; `USER`/`--chown` name lookup in the root; safe deletion | `rnk-rootfs` |
| [ ] | Deterministic tar (the writer sorts entries and adds missing parent headers), pigz-style parallel gzip, zstd in 128 KiB chunks, gzip/zstd decoder for pulled blobs, streaming digests | `rnk-layer` |
| [ ] | Pulls from public registries and with docker `config.json` auth; rustls with `ring`, trusting host CAs plus webpki roots (`ADR-0024`); `--registry-certificate` | `rnk-registry` |
| [ ] | Persistent store in `--state-dir`: `blobs/`, `layers/`, `caches/`, `tmp/`, per-digest `flock`, atomic rename, start-up cleanup; `BuildPaths` (`ADR-0006`, `ADR-0021`, `ADR-0022`) | `rnk-store` |
| [ ] | Process set-up: flags (`--mode`, `ADR-0023`), workdir checks, marker, private mount namespace, the probes in order, workdir `flock` in kaniko mode, spawner thread, all while single-threaded | `ranikust` |
| [ ] | Overlay-root probe (steps 1, 3, 4 of `design/modes-and-detection.md`): namespaces, `proc`, `pivot_root`, `sysfs`; overlay mount and `lowerdir+`; refusal with the diagnosis `[inferred]` | `rnk-sandbox`, `rnk-snapshot`; run by `ranikust` |
| [ ] | `RUN` sandbox: namespaces, mounts, `pivot_root`/`MS_MOVE`, capability drop, `USER` from the root, stubs and journal, PDEATHSIG | `rnk-sandbox` |
| [ ] | `RUN --mount=type=cache\|secret`; `--network=none` | `rnk-sandbox`, `rnk-dockerfile` |
| [ ] | Overlay stage roots: `StageRootfs`/`Snapshotter` (its signature is designed here, task 9), `fsconfig(lowerdir+)` with `mount(2)` fallback, `ADR-0016` options, one upper per instruction, root metadata from the lower; base-layer extraction into the store (`rnk-layer` decoder → `rnk-rootfs::extract`) | `rnk-snapshot` |
| [ ] | Upperdir → OCI layer exporter, `WriteUpperdir`-style: whiteouts first, raw modes, overlay xattrs stripped; entries fed to the `rnk-layer` writer, which sorts them and adds parent headers | `rnk-snapshot` → `rnk-layer` |
| [ ] | Executors: `FROM` (also `<stage>`), `ARG` (also before `FROM`), `ENV`, `RUN` (shell, exec, heredoc), local `COPY`/`ADD` (`--chown`, `--chmod`, `--from`), `WORKDIR`, `USER`, `LABEL`, `EXPOSE`, `VOLUME`, `ENTRYPOINT`, `CMD`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK` | `rnk-builder` |
| [ ] | Local directory context; locating the ignore file (`<Dockerfile>.dockerignore` first) | `rnk-context` |
| [ ] | Lazy export, config updates, image assembly; canonical JSON with sorted keys | `rnk-builder`, `rnk-oci` |
| [ ] | Outputs: parallel push with cross-repository mount and upload fallback, Docker tar, OCI layout; `--reproducible` | `rnk-builder`, `rnk-registry`, `rnk-layer` |
| [ ] | Stage DAG: up-front pulls, `--jobs` (default `available_parallelism()`, `ADR-0013`), memory limit, cancel on first failure, per-stage log prefix | `rnk-sched` |
| [ ] | Common kaniko flags and the subcommands (`ADR-0011`) | `ranikust` |
| [ ] | aarch64 in CI: cargo-zigbuild cross-build, unit tests under qemu-user (`ADR-0009`; today only `cargo check`) | CI |
| [ ] | T2 VM harness `just test-vm` (`docs/testing.md`); overlay as root cannot be tested without it `[inferred]` | tests |
| [ ] | `nextjs-kaniko` harness hooked up: `bench/run-fair.sh`, `digest-check/` (separate project, e.g. `../nextjs-kaniko`) | tests |

## Task order

Two mostly independent tracks (survey 04 §3): front end
(`rnk-dockerfile`, `rnk-ignore`, `rnk-context`, `rnk-builder`) and low level
(`rnk-rootfs`, `rnk-sandbox`, `rnk-snapshot`, `rnk-layer`, `rnk-registry`,
`rnk-store`).

| # | Task | Needs | Blocked by |
|---|---|---|---|
| 1 | `rnk-oci` types | | |
| 2 | `rnk-dockerfile` parser with BuildKit goldens | | |
| 3 | `rnk-ignore` | | |
| 4 | `rnk-rootfs`: `RootPath` → extraction → copy, ownership, xattrs → safe deletion | | |
| 5 | `rnk-layer`: tar → gzip → zstd → digests | | |
| 6 | `rnk-registry`: auth, TLS (`ADR-0024`), streamed pull | 1 | |
| 7 | `rnk-store`: layout, locks, publication, start-up clean-up (`ADR-0021`, `ADR-0022`) | 4, 5, 6 | |
| 8 | `ranikust` set-up and flags (`ADR-0023`); `rnk-sandbox` sandbox and its probes | 4 | |
| 9 | `rnk-snapshot` overlay roots, overlay probes, extraction and exporter; design the `Snapshotter` signature | 4, 5, 7 | |
| 10 | `rnk-context` local directory | 3 | |
| 11 | `rnk-builder`: executors, lazy export, assembly, outputs and push | 1, 2, 6, 8, 9, 10 | |
| 12 | `rnk-sched`: DAG, `--jobs`, cancellation | 2, 11 | |
| 13 | CLI end to end, outputs, acceptance runs | all | VM harness |

Any time: the aarch64 CI job and the VM harness (before 8 and 9 can be
tested as root).

## Acceptance criteria

- [ ] Builds W1 (`nextjs-kaniko/app`), W2 cold (`nextjs-kaniko/app-cache`;
      it needs `COPY --from=<image>`) and W3 (`nextjs-kaniko/app-par`).
- [ ] `--jobs 1`, `2` and default measured on W3 with `run-fair.sh`
      (`exp-08` at 32 CPUs, lazy: 8.44, 6.52, 6.02 s); peak memory recorded;
      with `--reproducible`, `--jobs 1` and default give identical digests.
- [ ] Flattened content `(path, type, mode, uid, gid, size, sha256, link,
      xattrs)` and config compared with kaniko and BuildKit: only `ADR-0012`
      differences, plus W1's 1,654 non-deterministic files (`exp-07` §3).
- [ ] Push to a loopback and a real network registry; `--tar-path` loads in
      podman or docker; `--oci-layout-path` is a valid layout.
- [ ] gzip and zstd run in parallel, tar streamed into the compressor, and
      give the same bytes at 1 and N threads.
- [ ] `--reproducible`: two builds at different times, same digest.
- [ ] `RUN --mount=type=cache` persists across builds; `type=secret` never
      reaches a layer, the config, a log or the environment.
- [ ] `--network=none`: `RUN` sees only loopback.
- [ ] Common kaniko flags: list fixed ([kaniko-map.md](kaniko-map.md#flags)),
      each accepted flag tested for its meaning, mapping in the book.
- [ ] aarch64 in CI: `just check-aarch64` (exists) plus the cross-build and
      qemu-user unit tests of `ADR-0009`.

## Deferred

| Phase | What |
|---|---|
| 2 | kaniko mode: walk and hash, stat cache, `LayeredMap`, safe filesystem deletion, per-stage directories, `--parallel-stages`, fake `/proc`, `JAVA_TOOL_OPTIONS` |
| 3 | Rootless overlay, the full probe and refusal guidance, AppArmor sample profile, privilege matrix, `--seccomp`, `--userns` |
| 4 | `rnk-cache`: registry layer cache with own versioned keys, lookahead, skipping cached stages, W2 warm and partial |
| 5 | `ADD` URL/git/tar, `RUN --mount=type=bind`, `ONBUILD`, `COPY --link`/`--parents`/`--exclude`, mirrors, credential helpers, tar/git/HTTPS contexts, `prune` |
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
- **O5 Dependency picks left open in survey 03 §3.** Open. Phase 1 picks
  (`reqwest` or `hyper`, `sha2` or `aws-lc-rs`, `rustix` or `nix`, gzip by
  hand or `gzp`, `docker_credential` as is or vendored, the CLI parser):
  [Q-012](../open-questions.md#q-012-which-crates-fill-the-phase-1-dependency-picks).
  Phase 2 picks (`highway` or `xxhash-rust`, `jwalk` or `rayon` +
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

Every open question, in task order, is in
[open-questions.md](../open-questions.md#open).
