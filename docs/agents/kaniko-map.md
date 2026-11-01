# kaniko map

**TL;DR:** Where each kaniko package lands in ranikust, what happens to each
common flag, and the behaviour to match, cited as `kaniko@44aa7ddc0 path:line`.
For anything else, use `kaniko-parity` or `upstream-oracle`, never memory.

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
| `--context`, `-c` | `:338` | `/workspace/` | accept; s3/gcs/azure schemes out of scope | parsed; a local directory as a plain path (`dir://` is dropped and refused, ADR-0053, ADR-0054; still stripped until task 7's branch); `git://`, `tar://`, `http(s)://` refused (Phase 5) |
| `--dockerfile`, `-f` | `:337` | `Dockerfile` | accept | parsed |
| `--destination`, `-d` | `:332` | none; required unless `--no-push` (`:201`) | accept | parsed, with kaniko's check |
| `--no-push` | `:349` | false | accept | parsed; pflag booleans (`--no-push=false`) |
| `--tar-path` | `:346` | none | accept; kaniko rejects it with zstd or `--image-format=oci` (`:182`, `:185`) | parsed; zstd with it refused; OCI media types allowed, unlike kaniko (ADR-0036); `--tarPath` refused, naming `--tar-path` (ADR-0052) |
| `--oci-layout-path` | `:356` | none | accept | parsed |
| `--build-arg` | `:343` | none | accept | parsed; `KEY` alone allowed |
| `--target` | `:333` | last stage; a list, the first is pushed | one stage only (ADR-0049): a list builds stages for nothing | a second stage, repeated or comma-separated, is a usage error |
| `--reproducible` | `:348` | false | accept | parsed |
| `--compression` | `:357` | unset (gzip) | accept (gzip, zstd) | parsed |
| `--cache`, `--cache-repo` | `:360`, `:351` | false; inferred from destination, `oci:` prefix = layout | Phase 4; `--cache=false` accepted (Q-021, ADR-0036) | refused until Phase 4; `--cache=false` accepted |
| `--registry-certificate` | `:402` | none | accept; added to host CAs plus embedded roots (`ADR-0024`) | parsed (`REGISTRY=PATH`) |
| `--insecure-registry` | `:399` | none | accept: plain HTTP to push and pull, HTTPS tried first (`ClientOptions::insecure`); `(Q-200)` interim (a) | accepted, repeatable |
| `--ignore-path` | `:371` | none | accept; absolute, cleaned as `filepath.Clean` (`pkg/util/fs_util.go:144-149`) | accepted, repeatable (ADR-0040); a relative path refused; refused in overlay mode for good (ADR-0049) |
| `--kaniko-dir` | `:345` | `/kaniko` | renamed `--state-dir` (`ADR-0011`); default `/var/lib/ranikust` in every mode (`ADR-0050`) | refused, naming `--state-dir`; `--state-dir` parsed |
| `--snapshot-mode` | `:341` | `full` (`full\|redo\|time`, `build.go:2474`) | accept, kaniko's meaning: what the kaniko-mode walk compares; ignored with a warning in overlay mode (`ADR-0023`) | parsed; warning with `--mode=overlay`; `--snapshotMode` refused, naming `--snapshot-mode` (ADR-0052) |
| `--custom-platform` | `:342` | host | `--platform`, `linux/amd64` only at first (`ADR-0009`) | refused, naming `--platform`, as `--customPlatform` (`ADR-0052`) |
| `--force` | `:63` | false | refused: ranikust never builds on a machine's own `/`; outside a container kaniko mode builds in its state directory (`ADR-0050`) | refused with the reason |
| `--secret` | `:378` | none | kaniko's and buildctl's syntax (ADR-0032): `type=file\|env`, `src=`, `env=`, the id as variable or file by default | accepted, same meaning |
| `--registry-mirror`, `--registry-map`, `--credential-helpers` | `:407`, `:406`, `:409` | none; helpers: all | Phase 5 (mirrors, helpers); a mirror or mapped registry never gets another registry's credentials nor serves a push, names chosen then (ADR-0049) | refused (Phase 5) |
| `--cache-dir` | `:352` | `/cache` | warmer's base-image cache; the warmer is out of scope, the store replaces it | refused (ADR-0049) |
| `--git`, `--bucket` | `:368`, `:340` | none | git context Phase 5: `--git` keeps kaniko's keys (`branch`, `single-branch`, `depth`, `recurse-submodules`, `insecure-skip-tls`) and `GIT_USERNAME`/`GIT_PASSWORD`/`GIT_TOKEN`/`GIT_PULL_METHOD` keep their meaning; ranikust adds `--git-backend`, `--git-binary`, `--git-config`, `--git-arg`, `--git-host-config` (ADR-0035); GCS out of scope | refused (`-b` too) |
| `FF_KANIKO_*` | `pkg/config/featureflags.go` | | not carried over: the design is the default (`ADR-0011`); how ranikust compares with each is on the book's usage page | not read, not warned about (ADR-0049) |

The rest of kaniko's flags, as decided by ADR-0036 and ADR-0049 (the
meaning tests are listed there):

| kaniko flags | ranikust |
|---|---|
| `-v`/`--verbosity`, `--log-format`, `--log-timestamp` (`:60-62`) | accepted, written as logrus (`pkg/logging/logging.go:48-60`): `color` forced with seconds since start, `text` `time=… level=… msg=…`, `json` with `level`/`msg`/`time`; `--log-timestamp` the full time in `color`; level case-insensitive, `warning` accepted, `fatal`/`panic` as `error`; times UTC (logrus: local); other crates at `info` at most |
| `--label` (`:366`), `--annotation` (`:374`) | accepted: labels on the pushed stage before its first instruction (`build.go:190-191,275-285`), annotations on the manifest |
| `--image-format` (`:358`) | accepted; default OCI (`ADR-0032`); `docker` with zstd exits 2 (`:178`) |
| `--compression-level` (`:359`) | accepted: gzip -1 is 6, 0-9; zstd 1-19; gzip -2 and zstd 20-22 refused |
| `--digest-file`, `--image-name-with-digest-file`, `--image-name-tag-with-digest-file` (`:353-355`) | accepted, kaniko's formats (`push.go:194-256`); both name files need a destination (`:212-217`); missing parent made `0700`; an `https://` path (kaniko `PUT`s it, `push.go:150-166`) refused |
| `--insecure`, `--insecure-pull`, `--skip-tls-verify`, `--skip-tls-verify-pull`, `--skip-tls-verify-registry` (`:392-400`) | accepted, as per-registry sets: destinations for the push flags, `FROM` and `COPY --from` registries for the pull flags; credentials go over plain HTTP only to `--insecure-registry` and destinations under `--insecure`, never to a registry only `--insecure-pull` allows (T11); `index.docker.io` is `docker.io` |
| `--push-retry`, `--image-download-retry` (`:396`, `:398`) | accepted as kaniko's outer loops (`util.go:221`, 1 s doubling, any error) around the client's own retries: the whole push (`push.go:364`), the image manifest fetch (`remote.go:79,121`) |
| `--skip-push-permission-check` (`:372`), `--preserve-context` (`:375`) | accepted: their meaning holds without code in overlay mode |
| `--use-new-run`, `--cleanup`, `--pre-cleanup`, `--image-fs-extract-retry`, `--force-build-metadata`, `--skip-unused-stages`, `--compressed-caching=false`, `--no-push-cache` | refused with the reason: they change nothing in ranikust, and no flag is ignored (ADR-0049) |
| `--context-sub-path` (`:339`) | kaniko joins it onto a `dir://` or fetched context (`root.go:599-631`, `buildcontext/dir.go:25`) and ignores it for a plain path; ranikust drops `dir://` (refused, exit 2) and applies it to fetched contexts only, and refuses it with a local context, exit 2 (ADR-0053, ADR-0054) |
| `--cache-ttl`, `--cache-copy-layers`, `--cache-run-layers`, `--compressed-caching` (true) | refused until Phase 4 |
| `--single-snapshot`, `--materialize` | accepted with kaniko's meaning in kaniko mode (ADR-0040); in overlay mode `--single-snapshot` refused for good (ADR-0049), `--materialize` warned about; wired to the walk by Phase 2 tasks 3–6 |
| `--ignore-var-run` (`ADR-0038`), `--whitelist-var-run` | refused, exit 2: kaniko mode ignores `/var/run` only where it is a mount point, as every mount point, so the flag changes nothing (ADR-0052 D4, ADR-0049) |
| `--registry-client-cert`, `--skip-default-registry-fallback`, `--push-ignore-immutable-tag-errors` | refused until Phase 5 |
| `--dryrun` | refused, not scheduled |
| `--azure-container-registry-config` (hidden, `:413`), `bake` | refused for good (`ADR-0008`) |

Variables: `DOCKER_AUTH_CONFIG` is read before `config.json`
(`pkg/creds/creds.go:39`), `auths` only. kaniko's own variables
(`KANIKO_NO_PUSH`, `:83-89`; `KANIKO_REGISTRY_MIRROR`, `KANIKO_REGISTRY_MAP`,
`KANIKO_DIR`, `KANIKO_METRICS_FILE`, every `KANIKO_*`/`FF_KANIKO_*`) are
not read and not warned about (ADR-0049).

ranikust-only flags: `--state-dir`,
`--mode=auto|overlay|kaniko` (new, `ADR-0023`), `--jobs`, `--network`,
`--metrics-file` and `--platform`, all parsed; `--parallel-stages`
(Phase 2, `ADR-0038`), `--seccomp`, `--userns` (Phase 3) not yet.
Subcommands: `build` (the default) and `version` exist; `push` and `prune`
do not yet (Q-048, Phase 5); `login` never will (ADR-0049).

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
  (`/var/lib/ranikust` in a container, `ADR-0050`;
  `design/kaniko-mode.md#ignore-list`), and, as kaniko, all of
  `/ranikust`, `/etc/mtab` and `/tmp/apt-key-gpghome` (`ADR-0038`), but not
  `/var/run` unless it is a mount point, so alpine's `/var/run -> /run`
  link survives (ADR-0052 D4; kaniko#1297). Overlay mode needs no list:
  mounts live in `RUN`'s namespace.
- **A second build on `/`.** kaniko extracts the first base over whatever
  `/` holds, so a second build in one container ships the first's files
  (kaniko#3310); ranikust marks `/` under `/ranikust` and empties it first
  when the marker is there (ADR-0052 D1).
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
  id; it builds on `/` only where kaniko's container check finds a
  container, and anywhere else in a root it makes in its state directory;
  `--force` is refused (ADR-0048, ADR-0050).
- **Parent directories.** kaniko writes ancestor directory headers
  (`pkg/snapshot/snapshot.go:432`); ranikust's exporters do too
  (`design/overlay-snapshot.md#exporting-a-diff-as-an-oci-layer`,
  `design/kaniko-mode.md#exporter-requirements-specific-to-the-walk`), but
  not for a socket alone, which both leave out (`ADR-0045`).
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
  - A kaniko-mode layer holds what changed: not the unchanged target of a
    changed symlink, which kaniko's `ResolvePaths` adds
    (`pkg/filesystem/resolve.go:69-98`), nor the parents of a changed
    socket (`ADR-0045`).
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
