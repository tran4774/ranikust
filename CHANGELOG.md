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
  `--mode=auto|overlay|kaniko`, `--workdir`, `--state-dir` (kaniko's
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
  and names the outcome. For now it builds only as root in overlay mode,
  which needs `--workdir` or `--state-dir`; elsewhere it stops with what
  failed and what to do instead (`sudo`, or a container started with
  `--privileged`), also when it runs inside a plain `chroot`. In a
  container the state directory must be on a volume: overlay cannot use
  the container's own overlay root. `--mode=kaniko` comes later.
- The state directory: `ranikust` marks it with `.ranikust-root` and
  refuses a non-empty directory without that mark, so a mistyped
  `--state-dir` never gets written to or cleaned up.
- A start-up check: on a kernel older than Linux 5.8, or where seccomp
  blocks `openat2` or `statx`, ranikust stops at once with a message naming
  the kernel.
- kaniko's flags, as listed in the user guide's command-line page:
  `-v`/`--verbosity`, `--log-format`, `--log-timestamp`, `--label`,
  `--annotation`, `--image-format`, `--compression-level`, the three digest
  files, `--insecure`, `--insecure-pull`, the `--skip-tls-verify` flags,
  `--push-retry`, `--image-download-retry`, `--context-sub-path` (for
  `dir://` contexts), `--custom-platform`, `--cache=false`, and kaniko's
  deprecated `--tarPath`, `--snapshotMode` and `--customPlatform`.
  Credentials go over plain HTTP only to registries named with
  `--insecure-registry` or destinations under `--insecure`, never to one a
  Dockerfile names under `--insecure-pull`. Flags that change nothing in the
  image built, such as
  `--cache-dir` and `--cleanup`, are accepted with a warning;
  the others are refused with the release that brings them.
- kaniko's variables: `KANIKO_NO_PUSH`, `DOCKER_AUTH_CONFIG` (read before
  `config.json`), and a warning naming every other `KANIKO_*` or
  `FF_KANIKO_*` variable that is set. `KANIKO_REGISTRY_MIRROR` and
  `KANIKO_REGISTRY_MAP` stop the build until mirrors are supported.
- Kaniko mode's workdir checks: `--mode=kaniko --workdir DIR` refuses `/`,
  a system directory such as `/usr` or `/home` or an ancestor of one
  (compared by device and inode, so `//` and symlinks are caught), a
  directory another build has locked, and a non-empty directory without a
  `.ranikust-root` marker. Kaniko-mode builds themselves still stop after
  these checks, before anything is written.
- The plan of a sequential kaniko-mode build, not yet wired to the CLI:
  kaniko's stage squashing on the shared root, which stages are walked for
  layers, what later stages read from each, and a scheduler mode that runs
  stages on the shared root one at a time in a fixed order.
- Kaniko mode is reachable: `--mode=auto` picks it where root may not mount
  (Docker's default, most Kubernetes pods) and says why. Without
  `--workdir` it builds on `/`, only inside a container as kaniko detects
  one, or with kaniko's `--force`. Its state directory defaults to
  `/ranikust/state` on `/` and `.ranikust` in the current directory with a
  `--workdir`. kaniko's `--ignore-path`, `--ignore-var-run` (and the old
  `--whitelist-var-run`), `--single-snapshot` and `--materialize` are
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

### Changed

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

### Removed

- `RANIKUST_METRICS_FILE`: the metrics report is asked for with
  `--metrics-file` only, which now works with every subcommand; a warning
  names the flag when the variable is set.

### Fixed

- A `RUN` heredoc whose body starts with `#!` failed with "resolve a mount
  target in the root"; its script now runs from `/dev/pipes`, as in
  BuildKit.

[Unreleased]: https://github.com/tran4774/ranikust/commits/main
