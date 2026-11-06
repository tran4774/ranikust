# Architecture

**TL;DR:** Fifteen crates under `crates/`. This page is the map of them (what
each owns and must not do, which depends on which) and the rules that hold
everywhere. How a part works is in [`docs/design/`](docs/design/); why, in
[`docs/adr/`](docs/adr/).

## Bird's-eye view

ranikust takes a Dockerfile and a build context and produces an OCI image:
pushed to a registry, written as a Docker tar, or written as an OCI layout. It
is one static Linux binary that runs on a host or inside a container, with no
daemon.

The core loop is kaniko's: for each stage, prepare a root filesystem, run each
instruction in it, capture the filesystem change as a layer, update the image
config. The privileges ranikust finds at start-up decide how the change is
captured:

- **overlay mode**: each stage root is an overlay mount; each instruction
  writes into a fresh upper directory, which *is* the layer's diff. Stages run
  in parallel along the stage graph.
- **kaniko mode**: the root is the build root (`/` in a container, else a
  directory ranikust makes in its state directory); each diff is found by
  walking and hashing it. Stages run one at a time on `/`, as in kaniko.

If neither mode can work, ranikust refuses and explains how to fix it. Today
every user but root is refused; rootless overlay mode is Phase 3.

ranikust keeps its own files, including the persistent store and every build
root it makes, in one state directory (`--state-dir`, default
`/var/lib/ranikust`); there is no work-directory flag
([ADR-0050](docs/adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md)).
It never writes outside the state directory, the requested outputs and, in
kaniko mode in a container, `/`, but for the `O_TMPFILE` fallback of the
sealed copy of its binary, a file with no name in `TMPDIR`
([host safety](#host-safety)).

## Code map

Each crate's `lib.rs` starts with a `# Boundaries` section; it is the
authority on what the crate must not do, and this map follows it. Entry points
and tests are on each crate's card in
[`docs/agents/crates/`](docs/agents/crates/).

### `ranikust`

The binary: the CLI and the orchestration of a build across the library crates
(ADR-0067). Parses the `build` (the default) and `version` subcommands, kaniko's
common flags (those it does not accept are refused by name) and
`--mode=auto|overlay|kaniko`
([ADR-0023](docs/adr/0023-mode-flag.md)); `prune` comes in Phase 5 (a minimal
`prune --all` in Phase 3, ADR-0105), `push` (an OCI layout, ADR-0052) has no
phase yet, and there is no `login` (ADR-0049).
Runs the set-up and the probes while
single-threaded, picks the mode and the root, runs `rnk-sched` with its own
`StageRunner` (which drives `rnk-builder`, one thread per stage) and writes the
outputs. **Must not** hold the rule of a single concern (Dockerfile semantics,
scheduling, executing an instruction, snapshots, the sandbox, filesystem
access, storage, layers, images, registries), or probe a capability itself.

### `rnk-dockerfile`

Dockerfile text in, typed instructions and a stage graph out, with BuildKit's
semantics ported by hand: directives, heredocs, variable expansion, stage
dependencies, kaniko-style stage squashing (sequential kaniko mode only).
**Must not** touch the filesystem, network or processes, or schedule and build
stages.

### `rnk-ignore`

`.dockerignore` matching with exactly moby `patternmatcher` semantics,
including the per-directory form BuildKit's context walk uses. **Must not**
read or walk the filesystem, choose the ignore file (`rnk-context`), or
approximate: any divergence from `patternmatcher` is a bug.

### `rnk-context`

Build context sources behind an open trait. A local directory is built; tar,
git and HTTPS contexts are parsed and refused until Phase 5. Locates the ignore
file (`<Dockerfile>.dockerignore` before `<context>/.dockerignore`) and walks
the context as BuildKit's filtered walk does, with `rnk-ignore`'s matcher
(ADR-0027). **Must not** fetch from cloud object stores (S3, GCS, Azure),
interpret the Dockerfile, or decide which files an instruction copies.

### `rnk-oci`

OCI and Docker v2 schema 2 types (digests, descriptors, manifests, indexes,
configs, image references), the canonical JSON of every config and manifest
ranikust writes (so equal values give equal digests), the config edits of the
metadata instructions, and the file names of a docker-save tarball and an OCI
layout. Owns its types instead of taking `oci-spec`'s, whose hash-map order
would change digests and which drops Docker-only config fields; `oci-spec`
comes in only with `oci-client`, inside `rnk-registry`. **Must not** access the
network or filesystem.

### `rnk-registry`

Client for the OCI distribution API: pushes over `oci-client`, pulls sent
itself with bounded reads (the `--limit` limits,
[ADR-0079](docs/adr/0079-resource-limits-with-a-limit-flag.md)), credentials
from docker `config.json`, TLS with rustls and `ring`
([ADR-0024](docs/adr/0024-tls-rustls-ring.md)), cross-repository mounts,
retries; mirrors and credential helpers come in Phase 5. **Must not** build,
compress or inspect layer contents, hold caching policy, or define the store
layout (`rnk-store`).

### `rnk-rootfs`

All filesystem access inside a build root. `RootPath` resolves every path
through file descriptors (`openat2` with `RESOLVE_IN_ROOT`), so `..` and
symlinks cannot escape. Built on it: layer extraction, copy, ownership,
xattrs, `USER` and `--chown` lookup in the root's own `/etc/passwd`, the
ignore list's matching, and deletion by descriptor that never crosses a mount
point (`rnk-snapshot` decides what to delete in a build root, `rnk-store` in
the store). Also the operator's host paths that the build context's owner must
not steer, such as an output's directory (ADR-0076), and the parser of
`/proc/self/mountinfo`. **Must not** reach a root
path by joining strings onto a host path, follow symlinks or cross mount points
when deleting, decompress layers, or mount, create namespaces or run
processes.

### `rnk-store`

The persistent, content-addressed store in the state directory
([ADR-0021](docs/adr/0021-rnk-store-crate.md)): blobs, extracted layers,
`RUN --mount=type=cache` directories, entries being written. Per-digest locks,
publication by atomic rename, start-up clean-up, and `BuildPaths`, the paths
handed down to every crate; `prune` comes in Phase 5, a minimal `prune --all` in
Phase 3 (ADR-0105). **Must not** decide what
an entry contains (the caller fills a temporary entry, `rnk-store` publishes
it), transfer blobs, decode layers, mount, or delete except through
`rnk-rootfs`.

### `rnk-sandbox`

Isolated execution of `RUN`: namespaces, `proc`/`dev`/`sys`, `pivot_root` (or
`MS_MOVE`), capability drop, seccomp profiles (Docker's JSON format compiled to
BPF, Docker's default embedded,
[ADR-0109](docs/adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)),
stub mount points, `USER` resolved inside the root, exec. Also the probes of
its own capabilities, each tried for real, and the sealed copy of ranikust's
binary a build runs from, with the mount of the binary as installed, for
`ranikust`'s refusal in kaniko mode on `/`
([ADR-0101](docs/adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md)). In
sequential kaniko mode on `/` it runs `RUN` directly, like kaniko
(`Isolation::Direct`). A bare `chroot` is never used when mounting is possible;
the bare-`chroot` path for `--parallel-stages` is held back, as the flag is
refused
([ADR-0060](docs/adr/0060-copy-leaves-special-files-out-where-run-has-no-nodev.md)).
**Must not** snapshot or diff a root, mount or probe overlays
(`rnk-snapshot`; the seal only reads whether `/` is one, ADR-0101), or look
up users on the host.

### `rnk-snapshot`

Per-stage roots and per-instruction diffs: the `StageRootfs` and `Snapshotter`
traits ([ADR-0046](docs/adr/0046-snapshotter-signature.md)) with two
implementations, and the probe of the overlay capabilities.

- **Overlay**: mounts with the
  [ADR-0016](docs/adr/0016-overlay-mount-options.md) options, one upper
  directory per instruction, overlay/OCI whiteout conversion, base layers
  extracted into the store's `layers/`.
- **kaniko**: the parallel walk and hash
  ([ADR-0039](docs/adr/0039-kaniko-mode-walk-and-hash-crates.md)), and stage
  roots on the one shared root, emptied between units with the ignore list.

Each feeds its diff to `rnk-layer`'s writer. **Must not** run instructions,
encode diffs as layers itself, own the store (`rnk-store`), or expose a stage
root as anything but a `RootPath`.

### `rnk-sched`

Runs the stage DAG: a stage starts when its dependencies finish, and
root-independent work (base pulls, cache lookups) starts up front. `--jobs`
bounds concurrent `RUN`s, available memory bounds how many stages run at once,
stages on the shared `/` run one at a time, the first failure cancels the rest.
A stage is built through the `StageRunner` trait, which the builder's side
implements and tests fake. **Must not** know how a stage is built, or let output
depend on execution order.

### `rnk-layer`

Deterministic tar with fully controlled headers, parallel gzip and zstd
compression, decompression of pulled blobs, digests computed while streaming.
**Must not** produce bytes that depend on thread count or input chunking,
decide what goes into a layer or where it is sent, or write extracted files
(`rnk-rootfs`).

### `rnk-cache`

Skeleton (Phase 4). Cache keys per instruction; lookup and push of cached
layers in a registry or an OCI layout. **Must not** be compatible with kaniko's
cache or let a key depend on the snapshot mode: keys depend on the instruction,
the context and the previous keys only, and carry a format version.

### `rnk-builder`

Builds one stage and assembles the image: one executor per instruction
(metadata edits the config, `COPY`/`ADD` go through `rnk-rootfs`, `RUN` through
`rnk-sandbox`), one layer per changing instruction, lazy layer export (a layer
is encoded only if pushed, cached or part of the target), then the image
(config and manifest at one build time) and its output: push, Docker tar, OCI
layout. Also pulls a base image into the store. It does not depend on
`rnk-sched`: the binary implements the scheduler's `StageRunner` with it.
**Must not** keep global state (paths arrive as `rnk-store`'s `BuildPaths`, the
clock and `SOURCE_DATE_EPOCH` as values), order stages, parse flags, or write
tar itself (`rnk-layer` does).

### `rnk-metrics`

The process's `tracing` subscriber: logs to stderr and, opt-in through
`--metrics-file`, a layer that aggregates every crate's spans into one JSON
report at exit, shaped like kaniko's `metrics.json`
([ADR-0025](docs/adr/0025-build-metrics-from-tracing-spans.md),
[metrics](docs/design/metrics.md)). The report is written on every exit path.
**Must not** be a dependency of a library (they only emit spans), write
anything but the report, start a thread, or keep state per file.

### Intended dependency direction

`ranikust` -> `rnk-sched` -> `rnk-builder` -> (`rnk-snapshot`, `rnk-sandbox`,
`rnk-cache`, `rnk-registry`, `rnk-context`, `rnk-ignore`, `rnk-dockerfile`) ->
(`rnk-layer`, `rnk-store`) -> (`rnk-rootfs`, `rnk-oci`). Lower crates never
depend on higher ones. Besides the chain:

- `ranikust` calls `rnk-sandbox` and `rnk-snapshot` for the probes, and
  builds the registry client, the context and the outputs (interim,
  `[open: #7]`). It runs `rnk-builder` for `rnk-sched` by implementing the
  `StageRunner` trait: `rnk-sched` does not depend on `rnk-builder`.
- `rnk-snapshot` uses `rnk-layer` (decoder for extraction, writer for export)
  and `rnk-store`; `rnk-cache` uses `rnk-store`; `rnk-registry` does not: it
  writes pulls into a sink its caller gets from `rnk-store`.
- `rnk-sandbox` uses `rnk-rootfs` for `USER`, for the parser of
  `/proc/self/mountinfo` it shares with `rnk-snapshot`, and for `RootPath`
  to reach a writable mount's mount point from `/` (ADR-0101).
- `rnk-context` uses `rnk-ignore` for its walk (ADR-0027), and `rnk-sched` uses
  `rnk-dockerfile` for the stage graph: both interim, `[open: #7]`.
- `rnk-rootfs` never depends on `rnk-layer`. `rnk-dockerfile`, `rnk-ignore` and
  `rnk-oci` stay pure data. Only `ranikust` depends on `rnk-metrics`; libraries
  reach it through `tracing` spans alone.

The edges actually declared are in each `crates/*/Cargo.toml`; `rnk-cache`
declares none yet.

## Data flow of a build

1. **Set-up** (`ranikust`, single-threaded): parse flags; refuse a kernel older
   than Linux 5.8 (`rnk_rootfs::probe_support`,
   [ADR-0028](docs/adr/0028-minimum-kernel-linux-5-8.md)); for `build`, run
   again from a sealed copy of the binary, which starts over from the top
   ([ADR-0101](docs/adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md),
   `rnk-sandbox`); install the logs and
   the opt-in metrics; validate the flags into a `BuildConfig`; read and
   compile the `--seccomp` profile, after the re-exec, so never in the
   process that makes the copy
   ([ADR-0109](docs/adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md));
   try to enter a private mount namespace (failing means root may not
   mount); check the state directory; clean up the store (`rnk-store`);
   start the `RUN` spawner thread (`rnk-sandbox`).
2. **Probe** (`ranikust`, running the probes of `rnk-sandbox` and
   `rnk-snapshot` in order): choose overlay, kaniko, or refuse;
   `--mode=overlay|kaniko` skips the choice but not the checks
   ([modes-and-detection](docs/design/modes-and-detection.md)). In kaniko mode,
   pick the root: `/` in a container (`flock` on `/` itself), else a directory
   per stage, `<state>/builds/<id>/stages/<n>/rootfs` (ADR-0059), where root
   must be able to mount.
3. **Front end**: read the context and `.dockerignore` (`rnk-context`,
   `rnk-ignore`); parse the Dockerfile into a stage graph and expand build
   arguments (`rnk-dockerfile`).
4. **Up-front I/O**: resolve and pull every stage's base image into the store
   and look up the cache, in parallel (`rnk-registry`, `rnk-store`,
   `rnk-cache`). In kaniko mode only manifests and configs are fetched here:
   base layers are streamed from the registry into extraction when a stage needs
   them, and no blob is kept unless an output needs its bytes
   ([streaming](docs/design/streaming.md), ADR-0055).
5. **Schedule** (`rnk-sched`): start each stage once its dependencies are done,
   at most `--jobs` `RUN`s at a time; one stage at a time on a shared `/`.
6. **Build a stage** (`rnk-builder`): prepare the root (`rnk-snapshot`: overlay
   mount over the layer store, or extraction plus a baseline walk). For each
   instruction compute the cache key; on a hit add the cached layer; on a miss
   execute it (`RUN` through `rnk-sandbox`, `COPY`/`ADD` through `rnk-rootfs`)
   and take the diff (upper directory, or walk and hash). Update the image
   config.
7. **Export lazily**: only layers that are pushed, cached or written out are
   turned into tar, compressed in parallel and digested: `rnk-snapshot`'s
   exporter feeds the entries, `rnk-layer` encodes them. When the build
   pushes, each exported layer is uploaded as soon as it is written and its
   blob released (ADR-0055).
8. **Output**: assemble config and manifest (`rnk-oci`), then push in parallel,
   or write a tar or OCI layout.

Instructions inside a stage run in order; parallelism is between stages and in
I/O. Every step emits `tracing` spans ([metrics](docs/design/metrics.md)).

## The two modes

| | overlay mode | kaniko mode |
|---|---|---|
| Needs | mount privileges (root; a usable user namespace is Phase 3) | root |
| Stage root | overlay mount in `<state>/builds/<id>/stages/<n>/rootfs` | `/` in a container; else one directory per stage, `<state>/builds/<id>/stages/<n>/rootfs` (ADR-0059), copied for a stage built `FROM` it or moved to it when it is the only reader (ADR-0061, ADR-0062), released once nobody reads it |
| Root lock | none: builds share the state directory | `/` is `flock`ed: one build per container |
| State directory | `/var/lib/ranikust` by default; a volume in a container | `/var/lib/ranikust` by default, on the ignore list when inside the root |
| Diff | the upper directory, plus a walk of the diff only | parallel walk and hash with a stat cache |
| Stages | parallel (DAG) | one at a time on a container's `/`; parallel (DAG) in per-stage directories; `--parallel-stages` refused (ADR-0060) |
| `RUN` | full sandbox | directly on `/` like kaniko (`Isolation::Direct`); the full sandbox in per-stage directories |

Images from the two modes may differ in documented ways
([ADR-0012](docs/adr/0012-accept-divergence-between-modes.md)). Details:
[overlay-snapshot](docs/design/overlay-snapshot.md),
[kaniko-mode](docs/design/kaniko-mode.md),
[sandbox](docs/design/sandbox.md), [scheduler](docs/design/scheduler.md),
[cache-and-store](docs/design/cache-and-store.md).

## Cross-cutting invariants

### Host safety

ranikust runs `RUN` as root, mounts filesystems and empties build roots.

- Nothing is written outside the state directory or store, the requested
  outputs and, in kaniko mode in a container, `/`. One exception, made
  before the state directory is checked: where the kernel refuses the
  sealed memfd copy of the binary, its `O_TMPFILE` fallback, a file with
  no name in `TMPDIR`, gone with the build
  ([ADR-0101](docs/adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md)).
- Every path that comes from an image, a context or a Dockerfile is reached
  through `rnk_rootfs::RootPath`, never through `std::fs` on a joined host
  path.
- Deletion works through file descriptors, never follows symlinks, and never
  crosses a mount point (compared by mount id, never by `st_dev` alone).
- ranikust never fills or empties a directory the operator names: the root is
  `/` in a container (kaniko mode) or a directory it makes in its state
  directory. The state directory is owned by the user running ranikust, not
  writable by others, made `0700`, below directories only root may write,
  checked by descriptor before anything is created (ADR-0050).
- In kaniko mode on `/` the state directory is on the ignore list: never
  walked, snapshotted or deleted.
- `RUN` enters its root with `pivot_root` (or `MS_MOVE`), never a bare `chroot`
  where mounting is possible; the capability bounding set is cut to Docker's
  defaults for every user.
- Cleanup never relies on unwinding: release builds abort on panic. Mounts live
  in a private mount namespace, `RUN` children get `PR_SET_PDEATHSIG`, and
  stubs are journaled before they are created.
- Anything ranikust puts into a root is removed, and its parent's metadata
  restored, before the diff.

### Determinism

- Layer contents and digests never depend on `--jobs`, thread count,
  scheduling or input chunking, or on the host's users. Parallel gzip is a
  single deterministic member; zstd is one independent frame per 1 MiB of tar,
  since libzstd's own multithreading is not deterministic (`exp-05`,
  [ADR-0029](docs/adr/0029-zstd-layers-as-independent-1-mib-frames.md)); tar
  entries are sorted; configs are canonical JSON.
- Timestamps: the current time by default (the config's `created` and history;
  files keep the times they have on disk). With `--reproducible`, as BuildKit
  (`ADR-0032`), the epoch is `SOURCE_DATE_EPOCH` when set, else 1970-01-01: the
  config's `created` and the build's history entries carry it, and built-layer
  mtimes later than it are clamped to it while earlier ones are kept.
- With `--reproducible`, a build with `--jobs 1` and one with the default give
  the same digests.

### No global state

- Paths travel as `BuildPaths { root, state }`, defined in `rnk-store`;
  configuration travels as values. No process-wide current directory, `PATH` or
  environment changes for a build.
- `unshare` happens only while the process is single-threaded. `RUN` children
  are forked from one thread that lives for the whole build.

### Observability

- Every crate describes its work as `tracing` spans named
  `<crate>.<operation>`, one per layer, blob, instruction or stage, never per
  file, with numeric fields that carry their unit; the list is
  [metrics](docs/design/metrics.md#spans-per-crate).
- Metrics are opt-in (`--metrics-file`, ADR-0037); off, no metrics layer is
  installed. Logs and metrics are separate layers, so a log level never hides a
  metric.
- Metrics never reach an image: the report is written at exit only, and no
  build code reads metric state.

### Portability and errors

- No architecture-specific code; syscall numbers only through `rustix` or
  `libc`. x86_64 is released; aarch64 is built and unit-tested natively on
  GitHub's aarch64 runner, on CI's full runs
  ([ADR-0009](docs/adr/0009-x86-64-first-aarch64-ready.md), ADR-0095).
- Errors are never dropped silently; a probe or mount failure reports the step
  and the errno.

## Where to read more

- [`docs/design/`](docs/design/): how each component works now.
- [`docs/adr/`](docs/adr/): decisions and why they were made.
- [`docs/testing.md`](docs/testing.md): test tiers (T0 to T3), what runs where,
  and the commands.
- [`docs/security/`](docs/security/): the threat model.
- [`docs/research/`](docs/research/): the frozen pre-code survey and its
  experiments, the evidence behind the numbers.
- [`docs/book/`](docs/book/): the user guide, including isolation per mode.
