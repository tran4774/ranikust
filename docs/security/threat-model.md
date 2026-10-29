# Threat model

| | |
|---|---|
| **Status** | Design stage. No mitigation below is implemented yet; each is a requirement on the code. Evidence comes from the pre-code experiments. |
| **Scope** | The `ranikust` binary, its workdir, state directory and store, and the processes it starts for `RUN`. |
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
| A1 | Host files outside the workdir, state directory, store and requested outputs | Data loss or host compromise |
| A2 | The host's privileges: root, capabilities, the host's namespaces | Host compromise |
| A3 | Registry credentials (docker `config.json`, credential helpers, tokens) | Registry takeover, supply-chain attack |
| A4 | Build secrets (`RUN --mount=type=secret`, `--secret`) | Secret disclosure |
| A5 | The store and caches: blobs, extracted layers, cache mounts, registry cache | Poisoned images in later builds |
| A6 | The integrity of the output image | Shipping content the Dockerfile did not ask for |
| A7 | Availability of the host: disk, memory, processes | Denial of service |

## 3. Inputs and how much they are trusted

| Input | Trust |
|---|---|
| The operator: flags, workdir, store location, output paths | Trusted. ranikust protects the operator from typing mistakes (wrong `--workdir`), not from intent. |
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
| kaniko, no mounts | Docker's default profile, ordinary Kubernetes pods | None. `RUN` runs in ranikust's own container on rootfs `/`, as in kaniko | The outer container only |
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
| T1 | A layer entry, `COPY` source or `WORKDIR` path escapes the root through `..`, an absolute symlink, a symlink chain or a whiteout aimed outside | A1, A6 | all | Resolve every path in a root with `openat2(RESOLVE_IN_ROOT \| RESOLVE_NO_MAGICLINKS)` through `rnk_rootfs::RootPath`; never join strings onto a host path | [survey 02](../research/survey/02-kaniko-mode-requirements.md) §4; `RootPath` escape suite, T0 (`crates/rnk-rootfs/tests/escape.rs`) [measured] |
| T2 | A hardlink entry links to a host file, so later writes change it | A1 | all | Resolve hardlink targets inside the root; refuse targets outside it | [survey 04](../research/survey/04-risks-and-roadmap.md) §4; `extract` refuses targets climbing out and links inside the root only, T0 (`crates/rnk-rootfs/src/extract/tests.rs`) [measured] |
| T3 | A race between check and use: a `RUN` process or another stage swaps a directory for a symlink | A1 | all, mostly parallel stages | Operate relative to open directory descriptors with `O_NOFOLLOW` and `RESOLVE_*` flags; never re-resolve a checked path | `RootPath` only: a thread swapping a directory for a symlink out, T0 (`crates/rnk-rootfs/tests/escape.rs`) [measured]; `RUN` and stage races not yet tested |
| T4 | A wrong `--workdir` makes the stage cleanup delete user data | A1 | kaniko | Refuse `/` and ancestors of system directories, compared by device and inode; require a marker file in a non-empty workdir; `flock` the workdir; delete through descriptors without following symlinks or crossing mounts, comparing the mount ID, since `st_dev` misses bind mounts of the same filesystem; keep the state directory (`/ranikust/state` in a container) on the ignore list, never walked or deleted ([ADR-0022](../adr/0022-common-state-dir.md)) | [survey 02](../research/survey/02-kaniko-mode-requirements.md) §4; [experiment 09](../research/experiments/09-kaniko-mode-prototype.md) [measured] |
| T5 | `RUN` escapes a bare `chroot` | A1, A2 | overlay, kaniko with mounts | `pivot_root`, or `MS_MOVE` + `chroot` only where seccomp refuses `pivot_root` with `EPERM` and `/` is a mount root, so `..` stops at the moved root ([Q-077](../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape)) | [experiment 02](../research/experiments/02-run-sandbox.md) [measured]; the `MS_MOVE` case [inferred] |
| T6 | `RUN` keeps `CAP_SYS_ADMIN`, unmounts a bind and writes the real file; a setuid binary regains capabilities from a full bounding set | A1, A2 | overlay, kaniko with mounts | Drop the bounding and inheritable sets to Docker's 14 defaults for every `USER`, root included | [experiment 02](../research/experiments/02-run-sandbox.md) [measured] |
| T7 | A descriptor ranikust holds (workdir, store, lock, socket) is inherited by `RUN` and opens a path outside the root | A1, A3 | all | `O_CLOEXEC` on everything; close every descriptor above stdio before `execve`; a test that lists `/proc/self/fd` inside `RUN` | [inferred] |
| T8 | A `RUN` process outlives a killed build and keeps writing to the root | A1, A6 | all | `PR_SET_PDEATHSIG(SIGKILL)` set from a thread that lives for the whole build, then a re-check that the parent is alive (the parent pid in the child's outer `/proc/self/stat`, since `getppid` is 0 in a new PID namespace); armed again after `setresuid`/`setresgid`, which clear it; `RUN` is PID 1 of its own PID namespace | [experiment 02](../research/experiments/02-run-sandbox.md) [measured]; the re-arm [inferred] |
| T9 | Mounts made for a build propagate to the host or outlive the build | A1, A7 | overlay, kaniko with mounts | Enter a private mount namespace while still single-threaded and make `/` a slave; all build mounts live there and vanish with the process | [experiment 02](../research/experiments/02-run-sandbox.md) [measured] |
| T10 | Stub mountpoints or files ranikust adds (`resolv.conf`, `hosts`, fake `/proc`) leak into a layer or survive a crash; copying them changes the parent's mtime so `/etc` lands in every layer | A6 | all | Journal stubs in the state directory before creating them; remove them before the diff; restore the parent's metadata; clean up from the journal at startup | [experiment 02](../research/experiments/02-run-sandbox.md), [experiment 12](../research/experiments/12-kaniko-mode-parallel.md) §4 [measured] |
| T11 | Registry credentials or build secrets reach logs, layers, the image config, cache keys, error messages, the `RUN` environment or a kernel keyring `RUN` can read | A3, A4 | all | Redact credentials in `Debug` and `Display`; mount secrets as files only for the `RUN` that asks; never pass them through the environment or command line; never write them outside the secrets directory; give each `RUN` a new session keyring (root's user and persistent keyrings stay reachable until `--seccomp` denies `keyctl`, as Docker's profile does) | [inferred] |
| T12 | With `--parallel-stages`, one stage reads another stage's root and secrets | A4, A6 | kaniko without mounts | Sequential by default; warn at build start when parallel stages are enabled; document it | [experiment 12](../research/experiments/12-kaniko-mode-parallel.md) §3 [measured], [ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md) |
| T13 | A build poisons the shared store: a blob that does not match its digest, a half-written layer after a crash, a cache-key collision | A5, A6 | all | Verify digests when writing and reading; extract to a temporary directory and `rename` atomically; per-digest locks; versioned cache keys | [ADR-0005](../adr/0005-own-versioned-cache-key.md), [ADR-0006](../adr/0006-persistent-store.md) [inferred] |
| T14 | Cache mounts (`RUN --mount=type=cache`) are writable by every build that names them | A5 | all | Shared by design; give each trust domain its own store | [inferred] |
| T15 | Decompression bombs, huge or very many files, deep trees, fork bombs | A7 | all | Stream instead of buffering; fail cleanly on exhaustion; fork bombs are bounded only by the outer cgroup | [survey 04](../research/survey/04-risks-and-roadmap.md) §4 [inferred] |
| T16 | Hostile input makes ranikust panic; the release build aborts mid-build | A6, A7 | all | No panics reachable from input; fuzz the parser, the ignore matcher and tar extraction; cleanup that survives an abort (T8–T10) | [inferred] |
| T17 | `RUN` as uid 0 writes a kernel interface of a fresh `proc`: a sysctl such as `kernel/core_pattern` or `kernel/modprobe` (the host kernel then runs a `RUN` binary as host root) or `/proc/sysrq-trigger`; writing needs uid 0, no capability | A1, A2 | overlay, kaniko with mounts | Docker's read-only paths (`/proc/sys`, `sysrq-trigger`, `irq`, `bus`, `fs`) and masked paths (`kcore`, `keys`, `timer_list`, `/sys/firmware`…) after mounting `proc` and `sysfs` | host-safety review of `rnk-sandbox`, moby `daemon/pkg/oci/defaults.go` [inferred] |
| T18 | A device node opens a host device: one `RUN` creates with `mknod` (`CAP_MKNOD` is a Docker default) or one from a layer, with no device cgroup to stop it; or `RUN` changes the mode of a host node bound into `/dev` | A1, A2 | overlay, kaniko with mounts | `nodev` on the root's bind, every `RUN --mount` bind and the `/dev` tmpfs; the `/dev` nodes are read-only binds of the outer ones, so I/O works and `chmod` or `chown` fails | host-safety review of `rnk-sandbox` [inferred] |
| T19 | `RUN` uses the operator's terminal: reads keystrokes, or pushes input into the operator's shell with `TIOCSTI` | A1, A3 | all | `setsid` before `execve` (no controlling terminal); stdin `/dev/null` by default, as Docker | host-safety review of `rnk-sandbox` [inferred] |

## 6. Accepted risks

These follow from decisions and are documented to users rather than fixed:

- `RUN` runs as root inside its sandbox, and the sandbox is about as strong
  as `docker build` ([ADR-0003](../adr/0003-dockerfile-trust-and-default-isolation.md)).
- kaniko mode inside a container that forbids mounts has no isolation
  between `RUN` and ranikust ([ADR-0002](../adr/0002-two-snapshot-modes.md)).
- Until `--seccomp` (Phase 3), a root `RUN` in the full sandbox can use
  uid 0's user and persistent keyrings, which Docker's seccomp profile
  denies (T11).
- `RUN` shares the host network by default
  ([ADR-0010](../adr/0010-run-network-host-or-none.md)); it can reach services
  that listen on the host or on a cloud metadata address.
- Some environments get no mode at all and are refused
  ([ADR-0004](../adr/0004-support-every-privilege-level.md)).

## 7. Not verified yet

From [survey 00](../research/survey/00-summary.md) and
[survey 04](../research/survey/04-risks-and-roadmap.md) §4; each needs a test
before 1.0:

- the host-safety attack suite: path traversal and symlink or hardlink
  escapes in layers, `RUN mount`, `setsid sleep 1000 &`, a fork bomb under a
  cgroup limit, writes to `/etc/resolv.conf`, `--workdir` at `/`, `/home` or
  an unmarked directory, each followed by a check that the host is unchanged
  and no mount or process is left;
- races during extraction and deletion, and concurrent builds on one store,
  including a crash halfway;
- `--seccomp` and `--userns` end to end;
- the `RUN` sandbox as real root (T2 VM): T5 under Docker's seccomp profile,
  T8 with a non-root `USER`, T17 (`/proc/sys` writes fail), T18 (`mknod`
  and a layer's device node do not open, `chmod /dev/null` fails), T19;
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
