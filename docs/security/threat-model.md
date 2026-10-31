# Threat model

| | |
|---|---|
| **Status** | Design stage. No mitigation below is implemented yet; each is a requirement on the code. Evidence comes from the pre-code experiments. |
| **Scope** | The `ranikust` binary, its state directory and store, the build roots it makes there (or `/` in a container), and the processes it starts for `RUN`. |
| **Used by** | [`SECURITY.md`](../../SECURITY.md) (what is a vulnerability), reviewers and the `reviewer`, `host-safety` and `security-audit` agents (how severe a finding is). |
| **Keep in sync** | Update this file in the same pull request as any change to isolation, privileges, paths written, credential handling or store sharing. |

Evidence labels follow [`docs/research/TEMPLATE.md`](../research/TEMPLATE.md):
**[measured]**, **[code]**, **[external]**, **[inferred]**.

## 1. Why ranikust is security-sensitive

- It runs Dockerfile `RUN` commands as uid 0, outside containers as real root
  on the host, separated only by namespaces
  ([survey 02](../research/survey/02-kaniko-mode-requirements.md) §3).
- It extracts image layers, copies build contexts and parses Dockerfiles, all
  of which come from outside.
- In kaniko mode it empties a whole root filesystem between stages, and
  inside a container that root is `/`.
- It holds registry credentials and build secrets.
- Builds may run concurrently and share one store
  ([ADR-0007](../adr/0007-run-anywhere-shared-store.md)).

## 2. Assets

| ID | Asset | Harm if lost |
|---|---|---|
| A1 | Host files outside the state directory, store, requested outputs and, in kaniko mode in a container, `/` | Data loss or host compromise |
| A2 | The host's privileges: root, capabilities, the host's namespaces | Host compromise |
| A3 | Registry credentials (docker `config.json`, credential helpers, tokens) | Registry takeover, supply-chain attack |
| A4 | Build secrets (`RUN --mount=type=secret`, `--secret`) | Secret disclosure |
| A5 | The store and caches: blobs, extracted layers, cache mounts, registry cache | Poisoned images in later builds |
| A6 | The integrity of the output image | Shipping content the Dockerfile did not ask for |
| A7 | Availability of the host: disk, memory, processes | Denial of service |

## 3. Inputs and how much they are trusted

| Input | Trust |
|---|---|
| The operator: flags, store location, output paths | Trusted. ranikust protects the operator from typing mistakes (a wrong `--state-dir`: the store's marker refuses a foreign non-empty directory), not from intent. |
| Dockerfile and build context | The operator decides whether to build them ([ADR-0003](../adr/0003-dockerfile-trust-and-default-isolation.md)). `RUN` commands get the mode's isolation (§4). ranikust's own code that reads them (parser, `COPY`, `.dockerignore`) treats them as hostile. |
| Base images and layers from a registry | Hostile to ranikust's own code: layer entries may name `..`, absolute paths, symlinks, hardlinks, device nodes, setuid files and whiteouts aimed outside the root. |
| Registry responses | Hostile: verified by digest. |
| Other builds sharing the store | May belong to another trust domain; only content-addressed data is shared. |
| The kernel and the outer container runtime | Trusted; their bugs are out of scope. |

## 4. Trust boundaries by mode

The mode is chosen at startup by actually trying each step
([ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0004](../adr/0004-support-every-privilege-level.md),
[survey 06](../research/survey/06-overlay-and-parallelism.md) §1).

| Mode | Typical environment | What separates `RUN` from the host | Strength |
|---|---|---|---|
| Overlay, root | Host as root or via sudo; `--privileged` container; Podman with `CAP_SYS_ADMIN` | New mount, PID, UTS and IPC namespaces; `pivot_root` (or `MS_MOVE` + `chroot`); capabilities cut to Docker's 14 defaults for every `USER`; `PR_SET_PDEATHSIG`; optional seccomp and user namespace | About `docker build` with a root daemon: a kernel bug or a retained capability crosses it |
| Overlay, rootless | Podman rootless; regular user on Debian, Fedora, RHEL, Ubuntu up to 22.04 | As above, inside a user namespace: root in `RUN` maps to the invoking user | About rootless Podman: the invoking user's privileges at most |
| kaniko, mounts available | Root where overlay cannot be used (NFS, FUSE, no overlay module) | Same sandbox as overlay root, one directory per stage | As overlay root |
| kaniko, no mounts | Docker's default profile, ordinary Kubernetes pods | No boundary to the container: `RUN` runs in ranikust's own container on rootfs `/`, as in kaniko. ranikust's own state and credentials are kept out of reach (T21, [ADR-0051](../adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)) | The outer container only, for everything but ranikust's state |
| kaniko, no mounts, `--parallel-stages` | Same, opted in ([ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md)) | A bare `chroot` per stage, no `/proc` | None: a `RUN` escapes the `chroot` and reads or writes other stages' roots and secrets **[measured]** |
| None | Regular user on Ubuntu 24.04+ without an AppArmor profile; Kubernetes PSS restricted | ranikust refuses to start a build | — |

A bare `chroot` is not a boundary: root with `CAP_SYS_CHROOT` leaves it, and
run as root on a host it reaches the host's real `/` **[measured]**
([experiment 02](../research/experiments/02-run-sandbox.md),
[experiment 06a](../research/experiments/06a-root-rerun.md),
[experiment 13](../research/experiments/13-docker-rootful-vm.md) §1). It is
therefore used only where nothing better is possible: kaniko mode with
`--parallel-stages` inside a container that forbids mounts.

## 5. Threats and mitigations

| ID | Threat | Assets | Modes | Required mitigation | Evidence |
|---|---|---|---|---|---|
| T1 | A layer entry, `COPY` source or `WORKDIR` path escapes the root through `..`, an absolute symlink, a symlink chain or a whiteout aimed outside | A1, A6 | all | Resolve every path in a root with `openat2(RESOLVE_IN_ROOT \| RESOLVE_NO_MAGICLINKS)` through `rnk_rootfs::RootPath`; never join strings onto a host path | [survey 02](../research/survey/02-kaniko-mode-requirements.md) §4; `RootPath` escape suite, T0 (`crates/rnk-rootfs/tests/escape.rs`); `copy` with symlinks out of both roots, T0 (`crates/rnk-rootfs/src/copy/tests.rs`) [measured] |
| T2 | A hardlink entry links to a host file, so later writes change it | A1 | all | Resolve hardlink targets inside the root; refuse targets outside it | [survey 04](../research/survey/04-risks-and-roadmap.md) §4; `extract` refuses targets climbing out and links inside the root only; `extract_overlay` copies a target from the lower layers, walked by name without following symlinks, T0 (`crates/rnk-rootfs/src/extract/tests.rs`) [measured] |
| T3 | A race between check and use: a `RUN` process or another stage swaps a directory for a symlink | A1 | all, mostly parallel stages | Operate relative to open directory descriptors with `O_NOFOLLOW` and `RESOLVE_*` flags; never re-resolve a checked path | `RootPath`, `extract`, `delete_contents` and `copy`: a thread swapping a directory for a symlink out, T0 (`crates/rnk-rootfs/tests/escape.rs`, `src/*/tests.rs`) [measured]; `RUN` and stage races not yet tested |
| T4 | Stage cleanup deletes user data | A1 | kaniko | No operator-named build root: ranikust builds on `/` only in a container (ADR-0048), else in `<state>/builds/<id>/rootfs`, a directory it made (ADR-0050); the state directory is owned by the user, `0700`, below directories only root may write, checked by descriptor; the state directory accepted only new, empty or marked `.ranikust-state`, never `/`, before its mode is set; `flock` on a descriptor of `/` in a container, whatever the state directory; delete through descriptors without following symlinks or crossing mounts, comparing the mount ID, since `st_dev` misses bind mounts of the same filesystem; keep the state directory (`/var/lib/ranikust` in a container) on the ignore list, never walked or deleted ([ADR-0022](../adr/0022-common-state-dir.md)) | [survey 02](../research/survey/02-kaniko-mode-requirements.md) §4; [experiment 09](../research/experiments/09-kaniko-mode-prototype.md) [measured] |
| T5 | `RUN` escapes a bare `chroot` | A1, A2 | overlay, kaniko with mounts | `pivot_root`, or `MS_MOVE` + `chroot` only where seccomp refuses `pivot_root` with `EPERM` and `/` is a mount root, so `..` stops at the moved root ([Q-077](../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape)). Kaniko mode outside a container where root may not mount, where `RUN` would run in a bare `chroot` on the host, is refused ([ADR-0050](../adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md), Q-330); on `/` in a container `RUN` runs directly, the container being the boundary | [experiment 02](../research/experiments/02-run-sandbox.md) [measured]; the `MS_MOVE` case [inferred] |
| T6 | `RUN` keeps `CAP_SYS_ADMIN`, unmounts a bind and writes the real file; a setuid binary regains capabilities from a full bounding set | A1, A2 | overlay, kaniko with mounts | Drop the bounding and inheritable sets to Docker's 14 defaults for every `USER`, root included | [experiment 02](../research/experiments/02-run-sandbox.md) [measured] |
| T7 | A descriptor ranikust holds (root, store, lock, socket) is inherited by `RUN` and opens a path outside the root | A1, A3 | all | `O_CLOEXEC` on everything; close every descriptor above stdio before `execve`; a test that lists `/proc/self/fd` inside `RUN` | [inferred] |
| T8 | A `RUN` process outlives a killed build and keeps writing to the root | A1, A6 | all | `PR_SET_PDEATHSIG(SIGKILL)` set from a thread that lives for the whole build, then a re-check that the parent is alive (the parent pid in the child's outer `/proc/self/stat`, since `getppid` is 0 in a new PID namespace); armed again after `setresuid`/`setresgid`, which clear it; `RUN` is PID 1 of its own PID namespace | [experiment 02](../research/experiments/02-run-sandbox.md) [measured]; the re-arm [inferred] |
| T9 | Mounts made for a build propagate to the host or outlive the build | A1, A7 | overlay, kaniko with mounts | Enter a private mount namespace while still single-threaded and make `/` a slave; all build mounts live there and vanish with the process | [experiment 02](../research/experiments/02-run-sandbox.md) [measured] |
| T10 | Stub mountpoints or files ranikust adds (`resolv.conf`, `hosts`, fake `/proc`) leak into a layer or survive a crash; copying them changes the parent's mtime so `/etc` lands in every layer | A6 | all | Journal stubs in the state directory before creating them; remove them before the diff; restore the parent's metadata; clean up from the journal at startup | [experiment 02](../research/experiments/02-run-sandbox.md), [experiment 12](../research/experiments/12-kaniko-mode-parallel.md) §4 [measured] |
| T11 | Registry credentials or build secrets reach logs, layers, the image config, cache keys, error messages, the `RUN` environment or a kernel keyring `RUN` can read | A3, A4 | all | Redact credentials in `Debug` and `Display`; give secrets only to the `RUN` that asks: as a read-only file (`0400`, root's, unless the mount asks otherwise), or with `env=` as a variable of that `RUN` alone, as BuildKit (ADR-0032); never on the command line; write a secret file only on the `RUN`'s own tmpfs, mounted by the child in its mount namespace, never on a disk (BuildKit's secret mount); keep values out of errors and `Debug` (`RunSpec`, `SecretFile`, `RunMounts`); give each `RUN` a new session keyring (root's user and persistent keyrings stay reachable until `--seccomp` denies `keyctl`, as Docker's profile does); send registry credentials over plain HTTP only to a registry the operator typed (`--insecure-registry`, or a destination under `--insecure`; `ClientOptions::http_credentials`, ADR-0036): a loopback registry is plain HTTP unasked (ADR-0032, as BuildKit) and a registry only `--insecure-pull` allows comes from the untrusted Dockerfile, so both get no credentials and a refusal names the flags, since a `RUN` shares the host network namespace and could sniff `lo` or serve the port; `localhost` resolves to loopback in ranikust's own requests; `--verbosity=trace` logs other crates at `info` at most, so a dependency's request internals never reach the log | [inferred]; `rnk-registry` tests `credentials_never_go_in_clear_to_an_unlisted_loopback_registry`, `an_insecure_registry_not_named_for_credentials_gets_none`; `rnk-metrics` `dependencies_log_at_info_at_most` |
| T12 | With `--parallel-stages`, one stage reads another stage's root and secrets | A4, A6 | kaniko without mounts | Sequential by default; warn at build start when parallel stages are enabled; document it | [experiment 12](../research/experiments/12-kaniko-mode-parallel.md) §3 [measured], [ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md) |
| T13 | A build poisons the shared store: a blob that does not match its digest, a half-written layer after a crash, a cache-key collision | A5, A6 | all | Verify digests when writing and reading; extract to a temporary directory and `rename` atomically; per-digest locks; versioned cache keys | [ADR-0005](../adr/0005-own-versioned-cache-key.md), [ADR-0006](../adr/0006-persistent-store.md) [inferred] |
| T14 | Cache mounts (`RUN --mount=type=cache`) are writable by every build that names them | A5 | all | Shared by design; give each trust domain its own store | [inferred] |
| T15 | Decompression bombs, huge or very many files, deep trees, fork bombs | A7 | all | Stream instead of buffering; fail cleanly on exhaustion; fork bombs are bounded only by the outer cgroup | [survey 04](../research/survey/04-risks-and-roadmap.md) §4 [inferred] |
| T16 | Hostile input makes ranikust panic; the release build aborts mid-build | A6, A7 | all | No panics reachable from input; fuzz the parser, the ignore matcher and tar extraction; cleanup that survives an abort (T8–T10) | [inferred] |
| T17 | `RUN` as uid 0 writes a kernel interface of a fresh `proc`: a sysctl such as `kernel/core_pattern` or `kernel/modprobe` (the host kernel then runs a `RUN` binary as host root) or `/proc/sysrq-trigger`; writing needs uid 0, no capability | A1, A2 | overlay, kaniko with mounts | Docker's read-only paths (`/proc/sys`, `sysrq-trigger`, `irq`, `bus`, `fs`) and masked paths (`kcore`, `keys`, `timer_list`, `/sys/firmware`…) after mounting `proc` and `sysfs`. Kaniko mode on `/` in a container where root may mount (`--privileged`), whose `Direct` `RUN` would see the host's devices and a writable `/proc/sys`, is refused, and so is one whose `/proc/sys` is writable without that (`systempaths=unconfined`, `procMount: Unmasked`) (Q-341). Host devices passed with `--device` stay readable by a `RUN` there: the operator's choice, which ranikust cannot see | host-safety review of `rnk-sandbox`, moby `daemon/pkg/oci/defaults.go` [inferred] |
| T18 | A device node opens a host device: one `RUN` creates with `mknod` (`CAP_MKNOD` is a Docker default) or one from a layer, with no device cgroup to stop it; or `RUN` changes the mode of a host node bound into `/dev` | A1, A2 | overlay, kaniko with mounts | `nodev` on the root's bind, every `RUN --mount` bind and the `/dev` tmpfs; the `/dev` nodes are read-only binds of the outer ones, so I/O works and `chmod` or `chown` fails | host-safety review of `rnk-sandbox` [inferred] |
| T19 | `RUN` uses the operator's terminal: reads keystrokes, or pushes input into the operator's shell with `TIOCSTI` | A1, A3 | all | `setsid` before `execve` (no controlling terminal); stdin `/dev/null` by default, as Docker | host-safety review of `rnk-sandbox` [inferred] |
| T20 | A `RUN` reaches network-configuration code of the kernel with capabilities over its network namespace: without `CAP_NET_ADMIN`, `--allow network.none-userns` puts a `--network=none` `RUN` in a network namespace owned by a user namespace of uid 0, where its root holds every capability, so it could reach nf_tables, x_tables, xfrm, IPVS or traffic control, or join that user namespace through `ioctl(NS_GET_USERNS)` and `setns` | A1, A7 | overlay, with the flag | Refused by default. With the flag, a seccomp filter installed after every mount refuses `setns`, `unshare`/`clone` with `CLONE_NEWUSER`, `clone3` (`ENOSYS`), `io_uring`, `socket(AF_KEY)`, netlink other than route, sock_diag, audit and uevent (`EPROTONOSUPPORT`), the x_tables, arp_tables, ebtables, IPVS, IPsec policy and multicast-routing socket options of `SOL_IP` and `SOL_IPV6` (not `IPV6_RECVTCLASS`/`IPV6_TCLASS`, which share ip6_tables' numbers), `TCP_REPAIR`, and syscalls of another architecture. Residual: every other `CAP_NET_ADMIN`-gated interface in that namespace stays open, rtnetlink first, including `ip link add type <kind>` loading `rtnl-link-<kind>` modules into the host kernel (ADR-0033) | `rnk-sandbox` T0 `the_filter_refuses_what_it_should_and_nothing_else`; VM smoke `tests/smoke/netns-check.py` |
| T21 | In kaniko mode on `/` (no mounts, one PID namespace, `RUN` as uid 0) a `RUN` reads or tampers with ranikust's state (pulled and encoded blobs), its descriptors, environment or memory through `/proc/<pid>/`, or leaves a process that does so during the walk, the export or the push | A3, A5, A6 | kaniko without mounts, on `/` | ([ADR-0051](../adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)) No path for state: blobs and encoded layers are anonymous files (`O_TMPFILE` in the state directory, `memfd` where unsupported), held by descriptor, the state directory left empty; ranikust is not dumpable (`PR_SET_DUMPABLE 0`, first in `main`, and a soft `RLIMIT_CORE` of 0), so `/proc/<ranikust>/{fd,environ,mem,maps,root}` need `CAP_SYS_PTRACE`; the `RUN` child drops every capability outside Docker's defaults from its bounding set, and the `RUN` is refused when any such capability remains without `CAP_SETPCAP` to drop it (`SYS_ADMIN` could mount over `/proc`, `BPF` with `PERFMON` read kernel memory, `SYS_MODULE`, `SYS_RAWIO`); the child always clears its ambient and inheritable sets and keeps only Docker's defaults in its permitted and effective sets; ranikust is a child subreaper and, after each `RUN`, kills the `RUN`'s process tree through pidfds after checking each process's parent and start time again (no pid reuse), and reaps by pid without blocking; the `RUN` environment is the Dockerfile's only; registry credentials go only over verified TLS (`ClientOptions::verified_credentials_only`), since a `RUN` may edit `/etc/hosts` or `/etc/resolv.conf`; kaniko mode on `/` where root may mount, or whose `/proc/sys` is not read-only (`statvfs`), is refused (Q-341); where seccomp refuses `pidfd_open`, only ranikust's own children get a plain `kill` (their pids are not reused before ranikust reaps them), deeper processes on later rounds once reparented. Not closed, warned: `/proc/<pid>/cmdline` (a `--build-arg` value), another process holding the credential variables or a `--secret` or `--build-arg` read from the environment (a CI shell), files mounted into the container. Not closed: anonymous requests (and any request to a plain-HTTP or unverified registry) can still be redirected by a `RUN` that edits the resolver files | VM `secrets` suite (default Docker, `--cap-add SYS_PTRACE`, started from `sh -c`), T2 `a_direct_run_leaves_no_process_behind`, `a_direct_run_on_slash_keeps_only_dockers_capabilities` [measured] |

## 6. Accepted risks

These follow from decisions and are documented to users rather than fixed:

- `RUN` runs as root inside its sandbox, and the sandbox is about as strong
  as `docker build` ([ADR-0003](../adr/0003-dockerfile-trust-and-default-isolation.md)).
- kaniko mode inside a container that forbids mounts has no isolation
  between `RUN` and the container ([ADR-0002](../adr/0002-two-snapshot-modes.md)).
  ranikust keeps its own state and credentials out of `RUN`'s reach (T21,
  ADR-0051), except what it can only warn about: a value on its command
  line (`/proc/<pid>/cmdline` is readable whatever `PR_SET_DUMPABLE` says),
  credentials another process of the container holds in its environment
  (a CI shell that started ranikust: run ranikust as the container's PID 1),
  and any file mounted into the container (a `config.json`, a
  service-account token: pass registry credentials in `DOCKER_AUTH_CONFIG`).
  A root `RUN` can still list the descriptor numbers of
  `/proc/<ranikust>/fd`, whose entries a non-dumpable process gives to root,
  but neither read their targets nor open them; before Linux 6.0 it can
  also read `/proc/<ranikust>/fdinfo/<n>` (position, flags, inode, mount
  id, sizes; no contents). The build context, `.dockerignore`d files such
  as `.env` or `.npmrc` included, every `--secret src=` file and a mounted
  `config.json` are files on `/` like any other: a `RUN` reads them; pass
  secrets and credentials in the environment of ranikust started as PID 1.
  A `RUN` may edit `/etc/hosts` and `/etc/resolv.conf` (bind mounts the
  ignore list keeps) and so redirect ranikust's later requests: credentials
  go only over verified TLS there, but an anonymous pull of a tag, or any
  request to a plain-HTTP or unverified registry, can reach a host of the
  `RUN`'s choosing. With `kernel.yama.ptrace_scope` 0 a `RUN` process may
  trace other dumpable processes of its user (ranikust warns). Nothing of ranikust's
  state outlives a kaniko-mode build on `/`.
- Until `--seccomp` (Phase 3), a root `RUN` in the full sandbox can use
  uid 0's user and persistent keyrings, which Docker's seccomp profile
  denies (T11).
- `RUN` shares the host network by default
  ([ADR-0010](../adr/0010-run-network-host-or-none.md)), as kaniko's does; it
  can reach services that listen on the host and the cloud metadata
  service (`169.254.169.254`, `fd00:ec2::254`), which hands out the node's or
  pod's cloud credentials. The maintainer accepted this on 2026-10-31
  rather than block the metadata addresses by default: ranikust does not
  filter `RUN`'s traffic. Mitigate outside ranikust: a network policy that
  denies the metadata addresses to the build pod, IMDSv2 with a hop limit
  of 1 so a container cannot reach it, workload identity instead of node
  credentials, or `--network=none` for builds that need no network.
- A registry listed with `--insecure-registry`, or a destination under
  `--insecure`, gets its credentials over plain HTTP, which a `RUN` sharing
  the host network could observe on the path; kaniko (go-containerregistry)
  does the same for its insecure registries. Unlisted loopback registries
  and registries only `--insecure-pull` allows are plain HTTP without
  credentials (T11, ADR-0036). `--skip-tls-verify*` skips verification for
  everything sent to that registry, its token service included, so a
  credential exchanged there goes to whoever answers. The `oci-client` requests resolve `localhost` with the
  system resolver; only ranikust's own requests pin it to loopback.
- Some environments get no mode at all and are refused
  ([ADR-0004](../adr/0004-support-every-privilege-level.md)).

## 7. Not verified yet

From [survey 00](../research/survey/00-summary.md) and
[survey 04](../research/survey/04-risks-and-roadmap.md) §4; each needs a test
before 1.0:

- the host-safety attack suite: path traversal and symlink or hardlink
  escapes in layers, `RUN mount`, `setsid sleep 1000 &`, a fork bomb under a
  cgroup limit, writes to `/etc/resolv.conf`, a `--state-dir` at `/`,
  below `/home/<user>` or in a directory others may write, each followed by a check that the host is unchanged
  and no mount or process is left;
- races during extraction and deletion, and concurrent builds on one store,
  including a crash halfway;
- `--seccomp` and `--userns` end to end;
- git contexts (Phase 5, ADR-0035): an untrusted repository or context
  argument must not run code on the host through the external `git`
  (hooks, `core.fsmonitor`, `ext::` and `file://` submodules, a ref or URL
  starting with `-`, host configuration read unasked), and a clone is read
  only through `RootPath`;
- overlay stage roots as real root (T2 VM, `rnk-snapshot`'s
  `tests/overlay.rs`): mounting, unmounting and releasing leave no mount
  under the state directory, and the read-only view refuses writes. Not
  yet tested: an upper directory a hostile `RUN` filled (planted symlinks,
  FIFOs and devices, deep trees) read by the exporter, whose reads go
  through `RootPath` without following or opening them; a stage released
  while a `RUN` still holds files open under its mount (`MNT_DETACH`);
- the `RUN` sandbox as real root (T2 VM): T8 with a non-root `USER`, T18
  for a layer's device node and `chmod /dev/null`. Checked by `just test-vm
  smoke` on kernels 5.15 and 6.8: T5's `chroot` escapes under `MS_MOVE`
  (Docker's seccomp profile) and `pivot_root`, T17 (a `/proc/sys` write
  fails), T18 for a node the `RUN` makes with `mknod`, T19;
- real Kubernetes (containerd's AppArmor profile, `procMount`,
  `hostUsers: false`, Pod Security Standards) and SELinux.

## 8. Rating findings

| Severity | Examples |
|---|---|
| Critical | Untrusted input makes ranikust write or delete a host file outside its directories (T1–T4); `RUN` reaches the host from a mode that promises isolation (T5, T6, T9) |
| High | Credential or secret leak (T11); descriptor or capability leak into `RUN` (T7); store or cache poisoning across builds (T13) |
| Medium | Orphan processes or leftover stubs and mounts (T8, T10); denial of service from untrusted input with default settings (T15, T16) |
| Low | Host metadata such as the hostname or timestamps leaking into image content |

Anything listed in §6, or anything that needs the operator to pass a
dangerous flag knowingly, is not a vulnerability; it may still be a
documentation bug.
