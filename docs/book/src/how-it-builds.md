# How ranikust builds

> This page describes what is built; where a part is not, it says so
> ([status](status.md)).

A Dockerfile build runs each instruction of each stage in a root filesystem
and records what the instruction changed as an image layer. ranikust can do
this in two ways, called *modes*. At start-up it tries, step by step, what the
environment allows, picks the mode that works and logs which one and why
([ADR-0002], [ADR-0004]). `--mode=overlay` or `--mode=kaniko` skips the
choice; ranikust still checks that the mode works and stops with the reason if
it does not ([ADR-0023]).

ranikust keeps downloaded and unpacked layers between builds in a **state
directory**: `/var/lib/ranikust` for root, `~/.local/share/ranikust` for
another user, unless `--state-dir` names another. Builds
running at the same time share it. Every root a build fills lies in it, except
the container's own `/` in kaniko mode: you never point ranikust at a
directory to fill and empty ([ADR-0050]). The directory is private: owned by
the user running ranikust, mode `0700`, below directories only root or that
user may write, because the image's setuid programs land there.

Base layers are unpacked entry by entry. An entry whose name leaves the root,
or that holds what the kernel refuses (a NUL byte, a name over 255 bytes, a
directory path or symlink target over 4095 bytes, a directory path over 4094
bytes on a container's `/` or, in kaniko mode on a host, over 4095 less the
stage directory's own path, an xattr name or value out of its bounds, a file
capability or POSIX ACL of a form the kernel never sets), fails the build,
naming the entry, before any of it is written. On a container's
`/`, a layer streamed from a registry is checked whole first, so such a layer
leaves `/` as it was.

## Overlay mode

Used where ranikust may mount filesystems: as root on a host or VM, or in a
privileged container. A regular user who may create user namespaces gets it
too, and so does root in a container that may not mount but has a volume at the
state directory (*rootless* overlay mode, [below](#rootless-overlay-mode)).
Where none of that works a regular user is refused (see
[When no mode works](#when-no-mode-works)).

- **Snapshots.** Each stage gets its own overlay filesystem in the state
  directory, and each instruction writes into a fresh layer of it, so the
  changes are known without scanning the filesystem.
- **`RUN` sandbox.** A `RUN` gets its own mount, process, hostname and IPC
  namespaces (and a cgroup namespace where it gets cgroups, see
  [Where to run it](where-to-run.md#cgroups-of-run-commands)), the stage's
  root as its `/`, and the capabilities and the seccomp profile Docker gives a
  container by default: about the isolation of `docker build` ([ADR-0003];
  `--seccomp` changes the profile, [ADR-0109]). It gets no SELinux label of
  its own: it keeps the context of the process that started ranikust, so on a
  host with SELinux enforcing the sandbox holds it, not policy as for a
  container ([ADR-0140]). Nor does ranikust apply an AppArmor profile of its
  own: where root has loaded
  [ranikust's sample profile](where-to-run.md#ubuntu-2310-and-later-the-apparmor-profile),
  ranikust and every `RUN` run under it, which confines nothing and allows
  user namespaces, a reach the
  [threat model](https://github.com/tran4774/ranikust/blob/main/docs/security/threat-model.md#6-accepted-risks)
  accepts.
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
  A `RUN --network=none` sees only a loopback interface, which is up; in a
  rootless build where the container's runtime masks `/sys`, its
  `/sys/class/net` lists no interface at all, and `/sys/fs/cgroup` and any
  other directory the runtime mounted below `/sys` are empty. In a
  container without `CAP_NET_ADMIN` it stops the build, unless `--allow
  network.none-userns` lets ranikust use a user namespace for it, under a
  seccomp filter ([ADR-0033]).
- **Parallel stages.** Stages that do not depend on each other build in
  parallel, up to `--jobs` at a time (default: the CPUs available to
  ranikust, [ADR-0013]).
- **In a container** the state directory must be a volume (`-v
  ranikust-state:/var/lib/ranikust`): overlay cannot use the container's own
  overlay root as an upper directory, and ranikust says so.
- **In a user namespace** that may mount, a base layer's block and character
  devices are left out, as the namespace's root may not make them; FIFOs and
  whiteouts are kept ([ADR-0106]), and `RUN` and `COPY --from` see the
  entry already at a left-out device's path, if any. Past a budget for their
  names (1 MiB, about 14 000 short ones), with a warning, a hardlink to a
  later one is linked or copied from what this layer or a lower one has at
  its name, or the layer is refused ([ADR-0128]). A hardlink to a device a
  lower layer left out takes the same path ([ADR-0131]). A layer with a
  `security.*` xattr other than `security.capability`, such as an SELinux
  label, is refused where no security module manages the name (SELinux
  decides `security.selinux` by policy), as the namespace may not set
  one.
- kaniko's `--snapshot-mode` is accepted and ignored with a warning: there is
  no filesystem walk to tune.

### Rootless overlay mode

Overlay mode in a user namespace of ranikust's own, where the mounts a build
needs belong to that namespace: for a regular user, where the kernel lets one
create user namespaces and the user has entries in `/etc/subuid` and
`/etc/subgid` for an image with several owners, and for root in a container
that may not mount, such as a default rootless Podman container
([ADR-0107]). ranikust probes the namespace and an overlay mount in
it first, in a directory it makes in the state directory and removes after,
and logs the verdict as `user namespace probe: ...`; only then does it enter
the namespace, so a refusal leaves nothing changed. The mode line says what the
build got: how `RUN` enters its root, the kind of `/proc` and `/sys`, and the
overlay mount options.

- **State directory.** It must not be on overlayfs, a container's own root:
  mount a volume there (`-v ranikust-state:/var/lib/ranikust`) or pass
  `--state-dir` on one. Without a volume, `--mode=auto` builds in kaniko mode
  on the container's `/` and logs the hint.
- **`/proc`.** A `RUN` gets a new `proc`. Where the kernel refuses it (Podman's
  and Docker's masked `/proc`), ranikust stops and names `--allow proc.rbind`,
  which binds the container's `/proc` read-only instead. Through it every `RUN`
  sees each process of the PID namespace and reads the files of `/proc/<pid>`
  not under ptrace's check, such as `cmdline`, `status`, `stat`, `mountinfo`,
  `cgroup` and `net/*`, ranikust's included, so keep no secrets in them, and
  `RUN`s run one at a time ([ADR-0126], [ADR-0127], [ADR-0130], [ADR-0132]).
  ranikust refuses the host's PID namespace, one whose PID 1 is another
  container's, and one it shares with another container's process that it can
  open as it starts, and warns about a `--build-arg` whose name looks like a
  secret ([ADR-0129], [ADR-0134]).
- **`--mode=auto` and root.** Root in a container that may not mount tries
  rootless overlay mode and, where it cannot run, builds in kaniko mode as
  before. It does not try where `/proc/sys` is writable or cannot be told: a
  `RUN` as the namespace's root could write it, and kaniko mode on `/` is
  refused there for the same reason ([ADR-0054]); nor with `--ignore-path`,
  which only kaniko mode honours. `--mode=overlay` there builds rootless or is
  refused with both reasons.
- **After the namespace.** A process in it cannot leave it, and kaniko mode
  never runs there. The probe that chose the mode has already run the same
  steps in a child process, so a failure there is one it could not see, such
  as the state directory's store. For a user other than root and for
  `--mode=overlay` it ends the build. Root's `--mode=auto` enters the
  namespace in a child process while ranikust waits outside it: where the
  child's set-up fails, before anything is built, ranikust builds in kaniko
  mode, as it did before rootless overlay mode, and logs why. `SIGINT` and
  `SIGTERM` reach the build through the waiting process, and the exit code
  is the build's.
- **Owners.** A file can only be given an owner, uid or gid, that the user
  namespace maps: without `newuidmap` and `newgidmap`, or without ranges,
  that is one id. Before extracting any layer of an image, ranikust reads
  their owners and refuses the image where one is not mapped, naming the
  owners, the files that have them and the ways out: install `uidmap`
  (Ubuntu does not by default) and give the user a first range in
  `/etc/subuid` and `/etc/subgid`, in decimal with no leading 0 (such a line
  is passed over), that reaches them, widen the one it has, run as root, or
  run in a container (whose ranges must reach them too). A
  range must not overlap another user's range or a real account's ids, so
  for an owner past 65536 (`useradd`'s default count) ranikust asks for no
  wider range and names root or a container instead. A `COPY --chown` or a
  `USER` the map lacks is refused the same way, when it comes. No flag lets
  a failed `chown` through: the file would get the wrong owner
  ([ADR-0098]). This holds in any user namespace, kaniko mode's too: with
  a directory per stage the layers are downloaded whole and all checked
  first; on `/`, in a rootless container, where they are streamed, each is
  checked just before it is extracted, so the layers before the refused one
  may already be there ([ADR-0137]). There an owner only on a path kaniko
  mode never writes (`/proc`, `/sys`, `/dev`, `--ignore-path`) is not
  refused; a `COPY --from=<image>`, unpacked whole, and a build with a
  directory per stage still check every path ([ADR-0144]).

### `--userns`

As root, `--userns` builds in rootless overlay mode in a user namespace whose
root is an unprivileged uid of the host, so that a process escaping a `RUN`
is not the host's root ([ADR-0104]). That uid and its gid are the first of a
`ranikust` range you add to `/etc/subuid` and `/etc/subgid`:

```text
ranikust:<start>:65536
```

in both files, in decimal with no leading 0, with a `<start>` above 0 that no
account, no other range and no other tool's range (below) uses. Without it
the build is refused before anything is written, and so it is where:

- the range shares an id with another line of those files, as any version of
  `newuidmap` may read that line, or with an account of `/etc/passwd` or
  `/etc/group`;
- a line of those files has a start or count that versions of `newuidmap`
  read differently or not at all: a `-` sign, an empty field, anything else
  in the field (letters, `0b`, an 8 or 9 after a leading 0) or a value past
  64 bits; white space, a `+` or `0x` before the number are read as
  `newuidmap` reads them;
- a line of those files has a NUL byte;
- an account is named `ranikust` in `/etc/passwd` (`+ranikust` too), or a
  file is named like a systemd userdb record of one (`ranikust.user` in
  `/etc/userdb` and the other drop-in directories): `newuidmap` would give
  it the range.

Only these files and the names of userdb records are checked: not accounts from
LDAP or SSSD, the ids of userdb records, nor `systemd-homed` and `DynamicUser`
accounts, so do not name a `DynamicUser=` unit `ranikust` (it gets an account
by that name that only NSS knows). Nor are the ranges other tools hand out, so
keep `<start>` outside those in use on the host: an NSS `subid:` module such as
SSSD's or FreeIPA's (`newuidmap` then reads it instead of the files, and
ranikust warns of it); the kubelet's pod user namespaces, which take every id
above 65535 unless an account named `kubelet` exists and `getsubids` is
installed, so with them on, create that account and give it lines in those
files, which the check counts; systemd's `DynamicUser` (61184–65519); and
`systemd-nspawn --private-users=pick` (524288–1879048191). Every `--userns`
build runs as that range: they are kept apart from the host, not from each
other.

- **State directory.** The range's first uid owns it: pass a `--state-dir` of
  its own, below directories only root owns and may write, each searchable by
  that uid through its mode bits (ACLs are not supported), such as
  `/var/lib/ranikust-userns` (ranikust makes a missing one). Root's own state
  directory is refused, never given away, and so is any at, below or above
  root's default, `/var/lib/ranikust`, whether it exists or not ([ADR-0139]).
  A build killed while it makes a missing one, or a missing directory
  above it, may leave an empty `.ranikust-new-*` directory in the parent of
  the one it was making. One killed while its probe's files were being made
  may leave a `.ranikust-anon-userns-probe-*` directory of root's, not
  empty, in the state directory, which the next builds warn of. ranikust
  removes neither, `prune --all --userns` included: remove them by hand, as
  root. `ranikust prune --all --userns --state-dir DIR` empties the state
  directory as the range's first uid
  ([Emptying the state directory](usage.md#emptying-the-state-directory)).
- **Owners.** The build can give a file only an owner of the range, ids 0
  to its count − 1. An image with an owner past them is refused before any
  of its layers is extracted, naming the owners, and so is a `COPY
  --chown` or a `USER` past them when it comes. The ways out: widen the
  `ranikust` line of `/etc/subuid`, `/etc/subgid` or both to the count
  named (id N needs N + 1), still sharing no id with an account, another
  range or another tool's, or build without `--userns`. For an owner past
  65535, which would need a count past 65536, ranikust asks for no wider
  range: build without `--userns` ([ADR-0098]).
- **What the build reaches** it reaches as that uid: the context, the
  Dockerfile, the working directory (where a relative `--dockerfile` is
  looked for first), `--secret` files, the registry credentials and the
  outputs. Root is to read the credentials and secrets itself first
  ([#206](https://github.com/tran4774/ranikust/issues/206)); until then that
  uid must be able to read them.
- **No other mode.** Where a step of the user namespace fails, the build is
  refused; it never falls back to a mode without it. `--mode=kaniko` with
  `--userns` is a usage error, and so is `--userns` from a user other than
  root, whose build already runs in a user namespace of its own.

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
as it starts, it can open a process of another mount namespace there, or
cannot tell (an error, or more than 32768 threads). It counts only the
processes it can open then, so give the build container a PID namespace of
its own even where ranikust does not refuse to build ([ADR-0101]).
Devices you give the container with `--device` stay reachable by a `RUN`:
ranikust cannot tell them from the container's own, so pass only what the
build needs.

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
namespaces, ranikust cannot build. It stops before it writes anything, names
the step of the probe that failed and says what would work, in order: running
as root with `sudo`; where Ubuntu's AppArmor switch restricts user namespaces
and may be what refused,
[ranikust's AppArmor profile](where-to-run.md#ubuntu-2310-and-later-the-apparmor-profile),
for a binary run on the host from a path only root can change, on a local
filesystem that root mounted (else after installing it in one, naming what
failed); or running inside a container. A regular user is also refused
where only an rbind of `/proc` would work and `--allow proc.rbind` is
absent, and where the state directory is on overlayfs.

## Privilege levels at a glance

| You run ranikust as | Mode | What separates `RUN` from the machine |
|---|---|---|
| root on a host or VM | overlay | namespaces, a private root, reduced capabilities |
| root on a host or VM, with `--userns` | overlay, rootless | the same, inside a user namespace whose root is an unprivileged uid of the host |
| root in a privileged container | overlay | the same, inside the container |
| a regular user allowed user namespaces | overlay, rootless | the same, inside a user namespace |
| root in a container that forbids mounts, with a volume at the state directory | overlay, rootless (with `--allow proc.rbind` where `/proc` is masked) | the same, inside a user namespace |
| root in a container that forbids mounts | kaniko | only the container |
| a regular user without user namespaces | none | ranikust refuses to build |

The [security policy](https://github.com/tran4774/ranikust/blob/main/SECURITY.md)
and the [threat model](https://github.com/tran4774/ranikust/blob/main/docs/security/threat-model.md)
give the details.

## Differences between the modes

The two modes build the same Dockerfile into images with the same meaning, but
not always the same bytes ([ADR-0012]):

- layer digests can differ between modes;
- when a `RUN` writes to a file that has several hard links, overlay mode
  changes only the name that was written, while kaniko mode changes every name
  of the file. BuildKit behaves like overlay mode;
- when a layer gives a directory the owner id 4294967295 (`chown`'s "leave
  unchanged") and an earlier layer made that directory, kaniko mode keeps the
  earlier owner and overlay mode gives the directory the build process's own
  id, root's. Each mode does what its upstream tool does ([ADR-0141]).

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
[ADR-0098]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md
[ADR-0101]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md
[ADR-0106]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0106-extraction-in-a-user-namespace-skips-device-nodes-and-counts-them.md
[ADR-0104]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md
[ADR-0107]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md
[ADR-0109]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md
[ADR-0126]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md
[ADR-0127]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md
[ADR-0128]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0128-a-canary-guards-nested-namespaces-a-budget-bounds-device-names-and-a-device-left-out-keeps-the-lower-entry.md
[ADR-0129]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0129-with-allow-proc-rbind-a-shared-pid-namespace-is-refused-and-the-store-is-anonymous.md
[ADR-0130]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0130-with-allow-proc-rbind-the-residual-is-the-measured-one-the-peer-scan-stays-and-the-build-uses-the-persistent-store.md
[ADR-0131]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0131-a-link-to-a-device-a-lower-layer-left-out-takes-the-normal-hardlink-path.md
[ADR-0132]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0132-the-proc-rbind-is-bound-read-only-so-a-run-writes-nothing-under-it.md
[ADR-0134]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0134-with-allow-proc-rbind-the-pid-namespace-check-reads-files-no-ptrace-check-guards.md
[ADR-0137]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0137-kaniko-mode-on-slash-checks-each-streamed-layers-owners-just-before-extracting-it.md
[ADR-0139]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0139-userns-builds-in-a-state-directory-of-its-own-and-root-reads-the-credentials-before-the-namespace.md
[ADR-0140]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0140-a-run-keeps-its-callers-selinux-context-and-gets-no-label-of-its-own.md
[ADR-0141]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0141-an-owner-of-4294967295-over-an-earlier-layers-directory-is-an-accepted-difference-between-the-modes.md
[ADR-0144]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0144-kaniko-modes-owner-check-skips-the-ignored-paths-on-slash-only.md
