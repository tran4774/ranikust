# Overview

**TL;DR:** ranikust builds OCI images from Dockerfiles like kaniko, as one
static binary: overlayfs snapshots when it may mount, a kaniko-style walk and
hash otherwise. Pre-alpha: Phase 1 is done; Phase 2 (kaniko mode) is in progress.

## What ranikust is

- A Rust rewrite of kaniko's executor: Dockerfile and context in; image
  pushed, or written as a Docker tar or an OCI layout.
- One static-pie musl binary (x86_64 released, aarch64 kept buildable) for
  hosts and containers; replaces kaniko's executor, `tini` and credential
  helpers (`ADR-0009`, `ADR-0020`).
- Common kaniko flags accepted (`ADR-0011`, [kaniko-map.md](kaniko-map.md));
  kaniko's cache is not (`ADR-0005`). Out of scope: bake, the warmer, cloud
  contexts, multi-platform builds (`ADR-0008`).
- Writes nothing outside the workdir, the state directory or store, and the
  requested outputs ([rules.md](rules.md#security-invariants)).

## Build pipeline

Instructions inside a stage run in order; parallelism is between stages and
in I/O. Full text: [`ARCHITECTURE.md`](../../ARCHITECTURE.md#data-flow-of-a-build).

| # | Step | Owner | Notes |
|---|---|---|---|
| 0 | Set-up: parse flags, check the workdir, enter a private mount namespace, clean up the store, start the `RUN` spawner thread, all while single-threaded | `ranikust`, `rnk-sandbox`, `rnk-store` | `design/sandbox.md#process-set-up-once-per-build` |
| 1 | Probe: each crate tries its own privileged steps for real; the binary runs them in order and picks the mode (`--mode`) or refuses; kaniko mode `flock`s the workdir | `ranikust`, `rnk-sandbox`, `rnk-snapshot` | `design/modes-and-detection.md#probe-order---modeauto` |
| 2 | Parse: Dockerfile → typed instructions, stage DAG, `ARG` expansion; squashing only in sequential kaniko mode | `rnk-dockerfile` | BuildKit semantics, hand-written (`ADR-0015`) |
| 3 | Context: local directory (Phase 1), tar/git/HTTPS later; `.dockerignore` | `rnk-context`, `rnk-ignore` | exact moby `patternmatcher` semantics |
| 4 | Pull bases of all stages up front, in parallel; verify digests into the store; extract layers | `rnk-registry`, `rnk-oci`, `rnk-store`; `rnk-snapshot` chains `rnk-layer` (decode) into `rnk-rootfs` (extract) | cache lookup (`rnk-cache`) joins here in Phase 4 |
| 5 | Schedule: start a stage when its dependencies finish; at most `--jobs` `RUN`s | `rnk-sched` | `design/scheduler.md` |
| 6 | Stage root: overlay mount over the layer store, or extraction plus a baseline walk | `rnk-snapshot` | exposed only as a `RootPath` |
| 7 | Per instruction: cache key → execute (`RUN` in the sandbox, `COPY`/`ADD` through `RootPath`, metadata into the config) → diff (upper directory, or walk and hash) | `rnk-builder`, `rnk-sandbox`, `rnk-rootfs`, `rnk-snapshot` | `commands` module in `rnk-builder` |
| 8 | Layer encode, lazily: only layers that are pushed, cached, written out or in the target; tar → parallel gzip/zstd → digest while streaming | `rnk-builder` decides, `rnk-snapshot`'s exporter feeds entries, `rnk-layer` sorts them, adds parent headers and encodes | `design/scheduler.md#lazy-export` |
| 9 | Image assembly: config, manifest | `rnk-builder`, `rnk-oci` | |
| 10 | Output: parallel push with cross-repository mounts, Docker tar, OCI layout | `rnk-registry`, `rnk-builder` | |

Every step emits `tracing` spans; with `--metrics-file=<path>`,
`rnk-metrics` writes them as one JSON report at exit (`ADR-0025`,
`design/metrics.md`).

## Modes and the probe

| | overlay mode | kaniko mode |
|---|---|---|
| Needs | mount privileges: root, or a usable user namespace (rootless) | root |
| Stage root | overlay mount in `<state>/builds/<id>/stages/<n>/rootfs` | the workdir (`/` in a container), or one directory per stage |
| Diff | the instruction's upper directory | parallel walk and hash with a stat cache |
| Stages | parallel along the DAG | sequential by default; `--parallel-stages` opts in (`ADR-0014`) |
| Phase | 1 (root), 3 (rootless) | 2 |

The probe (`--mode=auto`; `--snapshot-mode` keeps kaniko's meaning,
`ADR-0023`) tries each step for real, never by kernel version, and ends in:
overlay root, overlay rootless, kaniko with mounts (per-stage directories,
full sandbox), kaniko without mounts (sequential, `RUN` directly like
kaniko), or refuse with guidance. A forced mode still
runs the checks. Images from the two modes may differ as `ADR-0012` lists.

## Crate graph

Intended direction from [`ARCHITECTURE.md`](../../ARCHITECTURE.md#intended-dependency-direction);
lower crates never depend on higher ones. Today only `rnk-snapshot` →
`rnk-rootfs`, `rnk-context` → `rnk-rootfs`, `rnk-ignore` and `ranikust` →
`rnk-metrics` are declared in a `Cargo.toml`.

```
ranikust ── rnk-metrics   (subscriber, opt-in metrics; libraries only emit tracing spans)
└─ rnk-sched
   └─ rnk-builder
      └─ rnk-snapshot  rnk-sandbox  rnk-cache  rnk-registry
         rnk-context   rnk-ignore   rnk-dockerfile
         └─ rnk-layer  rnk-store
            └─ rnk-rootfs  rnk-oci    (pure data: rnk-dockerfile, rnk-ignore, rnk-oci)
```

Decided edges besides the chain (`[design]`):

- `ranikust` → `rnk-sandbox`, `rnk-snapshot` (probes, run in order while
  single-threaded), → `rnk-store` (`prune`).
- `rnk-snapshot` → `rnk-layer` (decoder for extraction, writer fed by the
  exporter) and → `rnk-store`; `rnk-registry`, `rnk-cache` → `rnk-store`.
- `rnk-sandbox` → `rnk-rootfs` (`USER` lookup); `rnk-store` → `rnk-rootfs`
  (deletion). Never `rnk-rootfs` → `rnk-layer`.
- Only `ranikust` → `rnk-metrics` (`ADR-0025`); every crate emits `tracing`
  spans ([design/metrics.md](../design/metrics.md#spans-per-crate)).

`[open: Q-028]` edges: `ranikust` → `rnk-registry` (`login`, `push`);
`rnk-sched` → `rnk-dockerfile` (stage graph type; declared in the
interim: the scheduler walks the graph); `rnk-cache` →
`rnk-registry`; `rnk-context` → `rnk-ignore` (declared in the interim:
ADR-0027 puts the matcher inside the walk).

## Where state lives

| Place | Set by | Holds |
|---|---|---|
| Workdir | `--workdir`; `/` by default in a container, required on a host | overlay mode: holds the default state directory; kaniko mode: the root being walked, `flock`ed (kaniko mode only). Marker `.ranikust-root` |
| State directory | `--state-dir` (kaniko's `--kaniko-dir`); default `<workdir>/.ranikust` in overlay mode, `/ranikust/state` in kaniko mode (`ADR-0022`) | the store, `builds/<id>/` (`design/overlay-snapshot.md#state-directory-layout`), stub journal, `deps/` for `COPY --from`, stage images, secrets. In kaniko mode on the ignore list: never walked, snapshotted or deleted |
| Store | in the state directory, owned by `rnk-store` (`ADR-0021`) | `blobs/sha256/`, `layers/<id>/`, `caches/`, `tmp/`: content-addressed, immutable, per-digest `flock`, atomic rename, shared by concurrent builds (`ADR-0006`, `ADR-0007`) |
| Outputs | `--destination`, `--tar-path`, `--oci-layout-path` | the image; the only other place written, besides the metrics report |
| Metrics report | `--metrics-file` (opt-in, `ADR-0037`) | one JSON file written at exit, never during the build (`ADR-0025`) |

One state directory serves every mode (`ADR-0022`); the default for kaniko
mode outside a container is `<cwd>/.ranikust`, `/ranikust/state` when
the root is `/` (`ADR-0038`).

## Phase status

| Phase | Content | Status |
|---|---|---|
| 0 | Spike: sandbox, overlay, detection | mostly done by the experiments (`docs/research/`, frozen 2026-10-29) |
| 1 | MVP: overlay mode as root, parallel stages, outputs | done (acceptance runs 2026-10-30); [phase-1.md](phase-1.md) |
| 2 | kaniko mode | in progress; [phase-2.md](phase-2.md) |
| 3 | Rootless overlay, full probe, `--seccomp`, `--userns` | not started |
| 4 | Registry layer cache | not started |
| 5 | Feature completion (`ADD` URL/git, more mounts, mirrors, `prune`) | not started |
| 6 | Hardening and release | not started |

Code today: 15 crates, each card giving its status. The binary builds
Dockerfiles in overlay mode as root and pushes or writes the image;
`--mode=kaniko` is refused until Phase 2 lands; `rnk-cache` is a
skeleton (Phase 4). CI runs what `just ci` runs. 1.0 needs all three
paths: overlay root, rootless overlay, kaniko mode.
