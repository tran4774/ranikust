# Architecture

This document describes the high-level architecture of ranikust. It is meant
to help you find where things are and which rules hold everywhere. It changes
rarely; details live in [`docs/design/`](docs/design/) and the reasons in
[`docs/adr/`](docs/adr/).

## Bird's-eye view

ranikust takes a Dockerfile and a build context and produces an OCI image:
pushed to a registry, written as a Docker tar, or written as an OCI layout.
It is one static Linux binary that runs on a host or inside a container,
with no daemon.

The core loop is kaniko's: for each stage, prepare a root filesystem, run
each instruction in it, capture the filesystem change as a layer, update the
image config. What differs is how the change is captured, chosen from the
privileges ranikust finds at start-up:

- **overlay mode**: each stage root is an overlay mount; each instruction
  writes into a fresh upper directory, which *is* the layer's diff. Stages run
  in parallel along the stage graph.
- **kaniko mode**: the workdir is the root filesystem (`/` in a container);
  each diff is found by walking and hashing it. Stages run one at a time by
  default, as in kaniko.

If neither mode can work (an unprivileged user without usable user
namespaces), ranikust refuses and explains how to fix it.

The build area is `--workdir`. ranikust keeps its own files, including the
persistent store, in one state directory (`--state-dir`): by default
`<workdir>/.ranikust` in overlay mode and `/ranikust/state` in kaniko mode
([ADR-0022](docs/adr/0022-common-state-dir.md)). ranikust never writes
outside the workdir, the state directory and the requested outputs.

## Code map

Fifteen crates under `crates/`. Each crate's `lib.rs` starts with a
`# Boundaries` section; it is the authority on what the crate must not do,
and this map follows it.

### `ranikust`

The binary. Parses the `build`, `push`, `prune`, `version` and `login`
subcommands, the common kaniko flags and `--mode=auto|overlay|kaniko`
([ADR-0023](docs/adr/0023-mode-flag.md)). While the process is still
single-threaded, it checks the workdir, enters a private mount namespace
when permitted, runs the privilege probes of `rnk-sandbox` and `rnk-snapshot`
in order and picks the mode, and in kaniko mode locks the workdir; then it
hands off to the libraries. `prune` only calls `rnk-store`. Before all
that, it parses the flags (kaniko flags it does not accept are refused by
name), refuses a kernel older than Linux 5.8 through
`rnk_rootfs::probe_support`
([ADR-0028](docs/adr/0028-minimum-kernel-linux-5-8.md)), installs the logs
and the opt-in metrics of `rnk-metrics`, and validates the flags into a
`BuildConfig`; it writes the metrics report on every way out.
**Must not** contain build logic, or probe a capability itself.

### `rnk-dockerfile`

Dockerfile text in, typed instructions and a stage graph out: lexer, parser
directives, heredocs, variable expansion, the instruction set, stage
dependencies, kaniko-style stage squashing (sequential kaniko mode only).
Semantics follow BuildKit's
parser, instructions and shell lexer, ported by hand.
**Must not** touch the filesystem, network or processes, or schedule and
build stages.

### `rnk-ignore`

`.dockerignore` matching with exactly moby `patternmatcher` semantics,
including the per-directory form BuildKit's context walk uses and its rule
for skipping excluded directories.
**Must not** read or walk the filesystem, choose the ignore file
(`rnk-context`), or approximate: any divergence from `patternmatcher` is a
bug.

### `rnk-context`

Build context sources: local directory, tar archive, git repository, HTTPS
URL, behind an open trait. Locates the ignore file:
`<Dockerfile>.dockerignore` before `<context>/.dockerignore`, as kaniko and
BuildKit do. Walks the context as BuildKit's filtered walk does, with
`rnk-ignore`'s matcher (ADR-0027).
**Must not** fetch from cloud object stores (S3, GCS, Azure), interpret the
Dockerfile, or decide which files an instruction copies.

### `rnk-oci`

Digests, media types, descriptors, manifests, indexes and image configs of
the OCI and Docker v2 schema 2 formats, image references, and the canonical
JSON (every object's keys sorted, no whitespace) of every config and
manifest ranikust writes, so equal values always give equal digests. The
types are owned here, not taken from `oci-spec`: `oci-spec` 0.10 serializes
labels, annotations, `ExposedPorts` and `Volumes` from hash maps, whose
order would change the digest, and drops Docker-only config fields such as
`Healthcheck` and `OnBuild`. `oci-spec` still comes in with `oci-client`,
inside `rnk-registry`. It also holds, as pure data, the config edits of the
metadata instructions (`ConfigEdit`: `ENV`, `LABEL`, `USER`, `WORKDIR`…),
history and `diff_ids` appends, and the file names and index types of a
docker-save tarball and an OCI layout; `rnk-builder` writes the files
[open: Q-007].
**Must not** access the network or filesystem.

### `rnk-registry`

Client for the OCI distribution API over `oci-client`: credentials from
docker `config.json` and credential helpers, TLS with rustls and `ring`
trusting the host's CAs plus embedded roots and `--registry-certificate`
([ADR-0024](docs/adr/0024-tls-rustls-ring.md)), mirrors, streamed pulls into
a sink the caller takes from `rnk-store` (a temporary entry), pushes,
cross-repository mounts with fallback to an upload, retries.
**Must not** build, compress or inspect layer contents, hold caching
policy, or define the store layout (`rnk-store`).

### `rnk-rootfs`

All filesystem access inside a build root. `RootPath` resolves every path
through file descriptors (`openat2` with `RESOLVE_IN_ROOT`), so `..` and
symlinks cannot escape. Built on it: layer extraction from an already
decompressed tar stream, writing whiteouts in the form the caller asks for;
copy, ownership, xattrs; `USER` and `--chown` name lookup in the root's
`/etc/passwd` and `/etc/group`, for `rnk-sandbox` and `rnk-builder`; the
ignore list; safe deletion. `rnk-rootfs` provides the deletion mechanism
(mount points compared by `mnt_id`); `rnk-snapshot` decides what to delete in
a build root and when, `rnk-store` in the store.
**Must not** reach a root path by joining strings onto a host path, follow
symlinks or cross mount points when deleting, decompress layers (the caller
chains `rnk-layer`'s decoder into `extract`), or mount, create namespaces or
run processes.

### `rnk-store`

The persistent, content-addressed store in the state directory
([ADR-0021](docs/adr/0021-rnk-store-crate.md)): compressed blobs
(`blobs/`), extracted layers (`layers/`), `RUN --mount=type=cache`
directories (`caches/`), entries being written (`tmp/`). Per-digest locks,
publication by atomic rename, start-up clean-up, `prune`, and `BuildPaths`,
the paths handed down to every crate. Used by `rnk-registry`,
`rnk-snapshot`, `rnk-cache`, `rnk-builder` and, for `prune`, the binary.
**Must not** decide what an entry contains (the caller fills a temporary
entry, `rnk-store` publishes it), transfer blobs, decode layers, mount, or
delete except through `rnk-rootfs`.

### `rnk-sandbox`

Isolated execution of `RUN`: namespaces, `proc`/`dev`/`sys`, `pivot_root`
(or `MS_MOVE`), capability drop, stub mount points, `USER` resolved inside
the root through `rnk-rootfs`, exec. Also the probes of its own
capabilities, each tried for real: namespaces, the uid map, `proc`,
`pivot_root`/`MS_MOVE`, `sysfs`. In sequential kaniko mode it runs `RUN`
directly on `/`, like kaniko (`Isolation::Direct`). When nothing can be
mounted, `--parallel-stages` uses a bare `chroot` into a per-stage directory
with `mknod` `/dev` nodes, the static fake `/proc` and `/sys`, and the
`JAVA_TOOL_OPTIONS` limits of
[ADR-0019](docs/adr/0019-java-tool-options-on-parallel-path.md); a bare
`chroot` is never used when mounting is possible.
**Must not** snapshot or diff a root, mount or probe overlays
(`rnk-snapshot`), or look up users on the host.

### `rnk-snapshot`

Per-stage roots and per-instruction diffs: the `StageRootfs` and
`Snapshotter` traits with two implementations (an interim signature from
Phase 1 task 9, [open: Q-024]: `begin_layer`/`end_layer` around each
instruction that changes files, giving a `LayerDiff`; a read-only `view`
between layers; the stage's host mount point only for the sandbox). Overlay
(`Overlay`, `OverlayStage`, Phase 1): mounts with the
[ADR-0016](docs/adr/0016-overlay-mount-options.md) options, one upper
directory per instruction, overlay/OCI whiteout conversion, extraction of
base layers into the store's `layers/` (`rnk-layer`'s decoder chained into
`rnk-rootfs::extract_overlay`), and the probe of its own capabilities: the overlay
mount and `fsconfig` with `lowerdir+`. Its exporter feeds diff entries to
`rnk-layer`'s writer. kaniko: parallel walk and hash, `LayeredMap`, path
resolution, filesystem deletion.
**Must not** run instructions, encode diffs as layers itself, own the store
(`rnk-store`), or expose a stage root as anything but a `RootPath`.

### `rnk-sched`

Runs the stage DAG. Each stage is an async task started when its
dependencies finish; root-independent work (base pulls, cache lookups)
starts for all stages up front. `--jobs` bounds concurrent `RUN`s, available
memory bounds how many stages run at once, a stage on the shared `/` runs
alone (Phase 2), the first failure cancels the rest, logs are tagged per
stage. A
stage is built through the `StageRunner` trait, which the builder's side
implements and tests fake.
**Must not** know how a stage is built, or let output depend on execution
order.

### `rnk-layer`

Deterministic tar with fully controlled headers: the writer sorts the
entries it is fed and emits missing parent directory headers, whose metadata
it asks the exporter for through `EntrySource` ([open: Q-013], interim),
which also gives file contents. Parallel gzip and zstd compression;
decompression of pulled gzip and zstd blobs into a plain tar stream; digests
computed while streaming.
**Must not** produce bytes that depend on thread count or input chunking,
decide what goes into a layer or where it is sent, or write extracted files
(`rnk-rootfs`).

### `rnk-cache`

Cache keys per instruction; lookup and push of cached layers in a registry
or an OCI layout; pulled cache layers enter the store through `rnk-store`.
**Must not** be compatible with kaniko's cache or let a key depend on the
snapshot mode: keys depend on the instruction, the context and the previous
keys only, and carry a format version.

### `rnk-builder`

Builds one stage: one executor per instruction (`commands` module), cache
checks, lazy layer export (a layer is encoded only if pushed, cached or part
of the target), image config updates, then image assembly and output.
The output side exists: `LayerExporter` encodes a snapshot's
`LayerSource` into the build directory when its `LayerDemand` needs it,
`assemble` writes the canonical config and manifest at one `BuildTime`,
and `push`, `write_docker_tar` and `write_oci_layout` write the image.
**Must not** keep global state (paths arrive as `rnk-store`'s
`BuildPaths`, the clock and `SOURCE_DATE_EPOCH` as values), order stages,
parse flags, or write tar itself (`rnk-layer` writes layers and the Docker
tarball).

### `rnk-metrics`

The process's `tracing` subscriber: logs to stderr and, opt-in through
`RANIKUST_METRICS_FILE`, a layer that aggregates every crate's spans into
one JSON report at exit (totals per span name, records per stage,
instruction and snapshot, bytes per registry host, `getrusage` and cgroup
v2 counters), shaped like kaniko's `metrics.json`
([ADR-0025](docs/adr/0025-build-metrics-from-tracing-spans.md),
[metrics](docs/design/metrics.md)). Writes the report on every exit path,
from a panic hook too.
**Must not** be a dependency of a library (they only emit spans), write
anything but the report, start a thread, or keep state per file.

### Intended dependency direction

`ranikust` → `rnk-sched` → `rnk-builder` → (`rnk-snapshot`, `rnk-sandbox`,
`rnk-cache`, `rnk-registry`, `rnk-context`, `rnk-ignore`, `rnk-dockerfile`)
→ (`rnk-layer`, `rnk-store`) → (`rnk-rootfs`, `rnk-oci`). Besides the
chain: `ranikust` calls `rnk-sandbox` and `rnk-snapshot` for the probes and
`rnk-store` for `prune`; `rnk-snapshot` uses `rnk-layer` (decoder for
extraction, writer for export) and `rnk-store`; `rnk-cache` uses
`rnk-store`; `rnk-registry` does not: it writes pulls into a sink its caller
gets from `rnk-store`; `rnk-sandbox` uses `rnk-rootfs` for `USER`;
`rnk-context` uses `rnk-ignore` for its walk (ADR-0027; interim, Q-028);
`rnk-sched` uses `rnk-dockerfile` for the stage graph (interim, Q-028).
`rnk-rootfs` never depends on `rnk-layer`. Lower crates never depend on
higher ones. `rnk-dockerfile`, `rnk-ignore` and `rnk-oci` stay pure data.
Only `ranikust` depends on `rnk-metrics`; libraries reach it through
`tracing` spans alone.

## Data flow of a build

1. **Set-up** (`ranikust`): parse flags; check the workdir and the state
   directory; enter a private mount namespace while single-threaded;
   start-up clean-up of the store (`rnk-store`).
2. **Probe** (`ranikust`, still single-threaded, running the probes of
   `rnk-sandbox` and `rnk-snapshot` in order): choose overlay root, overlay
   rootless, kaniko, or refuse; `--mode=overlay|kaniko` skips the choice but
   not the checks ([modes-and-detection](docs/design/modes-and-detection.md)).
   In kaniko mode, `flock` the workdir.
3. **Front end**: read the context and `.dockerignore` (`rnk-context`,
   `rnk-ignore`); parse the Dockerfile into a stage graph and expand
   build arguments (`rnk-dockerfile`).
4. **Up-front I/O**: resolve and pull every stage's base image into the
   store and look up the cache, in parallel (`rnk-registry`, `rnk-store`,
   `rnk-cache`).
5. **Schedule** (`rnk-sched`): start each stage once its dependencies are
   done, at most `--jobs` `RUN`s at a time; one at a time on a shared `/`
   (Phase 2).
6. **Build a stage** (`rnk-builder`): prepare the root (`rnk-snapshot`:
   overlay mount over the layer store, or extraction plus a baseline walk).
   For each instruction compute the cache key; on a hit add the cached
   layer; on a miss execute it (`RUN` through `rnk-sandbox`, `COPY`/`ADD`
   through `rnk-rootfs`) and take the diff (upper directory, or walk and
   hash). Update the image config.
7. **Export lazily**: only layers that are pushed, cached or written out are
   turned into tar, compressed in parallel and digested: `rnk-snapshot`'s
   exporter feeds the entries, `rnk-layer` encodes them; cache pushes run
   asynchronously.
8. **Output**: assemble config and manifest (`rnk-oci`), then push in
   parallel, or write a tar or OCI layout.

Instructions inside a stage run in order; parallelism is between stages and
in I/O.

## The two modes

| | overlay mode | kaniko mode |
|---|---|---|
| Needs | mount privileges (root, or a usable user namespace) | root |
| State directory | `<workdir>/.ranikust` by default | `/ranikust/state` by default, on the ignore list |
| Stage root | overlay mount in `<state>/builds/<id>/stages/<n>/rootfs` | the workdir (`/` in a container); per-stage directories when it can mount, or with `--parallel-stages` |
| Workdir lock | none: builds may share the workdir | exclusive `flock` |
| Diff | the upper directory, plus a walk of the diff only | parallel walk and hash with a stat cache |
| Stages | parallel (DAG) | sequential by default |
| `RUN` | full sandbox | directly on `/` like kaniko (`Isolation::Direct`); full sandbox or bare `chroot` with a fake `/proc` in per-stage directories |

Images from the two modes may differ in documented ways
([ADR-0012](docs/adr/0012-accept-divergence-between-modes.md)). Details:
[overlay-snapshot](docs/design/overlay-snapshot.md),
[kaniko-mode](docs/design/kaniko-mode.md),
[sandbox](docs/design/sandbox.md), [scheduler](docs/design/scheduler.md),
[cache-and-store](docs/design/cache-and-store.md).

## Cross-cutting invariants

### Host safety

ranikust runs `RUN` as root, mounts filesystems and empties build roots.

- Nothing is written outside the workdir, the state directory or store, and
  the requested outputs.
- Every path that comes from an image, a context or a Dockerfile is reached
  through `rnk_rootfs::RootPath`, never through `std::fs` on a joined host
  path.
- Deletion works through file descriptors, never follows symlinks, and never
  crosses a mount point (compared by mount id, never by `st_dev` alone).
- A workdir outside a container must not be `/` or contain a system
  directory (compared by `(dev, ino)`), and must carry the `.ranikust-root`
  marker if not empty. In kaniko mode, where the workdir is the root being
  built, it is locked exclusively.
- In kaniko mode the state directory (`/ranikust/state` in a container) is
  on the ignore list: never walked, snapshotted or deleted.
- `RUN` enters its root with `pivot_root` (or `MS_MOVE`), never a bare
  `chroot` where mounting is possible; the capability bounding set is cut to
  Docker's defaults for every user.
- Cleanup never relies on unwinding: release builds abort on panic. Mounts
  live in a private mount namespace, `RUN` children get `PR_SET_PDEATHSIG`,
  and stubs are journaled before they are created.
- Anything ranikust puts into a root is removed, and its parent's metadata
  restored, before the diff.

### Determinism

- Layer contents and digests never depend on `--jobs`, thread count,
  scheduling or input chunking, or on the host's users. Parallel gzip is a
  single deterministic member; zstd is one independent frame per 1 MiB of
  tar, since libzstd's own multithreading is not deterministic (`exp-05`,
  [ADR-0029](docs/adr/0029-zstd-layers-as-independent-1-mib-frames.md)); tar
  entries are sorted; configs are canonical JSON.
- Timestamps follow kaniko: the current time by default (the config's
  `created` and history; files keep the times they have on disk); with
  `--reproducible`, a fixed epoch, or `SOURCE_DATE_EPOCH` when set.
- With `--reproducible`, a build with `--jobs 1` and one with the default
  give the same digests.

### No global state

- Paths travel as `BuildPaths { root, state }`, defined in `rnk-store`;
  configuration travels as values. No process-wide current directory,
  `PATH` or environment changes for a build.
- `unshare` happens only while the process is single-threaded. `RUN`
  children are forked from one thread that lives for the whole build.

### Observability

- Every crate describes its work as `tracing` spans named
  `<crate>.<operation>`, one per layer, blob, instruction or stage, never
  per file, with numeric fields that carry their unit; the list is
  [metrics](docs/design/metrics.md#spans-per-crate).
- Metrics are opt-in (`RANIKUST_METRICS_FILE`); off, no metrics layer is
  installed. Logs and metrics are separate layers, so a log level never
  hides a metric.
- Metrics never reach an image: the report is written at exit only, and no
  build code reads metric state.

### Portability and errors

- No architecture-specific code; syscall numbers only through `rustix` or
  `libc`. x86_64 is released; aarch64 is built and unit-tested
  under qemu-user in CI
  ([ADR-0009](docs/adr/0009-x86-64-first-aarch64-ready.md)).
- Errors are never dropped silently; a probe or mount failure reports the
  step and the errno.

## Testing

Tiers by privilege; details and commands in [`docs/testing.md`](docs/testing.md).

- **T0, no privileges**: unit tests, the parser against BuildKit goldens, the
  `.dockerignore` cases, tar and compression round trips, cache keys. Run on
  every change.
- **T1, user namespaces**: rootless overlay and the sandbox in a user
  namespace.
- **T2, root**: only in a disposable VM, never on the host: overlay as real
  root, rootful Docker under cgroup v1 and v2, and the host-safety attack
  suite (path traversal in layers, escaping symlinks and hardlinks, fork
  bombs, `--workdir=/`).
- **Compatibility**: kaniko's 200 integration Dockerfiles in every mode;
  flattened content compared with kaniko, and between modes against the
  accepted-differences list.

## Where to read more

- [`docs/adr/`](docs/adr/): decisions and why they were made.
- [`docs/design/`](docs/design/): how each component works now.
- [`docs/research/`](docs/research/): the frozen pre-code survey and its
  experiments, the evidence behind the numbers.
- [`docs/book/`](docs/book/): the user guide, including isolation per mode.
- [`docs/security/`](docs/security/): the threat model.
