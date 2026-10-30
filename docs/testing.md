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
| T2 | Privileged integration: overlay as real root, `pivot_root`, `mknod`, Docker rootful variants, kaniko mode on `/`, the host-safety attack suite | root | A disposable VM, never the host | `just test-vm` ([below](#the-vm-harness-tier-t2)) | `unit`, `smoke`, `e2e` and `push` suites |
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
   Docker from the Ubuntu archive). Docker Hub images come through
   `RANIKUST_VM_IMAGE_REGISTRY`, by default `mirror.gcr.io` (Google's
   public Docker Hub mirror, which has no pull rate limit): the guest's
   Docker uses it as a `registry-mirrors` entry, and the `smoke` suite
   pulls `python:3.13-alpine` and the e2e base `alpine:3.20` from it by
   name. `RANIKUST_VM_IMAGE_REGISTRY=docker.io` pulls from Docker Hub
   itself, which allows anonymous clients only a few pulls per hour.
4. **Boot** QEMU as the invoking user (`/dev/kvm` through its group or an
   ACL, never `sudo`), user-mode networking, SSH forwarded to a free port
   on `127.0.0.1` only. The disk is 20 GB (`RANIKUST_VM_DISK`) and the
   memory 4 GiB (`RANIKUST_VM_MEM`), more for `e2e`.
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
| `smoke` | `smoke-build` on `python:3.13-alpine` as real root on the VM, in `docker run --privileged` and in `docker run --cap-add SYS_ADMIN --security-opt apparmor=unconfined` (Docker's seccomp profile, no `pivot_root`). Each checks the probe's root entry, runs `just smoke`'s checks (`tests/smoke/lib.bash`) plus real-root ones: a write to `/proc/sys` fails (T17), a device node the `RUN` makes does not open (T18), and no escape of [Q-077](open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape) reaches a marker outside the stage root: `chroot` into a subdirectory then `..`, the same from a descriptor of `/` opened before (`fchdir`), two `chroot`s deep, and `/proc/1/root`. Docker then runs each image. The `ranikust` binary then builds `tests/smoke/e2e` in the same environment (the workdir a volume in Docker, since overlay refuses an upper directory on the container's own overlay root; the `SYS_ADMIN` variant has no `NET_ADMIN`, so it passes `--allow network.none-userns`, [ADR-0033](adr/0033-network-none-loopback-through-a-user-namespace.md), and the build checks `lo` is up). In the `SYS_ADMIN` variant a `RUN --network=none` without that flag must be refused naming it, and with it, after `modprobe ip_tables ip6_tables af_key` in the VM, `tests/smoke/netns-check.py` must find, as root in the `RUN`, loopback up and working; `EPERM` for `setns` into the user namespace owning the network namespace (`NS_GET_USERNS`), `unshare(CLONE_NEWUSER)`, `IPT_SO_SET_REPLACE`, `IPT_SO_GET_INFO`, `IP6T_SO_GET_INFO`, `TCP_REPAIR` and `socket(AF_KEY)`; `EPROTONOSUPPORT` for `NETLINK_NETFILTER` and `NETLINK_XFRM`; rtnetlink and audit netlink open; and a later `RUN --network=none useradd` must succeed: it must reach overlay mode as root with the same root entry and a real overlay probe (either mount API) and print a digest; no stub or secret may reach a blob, and Docker loads the tar and runs the image as user `app`. On the VM itself, `--reproducible` must give one digest with `--jobs 1` and the default. A plain `chroot` into the same root is the escapes' control: there each must succeed. Then `smoke-build` and `ranikust` run inside a plain `chroot` in both Docker variants, where `/` is not a mount root: each must refuse before any `RUN`, with `rnk-sandbox`'s message that it runs inside a plain chroot. Afterwards the VM's `domainname` is unchanged and no mount or `smoke-build` process is left |
| `e2e` | The Phase 1 workloads of `nextjs-kaniko` ([below](#end-to-end-workloads-e2e)) built by kaniko and BuildKit as references and by `ranikust`; times, image comparison, metrics. Not part of the default `just test-vm` |
| `push` | One `ranikust build` as real root pushing to three registries and writing `--tar-path` and `--oci-layout-path` ([below](#test-registries)): a `registry:2` on the VM's loopback (plain HTTP, anonymous, unasked), the host's HTTP test registry as `10.0.2.2:5000` with `--insecure-registry`, and the host's HTTPS one as `10.0.2.2:5443` with `--registry-certificate` and credentials from a `config.json`. Each manifest read back must hash to the printed digest, the HTTPS one must refuse an anonymous read, Docker pulls and runs the loopback image and loads and runs the tarball and the layout, and every layout blob must hash to its name. Needs `just registry-up`; not part of the default `just test-vm` |

### Test registries

`tests/registry/compose.yaml` runs two `registry:2` (pinned by digest) with
rootless podman, both on the host's loopback:

| Service | Address | Access |
|---|---|---|
| `http` | `127.0.0.1:5000` | plain HTTP, anonymous |
| `https` | `127.0.0.1:5443` | TLS from a self-signed CA, basic auth (user `ranikust`) |

`just registry-up` runs `tests/registry/setup.sh`, which makes, under
`tests/registry/_data` (ignored by version control, with the registries'
storage): the CA (`certs/ca.crt`, what a client passes to
`--registry-certificate`), a server certificate for `localhost`, the host's
name, `127.0.0.1`, `::1` and `10.0.2.2`, and `auth/htpasswd` with a random
password kept in `auth/credentials`. It then starts both and checks them
with `tests/registry/check.sh`: an image pushed into each reads back, and
the HTTPS one refuses an anonymous read. `just registry-down` stops them and
keeps the data. The T2 VM reaches them as `10.0.2.2`, which QEMU's user
network maps to the host's loopback, so they need no wider binding. On a
host whose AppArmor restricts unprivileged user namespaces, only podman
(which has a profile) can read local storage, so the check pushes with
`podman push` and reads with `skopeo inspect`.

### End-to-end workloads (`e2e`)

`just test-vm e2e` (`tests/vm/guest/e2e.sh`) runs the acceptance
workloads of [Phase 1](agents/phase-1.md#acceptance-criteria) in the VM, the
way `nextjs-kaniko/bench/run-fair.sh` runs them, without touching the
host's `nextjs-kaniko` containers:

- **Seed.** `tests/vm/e2e-seed.py` streams in, read-only from the
  `nextjs-kaniko` checkout next to the main checkout
  (`RANIKUST_NEXTJS_KANIKO`): W1 `app`, W2 `app-cache`, W3 `app-par`;
  the base and tools images that the workloads name by digest, copied from
  the host registry's storage with their blobs so the digests hold; the
  verdaccio storage, so pnpm installs without npmjs; and
  `bench/digest-check/check-blobs.py`. The reference images (kaniko
  `localhost/kaniko-bench:stream`, BuildKit `v0.33.1-rootless`,
  `registry:2`, verdaccio 6.1.6) are saved once from the host's podman
  into `~/.cache/ranikust/vm/e2e/`. About 2.8 GB goes in per run.
- **Services.** A registry (`registry:5000`) and verdaccio
  (`verdaccio:4873`) on an internal Docker network, as
  `docker-compose.yml`, also published on the VM's loopback for
  `ranikust`, whose `RUN` shares the VM's network.
- **Builds.** kaniko with `cfg-final-opt.env` and
  `FF_KANIKO_STREAM_TAR_COMPRESS` (experiment 07's `k-stream`), BuildKit
  as `run-fair.sh`'s `bk` (zstd), each in `docker run --cpus=4
  --memory=8g`; `ranikust build` as root in a systemd scope with the same
  CPU quota and memory limit, a fresh `--workdir` each time. W2 is built
  cold, without a cache. On W3 `ranikust` runs once per `--jobs` value
  (`1 2 default`) with `--reproducible`. Every build starts from an empty
  `e2e/` namespace in a restarted registry. Time is taken as
  `run-fair.sh` takes it: start of the build container (or process) to its
  exit. One discarded kaniko build per workload warms the page cache and
  verdaccio first.
- **Checks** on the first round: each image is flattened from the registry
  (`guest/e2e/flatten.py`: path, type, mode, owner, size, sha256, link
  target, hardlink, xattrs; whiteouts applied; gzip and zstd) and
  compared pairwise (`guest/e2e/compare.py`), with experiment 07's
  non-deterministic W1 files counted as noise and hardlinked entries apart
  (ADR-0012); configs are compared field by field. `check-blobs.py` checks
  that every gzip layer describes itself. On W3 the raw digests at each
  `--jobs` value are reported, and the images are compared by flattened
  content against the first value (`compare-jobs<a>-jobs<b>.txt`), with
  the workload's `du`-derived files (`/out/a.txt`, `/out/b.txt`,
  `/out/c.txt`, `/out/all.txt`) passed to `compare.py --noise`; any other
  difference fails the suite
  ([Q-187](open-questions.md#q-187-how-does-the-w3-digest-check-treat-the-workloads-du-output)).
  kaniko's `metrics.json` and `ranikust`'s report are
  put side by side through the phase map of
  [metrics](design/metrics.md#comparing-with-kaniko)
  (`guest/e2e/metrics.py`).
- **Results** in `target/vm/results/<release>/e2e/`: `report.md`, and per
  workload and build `run.json`, `build.log`, metrics, the listing and
  config, `compare-*.txt`.

A `ranikust` build that stops because a part is not built yet (`not
implemented`, a flag refused until a later phase) is recorded as
`pending` and does not fail the suite; any other failure does. Options:
`RANIKUST_E2E_WORKLOADS`, `_BUILDERS`, `_ROUNDS` (default 1; use 3 or more
for the acceptance timings), `_WARMUP`, `_CPUS`, `_MEMORY`, `_JOBS`. The
VM gets 12 GiB and an 80 GB disk unless `RANIKUST_VM_MEM`/`_DISK` say
otherwise. A run of one round took about 15 minutes on noble, most of it
the seed copy, the builds and flattening W1's images.

First run (2026-10-30, noble, 4 vCPUs, one round, so indicative only):
kaniko W1 22.4 s, W2 16.3 s, W3 29.7 s; BuildKit W1 48.4 s, W2 13.2 s,
W3 15.3 s; every `ranikust` build pending on `--insecure-registry`
([Q-200](open-questions.md#q-200-how-does-the-e2e-suites-ranikust-reach-a-plain-http-registry)).
The references already differ from each other, which a `ranikust`
comparison has to read against:

- BuildKit (rootless, in rootful Docker) gives the directories that a
  `RUN` or `WORKDIR` creates the setgid bit (`2755` where kaniko has
  `755`): 13,045 directories on W1, 3,585 on W2.
- kaniko's W3 image has no `/out/all.txt`, which the last `RUN` writes,
  and its `/out` is `1000:1000 775` where BuildKit's is `0:0`; the
  `/out/c.txt` contents differ too. Not investigated yet.
- W1's non-deterministic files of experiment 07 (1,650) show up as noise,
  as expected.

Second run (2026-10-30, noble, same settings, `ranikust` with
`--insecure-registry`): every build succeeded. Wall time, kaniko /
BuildKit / `ranikust`: W1 24.0 / 63.9 / 20.5 s, W2 cold 16.5 / 12.0 /
8.6 s, W3 29.6 / 15.0 s and `ranikust` 15.0 (`--jobs 1`), 13.1 (2),
13.2 s (default). Compared with kaniko, `ranikust` differs in 3 entries
on W1 (a pnpm state file and an npm log named after the time), 1 on W2
(`MANIFEST.sha256`, 3 bytes) and 3 on W3 (`/out/a.txt`, see below; kaniko's
`/out` owner and mode and its missing `/out/all.txt`), configs equal;
compared with BuildKit, the same counts as kaniko's against BuildKit
(the setgid directories), plus that `MANIFEST.sha256` on W2. On W3 the
raw digests can differ across `--jobs` through the files that record
`du -sk node_modules`, disk usage rather than the files: later runs saw
`/out/a.txt`, `/out/b.txt` and `/out/all.txt` differ between `--jobs 1`
and the default and nothing else, so the content comparison passes
([Q-187](open-questions.md#q-187-how-does-the-w3-digest-check-treat-the-workloads-du-output)).

Not covered yet: cgroup v1 (`systemd.unified_cgroup_hierarchy=0` and a
reboot), Docker's default profile and `--cap-add SYS_ADMIN` alone (they
need kaniko mode), kernels before 5.11 (an older `..` walk; Debian 11
has 5.10), the host-safety attack suite, a CI job (GitHub's
hosted runners have no reliable KVM) and an aarch64 guest.

Also TODO: `just test-rootless` for tier T1, with the same rule that tests
write only below a temporary directory.

## aarch64 under qemu-user

`just test-aarch64` (CI job `test-aarch64`, `ADR-0009`) builds with
cargo-zigbuild. On an aarch64 host, which is where CI runs it (GitHub's
free `ubuntu-24.04-arm` runner for public repositories), everything runs
natively and no test is skipped. On an x86_64 host it runs everything under
`qemu-aarch64`, as below:

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
recipe prints the binfmt entry's interpreter. Four exceptions, all from
emulation (the native CI job runs them all):

- `root::tests::open_gives_cloexec_path_fd_and_mount_id` is skipped:
  qemu-user's `statx` leaves `stx_mnt_id` at 0 (QEMU 10.0 and 10.2), so
  other mount-id comparisons pass only because both sides are 0. The
  mount-ID checks that guard deletion (T4 of the threat model) are
  therefore not exercised under emulation; the native CI job covers them.
- `without_root_the_set_up_refuses_before_touching_the_workdir` is
  skipped: qemu-user runs a thread of its own, so the set-up stops at its
  single-thread check before the privilege refusal the test expects.
- `seccomp::tests::the_filter_refuses_what_it_should_and_nothing_else` is
  skipped: the filter is written for aarch64 syscall numbers, and qemu-user
  cannot install it in the x86_64 host kernel.
- The tests that spawn an aarch64 binary themselves (ranikust's `cli` and
  `metrics`, rnk-store's `processes`): without a binfmt_misc entry for
  aarch64 the kernel cannot execute it, so the recipe skips them with a
  warning.

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
