# kaniko map

**TL;DR:** Where each kaniko package lands in ranikust, what happens to each
common flag, and the behaviour to match, cited as `kaniko@44aa7ddc0 path:line`.
For anything else, use `kaniko-parity` or `kaniko-oracle`, never memory.

## Source

`44aa7ddc0` is osscontainertools/kaniko `34ba9eadf` (v1.28.6 + 7) plus 13
local performance commits behind `FF_KANIKO_*` flags
([research README](../research/README.md#source-snapshot)). Those commits
touched `root.go`, `build.go`, `push.go`, `run.go`, `snapshot.go`,
`layered_map.go` and `fs_util.go`, so upstream line numbers differ; prefer
behaviour that `34ba9eadf` also has. Checkout: `${KANIKO_SRC:-../kaniko}`.

## Packages to crates

| kaniko | Role | ranikust | Entry points |
|---|---|---|---|
| `cmd/executor/cmd/root.go` | flags, validation, build driver | `ranikust` | `AddKanikoOptionsFlags` `:330`, `AddSharedBuildFlags` `:336`, `AddRegistryOptionsFlags` `:391`, `runBuildTargets` `:233`, `checkContained` `:448` |
| `pkg/config` | options; global `RootDir`, `KanikoDir`; 68 `FF_KANIKO_*` flags | `BuildPaths { root, state }` (`rnk-store`), values; no feature flags | `pkg/config/init.go:41` (`KanikoDir`), `:96` (`RootDir = "/"`) |
| `pkg/dockerfile` | stages, targets, squashing, `ONBUILD` | `rnk-dockerfile` | `dockerfile.go:43` `ParseStages`, `:297` `MakeKanikoStages`, `:482` `Squash` |
| vendored BuildKit `frontend/dockerfile/` | parser, instructions, shell lexer | `rnk-dockerfile` (hand port, `ADR-0015`) | `parser/parser.go:283`, `instructions/parse.go:185`, `shell/lex.go:39` |
| vendored `moby/patternmatcher` | `.dockerignore` | `rnk-ignore` | `patternmatcher.go:42` `New`, `:128` `MatchesOrParentMatches` |
| `pkg/executor/build.go` | sequential stage loop | `rnk-sched` (DAG) + `rnk-builder` | `:1689` `DoBuild`, `:1410` `CalculateDependencies`, `:603` `stageBuilder.build`, `:968` `takeSnapshot` |
| `pkg/commands/*` | one file per instruction | `rnk-builder` (`stage.rs`, `build_stage`) | `commands.go:78` `GetCommand`, `copy.go:50`, `run.go:288` `runCommandInExec` |
| `pkg/commands/run.go`, `pkg/util/command_util.go` | `RUN` process, `PATH`, users | `rnk-sandbox`; user lookup `rnk-rootfs` | `run.go:350` `exec.Command`, `:356` `Setpgid`, `:330` global `PATH`, `command_util.go:534` `LookupUser` |
| `pkg/snapshot`, `pkg/filesystem` | walk, hash, `LayeredMap`, path resolution | `rnk-snapshot` (kaniko mode, Phase 2) | `snapshot.go:127` `TakeSnapshot`, `:199` `TakeSnapshotFS`, `:432` `addParentDirectories`, `layered_map.go:45`, `resolve.go:39` |
| `pkg/util/fs_util.go` | extraction, copy, ignore list, deletion, `.dockerignore` lookup | `rnk-rootfs` (mechanism), `rnk-snapshot` (policy), `rnk-context` (lookup) | `:165` `GetFSFromImage`, `:209` `extractLayer`, `:292` `DeleteFilesystem`, `:630` `DetectFilesystemIgnoreList`, `:1195-1204` `DockerignorePath` |
| `pkg/util/tar_util.go`, `pkg/pgzip`, `pkg/canonical` | tar, whiteouts, parallel gzip | `rnk-layer` | `tar_util.go:61` `AddFileToTar`, `:162` `Whiteout` |
| `pkg/image/remote`, `pkg/creds`, `pkg/mounts` | pull, keychain, cross-repo mounts | `rnk-registry`, `rnk-oci` | `remote.go:40` `RetrieveRemoteImage`, `creds.go:35` `GetKeychain`, `build.go:2434` `retrieveBaseImage` |
| `pkg/executor/push.go` | push, tar, OCI layout, digest files | `rnk-builder` (outputs), `rnk-registry` | `:184` `DoPush`, `:210` `layout.Write`, `:263` `tarball.MultiWriteToFile`, `:330` `remote.Write` |
| `pkg/cache`, `composite_cache.go` | layer cache and keys | `rnk-cache` (Phase 4) | `cache.go:51` `RetrieveLayer`, `build.go:322` `populateCompositeKey`, `push.go:404` `pushLayerToCache` |
| `pkg/buildcontext` | dir, tar, git, https, s3, gcs, azure | `rnk-context` (no cloud) | `buildcontext.go:47` `GetBuildContext` |
| `pkg/util/proc` | container detection | `ranikust`, `rnk-sandbox` | `proc.go:78` `GetContainerRuntime` |
| `pkg/bake`, `pkg/warmer` | HCL bake, cache warmer | out of scope (`ADR-0008`) | |
| `pkg/tracing`, `pkg/metrics`, `pkg/timing` | observability | `tracing` crate | |

Entry points are in the first column's package; `build.go` and `push.go`
mean `pkg/executor/`; `:N` repeats the file to its left. The
`deploy/Dockerfile:78-129` bundle (executor, `tini`, credential helpers)
becomes the single binary.

## Flags

`cmd/executor/cmd/root.go` registers 68 flags, 6 of them deprecated
(`ADR-0011`'s 71 counts every `Flags().` call). ranikust accepts the common
ones with the same meaning; the list is fixed in Phase 1. "accept" = an
`ADR-0011` candidate. "Now" is what the binary does today
(`crates/ranikust/src/cli.rs`): *parsed* flags are validated into
`BuildConfig`; *refused* flags stop the run with exit code 2 and a line
naming the reason, never silently ignored.

| kaniko flag | `root.go` | Default | ranikust | Now |
|---|---|---|---|---|
| `--context`, `-c` | `:338` | `/workspace/` | accept; s3/gcs/azure schemes out of scope | parsed; a local directory, `dir://` stripped; `git://`, `tar://`, `http(s)://` refused (Phase 5) |
| `--dockerfile`, `-f` | `:337` | `Dockerfile` | accept | parsed |
| `--destination`, `-d` | `:332` | none; required unless `--no-push` (`:201`) | accept | parsed, with kaniko's check |
| `--no-push` | `:349` | false | accept | parsed; pflag booleans (`--no-push=false`) |
| `--tar-path` | `:346` | none | accept; kaniko rejects it with zstd or `--image-format=oci` (`:182`, `:185`) | parsed; zstd with it refused |
| `--oci-layout-path` | `:356` | none | accept | parsed |
| `--build-arg` | `:343` | none | accept | parsed; `KEY` alone allowed |
| `--target` | `:333` | last stage; a list, the first is pushed | accept; list semantics `[open: Q-008]` | parsed as a list, comma-separated or repeated (Q-008 interim) |
| `--reproducible` | `:348` | false | accept | parsed |
| `--compression` | `:357` | unset (gzip) | accept (gzip, zstd) | parsed |
| `--cache`, `--cache-repo` | `:360`, `:351` | false; inferred from destination, `oci:` prefix = layout | accept; behaviour before Phase 4 `[open: Q-021]` | refused until Phase 4 (Q-021 interim) |
| `--registry-certificate` | `:402` | none | accept; added to host CAs plus embedded roots (`ADR-0024`) | parsed (`REGISTRY=PATH`) |
| `--insecure-registry` | `:399` | none | accept: plain HTTP to push and pull, HTTPS tried first (`ClientOptions::insecure`); `(Q-200)` interim (a) | accepted, repeatable |
| `--ignore-path` | `:371` | none | accept | refused until Phase 2 (kaniko mode) |
| `--kaniko-dir` | `:345` | `/kaniko` | renamed `--state-dir` (`ADR-0011`); default `/ranikust/state` in kaniko mode, `<workdir>/.ranikust` in overlay mode (`ADR-0022`) | refused, naming `--state-dir`; `--state-dir` parsed |
| `--snapshot-mode` | `:341` | `full` (`full\|redo\|time`, `build.go:2474`) | accept, kaniko's meaning: what the kaniko-mode walk compares; ignored with a warning in overlay mode (`ADR-0023`) | parsed; warning with `--mode=overlay` |
| `--custom-platform` | `:342` | host | `--platform`, `linux/amd64` only at first (`ADR-0009`); alias `[open: Q-020]` | refused, naming `--platform` (Q-020 interim); `--platform` parsed |
| `--force` | `:63` | false | replaced by the workdir checks; accept or reject `[open: Q-023]` | refused (Q-023 interim) |
| `--secret` | `:378` | none | kaniko's and buildctl's syntax (ADR-0032): `type=file\|env`, `src=`, `env=`, the id as variable or file by default | accepted, same meaning |
| `--registry-mirror`, `--registry-map`, `--credential-helpers` | `:407`, `:406`, `:409` | none; helpers: all | Phase 5 (mirrors, helpers); names `[open: Q-047]` | refused (Phase 5) |
| `--cache-dir` | `:352` | `/cache` | warmer's base-image cache; the store replaces it `[inferred]`; `[open: Q-023]` | refused (Q-023 interim) |
| `--git`, `--bucket` | `:368`, `:340` | none | git context Phase 5; GCS out of scope | refused (`-b` too) |
| `FF_KANIKO_*` | `pkg/config/featureflags.go` | | not carried over: the design is the default (`ADR-0011`) | not read |

`[open: Q-023]`, decided with the list: `--verbosity`, `--log-format`,
`--log-timestamp`, `--label`, `--annotation`, `--image-format`,
`--compression-level`, `--digest-file`, `--image-name-with-digest-file`,
`--image-name-tag-with-digest-file`, `--insecure`, `--insecure-pull`, `--skip-tls-verify*`,
`--registry-client-cert`, `--skip-default-registry-fallback`, `--push-retry`,
`--push-ignore-immutable-tag-errors`, `--image-download-retry`,
`--image-fs-extract-retry`, `--context-sub-path`, `--single-snapshot`,
`--use-new-run`, `--cleanup`, `--pre-cleanup`, `--preserve-context`,
`--materialize`, `--ignore-var-run`, `--cache-ttl`, `--cache-*-layers`,
`--compressed-caching`, `--no-push-cache`, `--skip-push-permission-check`,
`--dryrun`, and the deprecated forms (`:67`, `:384`–`:388`). Now: all
refused with the reason (Q-023 interim); the deprecated forms name their
replacement. `KANIKO_REGISTRY_MIRROR`, `KANIKO_NO_PUSH`,
`KANIKO_REGISTRY_MAP` and `KANIKO_METRICS_FILE` are not read.

ranikust-only flags: `--workdir`, `--state-dir`,
`--mode=auto|overlay|kaniko` (new, `ADR-0023`), `--jobs`, `--network`,
`--metrics-file` and `--platform`, all parsed; `--parallel-stages`
(placeholder name, Phase 2), `--seccomp`, `--userns` (Phase 3) not yet.
Subcommands: `build` (the default) and `version` exist; `push`, `prune`
and `login` do not (Q-048, Phase 5).

## Behaviour to match

Paths below are relative to the kaniko root at `44aa7ddc0`.

- **Stage squashing.** kaniko folds a locally built base stage into its
  child when exactly one stage builds `FROM` it and no `COPY --from` reads it
  (`pkg/dockerfile/dockerfile.go:393-406`); stages off the target path are
  dropped (`MakeKanikoStages`). ranikust squashes only in sequential kaniko
  mode; overlay mode reuses the parent's lower stack
  (`design/scheduler.md#the-graph`).
- **Stage names.** kaniko lower-cases a `FROM` base and matches it to an
  earlier stage case-insensitively (`pkg/dockerfile/dockerfile.go:80-93`);
  ranikust matches exactly, as BuildKit does (Q-051), so
  `Dockerfile_test_issue_519` (`FROM BUG_stage`) is an accepted difference.
- **`.dockerignore`.** kaniko filters with the deprecated `Matches`;
  ranikust follows BuildKit's walk (`ADR-0027`). A symlinked `COPY`
  source is followed only through kept entries, so a link cannot reach an
  ignored file.
- **Owner of copied context files.** Without `--chown`, kaniko gives them
  the active `USER`'s ids, and `0:0` only with `FF_KANIKO_COPY_AS_ROOT`
  (`pkg/commands/copy.go:80-95`); ranikust gives `0:0`, the Dockerfile
  reference's rule and BuildKit's. A difference to keep in the config
  and content comparison of task 13.
- **`MAINTAINER`.** kaniko skips it with a warning
  (`pkg/commands/commands.go:115-117`); ranikust sets the config's
  `author`, as BuildKit does.
- **`PATH`.** kaniko gives `FROM scratch` a `PATH` (`ScratchEnvVars`)
  and leaves any other image without one as it is; ranikust adds the
  default `PATH` to every config that lacks it, as BuildKit (`ADR-0032`,
  Q-185).
- **Ignore list.** kaniko ignores `/kaniko`, `/etc/mtab` and
  `/tmp/apt-key-gpghome` (`pkg/util/fs_util.go:72-89`), `/var/run` while
  `--ignore-var-run` is true (`cmd/executor/cmd/root.go:155-165`), every
  mount point in `/proc/self/mountinfo` (`pkg/util/fs_util.go:630`) and
  `--ignore-path`. ranikust's kaniko mode takes mount points under the root,
  `--ignore-path`, `/proc`, `/sys`, `/dev` and the state directory
  (`/ranikust/state` in a container, `ADR-0022`;
  `design/kaniko-mode.md#ignore-list`); `/etc/mtab`, `/tmp/apt-key-gpghome`
  and `/var/run` are `[open: Q-035]`. Overlay mode needs no list: mounts
  live in `RUN`'s namespace.
- **Timestamps and `--reproducible`.** kaniko stamps the pushed image with
  the current time (`pkg/executor/build.go:2100`); with `--reproducible` it
  rewrites the image through `mutate.Canonical` and switches to Docker v2s2
  unless `FF_KANIKO_REPRODUCIBLE_PRESERVE_FORMAT`
  (`pkg/executor/build.go:2104-2127`, `canonicalImage` at `:1181`).
  ranikust follows BuildKit instead (`ADR-0032`, Q-006, Q-160, Q-161): the
  current time by default; under `--reproducible` the epoch is
  `SOURCE_DATE_EPOCH` when set (kaniko never reads it: no match in `pkg/`
  or `cmd/`), else the Unix epoch (kaniko writes Go's zero time,
  `0001-01-01T00:00:00Z`); the config's `created` and the build's history
  entries carry it, the base image's history keeps its times, built-layer
  mtimes later than it are clamped to it and earlier ones kept, base layers
  are kept as pulled (kaniko rewrites them), and the image stays OCI (no
  switch to Docker v2s2). Same bytes for any thread count.
- **Cache keys.** kaniko chains composite keys
  (`pkg/executor/build.go:322`, `pkg/executor/composite_cache.go:35`).
  ranikust has its own versioned, mode-independent key and neither reads nor
  writes kaniko's cache (`ADR-0005`); migrating users start cold.
- **`RUN`.** kaniko runs on `/` with `Setpgid`, a process-wide `PATH`
  (`pkg/commands/run.go:320-339`) and host user lookup; cache mounts swap
  directories (`pkg/commands/run.go:104`, `:563`). ranikust sandboxes `RUN`
  (namespaces, `pivot_root`, the root's `/etc/passwd`, bind mounts), except
  in kaniko mode without mounts, which runs it directly like kaniko.
- **Deletion and containment.** `DeleteFilesystem` walks `RootDir` and calls
  `os.RemoveAll` (`pkg/util/fs_util.go:292-324`), which crosses mounts;
  kaniko refuses to run outside a container without `--force`
  (`cmd/executor/cmd/root.go:240`). ranikust deletes by descriptor and mount
  id, and requires a safe `--workdir` instead.
- **Parent directories.** kaniko writes ancestor directory headers
  (`pkg/snapshot/snapshot.go:432`); ranikust's exporter must too
  (`design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer`).
- **Following BuildKit** (`ADR-0032`). Where kaniko and BuildKit differ
  on what the image or a `RUN` sees, ranikust does what BuildKit does:
  - OCI media types by default, whatever the base and `--reproducible`
    (kaniko: the base's format, Docker v2s2 under `--reproducible`; Q-006).
  - A new `ENTRYPOINT` clears an inherited `CMD`; `EXPOSE` takes `sctp`
    and expands ranges; `HEALTHCHECK --start-interval` is kept (Q-130).
  - One history entry per instruction, `empty_layer` for metadata, no
    author (kaniko: one per layer, author `kaniko`; Q-131).
  - Empty values and non-spec fields of the base config are dropped
    (kaniko keeps the go-containerregistry fields; Q-132).
  - Loopback registries are reached over plain HTTP without a flag (Q-065).
  - Layers carry only the `security.capability` xattr (Q-025).
  - A `COPY` wildcard that matches nothing is no error, and several
    sources need no trailing `/` (kaniko's `IsSrcsValid`; Q-080);
    sockets become empty files and `--chmod` takes symbolic modes (Q-120).
  - `--secret` takes `type=env`; a secret is a private `0400` file
    (Q-022, Q-182); cache `sharing=` is enforced (Q-181);
    `RUN --network=host` needs `--allow network.host` (Q-184); without
    `CAP_NET_ADMIN`, `RUN --network=none` needs `--allow
    network.none-userns` (ADR-0033, ranikust's own); a cancel
    kills a running `RUN` (Q-186).

  The code for these lands in follow-up branches; until then the interims
  noted in each Q entry apply.
- **Accepted differences** between overlay and kaniko mode (`ADR-0012`):
  hardlink splitting on in-place writes, metadata changes on hardlinked
  files, link-count-only changes, copied-up but unchanged files.
