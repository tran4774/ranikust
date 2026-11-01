# Changelog

All notable changes to ranikust are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a
minor version may contain breaking changes.

Entries are written for users. `just changelog` drafts them from the
Conventional Commit messages with git-cliff; the draft is then edited by hand.

## [Unreleased]

### Added

- Repository setup: a Cargo workspace with one crate per component (no
  functionality yet), a static musl release build, CI (format, clippy, tests,
  aarch64 check, cargo-deny, docs), architecture decision records, the
  research that preceded the code, a user guide skeleton, and contributor
  and security policies.
- Opt-in build metrics: with `--metrics-file=<path>`, ranikust
  writes one JSON report when it exits, also after an error or a panic:
  time, counts and bytes per build phase, per stage, per instruction and
  per snapshot, bytes per registry host, CPU and peak memory of ranikust and
  of its `RUN` processes, and cgroup v2 and network counters. The sections
  follow the `KANIKO_METRICS_FILE` report of kaniko's metrics fork where the
  meaning matches. `RANIKUST_METRICS_SUMMARY=1` prints a short summary to
  stderr. Unset, nothing is collected and nothing changes; the image is the
  same either way.
- The command line: `ranikust` with kaniko's executor flags builds, as
  `executor` does (`ranikust build` spelt out), and `ranikust version`.
  Accepted kaniko flags keep their names, short forms and meaning:
  `--context`, `--dockerfile`, `--build-arg`, `--target`, `--destination`,
  `--no-push`, `--tar-path`, `--oci-layout-path`, `--compression`,
  `--reproducible`, `--registry-certificate`, `--snapshot-mode`. New flags:
  `--mode=auto|overlay|kaniko`, `--state-dir` (kaniko's
  `--kaniko-dir`), `--jobs`, `--network=host|none`, `--platform`,
  `--secret`, `--allow` and `--metrics-file`. Other kaniko flags are refused with the reason, never
  ignored.
- Building: `ranikust` builds a Dockerfile in overlay mode as root, its
  stages in parallel up to `--jobs`, one layer per `RUN`, `COPY`, `ADD`
  and `WORKDIR` (other than `/`), and writes an OCI layout
  (`--oci-layout-path`), a Docker tar (`--tar-path`) and pushes to each
  `--destination`, then prints the image digest. Supported: `FROM` an
  image, `scratch` or a stage; `ARG`, `ENV`, `LABEL`, `USER`, `WORKDIR`,
  `EXPOSE`, `VOLUME`, `ENTRYPOINT`, `CMD`, `SHELL`, `STOPSIGNAL`,
  `HEALTHCHECK`; local `COPY` and `ADD` with wildcards, heredocs,
  `--from`, `--chown` and octal or symbolic `--chmod`, `.dockerignore`
  honoured;
  `RUN` in shell, exec and heredoc form with `--mount=type=cache`,
  `--mount=type=secret` and `--network=none`, as BuildKit: cache
  `sharing=shared|locked|private` with `uid`/`gid`/`mode` when a cache is
  made, secrets as a `0400` root file on a tmpfs of the `RUN`'s own,
  never on a disk (or the requested mode and owner)
  or with `env=` as a variable of that `RUN`. `--secret` takes kaniko's
  and buildctl's syntax: a file (`src=`) or a variable (`env=`,
  `type=env`). `RUN --network=host` under `--network=none` needs
  `--allow network.host`. A cancel (SIGINT, SIGTERM or a failed stage)
  kills the running `RUN`s. Features of later phases stop the build with a
  message naming the phase. Where ranikust lacks `CAP_NET_ADMIN` (a
  container with only `SYS_ADMIN` added), `RUN --network=none` is refused
  unless `--allow network.none-userns` opts in to loopback through a user
  namespace, under a seccomp filter (ADR-0033).
- Where kaniko and BuildKit build differently, ranikust builds as
  BuildKit: images use OCI media types whatever the base image; a new
  `ENTRYPOINT` clears a `CMD` inherited from the base; `EXPOSE` takes
  `sctp` and port ranges; each instruction gets a history entry as
  BuildKit writes it; every image has a `PATH`; empty and legacy Docker
  fields of a base config are dropped; a `COPY` wildcard matching nothing
  copies nothing; `--reproducible` keeps file times older than the fixed
  time (`SOURCE_DATE_EPOCH`) and the base image's history times.
- The start-up probe: `ranikust` checks that it can isolate `RUN` (a
  private mount namespace, `pivot_root`, or `MS_MOVE` under Docker's
  seccomp profile) and mount an overlay where its stage roots will live,
  and names the outcome. It builds as root; elsewhere it stops with what
  failed and what to do instead (`sudo`, or a container started with
  `--privileged`), also when it runs inside a plain `chroot`. In a
  container overlay mode needs the state directory on a volume: overlay
  cannot use the container's own overlay root, and ranikust says so.
- The state directory: `ranikust` marks it with `.ranikust-state` and
  refuses a non-empty directory without that mark, `/` and empty system
  directories such as `/mnt` or `/srv` included, before
  changing anything in it, so a mistyped `--state-dir` is never written
  to, cleaned up or made private. A directory marked `.ranikust-root` by
  an earlier ranikust is refused too: delete it first.
- A start-up check: on a kernel older than Linux 5.8, or where seccomp
  blocks `openat2` or `statx`, ranikust stops at once with a message naming
  the kernel.
- kaniko's flags, as listed in the user guide's command-line page:
  `-v`/`--verbosity`, `--log-format`, `--log-timestamp`, `--label`,
  `--annotation`, `--image-format`, `--compression-level`, the three digest
  files, `--insecure`, `--insecure-pull`, the `--skip-tls-verify` flags,
  `--push-retry`, `--image-download-retry`, `--context-sub-path` (for
  `dir://` contexts) and `--cache=false`. kaniko's old spellings
  (`--tarPath`, `--snapshotMode`, `--customPlatform`, `--custom-platform`,
  `--whitelist-var-run`) are refused, each naming the flag to use.
  Credentials go over plain HTTP only to registries named with
  `--insecure-registry` or destinations under `--insecure`, never to one a
  Dockerfile names under `--insecure-pull`. Every other kaniko flag is
  refused with its reason, those that would change nothing (such as
  `--cache-dir` and `--cleanup`) too.
- `DOCKER_AUTH_CONFIG`, read before `config.json`. kaniko's own variables
  (`KANIKO_*`, `FF_KANIKO_*`) are not read.
- The plan of a sequential kaniko-mode build, not yet wired to the CLI:
  kaniko's stage squashing on the shared root, which stages are walked for
  layers, what later stages read from each, and a scheduler mode that runs
  stages on the shared root one at a time in a fixed order.
- Kaniko mode is reachable: `--mode=auto` picks it where root may not mount
  (Docker's default, most Kubernetes pods) and says why. kaniko's
  `--ignore-path`, `--ignore-var-run`, `--single-snapshot` and
  `--materialize` are
  accepted with kaniko's meaning; in overlay mode those that would change
  the image are refused. New flag `--snapshot-workers` (default 8) sizes
  kaniko mode's walk. Kaniko-mode builds still stop after the set-up.
- The kaniko-mode walk in `rnk-snapshot` (not yet used by a build):
  `Walker` walks a root on 8 threads by default (`WalkOptions::workers`),
  each directory opened by descriptor relative to its parent, never
  following a symlink or crossing a mount; hashes files with xxh3-128,
  content and xattrs, reusing the previous walk's hash for files unchanged
  outside a 2 s window; and diffs against the previous walk while walking,
  with kaniko's `--snapshot-mode=full|redo|time` meaning. The result does
  not depend on the thread count.
- The kaniko-mode exporter in `rnk-snapshot` (not yet used by a build):
  the walk's diff written as a layer as kaniko writes it, with every
  parent directory of a changed entry, one whiteout per deleted tree, a
  hard link into an earlier layer as a regular file, and an empty layer
  for a step that changes nothing. Content is read only from the file the
  walk saw; one changed in between fails the step.
- Groundwork for kaniko mode's ignore list: mount points below the root
  from `/proc/self/mountinfo`, kaniko's defaults (`/etc/mtab`, what is
  below `/tmp/apt-key-gpghome`, `/var/run`), `/proc`, `/sys`, `/dev`,
  `/ranikust` and the state directory; `--ignore-path` entries match as
  globs, as in kaniko. Base layers can be extracted into a live root
  without writing an ignored path or below a mount point, and, as kaniko,
  without creating device nodes or FIFOs. Not yet used by a build.
- Kaniko mode builds: `--mode=kaniko` (or `--mode=auto` where root may not
  mount) runs the stages one after another on one root, as kaniko does:
  `/` in a container, elsewhere a directory ranikust makes in its state
  directory (`<state>/builds/<id>/rootfs`, removed with the build); base
  layers unpacked into it, each
  instruction's layer found by walking it, the root emptied between stages
  and at the end, kaniko's stage squashing and lazy snapshots. `RUN` runs
  directly, as kaniko; on a root in the state directory where root may
  mount, in the full sandbox. It needs root. Not yet: a Dockerfile whose later stage reads an
  earlier one (`COPY --from`, a `FROM` after the root was emptied),
  `RUN --mount`, `#!` heredocs and `--network=none` without the sandbox,
  and `--single-snapshot`. On `/`, the base goes over what is there and
  the end of the build leaves `/` as it is, as kaniko without `--cleanup`;
  one build at a time uses `/` in a container. The context, outputs,
  secret files and the metrics report must not lie in the root.
- The sandbox can run a kaniko-mode `RUN` on a root other than `/` without
  mounting: it enters the directory with `chroot`, with ranikust's
  `/etc/hosts`, `/etc/resolv.conf`, `/etc/hostname` and `/dev` nodes put in
  place for the command and the image's own entries put back afterwards,
  also after a crash; `RUN` commands keep the open-file limit ranikust
  started with. Not used by a build yet.
- Kaniko mode builds multi-stage Dockerfiles: a stage read by a later
  `COPY --from` or `RUN --mount from=`, or built on with `FROM`, is copied
  aside (only the paths `COPY --from` names, when that is enough) before
  the root is emptied, and a `COPY --from=<image>` is unpacked beside the
  root.

- Kaniko mode on a container's `/` keeps ranikust's state and credentials
  out of `RUN`'s reach: pulled and encoded layers, and stages kept for a
  later `COPY --from` or `FROM` (as tars), are files without a name, on
  disk rather than in the container's memory (`O_TMPFILE`, or a name
  unlinked at once while no `RUN` runs where the filesystem lacks it, as a
  container's overlay root does; `memfd` only as a last resort, after a
  `RUN` that failed, or when an earlier build in the container was cut
  short, with a warning; one build at a time per state directory), and the
  state directory holds nothing a `RUN` could read; ranikust is not dumpable, so `/proc/<pid>/` of
  ranikust (descriptors, environment, memory) is closed to a `RUN`; `RUN`
  loses every capability outside Docker's defaults, and a container that
  grants `CAP_SYS_PTRACE` without `CAP_SETPCAP` is refused; processes a
  `RUN` leaves in the background, `setsid` ones included, are killed when
  it ends. Warnings for a secret-looking `--build-arg NAME=value` and for
  another process holding `DOCKER_AUTH_CONFIG`, a `--secret` or a
  `--build-arg` read from the environment. There, registry credentials go
  only over verified TLS, and a container granting any capability beyond
  Docker's defaults without `CAP_SETPCAP` is refused. ranikust's soft
  `RLIMIT_CORE` is 0; every `RUN` gets the original back (ADR-0051).

- `--mode=kaniko` in a container where root may mount (`--privileged`,
  `CAP_SYS_ADMIN`) refuses to build on `/`: its `RUN` would see the host's
  devices and a writable `/proc/sys`; overlay mode builds there. So does a
  container whose `/proc/sys` is writable (ADR-0054). Host devices given
  with `--device` stay reachable by `RUN`: they are the operator's
  responsibility.

- Layers are streamed (ADR-0055): in kaniko mode, a base layer is
  extracted while it downloads and never kept, unless `--tar-path` or
  `--oci-layout-path` needs its bytes; a push takes such a layer from
  its source registry (a mount, else streamed through). Built layers of
  the target's `FROM` chain are pushed to every destination as soon as
  they are exported (at most `--jobs` at once, `--jobs` more waiting) and
  released once there; a failed build cancels them. On `/`, where nothing
  empties the root, a streamed base layer is spooled to a disk-backed
  anonymous file and verified before it is extracted. Measured on a node
  base image in kaniko mode on `/`: the peak memory of a stage falls from
  about 160 to about 50 MiB.

- Kaniko mode on a container's `/` runs a `#!` heredoc from a sealed
  in-memory file, read by its interpreter as `/dev/fd/<n>`, so no file of
  it is named in the container (ADR-0057). A secret given as a variable
  (`RUN --mount=type=secret,id=…,env=NAME`) works there; a cache mount is
  left out with a warning (the `RUN` runs without the cache, and what it
  writes at the target stays in its layer); a secret as
  a file and `--network=none` (the flag before anything is pulled, `RUN
  --network=none` at that `RUN`) are refused with the reason, never run
  without them. A container granting capabilities
  beyond Docker's defaults without `CAP_SETPCAP` is refused before the
  pull rather than at the first `RUN`.
- The VM's `e2e` suite builds the Phase 2 acceptance workloads in kaniko
  mode too: on the `/` of a default `docker run` (`rk-kaniko`) and as
  root on the VM with a directory per stage (`rk-kaniko-host`, `--jobs`
  compared on W3), next to kaniko, BuildKit and overlay mode; every
  ranikust image is checked for state, runtime markers and mount points
  that must not reach a layer. `just test-vm e2e` is now a light check of
  the whole pipeline (W0: a few small Dockerfiles on a busybox base made in
  the VM, every builder, a few minutes); the W1–W3 workloads are the ultra
  benchmark, `just test-vm ultra`, run only when asked for. Images are
  checked on the first round only (`RANIKUST_E2E_CHECK_ALL=1` for every
  round), and flattening an image is much faster (whiteouts applied to a
  whole subtree at once, each layer read once per run).
- A container test lane (tier T2c, ADR-0063): the 25 root tests that mean
  the same in a container are marked `#[ignore = "T2c: <profile>: ..."]` and
  `just test-container unit-c` runs them as root in throwaway rootful Docker
  containers on the test machine (profiles `default`, `sysadmin`,
  `netadmin`; never `--privileged`, no host volume, no network), each
  profile after control checks that the container allows what its tests
  exercise and no more. The VM's `unit` suite still runs them too.

### Changed

- The test VM (`just test-vm`) gets 12 vCPUs and 16 GiB for every suite,
  shrunk to what the host can spare, and boots a cached base image with
  Docker already installed (built once per cloud image and provisioning
  seed; `RANIKUST_VM_GOLDEN=0` for the plain cloud image), with a virtio
  entropy source, the disk on an I/O thread through io_uring, and one SSH
  connection for the whole run; `timings.txt` records where a run's time
  went. In `e2e`, every builder is pinned to the same vCPUs as well as
  limited to the same CPU quota, memory and swap, and each build's limits,
  read from its cgroup, are in its results.
- Kaniko mode off `/` (a directory per stage): the last stage to read a
  stage takes its directory over instead of copying it, pruned of what
  the copy leaves out and moved with `renameat2(RENAME_NOREPLACE)`, with
  the same result as the copy; a stage directory nobody reads any more is
- Kaniko mode off `/` (a directory per stage): a stage read by one stage
  only, built `FROM` it, hands that stage its directory instead of a
  copy, pruned of what the copy leaves out and moved with
  `renameat2(RENAME_NOREPLACE)`, with the copy's contents (ADR-0061; the
  choice made from the stage graph, never by scheduling, ADR-0062); a stage directory nobody reads any more is
  released at once on a release thread, which the build's end waits for
  (ADR-0061, Q-410). A chain of stages copies nothing, and the peak disk
  use drops. A `FROM <stage>` copy now also keeps the parent root's own
  xattrs.
- `--parallel-stages` is recognised and refused with exit 2 and its reason
  (ADR-0060, Q-400): outside a container
  kaniko mode already builds stages in parallel, and on a container's `/`
  a bare `chroot` per stage would let a `RUN` reach the state directory,
  the credentials and the other stages.
- In kaniko mode on `/`, `COPY` leaves device nodes, FIFOs and sockets out,
  as extraction does there (ADR-0060); overlay mode and the full sandbox
  keep them, as BuildKit. A copy that leaves any out warns once, naming
  the first.
- The full sandbox makes every mount below a `RUN`'s root `nodev` too, not
  only the root's own mount (`mount_setattr`, Linux 5.12); on older kernels
  a root with a mount below it is refused.
- `rnk_sandbox::Sandbox::start` takes `Caps`: the library no longer offers
  a sandbox that runs `Isolation::Direct` in a bare `chroot` off `/`
  (ADR-0060); kaniko mode on `/` uses `Sandbox::start_direct`.
- Kaniko mode outside a container builds each stage in a directory of its
  own, `<state>/builds/<id>/stages/<n>/rootfs`, and runs independent
  stages in parallel (`--jobs`), as overlay mode does, instead of one
  stage at a time on one root (ADR-0059). A stage built `FROM` another
  gets a copy of it (a reflink where the filesystem has them, never a
  hardlink; device nodes, FIFOs and sockets left out), so each stage
  costs disk until the build ends. Kaniko mode on `/` in a container is
  unchanged.
- Copying a file (`COPY`, `ADD`, `COPY --from`, a stage built `FROM`
  another off `/`) clones it where the filesystem shares extents and
  otherwise copies only its data: a sparse file keeps its holes instead
  of being written out at its full size. A file that holds more than its
  size once copied (appended to meanwhile) fails the copy instead of
  being cut short.
- `COPY` and `ADD` refuse a source that expands to an empty string (it
  copied the whole context or stage), and `COPY --from=<name>` where
  `<name>` is no stage and has no tag, digest, registry or path (a
  mistyped stage name pulled an image of that name); write
  `--from=<image>:<tag>` to copy from an image.
- ranikust picks the build root itself and keeps everything in one state
  directory, `/var/lib/ranikust` by default (ADR-0050): `--workdir` is
  gone, and kaniko's `--force` is refused, since ranikust never builds on
  a machine's own `/`. The state directory must be owned by the user
  running ranikust and not writable by others, below directories only
  root may write (root's sticky `/tmp` is fine); ranikust makes it `0700`.
  A `--state-dir` under someone's home directory is refused. Kaniko mode
  outside a container needs mount privileges (it is refused otherwise,
  rather than run `RUN` in a bare `chroot` on the host), and on `/` one
  build per container holds a lock on `/` itself.
- Compatibility with kaniko pipelines is no longer a goal (ADR-0049):
  `--target` takes one stage (a repeated flag or a comma-separated list is
  refused); kaniko flags that change nothing (`--cache-dir`,
  `--use-new-run`, `--cleanup`, `--pre-cleanup`, `--image-fs-extract-retry`,
  `--force-build-metadata`, `--skip-unused-stages`,
  `--compressed-caching=false`, `--no-push-cache`) are refused instead of
  ignored with a warning; `KANIKO_NO_PUSH` no longer sets `--no-push`,
  `KANIKO_REGISTRY_MIRROR` and `KANIKO_REGISTRY_MAP` no longer stop the
  run, and no warning names kaniko's variables. There will be no `login`
  subcommand.

- Log lines look like kaniko's: `INFO[0005] message` by default, with the
  full time under `--log-timestamp`; `--log-format=text` and `json` write
  logrus's plain and JSON forms.
- Libraries ranikust uses log at `info` at most, whatever `--verbosity`.
- Layers leave out files a `RUN` copied up without changing them, and
  write an opaque directory as whiteouts of what it hides, as BuildKit's
  overlay differ does (ADR-0034).
- Building the static musl binary from source needs zig 0.16.0 and
  cargo-zigbuild 0.24.0, which compile its C dependencies; see
  `CONTRIBUTING.md`.
- Where kaniko and BuildKit differ, image and `RUN` behaviour now follows
  BuildKit (ADR-0032):
  - OCI media types by default;
  - a new `ENTRYPOINT` clears an inherited `CMD`;
  - BuildKit's `EXPOSE` rules;
  - one history entry per instruction;
  - a default `PATH` in every config;
  - empty base-config fields dropped;
  - layers carry only `security.capability`;
  - loopback registries over plain HTTP;
  - `COPY` wildcards that match nothing are no error;
  - `--secret` with `type=env`;
  - cache `sharing=` enforced;
  - `RUN --network=host` only with `--allow network.host`;
  - a cancel kills a running `RUN`;
  - times clamped to `SOURCE_DATE_EPOCH`.

  These land in the releases that follow; the decisions are recorded now.
- A local build context is a plain path and kaniko's `dir://` form is
  dropped (ADR-0053); `--context-sub-path` applies only to fetched contexts
  (git, tar, HTTPS). `--context=dir://...` is refused (exit 2) with a
  message naming the plain path, and `--context-sub-path` with a local
  context is refused (exit 2) with a message naming the sub-directory to
  pass as `--context` (ADR-0054).
- Fixes from mining kaniko's, BuildKit's and buildah's issues (ADR-0052):
  - a kaniko-mode build on a container's `/` that follows an earlier one
    in the same container empties `/` before its first base, so its image
    never holds what the earlier build left (a marker,
    `/ranikust/.built`, tells them apart);
  - kaniko mode keeps `/var/run` out of layers only where it is a mount
    point, so an image's `/var/run -> /run` link (alpine 3.12 and later)
    is kept; `--ignore-var-run` and `--whitelist-var-run` now change
    nothing and are refused (exit 2);
  - a step that changes nothing (`RUN true`, a `WORKDIR` that exists, a
    `COPY` whose wildcard matches nothing) adds no layer, only its history
    entry, as kaniko; BuildKit adds an empty layer, which repeated gave
    images containerd refused (ADR-0058).

### Removed

- `RANIKUST_METRICS_FILE`: the metrics report is asked for with
  `--metrics-file` only, which now works with every subcommand; a warning
  names the flag when the variable is set.

### Fixed

- The VM's `e2e` suite no longer fills the guest's disk: the registry
  freed no blob when the suite deleted its repositories before each build,
  so a few dozen builds filled the 80 GB disk and the VM stopped answering
  SSH. Unlinked blobs are now deleted before each build (following no
  symbolic link) and handed back to the host by `fstrim` over a disk with
  discard, a build does not start with less than 10 GiB free, and the
  host needs as much free space as the VM's disk can grow to, less 2 GiB.
  kaniko's results directory in the suite moved off `/out`,
  which W3 writes and kaniko leaves out of its image as a mount point.
- In kaniko mode, copying a stage and walking a root grew with the cube of
  a directory chain's depth (a `RUN` can make 20,000 levels in seconds;
  3000 took 17 s per copy and per walk): the ignore-list check looked up
  every ancestor of every entry. It now looks only at prefixes as long as
  the longest entry. A file cut short or rewritten while a stage was
  copied is refused instead of being copied with zeros in place of the
  cut part.
- A `RUN` heredoc whose body starts with `#!` failed with "resolve a mount
  target in the root"; its script now runs from `/dev/pipes`, as in
  BuildKit.
- A `RUN` under a `USER` other than root can write `/dev/stdout` and
  `/dev/stderr` (kaniko#2928), and no `RUN` holds ranikust's own stdout
  and stderr any more (a root `RUN` could truncate or rewrite a
  `> build.log`): every `RUN` writes to pipes of its own, owned by its
  user, which ranikust copies to its streams.
- An unsupported or malformed `RUN --mount` (`bind`, `tmpfs`, `ssh`,
  `cache,from=`, a cache without a target, a bad `env=`), or a `required`
  secret not given with `--secret`, is refused before any secret of the
  same `RUN` is read.
- In kaniko mode, a file deleted and created again between the walk and
  the layer export could get its old inode number back and be written with
  content the walk never compared. The export now also checks size,
  `mtime` and `ctime`, and in `full` snapshot mode hashes the content it
  writes against the walk's hash.

[Unreleased]: https://github.com/tran4774/ranikust/commits/main
