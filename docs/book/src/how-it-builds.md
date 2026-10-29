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
directory, which builds running at the same time share. `--state-dir` sets
it; the default depends on the mode
([ADR-0022](https://github.com/tran4774/ranikust/blob/main/docs/adr/0022-common-state-dir.md)).

## Overlay mode

Used when ranikust may mount filesystems: as root on a host or VM, in a
privileged container, or as a regular user who may create user namespaces.

- Each stage gets its own overlay filesystem inside the work directory. Each
  instruction writes into a fresh layer of it, so the changes are known
  without scanning the whole filesystem.
- `RUN` commands run in a sandbox: their own mount, process, hostname and IPC
  namespaces, the stage's root filesystem as their `/`, and the capabilities
  that Docker gives a container by default. The isolation is about that of
  `docker build`
  ([ADR-0003](https://github.com/tran4774/ranikust/blob/main/docs/adr/0003-dockerfile-trust-and-default-isolation.md)).
- Stages that do not depend on each other build in parallel, up to `--jobs`
  at a time, which defaults to the number of CPUs available to ranikust
  ([ADR-0013](https://github.com/tran4774/ranikust/blob/main/docs/adr/0013-jobs-default-available-parallelism.md)).
- As a regular user, the same happens inside a user namespace: root inside
  the build is your own user outside it. This is called *rootless* overlay
  mode.
- The state directory defaults to `.ranikust` inside the work directory.
- kaniko's `--snapshot-mode` is accepted and ignored with a warning: there
  is no filesystem walk to tune.

## kaniko mode

Used when ranikust runs as root but may not mount, as in a Docker container
with the default security profile or in most Kubernetes pods.

- It works the way kaniko does. The build root is the container's own `/`:
  base image layers are unpacked there, `RUN` commands run there, and changes
  are found by walking and hashing the filesystem after each instruction.
  Between stages, the root filesystem is emptied.
- `RUN` commands run in the same container as ranikust, without extra
  isolation. The container is the only boundary, as with kaniko. Run it in a
  container made for the build.
- Stages build one after another. The `--parallel-stages` option builds
  independent stages in parallel, each in its own directory, at a cost:
  `RUN` commands then have no `/proc`, and some tools fail or quietly
  misbehave without it
  ([ADR-0014](https://github.com/tran4774/ranikust/blob/main/docs/adr/0014-kaniko-mode-sequential-by-default.md)).
- If ranikust can mount but overlayfs is unusable (for example, a work
  directory on NFS), kaniko mode keeps one directory per stage and the full
  `RUN` sandbox of overlay mode.
- ranikust keeps its own files in `/ranikust/state`, as kaniko does in
  `/kaniko`. That directory is never snapshotted or emptied. `--state-dir`
  moves it.
- `--snapshot-mode=full|redo|time` means what it means in kaniko: what the
  walk compares to find changed files. The default is `full`.

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
