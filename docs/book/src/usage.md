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
| `-f`, `--dockerfile=PATH` | The Dockerfile. Default `Dockerfile`. |
| `--build-arg=KEY=VALUE` | A build argument; `KEY` alone takes its value from the environment. Repeatable. |
| `--target=STAGE` | Stages to build, comma-separated or repeated; the first is the image produced. Default: the last stage. |
| `-d`, `--destination=IMAGE` | An image to push to. Repeatable. Required unless `--no-push`. |
| `--no-push` | Do not push. |
| `--tar-path=PATH` | Also write a Docker `save` tarball. |
| `--oci-layout-path=PATH` | Also write an OCI image layout. |
| `--compression=gzip\|zstd` | Layer compression, gzip by default. zstd cannot go into a `--tar-path` tarball. |
| `--reproducible` | Fixed timestamps, for byte-identical images. |
| `--registry-certificate=REGISTRY=PATH` | An extra CA certificate for one registry. Repeatable. |
| `--insecure-registry=REGISTRY` | Reach this registry (`host[:port]`) over plain HTTP when HTTPS fails, as kaniko. Repeatable. |
| `--snapshot-mode=full\|redo\|time` | What kaniko mode compares between snapshots, as in kaniko; ignored, with a warning, in overlay mode. |

## ranikust's own flags

| Flag | Meaning |
|---|---|
| `--mode=auto\|overlay\|kaniko` | How to snapshot: `auto` (default) probes the environment and picks the fastest mode that works ([How ranikust builds](how-it-builds.md)). |
| `--workdir=DIR` | The work directory. |
| `--state-dir=DIR` | Where ranikust keeps its store; kaniko's `--kaniko-dir`. Default `<workdir>/.ranikust` in overlay mode, `/ranikust/state` in kaniko mode. |
| `--jobs=N` | Stages built at once. Default: the number of CPUs available. |
| `--network=host\|none` | The network `RUN` gets. Default `host`. |
| `--platform=linux/amd64` | The image platform; kaniko's `--custom-platform`. Only `linux/amd64` for now. |
| `--metrics-file=PATH` | Write the build metrics report there; takes the place of `RANIKUST_METRICS_FILE`. |
| `--secret=id=ID,src=PATH` | A file for `RUN --mount=type=secret,id=ID`, bound read-only into that `RUN` and kept out of every layer, the config and the logs; kaniko's syntax, file secrets only. Repeat for several. |

## kaniko flags it refuses

ranikust never ignores a kaniko flag silently. A kaniko flag it does not
take yet stops the run with exit code 2 and a line saying why, for every
such flag on the command line:

```text
ranikust: error: kaniko's --cache is not supported: the layer cache comes in Phase 4 (Q-021)
ranikust: error: kaniko's --kaniko-dir is not supported: renamed in ranikust: use --state-dir
```

- **Renamed:** `--kaniko-dir` is `--state-dir`; `--custom-platform` is
  `--platform`.
- **Later:** `--cache` and `--cache-repo` (the layer cache), `--ignore-path`
  (kaniko mode), `--git`, `--registry-mirror`,
  `--registry-map` and `--credential-helpers`.
- **Not planned:** `--bucket` (`-b`), GCS build contexts.
- **Undecided:** the remaining kaniko flags, among them `--force`,
  `--cache-dir`, `--verbosity`, `--label`, `--image-format`,
  `--digest-file`, `--insecure`, `--insecure-pull` and `--skip-tls-verify`; and kaniko's
  deprecated spellings, which the message points to the current name.

The `KANIKO_*` environment variables (`KANIKO_NO_PUSH`,
`KANIKO_REGISTRY_MIRROR`, `KANIKO_METRICS_FILE`, …) are not read.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success, including `--help` and `version`. |
| 1 | An error while running (a `RUN` that fails, a missing `COPY` source, an instruction of a later phase), or a kernel ranikust cannot use. |
| 2 | A usage error: an unknown or refused flag, a bad value, an invalid combination such as no `--destination` without `--no-push`. |
