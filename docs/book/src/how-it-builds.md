# How ranikust builds

> This page describes what is built; where a part is not, it says so. Rootless
> overlay mode (Phase 3) is not built, nor is the AppArmor advice that goes
> with it ([status](status.md)).

A Dockerfile build runs each instruction of each stage in a root filesystem
and records what the instruction changed as an image layer. ranikust can do
this in two ways, called *modes*. At start-up it tries, step by step, what the
environment allows, picks the mode that works and logs which one and why
([ADR-0002], [ADR-0004]). `--mode=overlay` or `--mode=kaniko` skips the
choice; ranikust still checks that the mode works and stops with the reason if
it does not ([ADR-0023]).

ranikust keeps downloaded and unpacked layers between builds in a **state
directory**: `/var/lib/ranikust` unless `--state-dir` names another. Builds
running at the same time share it. Every root a build fills lies in it, except
the container's own `/` in kaniko mode: you never point ranikust at a
directory to fill and empty ([ADR-0050]). The directory is private: owned by
the user running ranikust, mode `0700`, below directories only root may write,
because the image's setuid programs land there.

## Overlay mode

Used where ranikust may mount filesystems: as root on a host or VM, or in a
privileged container. A regular user who may create user namespaces would get
it too (*rootless* overlay mode, planned for Phase 3, not built). Until then a
regular user is refused (see [When no mode works](#when-no-mode-works)).

- **Snapshots.** Each stage gets its own overlay filesystem in the state
  directory, and each instruction writes into a fresh layer of it, so the
  changes are known without scanning the filesystem.
- **`RUN` sandbox.** A `RUN` gets its own mount, process, hostname and IPC
  namespaces, the stage's root as its `/`, and the capabilities Docker gives a
  container by default: about the isolation of `docker build` ([ADR-0003]).
  The host name is always `ranikust`. `/etc/hosts` is the one ranikust itself
  sees, so Kubernetes `hostAliases` keep working, or loopback names only with
  `--network=none` ([ADR-0031]). ranikust itself runs from a copy of its
  binary that no `RUN` can write, sealed in memory where the kernel allows
  it, else an unnamed file in `TMPDIR`; without either it builds on from
  the installed binary with a warning ([ADR-0101]).
- **Network.** `RUN` uses the network ranikust has, as kaniko's does
  ([ADR-0010]). It reaches services on the host and the cloud metadata service
  (`169.254.169.254`), which gives out the machine's or pod's cloud
  credentials; ranikust does not block it. Where builds run untrusted
  Dockerfiles, deny those addresses with a network policy, require IMDSv2 with
  a hop limit of 1, or pass `--network=none` when the build needs no network.
  A `RUN --network=none` sees only a loopback interface, which is up. In a
  container without `CAP_NET_ADMIN` it stops the build, unless `--allow
  network.none-userns` lets ranikust use a user namespace for it, under a
  seccomp filter ([ADR-0033]).
- **Parallel stages.** Stages that do not depend on each other build in
  parallel, up to `--jobs` at a time (default: the CPUs available to
  ranikust, [ADR-0013]).
- **In a container** the state directory must be a volume (`-v
  ranikust-state:/var/lib/ranikust`): overlay cannot use the container's own
  overlay root as an upper directory, and ranikust says so.
- kaniko's `--snapshot-mode` is accepted and ignored with a warning: there is
  no filesystem walk to tune.

## kaniko mode

Used when ranikust runs as root but may not mount: a Docker container with the
default security profile, most Kubernetes pods. Changes are found by walking
and hashing the filesystem after each instruction, as kaniko does.
`--snapshot-mode=full|redo|time` means what it means in kaniko (what the walk
compares; default `full`). Built: sequential builds on a container's `/`,
multi-stage included, and a directory per stage elsewhere, built in parallel.
Refused for good: `--parallel-stages` ([ADR-0060]) and `--single-snapshot`
([ADR-0065]).

### On a container's `/`

ranikust works the way kaniko does. The build root is the container's own
`/`: base layers are unpacked there and `RUN` commands run there. Only one
build at a time uses `/`.

**Stages and the root**

- Stages build one after another. `--parallel-stages` is refused: a bare
  `chroot` per stage, the only way to give stages their own root without
  mounting, would let a `RUN` reach the other stages, the state directory and
  the credentials. Allow mounts to get overlay mode's parallel stages
  ([ADR-0060]).
- Between stages `/` is emptied. A second build in the same container empties
  it before its first base too, so its image never holds what the first left.
  The first build leaves a marker, `/ranikust/.built`; without it the first
  base is unpacked over what the container holds, as kaniko does, so the
  container's own tools stay ([ADR-0052]).
- The first build makes `/ranikust` (or sets it) mode `0700`: a non-root
  process in the container loses access to it, a binary at `/ranikust/bin`
  included. A later build empties everything but `/ranikust`, mounts and the
  ignore list. A credential file elsewhere on `/` (a
  `/root/.docker/config.json` in the image) is gone after a build: that build
  still has it, since ranikust reads credentials at start-up, but the next one
  has none. Keep credentials under `/ranikust`, on a mount, or pass them to
  each build.
- The state directory `/var/lib/ranikust` and `/ranikust` (put the binary, the
  context and the outputs there) are never snapshotted or emptied.
- `/var/run` is kept out of layers only where it is a mount point, so an
  image's `/var/run -> /run` link is kept. Where the container's own
  `/var/run` is such a link, a base layer that holds `var/run/.wh..wh..opq`
  without its own `var/run/` entry is refused: the opaque marker would act on
  a directory reached through a link, which extraction never follows.
- What a later stage reads of an earlier one (`COPY --from`, `RUN --mount
  from=`, `FROM <stage>`) is copied aside first into the build's directory
  under the state directory: for `COPY --from` only the paths it names. An
  image read by `COPY --from=<image>` is unpacked there, never into `/`.
- `COPY` leaves device nodes, FIFOs and sockets out, as base layers are
  extracted there ([ADR-0060]).

**What `RUN` gets.** `RUN` commands run in the same container as ranikust,
without extra isolation: the container is the only boundary, as with kaniko.
Run ranikust in a container made for the build. Nothing can be mounted, so a
`RUN` gets only what needs no mount ([ADR-0057]):

- Heredocs work. One that starts with `#!` runs from an in-memory file its
  interpreter reads as `/dev/fd/<n>` (its `$0`), never a file in the image.
- A secret works as a variable: `RUN --mount=type=secret,id=tok,env=TOKEN`.
- A cache mount is left out, with a warning. The `RUN` runs without the cache;
  what it writes at the cache's target stays in the image (remove it in the
  same `RUN`: a tool's cache may hold tokens); a `sharing=locked` cache
  serializes nothing.
- A secret as a file (`/run/secrets/…`) and `--network=none` (the flag or `RUN
  --network=none`) are refused with the reason, not run without them. Use
  overlay mode, in a container that may mount, for those. Allowing a secret as
  a file here, kept out of the layers and removed after its `RUN`, is decided
  but not built ([ADR-0084], [ADR-0085]).

**What stays out of `RUN`'s reach.** Even there, ranikust keeps its own state
away from `RUN`: the layers it pulls and writes are files without a name, its
process cannot be inspected through `/proc`, no process a `RUN` starts
outlives that `RUN`, it drops any capability beyond Docker's defaults before
the first `RUN`, and it reads every `--secret` once at the start
([ADR-0051]). If the container grants `CAP_SYS_PTRACE` and ranikust cannot
take it away from `RUN`, it refuses to build. On a container's own root
filesystem, which takes no unnamed file on most kernels, each of those files
is made by a random name and removed at once, while no `RUN` runs, so it stays
on disk, not in the container's memory ([ADR-0056]). After a build in the
container was killed (ranikust not its first process), later builds there keep
this state in memory and cannot `COPY --from` an earlier stage or an image:
restart the container, or run ranikust as the container's first process
([ADR-0066]). On a volume (below) the state stays on disk even then, but
`COPY --from` still fails. ranikust runs from a sealed copy of its own binary
in memory, as large as the binary, so no `RUN` can change the code it runs
([ADR-0101]); if the kernel refuses that copy (`vm.memfd_noexec=2`), it makes
an unnamed one in `TMPDIR` where that file system allows it (a container's
overlayfs `/tmp` may not), else builds on with a warning.

**What ranikust cannot hide** (it warns about each):

- a value on its own command line (a `--build-arg TOKEN=…`): pass secrets with
  `--secret`;
- credentials in the environment of another process of the container: start
  ranikust as the container's first process (`exec` it), not from a shell that
  keeps them;
- a `--secret src=` file: every `RUN` can read it and copy it into the image,
  which is the Dockerfile author's choice. Pass the secret in ranikust's
  environment with `--secret id=…,env=VAR` if no `RUN` should read it;
- any file mounted into the container: pass registry credentials in
  `DOCKER_AUTH_CONFIG` rather than a mounted `config.json`.

**Signals.** `SIGINT` and `SIGTERM` stop the build only when they come from
outside the container (`docker stop`, a kubelet), from a terminal, or from
ranikust's parent process: a `RUN` or another process of the container cannot
stop it that way. Run ranikust as the container's first process, so that a
`RUN` cannot kill it either.

**Refusals.** ranikust refuses to build on the container's `/` where root may
mount (a `--privileged` container, `CAP_SYS_ADMIN`) and where `/proc/sys` is
not a read-only mount (Docker's `systempaths=unconfined`, Kubernetes'
`procMount: Unmasked`): a `RUN` there would see the host's devices and could
write `kernel/core_pattern`, which runs a program as the host's root. Use
overlay mode there (`--mode=overlay`, or `--mode=auto`, with a volume at
`/var/lib/ranikust`) ([ADR-0054]). It also refuses its own binary when a
writable mount other than the container's root reaches it, since a `RUN` could
then alter it: a bind without `:ro` or a volume without `readOnly: true`, a
directory holding it mounted read-write too, a link of it bound read-write
(even one whose name was removed), or another hard link of the file.
Mount a copy with one link read-only, outside any tree mounted read-write
([ADR-0101], [Installation](install.md#use-it-in-a-container)). On a container
root that is not an overlay (overlayfs or fuse-overlayfs), such as Docker's
btrfs, zfs or vfs drivers, containerd's native snapshotter or the directory
roots of podman `--rootfs`, systemd-nspawn or LXC, a binary in that root gets
the same checks. It refuses too when it cannot tell which mount its binary is
on, as under an emulator such as qemu-user or when started from a memfd: run
ranikust natively, from a regular file. When `/proc/self/mountinfo` does not
place the binary or changes while it is read, or a writable directory beside
it was removed at its source, or a writable mount beside it is hidden by
another mount, run it from a file that stays in place, or build it into an
image run on an overlayfs root, where the binary's check reads no mount table;
for a table that changed, start the build again once no mount changes while it
starts. ranikust looks for those mounts only when it finds itself in a
container; if it did not when it started but does later, it refuses: start the
build again.
When the measurement fails, mount `/proc` so that `/proc/self/exe` and
`/proc/self/mountinfo` can be read, and let the seccomp profile allow
`fstatfs`, `openat2` and `statx` (ranikust looks up the binary's name and the
writable mount points of its file system with `openat2` and `statx`).
A `RUN` shares the container's PID namespace, so where another container
shares it too (Kubernetes `shareProcessNamespace: true` or `hostPID: true`,
Docker or podman `--pid container:<name>` or `--pid=host`), a `RUN` could
reach that container's files through `/proc`. ranikust refuses to build when,
as it starts, it reaches a process of another mount namespace there, or cannot
tell (an error, or more than 32768 processes); it looks before it gives up
what a `RUN` does not keep, so it may refuse a process no `RUN` would reach.
Give the build container a PID namespace of its own ([ADR-0101]). Devices
you give the container with `--device` stay reachable by a `RUN`: ranikust
cannot tell them from the container's own, so pass only what the build needs.

**A volume as state directory.** A volume at `/var/lib/ranikust` works, one
per container.

- It must be a local filesystem. On NFS, FUSE, SMB or AFS, which keep a name
  for a file removed while open, ranikust refuses to keep its state there.
- It is never a cache in this mode: ranikust keeps nothing there between
  builds and empties it at the end, because a `RUN` could change it.
- A state directory in use by another build, or one that holds a store of
  overlay-mode builds (`.ranikust-state`), is refused.
- Mount the volume at the state directory itself, not at a parent of it, and
  never give a volume this mode used to overlay-mode or per-stage builds
  later: a `RUN` that kills ranikust before the end leaves what it wrote
  there ([ADR-0077]). A build whose state directory keeps anything a `RUN`
  left fails, and says so.

### Outside a container

kaniko mode never builds on the machine's `/`. It makes a directory per stage
in the state directory, `<state>/builds/<id>/stages/<n>/rootfs`, builds
independent stages in parallel (`--jobs`) as overlay mode does, and removes
each directory as soon as no later stage reads it. The same holds where root
may mount but overlayfs is unusable (a state directory on NFS, say).

- A stage built `FROM` another gets a copy of it (a reflink on xfs and btrfs,
  a copy of the data on ext4; holes stay holes). When it is the only stage
  that reads the parent, it takes the directory over instead: a chain of
  stages copies nothing. Plan for the roots read at the same time plus the
  copies being made.
- Device nodes, FIFOs and sockets a `RUN` made in the parent do not reach the
  stage built on it ([ADR-0061]).
- `RUN` gets the full sandbox of overlay mode, so ranikust needs mount
  privileges and refuses to build without them: run it in a container
  instead.
- The context, the outputs and secret files must not be inside the state
  directory's build area.

## When no mode works

As a regular user where user namespaces are restricted (the default on Ubuntu
24.04 and later), or in a pod that is neither root nor allowed user
namespaces, ranikust cannot build. It stops before doing anything and says
what would work: running as root, installing an AppArmor profile for the
binary, or running inside a container. Today every regular user is refused,
whatever user namespaces allow, and the message offers running as root or in a
container; refusing only where user namespaces are restricted, and the
AppArmor advice, come with rootless mode (Phase 3).

## Privilege levels at a glance

| You run ranikust as | Mode | What separates `RUN` from the machine |
|---|---|---|
| root on a host or VM | overlay | namespaces, a private root, reduced capabilities |
| root in a privileged container | overlay | the same, inside the container |
| a regular user allowed user namespaces | overlay, rootless (planned, Phase 3) | the same, inside a user namespace |
| root in a container that forbids mounts | kaniko | only the container |
| a regular user without user namespaces | none | ranikust refuses to build (today also for a regular user who has them) |

The [security policy](https://github.com/tran4774/ranikust/blob/main/SECURITY.md)
and the [threat model](https://github.com/tran4774/ranikust/blob/main/docs/security/threat-model.md)
give the details.

## Differences between the modes

The two modes build the same Dockerfile into images with the same meaning, but
not always the same bytes ([ADR-0012]):

- layer digests can differ between modes;
- when a `RUN` writes to a file that has several hard links, overlay mode
  changes only the name that was written, while kaniko mode changes every name
  of the file. BuildKit behaves like overlay mode.

Within one mode, the same build gives the same image whatever the value of
`--jobs`.

[ADR-0002]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0002-two-snapshot-modes.md
[ADR-0003]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0003-dockerfile-trust-and-default-isolation.md
[ADR-0004]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0004-support-every-privilege-level.md
[ADR-0010]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0010-run-network-host-or-none.md
[ADR-0012]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0012-accept-divergence-between-modes.md
[ADR-0013]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0013-jobs-default-available-parallelism.md
[ADR-0023]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0023-mode-flag.md
[ADR-0031]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0031-run-host-name-and-hosts-file.md
[ADR-0033]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0033-network-none-loopback-through-a-user-namespace.md
[ADR-0050]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md
[ADR-0051]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md
[ADR-0052]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0052-push-input-aliases-and-issue-mining-fixes.md
[ADR-0054]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md
[ADR-0056]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0056-kaniko-mode-state-on-disk-behind-a-run-fence.md
[ADR-0057]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0057-direct-run-heredoc-from-memfd-and-refusals.md
[ADR-0060]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0060-copy-leaves-special-files-out-where-run-has-no-nodev.md
[ADR-0061]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md
[ADR-0065]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0065-single-snapshot-refused-for-good.md
[ADR-0066]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0066-kaniko-mode-on-root-makes-no-names-after-a-build-cut-short.md
[ADR-0077]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0077-kaniko-mode-on-root-never-uses-a-persistent-store.md
[ADR-0084]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md
[ADR-0085]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0085-a-secret-file-on-slash-goes-at-its-target-is-ignored-by-its-own-path-and-removed-after-its-run.md
[ADR-0101]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md
