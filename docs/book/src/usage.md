# Command line

> **ranikust is pre-alpha.** It builds a Dockerfile in overlay mode as
> root only (on a host with `sudo`, or in a `--privileged` container, its
> `--workdir` on a volume), and needs `--workdir` or `--state-dir`;
> elsewhere it stops with exit code 1, the step that failed and what to do
> instead. It prints the image digest on stdout. Instructions of later
> phases (`ADD` of a URL or an archive, `COPY --link`, `RUN
> --mount=type=bind`, `ONBUILD`…) stop the build with a message naming
> the phase.

## Running it

Without a subcommand, ranikust builds, as kaniko's `executor` does, so the
flags of a kaniko job carry over:

```sh
ranikust --context=/workspace --dockerfile=Dockerfile \
  --destination=registry.example.com/team/app:1.0
```

`ranikust build …` is the same thing spelt out. `ranikust version` (or
`--version`) prints the version, and `ranikust --help` lists every flag.
`push`, `login` and `prune` do not exist yet.

Before anything else, ranikust checks that the kernel is Linux 5.8 or later
and that `openat2` and `statx` are allowed, and stops with a message naming
the kernel if not (see [Choosing where to run it](where-to-run.md)).
`version` and `--help` work on any kernel.

## kaniko flags it accepts

These keep kaniko's names, short forms and meaning. Booleans work as in
kaniko: `--no-push` and `--no-push=true` turn it on, `--no-push=false` off;
`--no-push false` is an error.

| Flag | Meaning |
|---|---|
| `-c`, `--context=DIR` | The build context, a local directory (`dir://` allowed). Default `/workspace/`. git, tar and HTTPS contexts come later; S3, GCS and Azure are not planned. |
| `--context-sub-path=DIR` | Build from this sub-directory of a `dir://` context (inside it: no `..`). Ignored, with a warning, for a plain path, as kaniko does. |
| `-f`, `--dockerfile=PATH` | The Dockerfile. Default `Dockerfile`. |
| `--build-arg=KEY=VALUE` | A build argument; `KEY` alone takes its value from the environment. Repeatable. |
| `--target=STAGE` | Stages to build, comma-separated or repeated; the first is the image produced. Default: the last stage. |
| `-d`, `--destination=IMAGE` | An image to push to. Repeatable. Required unless `--no-push`. |
| `--no-push` | Do not push. |
| `--tar-path=PATH` | Also write a Docker `save` tarball. |
| `--oci-layout-path=PATH` | Also write an OCI image layout. |
| `--compression=gzip\|zstd` | Layer compression, gzip by default. zstd cannot go into a `--tar-path` tarball. |
| `--reproducible` | Fixed timestamps, for byte-identical images: the image's time is `SOURCE_DATE_EPOCH`, else 1970-01-01, and file times later than it are set to it, as BuildKit does. |
| `--registry-certificate=REGISTRY=PATH` | An extra CA certificate for one registry. Repeatable. |
| `--insecure-registry=REGISTRY` | Reach this registry (`host[:port]`) over plain HTTP when HTTPS fails, as kaniko. Repeatable. A loopback registry (`localhost`, `127.0.0.0/8`, `::1`) is plain HTTP without it, as BuildKit. |
| `--snapshot-mode=full\|redo\|time` | What kaniko mode compares between snapshots, as in kaniko; ignored, with a warning, in overlay mode. `--snapshotMode` works too, with kaniko's deprecation warning. |
| `--compression-level=N` | gzip `-1` (6) to `9`, zstd `1` to `19`. gzip `-2` and zstd `20` to `22`, which kaniko takes, are refused. Default: ranikust's (gzip 6, zstd 3). |
| `--image-format=docker\|oci` | The image's media types. Default `oci`, as BuildKit; `docker` cannot go with `--compression=zstd`. Unlike kaniko, `oci` can go with `--tar-path`. |
| `--label=KEY=VALUE` | A label on the image built (the pushed stage only), set before its first instruction, so a `LABEL` of the same key in that stage wins. Repeatable. |
| `--annotation=KEY=VALUE` | An annotation on the image manifest. Repeatable. The base image's annotations are not carried over. |
| `--digest-file=PATH` | Write the image digest (`sha256:…`, no newline), also with `--no-push`. |
| `--image-name-with-digest-file=PATH` | Write `repository@digest`, one line per destination; needs `--destination`. |
| `--image-name-tag-with-digest-file=PATH` | Write `repository:tag@digest`, one line per destination (`latest` when no tag); needs `--destination`. Given with the previous flag, both files get this text, as in kaniko. The three digest files are local paths (a missing directory is made `0700`); kaniko's upload to an `https://` URL is not supported. |
| `--insecure` | Destinations may be plain HTTP; their credentials may then be sent over it. |
| `--insecure-pull` | The registries of `FROM` and `COPY --from` images may be plain HTTP. They are reached anonymously over it: a Dockerfile names them, and ranikust sends credentials over plain HTTP only to registries you name (`--insecure-registry`, or destinations under `--insecure`). |
| `--skip-tls-verify` | Do not verify the TLS certificates of destinations. |
| `--skip-tls-verify-pull` | Do not verify the TLS certificates of the registries pulled from. |
| `--skip-tls-verify-registry=REGISTRY` | Do not verify this registry's TLS certificate. Repeatable. These insecure and TLS settings apply per registry, so a registry both pushed to and pulled from gets both. |
| `--push-retry=N` | Start the whole push again up to N times after it failed, waiting 1 s, then twice as long each time, as kaniko. Each request keeps ranikust's own retries inside. |
| `--image-download-retry=N` | The same for fetching a base image's manifest. |
| `-v`, `--verbosity=LEVEL` | `trace`, `debug`, `info` (default), `warn` (or `warning`), `error`, in any case; `fatal` and `panic` log as `error`. Other programs' libraries log at `info` at most. Never changes the metrics report. |
| `--log-format=text\|color\|json` | As kaniko's: `color` (default) colors the level and counts seconds since start (`INFO[0005] message`); `text` writes `time=… level=… msg=…`; `json` one object per line with `level`, `msg` and `time`. Times are UTC. |
| `--log-timestamp` | In the `color` format, the full time instead of the seconds since start. |
| `--custom-platform=linux/amd64` | Another name for `--platform` (also `--customPlatform`, with a warning). |
| `--cache=false` | Accepted: no layer cache is what this version does. `--cache` itself comes later. `--compressed-caching=false` and `--no-push-cache` are accepted with a warning. |
| `--skip-push-permission-check`, `--preserve-context` | Accepted: ranikust makes no write check before the build, and never touches the context. |

## ranikust's own flags

| Flag | Meaning |
|---|---|
| `--mode=auto\|overlay\|kaniko` | How to snapshot: `auto` (default) probes the environment and picks the fastest mode that works ([How ranikust builds](how-it-builds.md)). |
| `--workdir=DIR` | The work directory. |
| `--state-dir=DIR` | Where ranikust keeps its store; kaniko's `--kaniko-dir`. Default `<workdir>/.ranikust` in overlay mode, `/ranikust/state` in kaniko mode. |
| `--jobs=N` | Stages built at once. Default: the number of CPUs available. |
| `--network=host\|none` | The network `RUN` gets. Default `host`. |
| `--platform=linux/amd64` | The image platform; `--custom-platform` is the same flag. Only `linux/amd64` for now. |
| `--metrics-file=PATH` | Write the metrics report there when ranikust exits, also after an error; works with `version` too. The only way to ask for it: `RANIKUST_METRICS_FILE` is not read (a warning says so). |
| `--secret=id=ID[,src=PATH\|,env=VAR]` | A secret for `RUN --mount=type=secret,id=ID`: a file (`src=`, `type=file`) or a variable of ranikust's environment (`env=`, or `type=env` with `src=VAR`); without either, the variable named ID if it is set, else the file ID. Kept out of every layer, the config and the logs; at most 500 KiB. kaniko's and buildctl's syntax. Repeat for several. |
| `--allow=network.host` | Lets a `RUN --network=host` lift `--network=none`, as BuildKit's entitlement. |
| `--allow=network.none-userns` | Where ranikust lacks `CAP_NET_ADMIN` (a container with only `SYS_ADMIN` added), lets `RUN --network=none` get loopback through a user namespace; without it such a `RUN` is refused. A root `RUN` there holds `CAP_NET_ADMIN` over its own network namespace, and a seccomp filter closes: joining or creating user namespaces, `io_uring`, PF_KEY, netlink other than route, sock_diag, audit and uevent (nf_tables, xfrm, generic netlink), the iptables, ip6tables, arptables, ebtables, IPVS, IPsec-policy and multicast-routing socket options, and `TCP_REPAIR`. Everything else `CAP_NET_ADMIN` allows in that namespace stays open, rtnetlink included: `ip link add type <kind>` can load a kernel module on the host ([ADR-0033](https://github.com/tran4774/ranikust/blob/main/docs/adr/0033-network-none-loopback-through-a-user-namespace.md)). ranikust's own. |

## kaniko flags it ignores

These change nothing in the image ranikust builds, so it accepts them and
says once, as a warning, that it ignores them:

| Flag | Why |
|---|---|
| `--force` | ranikust runs on hosts by design and checks `--workdir` instead. |
| `--cache-dir` | The warmer is not planned; base images are pulled into the store. |
| `--use-new-run`, `--cleanup`, `--pre-cleanup`, `--image-fs-extract-retry` | They tune kaniko's own filesystem handling, which overlay mode does not do. |
| `--force-build-metadata`, `--skip-unused-stages` | Deprecated in kaniko, which ignores them too. |

## kaniko flags it refuses

ranikust never ignores a kaniko flag that would change the image. One it
does not take stops the run with exit code 2 and a line saying why, for
every such flag on the command line:

```text
ranikust: error: kaniko's --cache is not supported: the layer cache comes in Phase 4 (Q-021)
ranikust: error: kaniko's --kaniko-dir is not supported: renamed in ranikust: use --state-dir
```

- **Renamed:** `--kaniko-dir` is `--state-dir`.
- **With the layer cache:** `--cache`, `--cache-repo`, `--cache-ttl`,
  `--cache-copy-layers`, `--cache-run-layers`, `--compressed-caching`,
  `--no-push-cache`.
- **With kaniko mode:** `--ignore-path`, `--single-snapshot`,
  `--materialize`, `--ignore-var-run` (and its old name
  `--whitelist-var-run`).
- **With registry mirrors and credential helpers:** `--git` (git
  contexts), `--registry-mirror`, `--registry-map`, `--credential-helpers`,
  `--skip-default-registry-fallback`, `--registry-client-cert`,
  `--push-ignore-immutable-tag-errors`.
- **Not scheduled:** `--dryrun`.
- **Not planned:** `--bucket` (`-b`), `--azure-container-registry-config`
  and kaniko's `bake`.

## kaniko's environment variables

- `KANIKO_NO_PUSH=true|false` sets `--no-push`, and wins over the flag, as
  in kaniko.
- `KANIKO_REGISTRY_MIRROR` and `KANIKO_REGISTRY_MAP` stop the run with exit
  code 2: mirrors come later, and pulling from another registry than the
  one they name would be wrong.
- `DOCKER_AUTH_CONFIG` holds a `config.json`'s `auths` inline (nothing else:
  no `credsStore` or `credHelpers`); it is asked before the
  file (`$DOCKER_CONFIG/config.json` or `~/.docker/config.json`), as in
  kaniko.
- `KANIKO_PRESERVE_CONTEXT` is accepted, as its flag; `KANIKO_DIR` is
  ignored with a warning naming `--state-dir`.
- Any other `KANIKO_*` or `FF_KANIKO_*` variable that is set is named in one
  warning at start-up. `KANIKO_METRICS_FILE` is one of them: use
  `--metrics-file`.

kaniko's feature flags (`FF_KANIKO_*`) are not read: ranikust has one
behaviour, which for most of them is what the flag turns on.

| ranikust | kaniko's feature flags |
|---|---|
| Behaves as if it were on | `COPY_AS_ROOT` (copied context files are owned by 0:0 without `--chown`), `BUILDKIT_ARG_ENV_PRECEDENCE`, `REPRODUCIBLE_PRESERVE_BASE_LAYERS`, `REPRODUCIBLE_PRESERVE_FORMAT` (always OCI), `NO_PROPAGATE_ANNOTATIONS`, `PRESERVE_HARDLINKS`, `SECUREJOIN_EXTRACTION`, `CONFINE_COPY_SOURCE`, `CROSS_REPO_MOUNT`, `SKIP_WRITE_WHITEOUTS`, `UNPACK_ZSTD`, `EXPAND_HEREDOC`, `CHOWN_ON_IMPLICIT_DIRS` (`WORKDIR` parents get the user), `COPY_CHMOD_ON_IMPLICIT_DIRS`, `RELATIVE_LINK_TARGETS`, `RUN_HONOR_GROUP`, `VOLUME_SKIP_MKDIR`, `OCI_SCRATCH_BASE`, `SCOPED_DOCKERIGNORE`, `SCOPED_REGISTRY_CERTIFICATES`, `SKIP_RELABEL_RECOMPRESS` (a relabelled base layer keeps its bytes) |
| ranikust does it its own way | `COPY_SKIP_SPECIAL_FILES`: device nodes and FIFOs are recreated, never read, and a socket becomes an empty file, as BuildKit. `RUN_VIA_TINI`: each `RUN` is PID 1 of its own namespace, and the kernel reaps what it leaves. `NATIVE_COPY`: ranikust has one copy, which keeps hardlinks. |
| Comes later with the feature | `ADD_CHECKSUM`, `ADD_UNPACK`, `COPY_LINK`, `RUN_MOUNT_BIND`, `UNPACK_XZ`, `UNTAR_SKIP_ROOT` (local `ADD` archives); `PATH_SCOPED_REGISTRY_AUTH` (with credential helpers); the cache flags (`CACHE_*`, `*_CACHE_KEY`, `SKIP_CACHED_STAGES`, `DEFER_CACHE_PUSH`, `IGNORE_CACHED_MANIFEST`, `DEPRECATE_LAYERLESS_CACHE_ENTRIES`, `IMAGE_STAGES`, `HASH_DIR_FRAMING`, `CACHE_HASH_BLAKE3`); kaniko mode's `PRESERVE_MOUNTED_PATHS`, `PRESERVE_MOUNTED_SYMLINKS` |
| Not applicable | Performance switches where ranikust's design is the default (`PARALLEL_*`, `PIPELINED_EXTRACT`, `STREAM_TAR_COMPRESS`, `SHARED_*`, `STAT_HASH_CACHE`, `FAST_*`, `POOL_REGISTRY_CONNECTIONS`, `PEEK_ARCHIVE_HEADER`, `PRECOMPILE_DOCKERIGNORE`, `CONTEXT_HASH_PARALLEL`); `DISABLE_HTTP2`; `LAYER_HINTS`; kaniko's own directory (`CLEAN_KANIKO_DIR`, `DEPRECATE_INTER_STAGE_RESTORE`); the warmer (`OCI_WARMER`, `WARMER_CACHE_LOCK`) |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success, including `--help` and `version`. |
| 1 | An error while running (a `RUN` that fails, a missing `COPY` source, an instruction of a later phase), or a kernel ranikust cannot use. |
| 2 | A usage error: an unknown or refused flag, a bad value, an invalid combination such as no `--destination` without `--no-push`. |
