# How ranikust builds

> This page describes the design. None of it is implemented yet.

A Dockerfile build runs each instruction of each stage in a root filesystem
and records what the instruction changed as an image layer. ranikust can do
this in two ways, called *modes*. At startup it tries, step by step, what the
environment allows, and picks the mode that works
([ADR-0002](https://github.com/tran4774/ranikust/blob/main/docs/adr/0002-two-snapshot-modes.md),
[ADR-0004](https://github.com/tran4774/ranikust/blob/main/docs/adr/0004-support-every-privilege-level.md)).
It logs which mode it chose and why. To skip the choice, pass
`--mode=overlay` or `--mode=kaniko`; ranikust still checks that the mode
works and stops with the reason if it does not
([ADR-0023](https://github.com/tran4774/ranikust/blob/main/docs/adr/0023-mode-flag.md)).

ranikust keeps downloaded and unpacked layers between builds in a state
directory, `/var/lib/ranikust` unless `--state-dir` names another, which
builds running at the same time share. Every root a build fills lies in
it, except the container's own `/` in kaniko mode: you never point
ranikust at a directory to fill and empty
([ADR-0050](https://github.com/tran4774/ranikust/blob/main/docs/adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md)).
The state directory is private: owned by the user running ranikust, mode
`0700`, below directories only root may write, since the image's setuid
programs land there.

## Overlay mode

Used when ranikust may mount filesystems: as root on a host or VM, in a
privileged container, or as a regular user who may create user namespaces.

- Each stage gets its own overlay filesystem inside the state directory. Each
  instruction writes into a fresh layer of it, so the changes are known
  without scanning the whole filesystem.
- `RUN` commands run in a sandbox: their own mount, process, hostname and IPC
  namespaces, the stage's root filesystem as their `/`, and the capabilities
  that Docker gives a container by default. The isolation is about that of
  `docker build`
  ([ADR-0003](https://github.com/tran4774/ranikust/blob/main/docs/adr/0003-dockerfile-trust-and-default-isolation.md)).
  The host name is always `ranikust`. `/etc/hosts` is the one ranikust
  itself sees (so Kubernetes `hostAliases` keep working), or only loopback
  names with `--network=none`
  ([ADR-0031](https://github.com/tran4774/ranikust/blob/main/docs/adr/0031-run-host-name-and-hosts-file.md)).
  A `RUN --network=none` sees only a loopback interface, which is up. In a
  container without `CAP_NET_ADMIN` such a `RUN` stops the build, unless
  `--allow network.none-userns` lets ranikust use a user namespace for it,
  under a seccomp filter
  ([ADR-0033](https://github.com/tran4774/ranikust/blob/main/docs/adr/0033-network-none-loopback-through-a-user-namespace.md)).
  Otherwise `RUN` uses the network ranikust itself has, as kaniko's does
  ([ADR-0010](https://github.com/tran4774/ranikust/blob/main/docs/adr/0010-run-network-host-or-none.md)):
  it reaches services on the host and the cloud metadata service
  (`169.254.169.254`), which gives out the machine's or pod's cloud
  credentials. ranikust does not block it. Where builds run untrusted
  Dockerfiles, deny those addresses to the build with a network policy,
  require IMDSv2 with a hop limit of 1, or pass `--network=none` when the
  build needs no network.
- Stages that do not depend on each other build in parallel, up to `--jobs`
  at a time, which defaults to the number of CPUs available to ranikust
  ([ADR-0013](https://github.com/tran4774/ranikust/blob/main/docs/adr/0013-jobs-default-available-parallelism.md)).
- As a regular user, the same happens inside a user namespace: root inside
  the build is your own user outside it. This is called *rootless* overlay
  mode.
- In a container, the state directory must be a volume: overlay cannot
  use the container's own overlay root as an upper directory, and
  ranikust says so (`-v ranikust-state:/var/lib/ranikust`).
- kaniko's `--snapshot-mode` is accepted and ignored with a warning: there
  is no filesystem walk to tune.

## kaniko mode

Used when ranikust runs as root but may not mount, as in a Docker container
with the default security profile or in most Kubernetes pods.

> Built so far: sequential builds. Not yet: a
> stage that reads an earlier stage with `COPY --from`, `RUN --mount`,
> `--parallel-stages` and per-stage directories.

- It works the way kaniko does. The build root is the container's own `/`:
  base image layers are unpacked there, `RUN` commands run there, and changes
  are found by walking and hashing the filesystem after each instruction.
  Between stages, the root filesystem is emptied.
- `RUN` commands run in the same container as ranikust, without extra
  isolation. The container is the only boundary, as with kaniko. Run it in a
  container made for the build.
- What ranikust holds stays out of `RUN`'s reach even there: the image
  layers it pulls and writes are files without a name, its own process
  cannot be inspected through `/proc` (unless the container grants
  `CAP_SYS_PTRACE` and ranikust cannot take it away from `RUN`, in which
  case it refuses to build), and no process a `RUN` starts in the
  background outlives that `RUN`. Three things ranikust cannot hide, and
  warns about: a value on its own command line (a `--build-arg TOKEN=…`:
  pass secrets with `--secret`), credentials held in the environment of
  another process of the container (start ranikust as the container's
  first process, `exec` it, rather than from a shell that keeps them), and
  any file mounted into the container (pass registry credentials in
  `DOCKER_AUTH_CONFIG` rather than a mounted `config.json`).
  A container's own root filesystem cannot hold such files on most
  kernels, so they are kept in memory there, counted against the
  container's memory limit; mount a volume at `/var/lib/ranikust` to keep
  them on disk.
- Stages build one after another. The `--parallel-stages` option builds
  independent stages in parallel, each in its own directory, at a cost:
  `RUN` commands then have no `/proc`, and some tools fail or quietly
  misbehave without it
  ([ADR-0014](https://github.com/tran4774/ranikust/blob/main/docs/adr/0014-kaniko-mode-sequential-by-default.md)).
- If ranikust can mount but overlayfs is unusable (for example, a state
  directory on NFS), kaniko mode keeps one directory per stage and the full
  `RUN` sandbox of overlay mode.
- In a container the state directory `/var/lib/ranikust` lies inside the
  root, as kaniko's `/kaniko`; it and `/ranikust` (put the binary,
  context and outputs there) are never snapshotted or emptied. Only one
  build at a time uses the container's `/`.
- `--snapshot-mode=full|redo|time` means what it means in kaniko: what the
  walk compares to find changed files. The default is `full`.
- Outside a container, kaniko mode never builds on the machine's `/`: it
  makes a root for the build in its state directory,
  `<state>/builds/<id>/rootfs`, and removes it with the build; `RUN`
  there gets the full sandbox, so ranikust needs mount privileges there
  and refuses to build without them (run it in a container instead). The context, the
  outputs and secret files must not be inside the root.

## When no mode works

As a regular user where user namespaces are restricted (the default on
Ubuntu 24.04 and later), or in a pod that is neither root nor allowed user
namespaces, ranikust cannot build. It stops before doing anything and says
what would work: running as root, installing an AppArmor profile for the
binary, or running inside a container.

## Privilege levels at a glance

| You run ranikust as | Mode | What separates `RUN` from the machine |
|---|---|---|
| root on a host or VM | overlay | namespaces, a private root, reduced capabilities |
| root in a privileged container | overlay | the same, inside the container |
| a regular user allowed user namespaces | overlay, rootless | the same, inside a user namespace |
| root in a container that forbids mounts | kaniko | only the container |
| a regular user without user namespaces | none | ranikust refuses to build |

The [security policy](https://github.com/tran4774/ranikust/blob/main/SECURITY.md)
and the [threat model](https://github.com/tran4774/ranikust/blob/main/docs/security/threat-model.md)
give the details.

## Differences between the modes

The two modes build the same Dockerfile into images with the same meaning,
but not always the same bytes
([ADR-0012](https://github.com/tran4774/ranikust/blob/main/docs/adr/0012-accept-divergence-between-modes.md)):

- layer digests can differ between modes;
- when a `RUN` command writes to a file that has several hard links, overlay
  mode changes only the name that was written, while kaniko mode changes
  every name of the file. BuildKit behaves like overlay mode.

Within one mode, the same build gives the same image whatever the value of
`--jobs`.
