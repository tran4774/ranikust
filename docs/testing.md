# Testing

ranikust runs `RUN` as root, mounts filesystems and deletes root filesystems,
and it must behave correctly at every privilege level
([ADR-0004](adr/0004-support-every-privilege-level.md)). Tests are therefore
split into tiers by the privileges they need, and the tiers that need more
than a regular user never run on a developer's machine.

## Tiers

| Tier | Contents | Privileges | Runs on | Command | Status |
|---|---|---|---|---|---|
| T0 | Unit tests and in-process tests: parsing, matching, encoding, path resolution on temp dirs | None | Any Linux machine, CI | `just test` | In use |
| T1 | Rootless integration: user namespaces, rootless overlay (`userxattr`), the `RUN` sandbox in a user namespace, Podman rootless | Regular user with entries in `/etc/subuid` and `/etc/subgid` | Developer machine | `just smoke` (below); TODO: `just test-rootless` | Smoke test only |
| T2 | Privileged integration: overlay as real root, `pivot_root`, `mknod`, Docker rootful variants, kaniko mode on `/`, the host-safety attack suite | root | A disposable VM, never the host | `just test-vm` ([below](#the-vm-harness-tier-t2)) | `unit` and `smoke` suites |
| T3 | CI on every pull request: `fmt`, `clippy`, T0, static musl build, aarch64 check, aarch64 T0 under qemu-user, cargo-deny, rustdoc, mdBook | None | GitHub Actions (`.github/workflows/ci.yml`) | `just ci` | In use |

### Rules

- T0 tests need no privileges and no network, and write only below a fresh
  temporary directory, which they remove on failure too.
- A test that needs more than T0 is marked `#[ignore = "T1: <reason>"]` or
  `#[ignore = "T2: <reason>"]`. Its harness runs it with
  `cargo test -- --ignored` inside the right environment. Never remove the
  `ignore` to run it by hand, and never run a T2 test on the host.
- Running the `ranikust` binary in kaniko mode with rootfs `/` deletes that
  filesystem. Outside the VM, run it only with a `--workdir` under a fresh
  temporary directory.
- A bug fix comes with a test that fails without it.
- `just test <filter>` passes a name filter to `cargo test`; for one crate,
  use `cargo test -p <crate> <filter>`.

## Planned test suites

From the verification plan of
[survey 04](research/survey/04-risks-and-roadmap.md) §4 and the open items of
[survey 00](research/survey/00-summary.md):

| Suite | What it checks | Tier |
|---|---|---|
| Parser golden tests | Each Dockerfile in kaniko's `integration/dockerfiles/` (200 Dockerfiles) parses and expands to the same instructions as BuildKit's parser | T0 |
| `.dockerignore` | Same results as moby's `patternmatcher` | T0 |
| Property and fuzz tests | Parser, ignore matcher, tar write/read round trips, cache-key determinism | T0 |
| Image content vs kaniko | Each image flattened to `(path, type, mode, uid, gid, size, sha256, link target, xattrs)` plus its config, compared with kaniko's | T1/T2 |
| Mode equivalence | Overlay and kaniko mode give the same flattened content, apart from the accepted differences of [ADR-0012](adr/0012-accept-divergence-between-modes.md); `--jobs 1` and the default give identical digests | T1/T2 |
| Privilege matrix | In each environment of the table below, ranikust picks the expected mode or refuses with guidance | T1/T2 |
| Host safety | Layers with `../etc/passwd`, symlinks and hardlinks pointing out; `RUN mount`, `setsid sleep 1000 &`, a fork bomb under a cgroup limit, writes to `/etc/resolv.conf`; `--workdir` at `/`, `/home` and an unmarked directory. After each case: the host is unchanged, and no mount or process is left | T2 |

### Environments for the privilege matrix

From [survey 06](research/survey/06-overlay-and-parallelism.md) §1 and
[experiment 13](research/experiments/13-docker-rootful-vm.md):

| Environment | Expected mode | Tier | Evidence |
|---|---|---|---|
| Host as root | overlay | T2 | measured ([experiment 06a](research/experiments/06a-root-rerun.md)) |
| Docker `--privileged` | overlay | T2 | measured (experiment 13) |
| Docker `--cap-add SYS_ADMIN` | kaniko (AppArmor blocks mounts) | T2 | measured (experiment 13) |
| Docker with seccomp and AppArmor unconfined | overlay rootless | T2 | measured (experiment 13) |
| Docker default, cgroup v2 and v1 | kaniko | T2 | measured (experiment 13) |
| Podman rootless default | overlay rootless | T1 | measured ([experiment 01](research/experiments/01-overlay-semantics.md)) |
| Podman `--cap-add SYS_ADMIN` | overlay | T1 | measured ([experiment 02](research/experiments/02-run-sandbox.md)) |
| Regular user, Debian or Fedora | overlay rootless | T1 | inferred |
| Regular user, Ubuntu 24.04+, no AppArmor profile | refuse with guidance | T2 (needs that distribution) | measured ([experiment 11](research/experiments/11-root-extras.md)) |
| Kubernetes pod, Pod Security Standards restricted | refuse with guidance | not covered yet | inferred |

## The VM harness (tier T2)

`just test-vm [suite]...` (`tests/vm/vm.sh`) runs the T2 suites as real
root in a disposable QEMU/KVM VM, following
[experiment 13](research/experiments/13-docker-rootful-vm.md) §6. Nothing
runs as root on the host:

1. **Build** on the host, as the user: the static musl test binaries of the
   crates that hold `#[ignore = "T2: ..."]` tests (`unit`) and the
   `smoke-build` example (`smoke`), with `cargo zigbuild`.
2. **Fetch** the Ubuntu cloud image of `RANIKUST_VM_RELEASE` (`noble`,
   kernel 6.8, by default; `jammy`, kernel 5.15) and check its SHA-256
   against the published `SHA256SUMS`. The verified image is cached in
   `~/.cache/ranikust/vm` (`RANIKUST_VM_CACHE`).
3. **Prepare** a run directory under `target/vm/`: a copy-on-write qcow2
   overlay on the cached image, a fresh SSH key and a NoCloud seed
   (`tests/vm/cloud-init/user-data`: user `rnk` with passwordless `sudo`,
   Docker from the Ubuntu archive).
4. **Boot** QEMU as the invoking user (`/dev/kvm` through its group or an
   ACL, never `sudo`), user-mode networking, SSH forwarded to a free port
   on `127.0.0.1` only.
5. **Copy in** the binaries and `tests/vm/guest/`, **run** the suites over
   SSH (`guest/run.sh`), **copy out** `target/vm/results/<release>/`:
   `env.txt` (kernel, Docker, cgroup), one log per suite, `summary.txt`
   and the VM's serial log.
6. **Tear down** on every exit path, Ctrl-C included: QEMU is stopped
   through its monitor (then `SIGKILL`), and the overlay, seed and key are
   deleted. The script fails if QEMU is still running.

| Suite | What it runs |
|---|---|
| `unit` | Every test marked `#[ignore = "T2: ..."]`, found by name in `crates/`, as root with `--ignored`; each must run exactly once and pass |
| `smoke` | `smoke-build` on `python:3.13-alpine` as real root on the VM, in `docker run --privileged` and in `docker run --cap-add SYS_ADMIN --security-opt apparmor=unconfined` (Docker's seccomp profile, no `pivot_root`). Each checks the probe's root entry, runs `just smoke`'s checks (`tests/smoke/lib.bash`) plus real-root ones: a write to `/proc/sys` fails (T17), a device node the `RUN` makes does not open (T18), and no escape of [Q-077](open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape) reaches a marker outside the stage root: `chroot` into a subdirectory then `..`, the same from a descriptor of `/` opened before (`fchdir`), two `chroot`s deep, and `/proc/1/root`. Docker then runs each image. The `ranikust` binary then builds `tests/smoke/e2e` in the same environment (the workdir a volume in Docker, since overlay refuses an upper directory on the container's own overlay root; the `SYS_ADMIN` variant also gets `NET_ADMIN` for `RUN --network=none`, [Q-183](open-questions.md#q-183-what-does-run---networknone-do-without-cap_net_admin)): it must reach overlay mode as root with the same root entry and a real overlay probe (either mount API) and print a digest; no stub or secret may reach a blob, and Docker loads the tar and runs the image as user `app`. On the VM itself, `--reproducible` must give one digest with `--jobs 1` and the default. A plain `chroot` into the same root is the escapes' control: there each must succeed. Then `smoke-build` and `ranikust` run inside a plain `chroot` in both Docker variants, where `/` is not a mount root: each must refuse before any `RUN`, with `rnk-sandbox`'s message that it runs inside a plain chroot. Afterwards the VM's `domainname` is unchanged and no mount or `smoke-build` process is left |

Not covered yet: cgroup v1 (`systemd.unified_cgroup_hierarchy=0` and a
reboot), Docker's default profile and `--cap-add SYS_ADMIN` alone (they
need kaniko mode), kernels before 5.11 (an older `..` walk; Debian 11
has 5.10), the host-safety attack suite, a CI job (GitHub's
hosted runners have no reliable KVM) and an aarch64 guest.

Also TODO: `just test-rootless` for tier T1, with the same rule that tests
write only below a temporary directory.

## aarch64 under qemu-user

`just test-aarch64` (CI job `test-aarch64`, `ADR-0009`) cross-builds with
cargo-zigbuild and runs everything under `qemu-aarch64` on an x86_64 host:

1. The static release binary: no loader and no `NEEDED` entries, then
   `ranikust version`. It is not static-pie, unlike x86_64 [open: Q-140].
2. The T0 tests of the workspace, through cargo's target runner
   (`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER`). `openat2` with
   `RESOLVE_IN_ROOT`, `statx` and the sandbox's unprivileged tests work
   under emulation.

QEMU 9.1 or later is needed, and the recipe refuses an older one: QEMU
8.2 (Ubuntu 24.04's `qemu-user`) has no `openat2` and fails most
`rnk-rootfs` tests with `ENOSYS`. `QEMU_AARCH64` names the emulator, else
`qemu-aarch64-static` or `qemu-aarch64` on `PATH`; `apt download
qemu-user` (if the distribution's is 9.1 or later) or the Debian 13 package
CI pins, unpacked with `dpkg -x`, gives one without installing it. The
recipe prints the binfmt entry's interpreter; CI points the entry at the
same binary as cargo's runner. Two exceptions, both from emulation:

- `root::tests::open_gives_cloexec_path_fd_and_mount_id` is skipped:
  qemu-user's `statx` leaves `stx_mnt_id` at 0 (QEMU 10.0 and 10.2), so
  other mount-id comparisons pass only because both sides are 0. The
  mount-ID checks that guard deletion (T4 of the threat model) are
  therefore not exercised on aarch64.
- ranikust's `cli` and `metrics` tests spawn the binary; without a
  binfmt_misc entry for aarch64 the kernel cannot execute it, so the
  recipe skips them with a warning. CI registers the entry and requires it.

Checked locally with QEMU 10.0.13 (CI's) and 10.2.1, without binfmt.
Real aarch64 hardware is Phase 6.

## Smoke test until the builder exists (tier T1)

`just smoke` builds the `smoke-build` example of `crates/ranikust`
statically and runs it inside rootless `podman run --privileged`, where
root is the user's own uid through the user namespace, so nothing it does
reaches the host as root. It strings the finished crates together:

1. pull `alpine:3.20` (`rnk-registry`), extract each layer and check its
   diff ID (`rnk-layer`, `rnk-rootfs`);
2. `enter_build_namespace`, the probe and three `RUN`s through
   `rnk-sandbox`: a file write, `apk add jq` (network and DNS through the
   sandbox's `/etc` copies), and checks run inside the sandbox (PID 1, host
   name, Docker's bounding set `a80425fb`, read-only `/proc/sys`, stdin not
   a terminal);
3. pack the root into one gzip layer and write an OCI layout
   (`rnk-layer`, `rnk-oci`).

The recipe then checks that the sandbox's stubs did not reach the layer
(`/etc/resolv.conf` not at all, `/etc/hosts` and `/etc/hostname` not with
the host name `ranikust`), loads the layout into podman as an OCI archive (`podman load`; not
`skopeo`, which calls `unshare` itself and is refused where AppArmor
restricts unprivileged user namespaces, as on Ubuntu 24.04) and runs it.
It is not a
builder: one squashed layer, no diff per instruction, no xattrs. The recipe
then builds `tests/smoke/e2e` with the `ranikust` binary in the same kind
of container (two stages, `COPY --from`, wildcards, `.dockerignore`, cache
and secret mounts, `RUN --network=none`, heredocs, `WORKDIR`, `USER`,
`ENV`, `LABEL`, `EXPOSE`, `CMD`), loads its Docker tar into podman and
runs it, checks that no stub or secret reached a blob, and that
`--reproducible` gives one digest with `--jobs 1` and the default. It does not replace the VM: the sandbox runs
in a user namespace there, so real-root behaviour (T17, T18 of the threat
model, `MS_MOVE` under Docker's seccomp) is checked by the `smoke` suite of
`just test-vm` instead.
