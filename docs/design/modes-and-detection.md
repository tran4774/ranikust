# Modes and privilege detection

**TL;DR:** at start-up ranikust probes what the environment allows, picks a
snapshot mode and a `RUN` execution path, or refuses with guidance.

Decisions: [ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0004](../adr/0004-support-every-privilege-level.md),
[ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md),
[ADR-0023](../adr/0023-mode-flag.md),
[ADR-0050](../adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md),
[ADR-0098](../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md),
[ADR-0100](../adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md),
[ADR-0107](../adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)
(ADR-0100 planned, Phase 3).

## Outcomes

The probe ends in exactly one of these:

| Outcome | Snapshot | Stage roots | `RUN` execution | Stages |
|---|---|---|---|---|
| **overlay, root** | upperdir | one overlay mount per stage in the state directory | full sandbox | parallel (DAG) |
| **overlay, rootless** | upperdir, user namespace | same, mounts the build's user namespace owns | full sandbox in the build's user namespace; `/proc` rbind on request only (`--allow proc.rbind`), if a new `proc` is refused ([ADR-0126](../adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)) | parallel (with the `/proc` rbind, stages parallel, their `RUN`s one at a time: [ADR-0127](../adr/0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md)) |
| **kaniko, can mount** | walk + hash | one directory per stage | full sandbox (bind mounts, `pivot_root`) | parallel by default |
| **kaniko, cannot mount** | walk + hash | `/` in a container (a host is refused, ADR-0050) | directly, like kaniko (`Isolation::Direct`) | sequential on `/`; `--parallel-stages` refused (ADR-0060) |
| **refuse** | none | none | none | exit with guidance |

"kaniko, can mount" is root where overlay is unusable: state directory on NFS
or FUSE, upper directory on the container's overlay root without a volume, or
a kernel without overlay.

**Built:** every outcome. Rootless overlay mode is `Outcome::OverlayRootless`
([#79](https://github.com/tran4774/ranikust/issues/79)): a user other than
root, and root that may not mount where ADR-0107 lets it, build in the build's
user namespace, which the process enters after the probe has routed
([step 2](#probe-order---modeauto); how:
[rnk-sandbox](../agents/crates/rnk-sandbox.md#owns),
[ranikust](../agents/crates/ranikust.md#owns)). Root's `--userns` takes the
same path whether or not it may mount, with its `ranikust` range, and never
another mode ([sandbox](sandbox.md#--userns),
[#83](https://github.com/tran4774/ranikust/issues/83)). Root's builds that may
mount keep `enter_build_namespace` (`unshare(CLONE_NEWNS)` only), and kaniko
mode is set up by `set_up_kaniko`. The refusal of an image with an owner the
namespace does not map and the AppArmor advice for a user with no usable
user namespace ([#81](https://github.com/tran4774/ranikust/issues/81)) are
built, under [Refusal and guidance](#refusal-and-guidance).

## Where environments land

From [experiment 03](../research/experiments/03-privilege-matrix.md) (full
table per CI system in §6) and
[experiment 13](../research/experiments/13-docker-rootful-vm.md).

| Environment | Outcome |
|---|---|
| Host as root or via `sudo` (GitHub Actions with `sudo`, GitLab.com SaaS) | overlay, root |
| Container `--privileged`; podman `--cap-add SYS_ADMIN` | overlay, root (the state directory must be a volume or tmpfs) |
| Docker `--cap-add SYS_ADMIN` alone | kaniko: AppArmor `docker-default` denies `mount` with `EACCES` |
| Rootless podman (default); Docker with `seccomp=unconfined,apparmor=unconfined` | overlay, rootless, with a volume at the state directory and `--allow proc.rbind`, the container's `/proc` being masked ([ADR-0126](../adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)); without the flag `--mode=overlay` is refused, naming it, and `--mode=auto` takes kaniko mode on `/` with the ADR-0054 checks (refused on a host); without a volume the state directory is on overlayfs, so kaniko mode on `/` with a hint to mount one ([ADR-0107](../adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)); an image owner outside the container's id map is refused before its layers are extracted ([ADR-0098](../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md), [below](#owners-the-user-namespace-does-not-map)) |
| Unprivileged user on Debian 12+, Fedora, RHEL 9+, Ubuntu 22.04 | overlay, rootless (several owners only with `newuidmap` and ranges in `/etc/subuid` and `/etc/subgid`; else such an image is refused, [below](#owners-the-user-namespace-does-not-map)) |
| Docker default, GitHub Actions `container:` jobs, Kubernetes pods on Ubuntu/COS nodes, GKE Autopilot, Bottlerocket | kaniko, cannot mount |
| Kubernetes pod with `hostUsers: false` and `procMount: Unmasked`, AppArmor confined or not | refuse: `/proc/sys` writable ([ADR-0107](../adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md) case (ii), which ranikust (`rootless_kept_off`) also applies to an unconfined pod, where ADR-0100 row 7b expected overlay, rootless: [experiment 18](../research/experiments/18-kubernetes-pods-in-k3s.md)) [open: #201] |
| Unprivileged user on Ubuntu ≥ 24.04, binary in `~/bin` | refuse, with `sudo`, the AppArmor profile after installing the binary under `/usr/local/bin`, or a container |
| Unprivileged user on Ubuntu ≥ 24.04, binary at `/usr/local/bin/ranikust` with `contrib/apparmor/ranikust` loaded | overlay, rootless (as on 22.04) |
| Pod Security Standards `restricted`, OpenShift `restricted-v2` | refuse |

Most locked-down CI pods land in kaniko mode. Overlay is available only
where BuildKit or buildah would also run. RHEL 8 (kernel 4.18) is in no row:
ranikust refuses to start on a kernel before 5.8
([ADR-0028](../adr/0028-minimum-kernel-linux-5-8.md)).

Which of these environments the probe is tested in, and in which tier, is
decided in
[ADR-0100](../adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md).
Built: the T1 `rootless` smoke case (rootless Podman with a volume, row 5),
the T1 kaniko leg's cases (row 5b, in part), the k3s leg, the T2 `k8s`
suite (rows 7a to 7d, measured;
[testing](../testing.md#privilege-matrix-in-kubernetes)), and the runner
jobs: row 5 in the runner's rootless Podman (the T1 `matrix-podman` case)
and rows 6 and 6b as its user on Ubuntu 24.04 (the T2 `matrix-user` suite),
each asserting ADR-0100's rows
([testing](../testing.md#privilege-matrix-on-the-runner)). The VM legs are
planned (#85), but for row 9e: the T2 `el9` suite ([testing](../testing.md#the-vm-harness-tier-t2)) boots
AlmaLinux 9.8 (kernel 5.14, SELinux enforcing, policy `targeted`). Measured
there, as expected: root gets overlay mode as root and a regular user with
ranges rootless overlay mode, with a new `proc` and a read-only rbind of
`/sys`. In both the user-namespace probe passes (map `multi-range` as root,
`newuidmap` as the user), with a `sysfs` of its own and `mknod` refused
with `EPERM`, and overlay mounts with `fsconfig(lowerdir+)`: the kernel, a
5.14 with backports, has it ([step 4](#probe-order---modeauto)). The audit
log records no SELinux denial in either leg. A `RUN` runs in the SELinux
context of the process that started ranikust (`unconfined_t` from a login
shell), since ranikust sets none, by design
([ADR-0140](../adr/0140-a-run-keeps-its-callers-selinux-context-and-gets-no-label-of-its-own.md)).
The default state directories are labelled
`var_lib_t` (root's) and `data_home_t` (the user's), and no label reaches a
layer: the two legs' `--reproducible` digests are one.

## Probe order (`--mode=auto`)

Each crate probes its own capability. `ranikust` runs the probes in this
order while the process is still single-threaded, then picks the outcome:

- `rnk-sandbox` (`probe.rs`): the real `RUN` sequence in an empty root: the
  namespaces, `proc`, `sysfs`, the `/dev` tmpfs and its node binds,
  `pivot_root` then `MS_MOVE`, the capability drop and the `USER` switch.
  The user-namespace steps, the maps and `mknod` among them, are a probe of
  their own (`userns.rs`, step 2), reported only.
- `rnk-snapshot`: the overlay mount with the
  [ADR-0016](../adr/0016-overlay-mount-options.md) options, and `fsconfig`
  with `lowerdir+`.

Each step performs the real operation. Many environments let an early step
succeed and fail a later one, so the probe never stops at the first success
and never compares kernel versions (Ubuntu and Debian patched overlay in user
namespaces before 5.11; SELinux needs 5.13).

Each attempt runs in a forked child that reports through a pipe and exits, so
a failed attempt leaves no namespace or mount behind [inferred]. The overlay
probe as root (`rnk_snapshot::probe`, steps 1 and 4) is the exception:
`ranikust` has already entered the build's private mount namespace, so it
mounts there directly and unmounts; a mount a failure leaves goes with the
process. It runs after the store is open, in `builds/<id>/overlay-probe`
(where it mounts is [open: #4]).

1. **Overlay without a new user namespace.** `unshare(CLONE_NEWNS)`, mount a
   small overlay where the stage roots will live (the state directory) with
   the full option set ([ADR-0016](../adr/0016-overlay-mount-options.md)),
   then mount a new `proc` in a new PID namespace. With `CAP_SYS_ADMIN` this
   is the only way to get a new `proc` when `/proc` is masked: a new user
   namespace makes the kernel refuse it (`EPERM`).
2. **Overlay in a user namespace**
   ([#74](https://github.com/tran4774/ranikust/issues/74),
   [#79](https://github.com/tran4774/ranikust/issues/79)). It decides rootless
   overlay mode where it routes (below); where root may mount it routes
   nothing, and `--mode=auto` as root only reports it, during the set-up,
   before the spawner starts. It logs one line either way (whether the path
   passes or where it stops, each step with its errno, the map, the overlay,
   the AppArmor switch below): `user namespace probe: ...`, or `user
   namespace probe (reported only): ...` where it routes nothing. An rbind of
   `/proc` counts as a pass only with `--allow proc.rbind`; without it the
   line stops at `proc`, whatever the rbind would do. Root's map (below)
   makes uid 0 in that namespace ranikust's own uid 0, host uid 0 unless
   ranikust itself runs in a user namespace: a line that passes says these
   steps work here, not that a `RUN` may get this map.
   `rnk_sandbox::probe_user_namespace` runs the steps in forked children, in
   a directory made for them and removed after:
   - for rootless overlay mode (`setup::rootless::plan`), in the state
     directory, once the store's check, which writes nothing, takes it as one
     (Q-111, (a)), and holds the state directory's lock until the store
     opens; named `.ranikust-anon-userns-probe-<pid>-<nanos>`
     (`rnk_store::USERNS_PROBE_PREFIX`; `-again` added once where another
     build's start took the first for a leftover before it was held) and held
     by its exclusive `flock` while the probe uses it, so that a build cut
     short there leaves nothing the next start counts as foreign: the store,
     once it opens, removes each such directory whose lock it takes
     ([cache and store](cache-and-store.md)); it takes the process's gid and
     no setgid bit, as the store's own directories do
     ([ADR-0099](../adr/0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)).
     Under root's `--userns` it is given to the range, so root deletes
     nothing in it: it is left, unlocked, to the build's store, which
     removes it from inside the namespace, and root removes only an empty
     one of its own that a build killed before giving it away left
     ([sandbox](sandbox.md#--userns));
   - where it only reports (`report_user_namespace`, root only: it checks the
     effective uid), in a directory of the build under the state directory
     (`userns-probe`, or a name the next start removes when the store is
     anonymous).

   If a child could not be reaped, which may still work in it, the directory
   stays: the report's until the build directory is removed (or the state
   directory emptied) at the end of the run, at once when `--mode=auto`
   falls back to kaniko mode, else at the next start (a refused set-up, a
   run cut short); rootless overlay mode's as a leftover above.
   - `unshare(CLONE_NEWUSER)`, then write `uid_map` and `gid_map`. Root
     writes a map of every id it has, each to itself; inside a container
     that is several ranges (each must fit inside one range of the parent
     map: `0 0 1` + `1 1 65536` works where `0 0 65536` is `EPERM`). An
     unprivileged host user needs `newuidmap`/`newgidmap` and
     `/etc/subuid`; without them only one uid maps and `chown` to any other
     fails with `EINVAL`, as does a `chown` to an owner outside the ranges;
     ranikust refuses such an image
     ([below](#owners-the-user-namespace-does-not-map)).
     Each map is tried in turn (root: its ranges, then one uid; a user:
     `newuidmap`, then one uid); the log names the one that worked
     (multi-range, newuidmap, one uid) or none.
   - `chown` to a mapped id other than 0 (not with one uid), then
     `unshare(CLONE_NEWNS)`: a second call, after the map, so that the mount
     namespace is owned by the new user namespace and a refusal names its
     step.
   - A new `proc` in a new PID namespace; if that fails, an rbind of the
     existing `/proc`, which the build may use only on request
     ([sandbox.md, step 3.2](sandbox.md#per-run-sequence)). Read-only
     `sysfs`, else an rbind of `/sys`. `mknod`, reported only: no user
     namespace allows it, and the sandbox binds `/dev`'s nodes.
   - The overlay mount (`rnk_snapshot::probe`; `userxattr` is implied) as
     root of a namespace with the map that worked, in a child of its own,
     under the state directory ([open: #4]).
   - `kernel.apparmor_restrict_unprivileged_userns`, read and logged.
   - On Ubuntu 24.04 (later releases probably the same) `unshare` succeeds,
     then the `unprivileged_userns` AppArmor profile strips the namespace of
     capabilities. Measured for an unconfined binary: with one id the `uid_map`
     write fails with `EPERM`; with ranges the map is set up, then `chown` and
     `unshare(CLONE_NEWNS)` fail with `EPERM` and a rename with `EACCES`
     ([ADR-0099](../adr/0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md#evidence)).
     The probe reaches those steps; the T2 suite checks where it stops for a
     regular user.
3. **Sandbox capabilities.** Try `pivot_root` (Docker's seccomp profile
   blocks it even with `CAP_SYS_ADMIN`; fallback `MS_MOVE` + `chroot`) and a
   read-only `sysfs`, so the sandbox knows which variant to use
   ([sandbox](sandbox.md)). In the build's user namespace, a `sysfs` mount
   refused with `EPERM` is tried again as an rbind, only where the
   user-namespace probe (step 2) found the same; a refused `proc` mount
   only at the operator's request ([ADR-0126](../adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md);
   `--allow proc.rbind`)
   (`rnk_sandbox::probe_with_fallbacks`,
   [#78](https://github.com/tran4774/ranikust/issues/78)). Every `RUN` of
   the build then gets what worked, and an rbind of `/proc` is logged once;
   with it the `RUN`s run one at a time
   ([ADR-0127](../adr/0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md)),
   and a PID namespace shared with a process of another mount namespace that
   ranikust can open as it starts is refused before the process enters the
   namespace
   ([ADR-0129](../adr/0129-with-allow-proc-rbind-a-shared-pid-namespace-is-refused-and-the-store-is-anonymous.md)),
   as is the host's, or one whose PID 1 is of another mount namespace, told
   from files with no ptrace check
   ([ADR-0134](../adr/0134-with-allow-proc-rbind-the-pid-namespace-check-reads-files-no-ptrace-check-guards.md));
   the store stays the persistent one
   ([ADR-0130](../adr/0130-with-allow-proc-rbind-the-residual-is-the-measured-one-the-peer-scan-stays-and-the-build-uses-the-persistent-store.md)).
   Elsewhere a refused mount fails this probe.
4. **Mount API.** Create a test overlay with `fsconfig(lowerdir+)`. Kernel
   5.15 accepts `FSCONFIG_SET_STRING` for `lowerdir+` and fails only at
   `FSCONFIG_CMD_CREATE` (`EINVAL`), so only a real create counts, and a
   kernel's version says nothing: AlmaLinux 9.8's 5.14 creates it
   (measured, the `el9` suite). On failure
   use `mount(2)` ([overlay-snapshot](overlay-snapshot.md)).
5. **No overlay path passed.**
   - Root: **kaniko mode**. A bind-mount test decides between "can mount"
     (per-stage directories, full sandbox) and "cannot mount" (sequential on
     `/` in a container; a host where root may not mount is refused,
     ADR-0050).
   - Otherwise: **refuse** with guidance (below).
6. **Log the reason** for the chosen outcome, including each failed step and
   its errno.

How `--mode=auto` routes (ADR-0040, ADR-0059, ADR-0107):

- A user other than root builds in rootless overlay mode, or is refused with
  the step the probe stopped at and the way out. `--mode=overlay` routes the
  same; `--mode=kaniko` needs root.
- Otherwise `ranikust` enters the build's mount namespace first, before
  anything is written. Root refused there for lack of privilege tries
  rootless overlay mode, except where `/proc/sys` is writable or cannot be
  told (ADR-0107, case (ii)): a `RUN` as the map's uid 0 could write it, and
  kaniko mode on `/` is refused there for the same reason (ADR-0054); nor
  with `--ignore-path`, which only kaniko mode honours (ADR-0049): overlay
  mode would refuse it, and kaniko mode builds it, with a note saying so.
  Where rootless overlay mode cannot run, its refusal is logged and root gets
  kaniko mode "cannot mount", with the reason logged. A forced
  `--mode=overlay` there builds rootless or is refused with both reasons.
  Where root may mount, the overlay probes run.
- Where they refuse because overlayfs cannot use the state directory (the
  probe's mount refused, or a state directory on overlayfs), `--mode=auto`
  outside a container builds in kaniko mode "can mount": a directory per stage
  and the full sandbox, with the reason logged. A failed step of the sandbox
  itself never routes there, since that variant needs the same sandbox.
- In a container where root may mount, kaniko mode on `/` is refused
  (ADR-0054), so the overlay refusal stands, with what makes overlay mode
  work (a volume at the state directory). No refusal's guidance sends a user
  back to the mode they used.
- Kaniko mode on `/` with the state directory on overlayfs, where the
  user-namespace steps pass, logs a hint (ADR-0107, case (i)): a volume at
  the state directory (`-v ranikust-state:/var/lib/ranikust`) would let
  rootless overlay mode build here, with `--allow proc.rbind` too where the
  `/proc` is masked.

Rootless overlay mode (`setup/rootless.rs`) has two phases, since the process
cannot leave the namespace once it is in:

1. `plan`, the process unchanged: the state directory's checks, refused on
   overlayfs (a container's own root cannot hold the upper directories: a
   volume is the way out); the user's subordinate ids
   (`/etc/subuid`, `/etc/subgid`); the user-namespace probe (step 2) with the
   overlay mount in it. A step that fails, or `proc` where only an rbind
   works and `--allow proc.rbind` is absent, ends here as "rootless overlay
   mode cannot run here" with the way out, and routing goes on as above.
   With the flag and an rbind of `/proc` needed, the peer scan of kaniko mode
   on `/` runs (ADR-0101, ADR-0129; T21) and secret-looking `--build-arg`
   names are warned about, both before the namespace, where ranikust can
   still open the other processes.
2. `set_up`, in the namespace: `enter_build_user_namespace` (user and mount
   namespace), the store, the sandbox probe with the fallbacks `plan`
   allows (step 3), the overlay probe (step 4), the spawner, then the mode
   line. A failure here is `Kind::Broken`: no other mode follows in the
   process. `plan`'s probe child has already run what this repeats, each
   step a `RUN` needs and an overlay mount on the state directory's
   filesystem, so what fails here is what that child could not see: the
   store, the spawner's start, or a change between the two. For root's
   `--mode=auto`, which built in kaniko mode before rootless overlay mode,
   the process forks first (`rnk_sandbox::fork_to_wait`, ADR-0136): the
   child runs `set_up` and the build; the parent waits outside the
   namespace, sends the child `SIGUSR1` for each `SIGINT` or `SIGTERM` it
   gets (the child ignores those two itself), and ends as the child ends.
   Where the child's `set_up` fails, the child empties the state directory
   where its store holds no data, whoever made it (`Store::abandon`, or
   `Store::open_or_abandon` for an open that failed once it took the
   directory; not a store with data, or one another build holds; a
   forwarded stop is ignored until it is done) and sends the reason, and
   the parent, with no stop received, warns `<reason>; kaniko mode
   instead, before anything was built (ADR-0136)` and builds in kaniko
   mode as below, which empties such a store too where a set-up killed
   before its end left one (`Store::open_anonymous`). A user other than
   root and `--mode=overlay` get the error.

The mode line, logged once the set-up can no longer fail, so a fallback
logs kaniko mode's alone, says what the build got: `overlay mode, rootless, in the
build's user namespace, RUN enters its root with pivot_root, proc: a new
instance, sysfs: a read-only rbind (for a RUN with --network=none, a new
instance where it mounts, else the rbind with no network device), overlay
mounts with fsconfig(lowerdir+)`;
`proc: a read-only rbind` is the one that needs the flag. Root's `--userns`
says `overlay mode with --userns, in the build's user namespace of the
ranikust range` in place of the first part.

`--mode=overlay|kaniko` skips the choice but not the checks: a forced mode
that cannot work fails with the probe's diagnosis
([ADR-0023](../adr/0023-mode-flag.md)). `--snapshot-mode=full|redo|time`
keeps kaniko's meaning, what the kaniko-mode walk compares; in overlay mode
it is accepted and ignored with a warning.

Flags that cannot work are refused
([ADR-0049](../adr/0049-security-and-performance-over-kaniko-compatibility.md)):
`--ignore-path` in overlay mode where it would change the image,
`--single-snapshot` in every mode for good (ADR-0065), `--materialize` until
Phase 4 (ADR-0064), `--ignore-var-run` in both modes (ADR-0052 D4).

<a name="workdir-rules"></a>
## Build root and state directory

- There is no work-directory flag (ADR-0050). Kaniko mode builds on `/`, like
  kaniko, where kaniko's container check finds a container (`container.rs`,
  ported from `kaniko@44aa7ddc0 pkg/util/proc/proc.go`, ADR-0048). Everywhere
  else it builds in a directory per stage in its state directory,
  `<state>/builds/<id>/stages/<n>/rootfs` (ADR-0059). It never builds on a
  machine's own `/`, and kaniko's `--force` is refused.
- Outside a container kaniko mode needs mount privileges, for the full
  sandbox; a host where root may not mount is refused (ADR-0050).
- The state directory is `/var/lib/ranikust` for root in every mode, and
  `~/.local/share/ranikust` for another user (from `$HOME`, which must be
  absolute; else `--state-dir` is required); `--state-dir` replaces it
  ([cache-and-store](cache-and-store.md#store-layout)). It must be owned by
  the user running ranikust, not writable by others, below directories only
  root or that user may write; ranikust makes it `0700`
  ([kaniko-mode](kaniko-mode.md#host-safety-checks)). The checks run outside
  the build's user namespace, where the owners read as on the host; the
  store then opens inside it.
- Kaniko mode on `/` takes a `flock` on `/` itself: one build per container.
  Overlay-mode builds and kaniko-mode builds on a host share the state
  directory and its store, each with its own `builds/<id>`.
- Overlay mode needs the state directory, where the upper directories live,
  on a filesystem that can hold an upper directory: ext4, xfs, btrfs or
  tmpfs. Not overlayfs (`EINVAL`: "not supported as upperdir"), not NFS. A
  container's own overlay root can be a lower layer but not the upper one,
  and overlay nests at most two deep. The probe in step 1 or 2 catches all of
  these, and the result is kaniko mode. A state directory on overlayfs is
  refused before the probe, with the advice to mount a volume there, in
  rootless overlay mode too.
- Overlay mode, as root, rootless or under `--userns`, refuses its state
  directory before it changes anything in it
  (`setup::hold_state_dir`): overlayfs first, told from the nearest
  directory that exists, where a missing one would be made; then the
  owner and chain checks, which make a missing one; then the store's own
  refusals (`Store::check_state_dir`, writing nothing); only then is its
  mode set.

## Refusal and guidance

kaniko mode needs root (`chown`, `chroot`, switching users). An unprivileged
process with no usable user namespace cannot build, and ranikust says so
instead of producing a broken image.

Built today (`crates/ranikust/src/setup.rs`, `setup/rootless.rs`):

- Root lacking the privilege to mount (`Error::Privilege`,
  `namespace_refusal`) tries rootless overlay mode, except where `/proc/sys`
  is writable or cannot be told (ADR-0107), then takes kaniko mode: on `/` in
  a container, refused on a host (`host_without_mounts`, ADR-0050). A forced
  `--mode=overlay` there builds rootless or is refused with both reasons: the
  mount's, with the advice to use kaniko mode (`NEEDS_KANIKO_MODE`), and
  rootless overlay mode's.
- Root's `--userns` builds in the build's user namespace of its `ranikust`
  range or is refused, before anything is written, where the range is
  missing (naming the line to add to `/etc/subuid` and `/etc/subgid`),
  shares an id with an account or another range or sits beside a line it
  cannot read as `newuidmap` may (naming it), or an account is named
  `ranikust`, or the
  state directory is not one for the range's first uid (saying what it must
  be), and otherwise where a step of the namespace fails, never in another
  mode (`USERNS_UNUSABLE`, never the advice
  [below](#no-usable-user-namespace), which is a user's, nor its capability
  check); with `--mode=kaniko` it is a usage error.
- A process that is not root builds in rootless overlay mode or is refused
  with the step it stopped at and the way out: where no user namespace is
  usable (`unshare`, the map, `chown` or the mount namespace failed), the
  advice [below](#no-usable-user-namespace) (`setup/no_userns.rs`); `--allow
  proc.rbind`, what it exposes and the other ways, where only an rbind of
  `/proc` would do (`PROC_RBIND`); a volume at the state directory where it
  is on overlayfs; root or a container otherwise.
- Kaniko mode on `/` is refused where `refuse_unsafe_slash` stops it: root may
  mount, a writable `/proc/sys` (ADR-0054), `--network=none` (ADR-0057), or
  ranikust's binary on a writable mount other than `/`'s, reached by one, or
  on one it cannot tell, or a process of another mount namespace in its PID
  namespace that it can open through `/proc` as it starts, the only ones
  it counts, or one it cannot tell (ADR-0101).
- A container where root may mount but overlayfs is unusable is refused
  (`Route::RefuseInContainer`).
- A failed probe step is refused with the step, its errno and the way out
  (root, or a container, `--privileged` for overlay mode, for a user whose
  rootless probe failed).
  A host-side failure such as an unwritable state directory is reported as it
  is.

Inside a plain `chroot`, `/` is not a mount root: the build namespace cannot
make it a slave mount, and `MS_MOVE` would let `..` climb out of a stage root
([sandbox](sandbox.md#variants-by-environment)). ranikust refuses there
before changing anything, and says to run on the host or in a container, or
to bind-mount the chroot directory onto itself first. Where the kernel does
not report whether `/` is a mount root (`STATX_ATTR_MOUNT_ROOT` missing from
the mask), it neither refuses on that ground nor falls back to `MS_MOVE`.

### No usable user namespace

Where a user other than root gets no usable user namespace, ranikust refuses
before it writes anything, the state directory included: before the probe, a
forked child makes a user namespace and, with no id map, a mount namespace in
it, which needs `CAP_SYS_ADMIN` there
(`rnk_sandbox::probe_user_namespace_capabilities`). Either refused (`unshare`
under a seccomp filter, the mount namespace in a namespace without
capabilities, as the AppArmor switch gives) stops the build at that step;
otherwise, and where the check cannot tell (a child cut short), the probe
then runs, and its stops come after the state directory is made. The
refusal's guidance line gives, in order
([#81](https://github.com/tran4774/ranikust/issues/81)):

1. Run ranikust as root, with `sudo`.
2. Only where `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` reads
   `1`, where ranikust runs unconfined (AppArmor's label `unconfined`), in the
   host's user namespace (the initial one, told by its inode,
   `rnk_snapshot::UserNamespace::current`) and in no container
   `container::detect` finds (`no_userns::Here`), and where AppArmor may be
   what refused (`no_userns::Stop`): the user namespace was made and a later
   step failed (the map, `chown`, the mount namespace), or `unshare` failed
   with `EACCES`, AppArmor's errno. A confined program's own profile decides
   its user namespaces, and ranikust's would not attach to it; in a container
   or a user's own user namespace the tree ranikust sees, and the owners it
   reads, are not the host's, so no path checked there is the one a profile
   loaded on the host attaches to. Another errno of `unshare` (`EPERM` from a
   seccomp filter or `kernel.unprivileged_userns_clone`, `ENOSPC` from
   `user.max_user_namespaces`), none, a step that did not run, any stop where
   the capability check found the namespace's capabilities, or a refusal once
   the probe passed gets items 1 and 3 alone. Have root load ranikust's
   AppArmor profile,
   [`contrib/apparmor/ranikust`](../../contrib/apparmor/ranikust), three lines
   granting `userns,` with `flags=(unconfined)`, bound to
   `/usr/local/bin/ranikust` (as podman's profile is to podman), into
   `/etc/apparmor.d/ranikust` with `apparmor_parser -r`. A profile gives a
   user namespace to whatever is at its path, so it is offered for the
   binary's own path only where root alone may change it and it holds ranikust
   itself: as shipped for `/usr/local/bin/ranikust`; with the path changed for
   another such path made only of letters, digits and `._+-/`, which AppArmor
   reads as no pattern; otherwise after installing the binary, owned by root
   (`install -o root -g root -m 0755`), as `/usr/local/bin/ranikust`, or,
   where `/usr/local/bin` fails the same check, in a directory that passes,
   naming the first part of the path that failed and why.
3. Run ranikust in a container.

The path is the one `/proc/self/exe` named before the sealed copy ran
(ADR-0101), the one AppArmor attaches a profile to where ranikust runs in
the host's mount namespace, with no chroot; the sealed copy's marker
carries it (`rnk_sandbox::Sealing::binary_path`). A chroot, or a mount
namespace that no container check finds in a user namespace that maps
every id as the host's does, shows a tree root chose, which the check
takes for the host's: only root can set one up. A setuid sandbox a user
can start (bubblewrap, Apptainer) mounts what it shows `nosuid`, which the
check refuses (below) **[external]** (bubblewrap 1fb0aa2
`bind-mount.c:511,532`, `bubblewrap.c:980,1354,3284`; Apptainer 330fb19
`internal/pkg/runtime/engine/apptainer/container_linux.go:117,152-158`,
which drops `nosuid` only for root with "allow setuid"). Neither that path nor
`/proc/self/exe` proves the file is ranikust (a marker may name any file,
and under a loader `/proc/self/exe` is the loader), so the path holds
ranikust itself only where `/proc/self/exe` is the file mapped at one of
ranikust's functions in `/proc/self/maps` (same device and inode) and the
path's file has its size and bytes. Where the maps name that file
otherwise than `fstat` (btrfs, overlayfs before Linux 6.8) and ranikust
runs from no memfd copy, no path holds it, and the advice falls back to
the install. Root alone may change the path when
`/`, each directory below it and the file are owned by root and written
by neither group nor others (an ACL that grants writing shows in the
group bits), none is a symlink, each is on a mount without `nosuid`
(udisks2 and fusermount always add it, an fstab `user` mount by default)
of a local filesystem, by `f_type` (ext2/3/4, xfs, btrfs, squashfs, erofs,
tmpfs, overlayfs, zfs; any other, such as FUSE, NFS, SMB, 9p, Ceph or AFS,
shows owners and modes a user or a server sets), and the file is a regular
one: each component is opened `O_PATH` from the one above without
following a symlink. A sticky directory such as `/tmp` fails, since any
user may take the name once it is gone; so does any error, a removed
binary (`(deleted)`) included. Any value of the switch but `1`, or none
(Ubuntu 22.04, other distributions), offers no profile. With the profile
loaded the binary keeps it through the sealed copy's `execveat`, which no
other profile attaches to, and so does a `RUN` (T2 smoke). Where
`kernel.apparmor_restrict_unprivileged_unconfined` is 0, 24.04's default,
any user's unconfined program may also enter the profile with
`aa-exec -p ranikust` (measured, T2 smoke), as it may any profile that
allows `userns`. What else a loaded profile reaches is in the
[threat model, section 6](../security/threat-model.md#6-accepted-risks).
The test hook `RANIKUST_DEBUG_FAULT=no-userns` gives the advice of a stop
in the user namespace.

### Owners the user namespace does not map

A build in a user namespace refuses an image with an owner, uid or gid, that
the namespace does not map, whether it maps one id or ranges; no flag lets a
failed `chown` pass
([ADR-0098](../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)).
The map is the process's own `/proc/self/uid_map` and `gid_map`: the one
ranikust wrote (probe step 2), or the container's, read by
`rnk_snapshot::UserNamespace`. In the initial namespace every id maps and
nothing is checked.

- **Base layers, before extraction.** `Snapshotter::check_owners`
  decompresses each base layer once more and reads its tar headers as the
  extraction frames them, noting each owner the extraction would set;
  `rnk-builder`'s pull calls it for all of an image's layers before any is
  extracted. Overlay mode skips a layer already extracted for this map
  ([ADR-0104](../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)).
  kaniko mode off `/` (a directory per stage) streams no base layer in a
  user namespace: it pulls the blobs, which are checked the same way
  before any is extracted. On `/`, where no blob is kept, a streamed layer
  is checked in the pass that spools and verifies it, before that layer is
  extracted, its paths the ignore list keeps skipped as the extraction
  skips them, so a base image's earlier layers may be on `/` already
  ([ADR-0137](../adr/0137-kaniko-mode-on-slash-checks-each-streamed-layers-owners-just-before-extracting-it.md));
  a `COPY --from=<image>` spools and checks all its layers, the ignored
  paths too, before any is unpacked: a spool or kept blob checked for a
  base is read again for it. Off `/` every path is checked, the ignored
  ones too; a cached layer is checked as a base layer is, before
  `apply_layer` extracts it
  ([ADR-0144](../adr/0144-kaniko-modes-owner-check-skips-the-ignored-paths-on-slash-only.md)).
- **What the build creates.** A `chown` that fails with `EINVAL` (a layer's
  entry, `COPY --chown`, a cache mount's owner, a secret file's) refuses at
  once, naming the file (a mount by its target). So does a `RUN`'s switch to
  its user (`setgroups`, `setgid`, `setuid`, for `USER`), naming the ids,
  where nothing else explains the `EINVAL`: in a user namespace other than the
  initial one, with no more supplementary groups than the kernel's
  `NGROUPS_MAX` (65536); otherwise the step's error stays.
- **The message** names up to eight uids and eight gids, each with the first
  path that has it, counts the other entries, and gives the map; a `RUN`'s
  switch names its uid, gid and up to eight supplementary groups, counting
  the rest. `ranikust` adds the ways out, by who made the namespace and
  from the ids its maps lack (a refused call does not tell which of its ids
  failed; `-1`, which `chown` reads as "unchanged", is none): with one id
  (a user without `newuidmap` and `newgidmap`, or without ranges), install
  `uidmap` (Ubuntu does not by default) and give the user a first range in
  `/etc/subuid` and `/etc/subgid`, in decimal with no leading 0, of at least
  as many ids as the highest one named, run as root, or run in a container;
  with a user's ranges, widen the first range to that count (ranikust maps
  only that one, and id N is its N-th); in a namespace ranikust was started
  in, or root's copy of its map (a single uid or gid where it maps no more,
  or where its ranges could not be mapped), widen its ranges (rootless
  Podman: its user's, then `podman system migrate`), or run as root on a
  host or in a rootful container; under root's `--userns`, the `ranikust`
  range's ways out ([sandbox](sandbox.md#--userns)). Each range asked for comes with the rule that
  it must not overlap another user's range or a real account's ids. Past 65536
  ids, `useradd`'s default count, no wider range is asked for: the ways
  out name the ids and say to run as root or in a container.

## Failure handling

- A probe step that fails with an unexpected errno is reported verbatim; the
  probe does not guess.
- The probe result is fixed for the whole build. A later mount failure in the
  chosen mode is a build error, not a reason to switch modes mid-build. Once
  the process is in the build's user namespace no other mode can follow in
  it, so a failure there ends the build (`Kind::Broken`), unless root's
  `--mode=auto` set up in a child, whose parent then builds in kaniko mode
  (ADR-0136).
- When an overlay-mode `RUN` fails with `EXDEV`, ranikust suggests rerunning
  with `--mode=kaniko` ([ADR-0016](../adr/0016-overlay-mount-options.md),
  [ADR-0023](../adr/0023-mode-flag.md)).

## Open questions

- [Q-018](https://github.com/tran4774/ranikust/issues/4) Where does the probe mount its test overlay?
- [Q-017](https://github.com/tran4774/ranikust/issues/3) Which type carries the probe outcome to the libraries?

## References

- [Survey 06](../research/survey/06-overlay-and-parallelism.md) §1;
  [survey 02](../research/survey/02-kaniko-mode-requirements.md) §1–§2.
- [Experiment 01](../research/experiments/01-overlay-semantics.md) §2 (privilege
  matrix, masked `/proc`, single-uid map),
  [experiment 02](../research/experiments/02-run-sandbox.md) §2 (minimum
  privileges), [experiment 03](../research/experiments/03-privilege-matrix.md),
  [experiment 11](../research/experiments/11-root-extras.md) (AppArmor
  profile, nesting), [experiment 13](../research/experiments/13-docker-rootful-vm.md)
  (Docker 29.1, cgroup v1 and v2, kernel 5.15).
