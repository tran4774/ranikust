# Command line

> **ranikust is pre-alpha.** It builds a Dockerfile as root: in overlay mode on
> a host with `sudo`, or in a `--privileged` container with a volume at its
> state directory `/var/lib/ranikust`; where root may not mount (Docker's
> default, most pods) it picks kaniko mode, which builds on the container's
> `/`. Elsewhere it stops with exit code 1, the step that failed and what to do
> instead. It prints the image digest on stdout. Instructions of later phases
> (`ADD` of a URL or an archive, `COPY --link`, `RUN --mount=type=bind`,
> `ONBUILD`…) stop the build with a message naming the phase.

Where a Dockerfile most likely says something it did not mean, ranikust stops
instead of guessing ([ADR-0052]):

- A `COPY` or `ADD` source that expands to an empty string (an unset `ARG`) is
  refused; BuildKit copies the whole context. Write `.` for that.
- `COPY --from=<name>`, where `<name>` is no stage and a bare word, such as a
  mistyped stage name, is refused; BuildKit pulls an image of that name from
  Docker Hub. Name an image with a tag, a digest or a registry:
  `--from=busybox:latest`, `--from=docker.io/library/busybox`.
- As in BuildKit, an octal `COPY --chmod` applies to the directories it copies
  and creates too: `--chmod=0640` leaves them without `x`, so only root can
  enter them. Use a symbolic mode (`--chmod=go+rX`) to keep directories
  traversable.

A step that changes nothing (`RUN true`, a `WORKDIR` that exists, a `COPY`
whose wildcard matches nothing) adds no layer, only its history entry, as
kaniko; BuildKit adds an empty layer ([ADR-0058]).

## Running it

Without a subcommand, ranikust builds, as kaniko's `executor` does, so the
flags of a kaniko job carry over:

```sh
ranikust --context=/workspace --dockerfile=Dockerfile \
  --destination=registry.example.com/team/app:1.0
```

`ranikust build …` is the same thing spelt out. `ranikust version` (or
`--version`) prints the version, and `ranikust --help` lists every flag.
`ranikust prune --all` empties a state directory ([below](#emptying-the-state-directory)).
`push` does not exist yet; there is no `login`: ranikust reads existing
credentials and never writes them.

Before anything else, ranikust checks that the kernel is Linux 5.8 or later
and that `openat2` and `statx` are allowed, and stops with a message naming
the kernel if not (see [Choosing where to run it](where-to-run.md)). `version`
and `--help` work on any kernel.

### Emptying the state directory

```sh
ranikust prune --all                  # ~/.local/share/ranikust
ranikust prune --all --state-dir=DIR
```

As a user other than root, `prune --all` empties the state directory, files a
build's user namespace left under the user's subordinate ids included, which
the user cannot remove otherwise; `rmdir` then removes the directory. It
removes nothing while a build uses the directory, refuses a directory
ranikust did not make (one without its `.ranikust-state` marker, or the
`.ranikust-pruning` an interrupted prune leaves, or holding anything other
than what ranikust puts there), leaves a missing or empty one as it is,
and prints what it did. If it is interrupted or cannot remove everything,
builds refuse the directory until it is emptied, saying how: `prune --all`
again for a user, root's commands below for root; where `prune --all`
cannot finish (a mount below the directory, no subordinate ids for the
user), its error says how to get out, naming the directory.
Root is refused, a container's root included: it empties its state directory
with ordinary tools while no build uses it and nothing is mounted below it,
the marker first, so that no build takes what an interrupted removal leaves
([ADR-0105]), then removes it with `rmdir`. The emptying needs GNU
findutils (Alpine: `apk add findutils`): without it, it stops before
changing anything. The commands enter the directory once, check that it
is the one ranikust checked, by its device, inode and owner, and work
only inside it; `flock` takes the lock builds hold, so they fail while
one runs. The error gives them with the directory, its parent, its name
and its identity in place of `'DIR'`, `'PARENT'`, `'./NAME'` and `ID`:

```sh
( CDPATH= cd -P -- 'DIR' && stat -c %d:%i:%u . | grep -qxF ID &&
  exec flock -xn . sh -c 'find --version 2>/dev/null | grep -q "GNU findutils" ||
  { echo "needs GNU findutils (Alpine: apk add findutils)" >&2; exit 1; };
  { test -f .ranikust-state &&
  ! test -h .ranikust-state && rm .ranikust-state || { test -f .ranikust-pruning &&
  ! test -h .ranikust-pruning && rm .ranikust-pruning; }; } &&
  find . -xdev -mindepth 1 -delete' )
( CDPATH= cd -P -- 'PARENT' && stat -c %d:%i:%u './NAME' | grep -qxF ID &&
  rmdir -- './NAME' )
```

The error gives no commands for an empty directory (ranikust removes
only one that holds its marker), for a path that ends in `..` (give the
directory's own path), or for a path ranikust cannot check as given (a
symlink on it, say): give the path with no symbolic link on it, or, for
a user's directory, run `ranikust prune --all` as that user.

Without `--all`, `prune` is a usage error: removing by size and age comes in
Phase 5.

## kaniko flags it accepts

These keep kaniko's names, short forms and meaning. Booleans work as in
kaniko: `--no-push` and `--no-push=true` turn it on, `--no-push=false` off;
`--no-push false` is an error.

| Flag | Meaning |
|---|---|
| `-c`, `--context=DIR` | The build context: a local directory, as a plain path. kaniko's `dir://` form is refused, naming the plain path ([ADR-0053], [ADR-0054]). Default `/workspace/`. A symlink in any component of the path is refused, since whoever owns the context could point one anywhere: pass the real path, as `realpath` prints it ([ADR-0076]). git, tar and HTTPS contexts come later; S3, GCS and Azure are not planned. |
| `--context-sub-path=DIR` | A sub-directory of a fetched context (git, tar or HTTPS, which come later; no `..`). Refused (exit code 2) with a local directory: pass the sub-directory itself as `--context` ([ADR-0054]). |
| `-f`, `--dockerfile=PATH` | The Dockerfile. Default `Dockerfile`. As in kaniko, the path as given when it exists, else inside the context. See [Reading the Dockerfile](#reading-the-dockerfile). |
| `--build-arg=KEY=VALUE` | A build argument; `KEY` alone takes its value from the environment. Repeatable. |
| `--target=STAGE` | The stage to build and produce. One only: kaniko's list (comma-separated or repeated) is refused, since the stages after the first would be built for nothing. Default: the last stage. |
| `-d`, `--destination=IMAGE` | An image to push to. Repeatable. Required unless `--no-push`. |
| `--no-push` | Do not push. |
| `--tar-path=PATH` | Also write a Docker `save` tarball. See [Where outputs go](#where-outputs-go). |
| `--oci-layout-path=PATH` | Also write an OCI image layout. See [Where outputs go](#where-outputs-go). |
| `--compression=gzip\|zstd` | Layer compression, gzip by default. zstd cannot go into a `--tar-path` tarball. |
| `--compression-level=N` | gzip `-1` (6) to `9`, zstd `1` to `19`. gzip `-2` and zstd `20` to `22`, which kaniko takes, are refused. Default: ranikust's (gzip 6, zstd 3). |
| `--image-format=docker\|oci` | The image's media types. Default `oci`, as BuildKit; `docker` cannot go with `--compression=zstd`. Unlike kaniko, `oci` can go with `--tar-path`. |
| `--reproducible` | Fixed timestamps, for byte-identical images: the image's time is `SOURCE_DATE_EPOCH`, else 1970-01-01, and file times later than it are set to it, as BuildKit does. |
| `--label=KEY=VALUE` | A label on the image built (the pushed stage only), set before its first instruction, so a `LABEL` of the same key in that stage wins. Repeatable. |
| `--annotation=KEY=VALUE` | An annotation on the image manifest. Repeatable. The base image's annotations are not carried over. |
| `--digest-file=PATH` | Write the image digest (`sha256:…`, no newline), also with `--no-push`. |
| `--image-name-with-digest-file=PATH` | Write `repository@digest`, one line per destination; needs `--destination`. |
| `--image-name-tag-with-digest-file=PATH` | Write `repository:tag@digest`, one line per destination (`latest` when no tag); needs `--destination`. Given with the previous flag, both files get this text, as in kaniko. |
| `--registry-certificate=REGISTRY=PATH` | An extra CA certificate (a PEM file) for one registry. Repeatable. `PATH` must name a file that you own and trust, never one inside a build context you do not trust: ranikust reads it before any transfer, as the user that runs it, following symlinks and checking neither type nor size, so a symlink in a context could make it open any file it can read, and a certificate from a context would let the context's author vouch for that registry. A relative path starts at the working directory. |
| `--insecure-registry=REGISTRY` | Reach this registry (`host[:port]`) over plain HTTP when HTTPS fails, as kaniko. Repeatable. A loopback registry (`localhost`, `127.0.0.0/8`, `[::1]`) is plain HTTP without it, as BuildKit. |
| `--insecure` | Destinations may be plain HTTP; their credentials may then be sent over it. |
| `--insecure-pull` | The registries of `FROM` and `COPY --from` images may be plain HTTP. They are reached anonymously over it: a Dockerfile names them, and ranikust sends credentials over plain HTTP only to registries you name (`--insecure-registry`, or destinations under `--insecure`). |
| `--skip-tls-verify` | Do not verify the TLS certificates of destinations. |
| `--skip-tls-verify-pull` | Do not verify the TLS certificates of the registries pulled from. |
| `--skip-tls-verify-registry=REGISTRY` | Do not verify this registry's TLS certificate. Repeatable. These insecure and TLS settings apply per registry, so a registry both pushed to and pulled from gets both. In kaniko mode on `/`, credentials go only over verified TLS: never over plain HTTP, even to a registry you named, nor to one whose certificate is not verified, since a `RUN` could redirect its name. |
| `--push-retry=N` | Start the whole push again up to N times after it failed, waiting 1 s, then twice as long each time, as kaniko. Each request keeps ranikust's own retries inside. |
| `--image-download-retry=N` | The same for fetching a base image's manifest. |
| `--snapshot-mode=full\|redo\|time` | What kaniko mode compares between snapshots, as in kaniko; ignored, with a warning, in overlay mode. kaniko's `--snapshotMode` is refused, naming this flag. |
| `--ignore-path=PATH` | An absolute path kaniko mode keeps out of every layer, as in kaniko; each part may be a glob, so `/var/cache/*` keeps what is in `/var/cache` but not the directory. Repeatable. Refused: a relative path, the whole root (`/` or a path that cleans to it), a malformed pattern, and `--ignore-path`s that together take every top-level entry of an image, such as `/*` or `/??*` (a best-effort guard against typos: `/[a-z]*` still passes, since it only widens what is kept out). Refused in overlay mode, whose layers never hold mounts. |
| `-v`, `--verbosity=LEVEL` | `trace`, `debug`, `info` (default), `warn` (or `warning`), `error`, in any case; `fatal` and `panic` log as `error`. Other programs' libraries log at `info` at most. Never changes the metrics report. |
| `--log-format=text\|color\|json` | As kaniko's: `color` (default) colors the level and counts seconds since start (`INFO[0005] message`); `text` writes `time=… level=… msg=…`; `json` one object per line with `level`, `msg` and `time`. Times are UTC. Control characters in a message, from a file name in an image or the context for example, are shown escaped (`\n`, `\u{1b}`), as in the `ranikust: error:` line; a `RUN`'s own output is copied as it is. |
| `--log-timestamp` | In the `color` format, the full time instead of the seconds since start. |
| `--cache=false` | Accepted: no layer cache is what this version does. `--cache` itself comes later. |
| `--skip-push-permission-check`, `--preserve-context` | Accepted: ranikust makes no write check before the build, and never changes or deletes the context. In kaniko mode on a container's `/`, a context inside the root is refused unless it is kept out of the layers (a volume, `/ranikust`). |
| `--single-snapshot` | Not offered, in any mode and whatever its value: one snapshot at the end saves a few seconds at most and loses the layer per step that a cached rebuild reuses ([ADR-0065]). Drop it. |
| `--materialize` | Refused until the layer cache, the only place it has an effect, as `--cache` ([ADR-0064]). With the cache (Phase 4) it is accepted in kaniko mode on a container's `/` only, as in kaniko, and refused in the other modes, whose stage roots are deleted at the end ([ADR-0114]). |

### Reading the Dockerfile

- A path that leads into the context (`$CI_PROJECT_DIR/Dockerfile`, or the
  default from the checkout) is read through the context, so a symlink there
  never leaves it.
- Outside the context, a symlink is followed only when root (or ranikust's
  user) owns it and the directory it is in, and no other account may write that
  directory. A root-owned directory on a read-only mount counts as not
  writable. A link another account made or could have placed is refused.
- A ConfigMap mount is expected to work, its root being root's and read-only
  [inferred: from kubelet's source, not yet run on a cluster]. Mounting the
  Dockerfile with `subPath`, a plain file, always does.
- A Dockerfile outside the context, and the `.dockerignore` beside it, is
  refused when it is a hardlink owned by another account than its directory, or
  a file the directory's owner could not read.
- Only a regular file is read, so `--dockerfile=/dev/stdin` does not work:
  write the Dockerfile to a file first.
- The path is walked one name at a time without triggering automounts, so a
  path through an autofs mount point that is not mounted yet fails: list it
  once (`ls`) or pass the mounted path. A symlink's text is walked in
  ranikust's own mount namespace, so `/proc/<pid>/root/...` reaches ranikust's
  own `/`, not that process's.
- An unknown keyword is shown in the error only when it is within two edits of
  an instruction, with a "did you mean" suggestion; otherwise nothing of it is
  shown ([ADR-0076]).

### Where outputs go

Every output (`--tar-path`, `--oci-layout-path`, the three digest files,
`--metrics-file`) is refused before the build when it lies in the build
context, whose owner controls every name there, or when any component of its
directory is a symlink. Write it beside the context, for a CI artifact for
example `--context=$CI_PROJECT_DIR/app --digest-file=$CI_PROJECT_DIR/digest`
([ADR-0076]).

- `--tar-path` is written to a temporary file renamed over `PATH`, which
  replaces only a regular file: a device node, FIFO, socket or symlink there is
  refused, and so is a directory on `/proc` or `/sys`. (An entry another
  writer of that directory swaps in during the write may be replaced, but is
  never followed.) So
  `--tar-path=/dev/stdout` does not work: write the tarball to a file and
  stream that (`ranikust … --tar-path=/tmp/image.tar && cat /tmp/image.tar`).
- The three digest files are local paths (a missing directory is made `0700`),
  written in place, so a mounted file such as a Kubernetes termination log
  keeps working. An existing file must be a regular file with no other hard
  link. kaniko's upload to an `https://` URL is not supported.

## ranikust's own flags

| Flag | Meaning |
|---|---|
| `--mode=auto\|overlay\|kaniko` | How to snapshot: `auto` (default) probes the environment and picks the fastest mode that works: overlay mode where root may mount, kaniko mode where it may not, and, outside a container where overlayfs refuses the state directory (NFS, overlayfs), kaniko mode with a directory per stage ([How ranikust builds](how-it-builds.md)). As root it also logs whether a `RUN` could run in a user namespace here (`user namespace probe (reported only)`), for rootless overlay mode, which is not built yet. |
| `--state-dir=DIR` | Where ranikust keeps its store and every build root it makes; kaniko's `--kaniko-dir`. Default `/var/lib/ranikust` as root, else `~/.local/share/ranikust` (a user other than root cannot build yet). It must be new, empty, or a state directory ranikust made (it holds `.ranikust-state`; `/` never), owned by the user running ranikust and writable by no one else, below directories only root or that user may write (root's sticky `/tmp` is fine); ranikust makes it mode `0700`. The default is refused where `/home` or `~/.local` is a symlink (no directory on the path may be one; Fedora Atomic links `/home` to `/var/home`) or `~/.local` or `~/.local/share` is writable by group or others: pass `--state-dir` then. In a container, overlay mode needs it on a volume. In kaniko mode on a container's `/` it must be new or empty: that mode keeps nothing there between builds, refuses a state directory another build uses, and never shares one with overlay-mode builds ([ADR-0077]). There is no work-directory flag: ranikust builds on `/` in a container in kaniko mode, and everywhere else in directories it makes here ([ADR-0050]). |
| `--jobs=N` | Stages built at once, and blobs uploaded at once. Default: the number of CPUs available. In kaniko mode on a container's `/` the stages run one at a time and only the uploads follow it. |
| `--snapshot-workers=N` | Threads of kaniko mode's filesystem walk. Default 8, the fastest measured; ignored, with a warning, in overlay mode. |
| `--network=host\|none` | The network `RUN` gets. Default `host`. `none` is refused in kaniko mode on a container's `/`, where a `RUN` gets no network namespace of its own. `none` isolates the `RUN` only: ranikust's own registry requests use the network ranikust runs in, including the token service a registry names, which a Dockerfile-named registry can point at an internal address. |
| `--platform=linux/amd64` | The image platform. Only `linux/amd64` for now. kaniko's `--custom-platform` and `--customPlatform` are refused, naming this flag. |
| `--metrics-file=PATH` | Write the metrics report (JSON) there when ranikust exits, also after an error; works with `version` too. The only way to ask for it: `RANIKUST_METRICS_FILE` is not read (a warning says so). Like every output, it must lie outside the context and be reached without a symlink ([Where outputs go](#where-outputs-go)); otherwise ranikust refuses before anything else runs (exit code 2), and so is a build asking for a report whose `--context` cannot be opened, since the report could not be kept out of it. |
| `--secret=id=ID[,src=PATH\|,env=VAR]` | A secret for `RUN --mount=type=secret,id=ID`: a file (`src=`, `type=file`) or a variable of ranikust's environment (`env=`, or `type=env` with `src=VAR`); without either, the variable named ID if it is set, else the file ID. kaniko's and buildctl's syntax. Repeatable. Read once, at the start of the build, and kept in memory: a change to the file during the build is not seen. Kept out of every layer, the config and the logs; at most 500 KiB. In kaniko mode on a container's `/`, every `RUN` can read a file secret and copy it into the image or anywhere else, and ranikust does not stop it ([ADR-0084]); pass the secret in the environment there if no `RUN` should read it. |
| `--allow=network.host` | Lets a `RUN --network=host` lift `--network=none`, as BuildKit's entitlement. |
| `--allow=network.none-userns` | Where ranikust lacks `CAP_NET_ADMIN` (a container with only `SYS_ADMIN` added), lets `RUN --network=none` get loopback through a user namespace; without it such a `RUN` is refused. A root `RUN` there holds `CAP_NET_ADMIN` over its own network namespace. A seccomp filter closes the riskiest parts (joining or creating user namespaces, `io_uring`, most netlink families, netfilter and IPsec socket options, `TCP_REPAIR`; the full list is in [ADR-0033]), but the rest of what `CAP_NET_ADMIN` allows stays open: `ip link add type <kind>` can load a kernel module on the host. |
| `--seccomp=default\|unconfined\|FILE` | The seccomp profile every `RUN` runs under, as `docker run --security-opt seccomp=`. `default` is Docker's own (moby/profiles v0.2.4), which applies without the flag too, as under `docker build`; `unconfined` turns it off; a file is a profile in Docker's JSON format (`./default` for a file named `default`). A file that cannot be read, is not a regular file, is over 1 MiB or asks for what ranikust does not support (`SCMP_ACT_NOTIFY`, `SCMP_ACT_TRACE`, a listener, an unknown flag, two conditions on one argument, a rule with no names, an `errnoRet` or `defaultErrnoRet` of 0, which would answer a call with a success it never had, or one from 512 up, where the kernel keeps its own codes, of which 516 can turn a refusal into a success) is refused before the build starts (exit code 2). A profile that refuses `exit` and `exit_group`, or every call, with an errno leaves the build running until it is cancelled; one that kills or traps a call the `RUN`'s init makes (`SCMP_ACT_KILL_PROCESS`, `SCMP_ACT_KILL_THREAD`, `SCMP_ACT_TRAP`) fails the build with "the RUN's init was killed by signal 31 (SIGSYS)", naming the profile as the likely cause, not with an exit code of the command, which may not have run; one that refuses the init's wait (`wait4`) with an errno fails it with "the RUN's init exited with code 125", the init's own code, never the command's, which may have started; one that kills or traps only a call the command makes before it starts (`setsid`, the `USER` switch) fails it as "the command returned a non-zero code: 159" (128 + `SIGSYS`), as a command killed after it started does. The limits of a profile's rules are in the [threat model]. Only the native architecture's calls are allowed: a 32-bit program in a `RUN` gets `EPERM` and needs `--seccomp=unconfined`; a call too new for the table ranikust was built with gets `ENOSYS`. Where a `RUN` cannot get a session keyring of its own (an LSM or a filter around ranikust refuses it) while ranikust's own can still be looked up, a profile that lets `keyctl`, `add_key` or `request_key` run, `unconfined` included, fails the `RUN`; under Docker's default profile around ranikust, which refuses `keyctl`, the `RUN` goes on. Refused in kaniko mode on a container's `/` (exit code 2), where a `RUN` runs under the container's own profile: set that one with the container ([ADR-0109]). |
| `--limit=NAME=VALUE` | Changes a bound on what a registry or an image can make ranikust hold or wait for; see [Limits](#limits). Repeatable; the last of one name wins. |

### Limits

A registry or an image a Dockerfile names could otherwise make ranikust hold
gigabytes in memory or wait forever. These bounds apply by default;
`--limit NAME=VALUE` changes one. Sizes are a number of bytes, or a number
with `KiB`, `MiB` or `GiB`; durations a number with `s`, `m` or `h`. A value
must be above 0 and at most `1GiB` (sizes) or `24h` (durations); there is no
"unlimited": name a larger number within that.

| Name | Default | Bounds |
|---|---|---|
| `tar-header` | `1MiB` | each GNU long name, GNU long link or PAX record set in a layer, as Go's `archive/tar` (kaniko, BuildKit) |
| `manifest` | `4MiB` | a manifest or index, refused while it arrives (distribution's own limit) |
| `registry-body` | `1MiB` | a token response; an error body is read no further |
| `config` | `8MiB` | an image config, checked before it is pulled |
| `registry-idle` | `2m` | how long a registry may send nothing before the request fails; it is then retried. A slow but steady transfer is never cut off. |

A build that hits a limit fails with a message that names it, for example
`manifest ... is larger than 4 MiB (limit "manifest"); raise it with --limit
manifest=<size>`. The limits in force are in the metrics report, under
`options.limits`.

## kaniko flags it refuses

ranikust ignores no kaniko flag. One it does not take, including those that
would change nothing, stops the run with exit code 2 and a line saying why,
for every such flag on the command line ([ADR-0049]):

```text
ranikust: error: kaniko's --cache is not supported: the layer cache comes in Phase 4 (Q-021)
ranikust: error: kaniko's --kaniko-dir is not supported: renamed in ranikust: use --state-dir
```

- **Renamed:** `--kaniko-dir` is `--state-dir`; kaniko's old spellings
  `--tarPath`, `--snapshotMode`, `--customPlatform` and `--custom-platform`
  are `--tar-path`, `--snapshot-mode` and `--platform`.
- **`/var/run`:** `--ignore-var-run` and `--whitelist-var-run`. kaniko mode
  keeps `/var/run` out of layers only where it is a mount point, as every
  mount point, so an image's `/var/run -> /run` link is kept ([ADR-0052]).
- **Never on this machine's `/`:** `--force`. Outside a container kaniko mode
  builds in a directory of the state directory instead.
- **Change nothing in ranikust:** `--cache-dir` (the warmer is not planned;
  base images are pulled into the store, or in kaniko mode streamed from the
  registry straight into the root), `--use-new-run`, `--cleanup`,
  `--pre-cleanup` (ranikust decides what to empty itself),
  `--image-fs-extract-retry` (use `--image-download-retry`; a base layer
  streamed in kaniko mode fails the build if its download breaks after its
  first bytes, since they are already extracted), `--force-build-metadata`,
  `--skip-unused-stages` (deprecated in kaniko). Drop them. `--cache=false` is
  accepted: it asks for what ranikust does.
- **With the layer cache (Phase 4):** `--cache`, `--cache-repo`, `--cache-ttl`,
  `--cache-copy-layers`, `--cache-run-layers`, `--compressed-caching` (`=true`
  only: `=false` is refused, since ranikust streams layers and spends no memory
  to save), `--no-push-cache`.
- **With registry mirrors, external credential helpers and git contexts
  (Phase 5):** `--git`, `--registry-mirror`, `--registry-map`,
  `--skip-default-registry-fallback`, `--registry-client-cert`,
  `--push-ignore-immutable-tag-errors`.
- **Backlog:** `--credential-helpers`, for built-in cloud helpers
  ([#138](https://github.com/tran4774/ranikust/issues/138)). External
  `docker-credential-*` helpers come in Phase 5; until then no helper runs.
- **Not scheduled:** `--dryrun`.
- **Not planned:** `--bucket` (`-b`), `--azure-container-registry-config` and
  kaniko's `bake`.

## Environment variables

- `DOCKER_AUTH_CONFIG` holds a `config.json`'s `auths` inline (nothing else:
  no `credsStore` or `credHelpers`). It is asked before the file
  (`$DOCKER_CONFIG/config.json` or `~/.docker/config.json`), as in kaniko.
- kaniko's variables are not read, and not warned about ([ADR-0049]):
  `KANIKO_NO_PUSH` does not set `--no-push` (pass the flag),
  `KANIKO_REGISTRY_MIRROR` and `KANIKO_REGISTRY_MAP` change nothing (images
  come from the registry they name), `KANIKO_DIR` is `--state-dir`, and
  `KANIKO_METRICS_FILE` is `--metrics-file`.

kaniko's feature flags (`FF_KANIKO_*`) are not read: ranikust has one
behaviour, which for most of them is what the flag turns on.

| ranikust | kaniko's feature flags |
|---|---|
| Behaves as if it were on | `COPY_AS_ROOT` (copied context files are owned by 0:0 without `--chown`), `BUILDKIT_ARG_ENV_PRECEDENCE`, `REPRODUCIBLE_PRESERVE_BASE_LAYERS`, `REPRODUCIBLE_PRESERVE_FORMAT` (always OCI), `NO_PROPAGATE_ANNOTATIONS`, `PRESERVE_HARDLINKS`, `SECUREJOIN_EXTRACTION`, `CONFINE_COPY_SOURCE`, `CROSS_REPO_MOUNT`, `SKIP_WRITE_WHITEOUTS`, `UNPACK_ZSTD`, `EXPAND_HEREDOC`, `CHOWN_ON_IMPLICIT_DIRS` (`WORKDIR` parents get the user), `COPY_CHMOD_ON_IMPLICIT_DIRS`, `RELATIVE_LINK_TARGETS`, `RUN_HONOR_GROUP`, `VOLUME_SKIP_MKDIR`, `OCI_SCRATCH_BASE`, `SCOPED_DOCKERIGNORE`, `SCOPED_REGISTRY_CERTIFICATES`, `SKIP_RELABEL_RECOMPRESS` (a relabelled base layer keeps its bytes) |
| ranikust does it its own way | `COPY_SKIP_SPECIAL_FILES`: device nodes and FIFOs are recreated, never read, and a socket becomes an empty file, as BuildKit. `RUN_VIA_TINI`: each `RUN` runs as PID 2 under ranikust's own init (`ranikust-init`), which reaps what it leaves and dies with ranikust, taking the whole namespace with it. `NATIVE_COPY`: ranikust has one copy, which keeps hardlinks. |
| Comes later with the feature | `ADD_CHECKSUM`, `ADD_UNPACK`, `COPY_LINK`, `RUN_MOUNT_BIND`, `UNPACK_XZ`, `UNTAR_SKIP_ROOT` (local `ADD` archives); `PATH_SCOPED_REGISTRY_AUTH` (with credential helpers); the cache flags (`CACHE_*`, `*_CACHE_KEY`, `SKIP_CACHED_STAGES`, `DEFER_CACHE_PUSH`, `IGNORE_CACHED_MANIFEST`, `DEPRECATE_LAYERLESS_CACHE_ENTRIES`, `IMAGE_STAGES`, `HASH_DIR_FRAMING`, `CACHE_HASH_BLAKE3`); kaniko mode's `PRESERVE_MOUNTED_PATHS`, `PRESERVE_MOUNTED_SYMLINKS` |
| Not applicable | Performance switches where ranikust's design is the default (`PARALLEL_*`, `PIPELINED_EXTRACT`, `STREAM_TAR_COMPRESS`, `SHARED_*`, `STAT_HASH_CACHE`, `FAST_*`, `POOL_REGISTRY_CONNECTIONS`, `PEEK_ARCHIVE_HEADER`, `PRECOMPILE_DOCKERIGNORE`, `CONTEXT_HASH_PARALLEL`); `DISABLE_HTTP2`; `LAYER_HINTS`; kaniko's own directory (`CLEAN_KANIKO_DIR`, `DEPRECATE_INTER_STAGE_RESTORE`); the warmer (`OCI_WARMER`, `WARMER_CACHE_LOCK`) |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success, including `--help` and `version`. |
| 1 | An error while running (a `RUN` that fails, a missing `COPY` source, an instruction of a later phase), or a kernel ranikust cannot use. |
| 2 | A usage error: an unknown or refused flag, a bad value, an invalid combination such as no `--destination` without `--no-push`. |

[ADR-0033]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0033-network-none-loopback-through-a-user-namespace.md
[ADR-0049]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0049-security-and-performance-over-kaniko-compatibility.md
[ADR-0050]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md
[ADR-0052]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0052-push-input-aliases-and-issue-mining-fixes.md
[ADR-0053]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0053-local-context-is-a-plain-path-no-dir-scheme.md
[ADR-0054]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md
[ADR-0058]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0058-a-step-that-changes-nothing-adds-no-layer.md
[ADR-0064]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0064-materialize-refused-until-the-layer-cache.md
[ADR-0065]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0065-single-snapshot-refused-for-good.md
[ADR-0076]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0076-the-build-context-owner-is-a-lower-trust-principal.md
[ADR-0077]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md
[ADR-0084]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md
[ADR-0105]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md
[ADR-0109]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md
[ADR-0114]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md
[threat model]: https://github.com/tran4774/ranikust/blob/main/docs/security/threat-model.md#6-accepted-risks
