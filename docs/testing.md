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
- Running the `ranikust` binary in kaniko mode in a container builds on and
  empties that container's `/`; outside a container it builds in its state
  directory (ADR-0050). Outside the VM, run kaniko mode as root only in a
  throwaway container, with `--state-dir` under a fresh temporary
  directory.
- A bug fix comes with a test that fails without it.
- `just test <filter>` passes a name filter to `cargo test`; for one crate,
  use `cargo test -p <crate> <filter>`.

## Which test for what

| Purpose | Run | Where | Time |
|---|---|---|---|
| A branch's correctness | `just check`; T1 smoke cases in both modes (`rnk-vps-lane run <branch> 'RANIKUST_SMOKE_REGISTRY=mirror.gcr.io just smoke-case'`, and with `RANIKUST_CASE_MODE=kaniko`); VM `unit` and `quick` (`sg kvm -c 'just test-vm unit quick'`, `RANIKUST_QUICK_MODE=kaniko` for kaniko mode) | host or the VPS lane; the local VM | minutes |
| A change to the e2e harness or to how builders are compared | the light e2e: `sg kvm -c 'just test-vm e2e'` (W0, every builder) | the local VM | a few minutes |
| Performance numbers | the ultra benchmark: `sg kvm -c 'just test-vm ultra'` (nextjs-kaniko's W1–W3), only when the maintainer asks | the local VM, nothing else running on the host | tens of minutes per round |
| Acceptance of a phase | the ultra benchmark with rounds: `RANIKUST_E2E_ROUNDS=5 sg kvm -c 'just test-vm ultra'`, only when the maintainer asks | the same | an hour or more |

Nothing heavy runs by default: `just test-vm` alone runs `unit` and
`smoke`, and `e2e` is the light workload.

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
| Host safety | Layers with `../etc/passwd`, symlinks and hardlinks pointing out; `RUN mount`, `setsid sleep 1000 &`, a fork bomb under a cgroup limit, writes to `/etc/resolv.conf`; a `--state-dir` at `/`, below `/home/<user>` and in a directory others may write. After each case: the host is unchanged, and no mount or process is left | T2 |

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
   `tests/e2e/_vm-image/` of the main checkout (gitignored,
   `RANIKUST_VM_CACHE`), which every git worktree of it shares. Only one
   VM runs at a time on the machine: a run holds `flock` on
   `_vm-image/.vm.lock` from start to teardown, and a second run waits for
   it (`RANIKUST_VM_LOCK_WAIT`, default an hour) instead of booting
   another 16 GiB VM beside it.
3. **Provision** the base image once: the verified cloud image booted with
   `tests/vm/cloud-init/provision`, which installs the packages (Docker
   from the Ubuntu archive, `zstd`, `pigz`), disables the background apt timers,
   removes what is per machine (SSH host keys, machine ID; no user, no
   key, no password is created) and powers the VM off. The disk is
   flattened into one qcow2, checked, made read-only, and kept in the cache
   as `base-<release>-<key>.qcow2` with its SHA-256 beside it, checked
   before every use. `<key>` hashes what it is made from (the release, the
   cloud image's SHA-256, the package list, the provisioning seed), so a
   new cloud image or a changed seed builds a new one (a few minutes, once)
   and deletes the release's old one; a base image that no longer matches
   its SHA-256 is built again. `RANIKUST_VM_GOLDEN=0` skips it: the run
   boots the cloud image and cloud-init installs the packages, as every run
   did before.
4. **Prepare** a run directory under `target/vm/`: a copy-on-write qcow2
   overlay on the base image (only ever a read-only backing file: every
   write goes to the overlay, deleted with the run), a fresh SSH key and a
   NoCloud seed with a new instance ID (`tests/vm/cloud-init/user-data`:
   user `rnk` with passwordless `sudo` and the run's key; new SSH host
   keys and machine ID at first boot). Docker Hub images come through
   `RANIKUST_VM_IMAGE_REGISTRY`, by default `mirror.gcr.io` (Google's
   public Docker Hub mirror, which has no pull rate limit): the guest's
   Docker uses it as a `registry-mirrors` entry, and the `smoke` suite
   pulls `python:3.13-alpine` and the e2e base `alpine:3.20` from it by
   name. `RANIKUST_VM_IMAGE_REGISTRY=docker.io` pulls from Docker Hub
   itself, which allows anonymous clients only a few pulls per hour.
5. **Boot** QEMU as the invoking user (`/dev/kvm` through its group or an
   ACL, never `sudo`), user-mode networking, SSH forwarded to a free port
   on `127.0.0.1` only, one SSH connection shared by every command. Every
   suite gets 12 vCPUs (`RANIKUST_VM_CPUS`; at most the host's CPUs less
   2) and 16 GiB (`RANIKUST_VM_MEM`). The memory is measured once the
   previous run's VM has gone: when the host has less than 16 GiB plus
   3 GiB available, the default shrinks to fit, never below 4 GiB (8 GiB
   for `e2e` and `ultra`), and the run says so; when even that would leave
   the host under 2 GiB, the run stops before booting and says what to
   close. A value set in the variables is used as is, with a warning if it
   leaves the host under 2 GiB. The disk is 20 GB (`RANIKUST_VM_DISK`,
   40 GB for `ultra`, at least 10 GB over the base image). The cache
   directory must be yours, not a symbolic link, and writable by no one
   else: the default one, when others may write it and everything in it is
   yours and no link, is made 0700; one named by `RANIKUST_VM_CACHE` is
   refused instead. The base image must be a self-contained qcow2
   (no backing or data file, checked with `qemu-img info` before each
   use), and the cache and target paths absolute without a comma (QEMU's
   option separator). All devices are virtio: the disk on its own I/O thread with
   `cache=unsafe` (the disk is thrown away) and `aio=io_uring` where QEMU
   and the kernel allow it (else the thread pool), an entropy source, and
   a balloon with free-page reporting that hands memory the guest frees
   back to the host. The effective size goes into `env.txt` and
   `timings.txt` (boot started, SSH up, cloud-init done, suites done,
   total, in seconds since the start).
6. **Copy in** the binaries and `tests/vm/guest/`, **run** the suites
   (`guest/run.sh`) detached in the guest while the host follows their log
   over SSH every 5 s, reconnecting for up to 15 minutes, so a VM that
   stalls while the host swaps loses neither the run nor its results;
   **copy out** `target/vm/results/<release>/` (retried):
   `env.txt` (kernel, Docker, cgroup, CPUs, memory, the VM's size, and
   what must be the run's own: its one user with one key, the SSH host key
   made at this boot, its machine ID), one
   log per suite, `summary.txt`, `suite-times.txt` (each suite's wall time),
   `timings.txt` and the VM's serial log (`base-image-serial.log` too when
   building the base image failed). The results are extracted into a new
   directory of the run, cleared of anything that is not a regular file or
   a directory, and only then put in place; teardown clears them the same
   way before it writes the serial log and `timings.txt`, through new
   files renamed into place. The guest refuses to run any suite when it
   holds a user other than `rnk`, no SSH host key, or one older than its
   boot.
7. **Tear down** on every exit path, Ctrl-C included: QEMU is stopped
   through its monitor (then `SIGKILL`), the shared SSH connection is
   closed, and the overlay, seed and key are deleted (the base image stays
   in the cache). The script fails if QEMU is still running.

| Suite | What it runs |
|---|---|
| `unit` | Every test marked `#[ignore = "T2: ..."]`, found by name in `crates/`, as root with `--ignored`; each must run exactly once and pass. Among them experiment 09's deletion case D2 (`rnk-rootfs` `mounts_inside_the_root_survive`: a same-filesystem bind mount and a tmpfs inside a root survive its emptying, in a private mount namespace); D1 (emptying a container's `/` keeps its volumes and runtime files) is the `hygiene-mounts` smoke case in kaniko mode, and D3 is `ranikust`'s state-directory tests (`/`, the system directories, an unmarked non-empty directory, other users) and `rnk-store`'s lock of the state directory, since ADR-0050 replaced the workdir |
| `smoke` | `smoke-build` on `python:3.13-alpine` as real root on the VM, in `docker run --privileged` and in `docker run --cap-add SYS_ADMIN --security-opt apparmor=unconfined` (Docker's seccomp profile, no `pivot_root`). Each checks the probe's root entry, runs `just smoke`'s checks (`tests/smoke/lib.bash`) plus real-root ones: a write to `/proc/sys` fails (T17), a device node the `RUN` makes does not open (T18), and no escape of [Q-077](open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape) reaches a marker outside the stage root: `chroot` into a subdirectory then `..`, the same from a descriptor of `/` opened before (`fchdir`), two `chroot`s deep, and `/proc/1/root`. Docker then runs each image. The `ranikust` binary then builds `tests/smoke/e2e` in the same environment (the workdir a volume in Docker, since overlay refuses an upper directory on the container's own overlay root; the `SYS_ADMIN` variant has no `NET_ADMIN`, so it passes `--allow network.none-userns`, [ADR-0033](adr/0033-network-none-loopback-through-a-user-namespace.md), and the build checks `lo` is up). In the `SYS_ADMIN` variant a `RUN --network=none` without that flag must be refused naming it, and with it, after `modprobe ip_tables ip6_tables af_key` in the VM, `tests/smoke/netns-check.py` must find, as root in the `RUN`, loopback up and working; `EPERM` for `setns` into the user namespace owning the network namespace (`NS_GET_USERNS`), `unshare(CLONE_NEWUSER)`, `IPT_SO_SET_REPLACE`, `IPT_SO_GET_INFO`, `IP6T_SO_GET_INFO`, `TCP_REPAIR` and `socket(AF_KEY)`; `EPROTONOSUPPORT` for `NETLINK_NETFILTER` and `NETLINK_XFRM`; rtnetlink and audit netlink open; and a later `RUN --network=none useradd` must succeed: it must reach overlay mode as root with the same root entry and a real overlay probe (either mount API) and print a digest; no stub or secret may reach a blob, and Docker loads the tar and runs the image as user `app`. In every variant the e2e Dockerfile checks that a `RUN --network=none` sees only `lo` (in `/sys/class/net` and `/proc/net/dev`) and reaches nothing outside, and compares the secret by its sha256, so the value appears in no instruction. On the VM itself (Phase 1 acceptance): `--reproducible` must give one digest with `--jobs 1`, the default, 2 s later and with `--metrics-file`; the secret must be in no build log and no metrics report (`smoke_check_no_secret_in`); a cache mount written by one build must be read by the next with the same state directory, and not by a fresh one. A plain `chroot` into the same root is the escapes' control: there each must succeed. Then `smoke-build` and `ranikust` run inside a plain `chroot` in both Docker variants, where `/` is not a mount root: each must refuse before any `RUN`, with `rnk-sandbox`'s message that it runs inside a plain chroot. Afterwards the VM's `domainname` is unchanged and no mount or `smoke-build` process is left |
| `quick` | The quick smoke cases ([below](#quick-smoke-cases)) built by the `ranikust` binary as real root on the VM, Docker loading and running each image; `RANIKUST_QUICK_CASES` picks cases. A minute or two: the suite for a smoke test or a proof of concept that needs real root. Not part of the default `just test-vm` |
| `e2e` | The light workload W0 ([below](#end-to-end-workloads-e2e)) built by kaniko and BuildKit as references and by `ranikust` in overlay mode, in kaniko mode on the `/` of a default `docker run` (`rk-kaniko`) and in kaniko mode as root on the VM (`rk-kaniko-host`, one directory per stage); times, image comparison, leaks, metrics. A few minutes with the VM's boot: the check of a change to the harness. Not part of the default `just test-vm` |
| `ultra` | The ultra benchmark: the same on `nextjs-kaniko`'s W1–W3 ([below](#end-to-end-workloads-e2e)), with its npm proxy; tens of minutes per round and a 40 GB disk. Only when the maintainer asks for performance numbers or an acceptance run. Not part of the default `just test-vm` |
| `push` | One `ranikust build` as real root pushing to three registries and writing `--tar-path` and `--oci-layout-path` ([below](#test-registries)): a `registry:2` on the VM's loopback (plain HTTP, anonymous, unasked), the host's HTTP test registry as `10.0.2.2:5000` with `--insecure-registry`, and the host's HTTPS one as `10.0.2.2:5443` with `--registry-certificate` and credentials from a `config.json`. Each manifest read back must hash to the printed digest, the HTTPS one must refuse an anonymous read, Docker pulls and runs the loopback image and loads and runs the tarball and the layout, and every layout blob must hash to its name. Needs `just registry-up`; not part of the default `just test-vm` |
| `secrets` | ADR-0051: kaniko mode on the `/` of default Docker containers (`ubuntu:24.04`, no `--privileged`), with `RUN` steps that attack ranikust (`tests/vm/guest/secrets/attack.sh`): the state directory, `/proc/*/fd`, ranikust's `environ` and `mem`, the `RUN`'s own environment, processes left alive, the same from `unshare -Ur`, `cmdline`, and another process's `environ`. The state directory may hold only the store's marker of a build in progress, `.ranikust-building`, holding the PID namespace and boot id. Three scenarios: ranikust as PID 1, the same with `--cap-add SYS_PTRACE`, and started from `sh -c`; every vector blocked but the two ranikust only warns about (a `--build-arg` value in `cmdline`; the shell's `environ`), whose warnings must appear. Also records whether the container's overlayfs takes `O_TMPFILE` and how ranikust made its state files; then `memlimit` (ADR-0056): a 512 MiB layer built under `--memory 256m --memory-swap 256m` must succeed, its state on disk, never in `memfd`. Not part of the default `just test-vm` |

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

`tests/vm/guest/e2e.sh` builds container images with every builder in the
VM, the way `nextjs-kaniko/bench/run-fair.sh` runs them, times each build,
and compares and checks the images. It has two profiles:

- **Light** (`just test-vm e2e`, the default): W0, the Dockerfile and files
  in `tests/vm/guest/e2e/w0`, on a base image the suite makes and pushes to
  its registry (`guest/e2e/base-w0.py`: the VM's static busybox, a file
  with an extended attribute, two names of one inode, a setuid file, a
  symlink, a sticky `/tmp`, files to delete; the same bytes every run).
  Its stages exercise what the comparison and kaniko mode's stage
  directories need: a `RUN` making a hardlink, a symlink and a setuid file
  and deleting a file and a directory of the base (whiteouts); three
  children of one stage and a grandchild (`FROM <stage>`, ADR-0059,
  Q-410), two of them independent (`--jobs`); `COPY --from` two stages;
  `COPY` from the context and a `RUN` deleting a copied file. `ranikust`
  and `rk-kaniko-host` build it with `--reproducible` at `--jobs 1` and the
  default, whose contents must match. Every builder, one round, no
  warm-up: a few minutes with the VM's boot, nothing pulled from a
  network. Only the reference images go in (kaniko, BuildKit, `registry:2`);
  no `nextjs-kaniko` checkout is needed.
- **Ultra** (`just test-vm ultra`, `guest/ultra.sh`): the acceptance
  workloads of [Phase 1](agents/phase-1.md#acceptance-criteria), W1–W3 of
  `nextjs-kaniko`, without touching the host's `nextjs-kaniko`
  containers; tens of minutes per round. Only when the maintainer asks
  for performance numbers or an acceptance run
  ([which test for what](#which-test-for-what)). The rest of this section
  describes it; the light profile shares everything but the seed, the
  npm proxy, the warm-up and the blob check (nextjs-kaniko's script).

- **Seed.** `tests/vm/e2e-seed.py` streams in, read-only from the
  `nextjs-kaniko` checkout next to the main checkout
  (`RANIKUST_NEXTJS_KANIKO`): W1 `app`, W2 `app-cache`, W3 `app-par`;
  the base and tools images that the workloads name by digest, copied from
  the host registry's storage with their blobs so the digests hold; the
  verdaccio storage, so pnpm installs without npmjs; and
  `bench/digest-check/check-blobs.py`. The reference images (kaniko
  `localhost/kaniko-bench:stream`, BuildKit `v0.33.1-rootless`,
  `registry:2`, verdaccio 6.1.6) are saved once from the host's podman
  into `tests/e2e/_vm-image/e2e/`. About 2.8 GB goes in per run.
- **Services.** A registry (`registry:5000`) and verdaccio
  (`verdaccio:4873`) on an internal Docker network, as
  `docker-compose.yml`, also published on the VM's loopback for
  `ranikust`, whose `RUN` shares the VM's network.
- **Builds.** kaniko with `cfg-final-opt.env` and
  `FF_KANIKO_STREAM_TAR_COMPRESS` (experiment 07's `k-stream`), BuildKit
  as `run-fair.sh`'s `bk` (zstd), each in `docker run --cpus=4
  --memory=8g` pinned to the VM's last 4 vCPUs (`--cpuset-cpus`; the
  registry and verdaccio on the others); `ranikust` three ways (Phase 2
  acceptance): `ranikust`, overlay mode as root in a systemd scope with the
  same budget (`CPUQuota=400%`, `MemoryMax` and `MemorySwapMax` 8 GiB,
  as Docker gives a container with `--memory` alone, `AllowedCPUs` the
  same vCPUs); `rk-kaniko`, `--mode=kaniko` on the `/` of a default
  `docker run` (root, Docker's capabilities, no option) from an empty
  image, the static binary mounted at `/ranikust/bin` (ADR-0048);
  `rk-kaniko-host`, `--mode=kaniko` as root on the VM in the same kind of
  scope, one directory per stage in its state directory (ADR-0059). Each
  gets a fresh state directory. The builders' results directories are
  `/bench-out` and `/rnk-out`, not `/out`, which W3's Dockerfile writes: a
  mount point never reaches a layer in kaniko mode, kaniko's or
  ranikust's. W2 is built cold, without a cache. On W3 `ranikust` and
  `rk-kaniko-host` run once per `--jobs` value (`1 2 default`) with
  `--reproducible`. A builder container writes only its own directory
  (`<build>/mnt`, mounted as `/bench-out` or `/rnk-out`), whose
  `metrics.json` and `digest.txt` are copied out if they are regular files
  once the container is gone; everything the harness writes (`build.log`,
  `limits.txt`, `run.json`, the listing) goes to the build's directory,
  which no container reaches. Every build starts from an empty `e2e/` namespace in
  a restarted registry, whose blobs no repository links any more are
  deleted first (`guest/e2e/prune-registry.py`; the registry itself never
  frees them, and an earlier version of the suite filled the guest's
  80 GB disk after a few dozen W1 builds, which stopped sshd and lost the
  VM). The pruner runs as root on a directory the registry's container
  can write, so it follows no symbolic link (every directory opened with
  `O_NOFOLLOW`, blobs reached through their parent's descriptor and named
  as the registry names them) and stops before deleting anything when a
  repository cannot be read or no blob is linked at all. `fstrim /` then
  hands the freed blocks back to the host (the VM's disk has
  `discard=unmap`). A state directory is deleted only when nothing is
  mounted below it, and never across a filesystem. No build starts with less than `RANIKUST_E2E_MIN_FREE_GB` (10)
  free on the guest's disk once leftovers are pruned: the suite fails
  instead. Time is taken as
  `run-fair.sh` takes it: start of the build container (or process) to its
  exit. One discarded kaniko build per workload warms the page cache and
  verdaccio first.
- **Checks** on the first round only (later rounds are timed only;
  `RANIKUST_E2E_CHECK_ALL=1` checks every round), each after its build has
  been timed and before the next starts, never beside a timed build: each
  image is flattened from the registry
  (`guest/e2e/flatten.py`: path, type, mode, owner, size, sha256, link
  target, hardlink, xattrs; whiteouts applied; gzip and zstd; each layer
  read once per run into a cache under its blob digest, so a base layer
  every builder's image shares is read once, the others side by side on
  the vCPUs the builds are not pinned to, through `pigz`; whiteouts hide
  a whole subtree at once, where the first version scanned every path for
  each and took over three minutes per W1 image; `flatten_v1.py` is kept
  as the reference its listing is checked against, byte for byte, by
  `python3 -I -m unittest discover -s tests/vm/guest/e2e -p 'test_*.py'`) and
  compared pairwise (`guest/e2e/compare.py`), with experiment 07's
  non-deterministic W1 files counted as noise and hardlinked entries apart
  (ADR-0012); configs are compared field by field. `check-blobs.py` checks
  that every gzip layer describes itself. On W3 the raw digests at each
  `--jobs` value are reported, and the images are compared by flattened
  content against the first value (`compare-jobs<a>-jobs<b>.txt`), with
  the workload's `du`-derived files (`/out/a.txt`, `/out/b.txt`,
  `/out/c.txt`, `/out/all.txt`) passed to `compare.py --noise`; any other
  difference fails the suite
  ([Q-187](open-questions.md#q-187-how-does-the-w3-digest-check-treat-the-workloads-du-output)),
  for each builder that takes `--jobs` (`digests-<builder>.txt`,
  `compare-<builder>-jobs<a>-jobs<b>.txt`). The kaniko-mode images are
  compared with kaniko's and with overlay mode's (ADR-0012). Every
  `ranikust` image must hold nothing of `/ranikust`, `/var/lib/ranikust`,
  a `.ranikust-*` name, `/.dockerenv`, `/run/.containerenv`, the build
  context's or results' mount points, or the VM's state directories, and
  no `/etc/hosts`, `/etc/resolv.conf`, `/etc/hostname` or file under
  `/etc/ssl/certs` that kaniko's image of the workload lacks or holds with
  other content (the runtime mounts the first three, the harness binds
  the certificates; `guest/e2e/leaks.py`; a leak fails the suite). The
  options are checked at start-up (whole numbers, known workloads and
  builders); a wrong one stops the suite.
  kaniko's `metrics.json` and `ranikust`'s report are
  put side by side through the phase map of
  [metrics](design/metrics.md#comparing-with-kaniko)
  (`guest/e2e/metrics.py`).
- **Results** in `target/vm/results/<release>/e2e/` (`ultra/` for the
  ultra benchmark): `report.md`, with the time of each step summed over
  the builds (wipe, build, flatten, leaks, blobs, compare) and the suite's
  wall time; `phases.tsv` with each build's steps; and per workload and
  build `run.json`, `build.log`, `limits.txt`, metrics, the listing and
  config, `compare-*.txt`.

A `ranikust` build that stops because a part is not built yet (`not
implemented`, a flag refused until a later phase) is recorded as
`pending` and does not fail the suite; any other failure does. Options:
`RANIKUST_E2E_WORKLOADS` (light `w0`; ultra `w1 w2 w3`, a subset such as
`w1 w3` for a shorter ultra run), `_BUILDERS`, `_ROUNDS` (default 1; use 3
or more for the acceptance timings), `_WARMUP` (light 0, ultra 1), `_CPUS`,
`_MEMORY`, `_CPUSET` (the vCPUs every builder is pinned to: by default the
VM's last `_CPUS` when it has at least `_CPUS` + 2; `none` for the quota
alone), `_JOBS` (light `1 default`, ultra `1 2 default`), `_CHECK_ALL`
and `_MIN_FREE_GB`. Phase 2 acceptance runs everything, when the
maintainer asks: `RANIKUST_E2E_ROUNDS=5 sg kvm -c 'just test-vm ultra'`.
vm.sh refuses to
start any suite when the host has less free space where the VM's disk
lives than the disk's size (`RANIKUST_VM_DISK`, a whole number of M, G or
T) less 2 GiB, about the cloud image's own content, which stays in the
backing file: the overlay's worst case. It checks before it writes or
locks anything, and again right before it creates the disk. What really
bounds the disk is the guest: e2e stops before its free space falls under
`RANIKUST_E2E_MIN_FREE_GB`, and what it frees and trims goes back to the
host. The guest's results reach the host as regular files and
directories only (anything else is deleted on both sides), after any
builder container left behind is removed. The
VM gets 12 vCPUs, 16 GiB and a 20 GB disk, 40 GB for ultra (it peaked at
6.3 GB on W1 and W3 with the registry pruned; it was 80 GB before) unless
`RANIKUST_VM_CPUS`/`_MEM`/`_DISK` say otherwise; when the host has less
than 16 GiB plus 3 GiB available, the default shrinks to fit (never below
8 GiB for `e2e` and `ultra`, 4 GiB for the other suites) and the run says
so, or stops when even that would leave the host under 2 GiB. The builders' limits do not grow with the VM:
each build's own, read from its cgroup while it runs (`cpu.max`,
`cpuset.cpus.effective`, `memory.max`, `memory.swap.max`), are in its
`limits.txt` and `run.json`, and in the log after its time. Runs before
2026-11-01 had a 4-vCPU VM and no pinning: every builder already had the
same quota and memory limit, but the VM's own services shared its 4
vCPUs, so their times are not comparable with later ones. A guest
fills its memory with page cache, which QEMU keeps: on a host short of
memory, with swap full, an earlier version of the harness lost whole runs
when the VM stalled for over 45 s and its single SSH session dropped. A run of one round took about 15 minutes on noble, most of it
the seed copy, the builds and flattening W1's images (before the flatten
cache and its subtree whiteouts).

First light runs (2026-11-01, noble, 12 vCPUs, base image cached): 23 s
from start to teardown, 8 s of it the boot, the suite itself 7 s; every
build under a second (W0 is small: kaniko 0.3 s, BuildKit 0.5 s, overlay
mode 0.1 s); `ranikust`, `rk-kaniko` and `rk-kaniko-host` each match
kaniko's image with no difference at all; BuildKit's differs in its five
known ways (setgid on `/files` and `/out`, `/dev`, `/proc`, `/sys`); with
`--reproducible`, overlay mode and `rk-kaniko-host` give the same digest
at `--jobs 1` and the default, and each other's.

First run with kaniko mode (2026-11-01, W1 and W3 of the ultra benchmark, noble,
4 vCPUs, one round, so indicative only; 19 minutes with the VM's boot and
the seed): W1 kaniko 24.3 s, overlay mode 20.5 s, `rk-kaniko` 22.9 s,
`rk-kaniko-host` 31.4 s
([Q-410](open-questions.md#q-410-how-do-per-stage-directories-stop-costing-more-than-kaniko-on-w1));
W3 kaniko 28.9 s, overlay mode 15.3/14.1 s (`--jobs` 1/default),
`rk-kaniko` 20.5 s, `rk-kaniko-host` 15.2/14.5 s. Every image matched
kaniko's with no `other` difference, no leak, and W3's content did not
depend on `--jobs` in either mode (raw digests differ only by the `du`
files). The guest ended with 6.3 GB of its disk used.

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
   `ranikust version`; static-pie with a symbol table, like x86_64, when
   linked by `aarch64-linux-gnu-gcc` (natively by `cc`); with zig's
   linker (no cross gcc installed) it is not, and the recipe warns (Q-140).
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
- `without_root_the_set_up_refuses_before_touching_the_state_dir` is
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

## Quick smoke cases

Small Dockerfiles in `tests/smoke/cases/<name>/`, one concern each, built
by the `ranikust` binary in a few seconds from `alpine:3.20` or `scratch`.
They are the default for a smoke test or a proof of concept; the
nextjs-kaniko workloads (`e2e`) are for large benchmarks only.

| Case | What it checks |
|---|---|
| `minimal` | One `RUN` and a `CMD`; the image runs |
| `scratch` | `FROM scratch` with a root built in another stage, an exec-form `ENTRYPOINT`, the default `PATH` added to a config without one (ADR-0032) |
| `metadata` | The image config as BuildKit writes it: `ARG` before `FROM`, `ARG`/`ENV` expansion, relative and absolute `WORKDIR`, `USER name:group`, `LABEL`, `EXPOSE` (tcp, udp, a range expanded), `STOPSIGNAL`, `HEALTHCHECK`, `SHELL`, exec-form `ENTRYPOINT` with `CMD`; with `--target shellform`, a shell-form `ENTRYPOINT` that clears the `CMD` inherited from its base stage |
| `copy` | `COPY` wildcards, one matching nothing (not an error), `.dockerignore` with an exception, `--chown`, `--chmod` octal and symbolic, a symlinked source (followed), a directory into a new destination, a heredoc; checked inside the image |
| `multistage` | Two independent stages at `--jobs 2`, `FROM <stage>`, `COPY --from` all of them |
| `copy-from` | Red team of what kaniko mode keeps (ADR-0051): a hostile stage (links out of its root, to the state directory, `/ranikust`, `/` and `/proc`; a FIFO, a setuid file, a hardlink, a `0700` directory of uid 1000, a device node where root may make one) read with `COPY --from` and built `FROM` after another stage reused the root; in the image its metadata as it was; four builds whose sources reach the state directory, `/ranikust` or `/proc` through links or by name fail without an image |
| `copy-from-root` | `COPY --from=<stage> / /all/`: the whole stage, without the state directory, `/ranikust` or `/proc`; in kaniko mode on the container's `/` the whole root is kept (a red-team finding, fixed) |
| `copy-from-daemon` | A `setsid` process left by a `RUN` (kaniko#1697) changes the root while kaniko mode with `RUN` `Direct` walks, keeps, empties and copies it back: the build fails closed, or its image holds nothing of the state directory, `/ranikust` or the secret |
| `run` | `RUN` in shell and exec form, a heredoc and a `#!` heredoc, a cache mount a second build sees, a secret as a file (`0400`, by its sha256) and as `env=`, `--network=none` seeing only `lo`, a `RUN` that changes nothing adding no layer, only history (ADR-0058; no empty layer blob in the image); with `RANIKUST_CASE_MODE=kaniko` (`RUN` `Direct` on the container's `/`, ADR-0057) `direct.Dockerfile` instead, and the red team of `direct-attack.Dockerfile`: a `#!` heredoc (`<<-`, a quoted delimiter, interpreters with arguments and through `env`) that cannot append to, truncate or rewrite its memfd through `/proc`, descriptors limited to the `RUN`'s pipes, `/dev/null` and its script, a non-root `USER` reopening `/proc/self/fd/1` and `2`, a 1 MiB flood relayed whole, streams closed early, a background writer and a `setsid` loop gone before the next `RUN`, secret mounts spelled in mixed case or through `ARG`; refused: a secret file through `dst=`/`Destination=`, an empty `env=`, `type=bind` after `type=secret`, `--network` as `$N` or `NONE`, `--network=none` on a heredoc; no layer names the script |
| `hygiene` | Issue-mining fixes (ADR-0052, ADR-0058): a `RUN` sees the base's `/var/run -> /run` link and the image keeps it (D4); `COPY --chmod=0640` applies to the copied directories too (D3); `RUN true` twice and an existing `WORKDIR` twice add no layer, only `empty_layer` history. Kaniko mode: two builds in one container (`rnk_run_twice`), the second finds the marker and starts from an emptied `/` (D1) |
| `hygiene-mounts` | Red team (ADR-0052 D1, D4; ADR-0058). Every mode: steps that only delete a file, replace a directory by an empty one, or change a mode or an mtime keep their layer, `RUN true` gets `empty_layer` only. Kaniko mode (`RANIKUST_CASE_MODE=kaniko`): two builds in one container holding volumes at `/run/vol` (reached through alpine's `/var/run -> /run`) and `/srv/vol`; the second finds the marker and empties `/`, both volumes keep exactly their files, and no layer of either image holds them, `/run/.containerenv`, `/.dockerenv` or `/ranikust` |
| `reproducible` | `--reproducible`: one digest across builds a second apart and at `--jobs 1` and `2`, `created` at the epoch, another digest without the flag |
| `stage-dirs` | Red team of per-stage directories (ADR-0059). Every mode: two stages at `--jobs 4` each looking for the other's marker through `/proc/<pid>/root`, pid 1's root and its own tree, and trying a tmpfs mount, a `core_pattern` write and a read of every disk in `/proc/partitions` through its own `mknod` (all refused or `nodev`); a `setsid` writer a `RUN` leaves is gone before a stage built `FROM` its stage starts; a child built `FROM` a stage leaving links out (`/`, `../..`, `/proc/self/root`), a FIFO, device nodes, a setuid file with a hardlink, a 15 TiB sparse file and a name with a newline gets the links as links, the hardlink, the file sparse; `--reproducible` gives one digest at `--jobs 1` and `4` with stages built `FROM` stages, one with two children (one copies, the last takes the directory over, in an order `--jobs` changes; ADR-0061) and a FIFO. The VM's kaniko mode off `/` (`RANIKUST_QUICK_MODE=kaniko`): the log names the per-stage directories, the metrics show a hand-over (`rnk_snapshot.hand_over`), the child gets no FIFO or device node, no build directory outlives a build, and on a 160 MiB tmpfs state directory a `RUN` filling it and a copy of a 100 MiB parent (read by a later stage too, so never handed over) fail, leave no build directory, mount or process, and a later build works |
| `special-files` | Red team of `COPY` and special files (ADR-0060). The context, made by `case.sh` (git keeps none), holds FIFOs and Unix sockets at the top, nested and behind a symlinked directory; a stage leaves FIFOs and, where root may make one, a device node. Kaniko mode on a container's `/` (`RANIKUST_CASE_MODE=kaniko`, `RUN` `Direct`): none reaches the image, from the context or `COPY --from`, and the log warns once per copy; with the full sandbox the FIFOs are kept, as BuildKit. A node a `RUN` makes never opens in a later `RUN` |

Every build is also checked for a stub or the secret in any blob and the
secret in the build log. `tests/smoke/cases.bash` runs them; a case is a
`Dockerfile`, its context and a `case.sh` with a `case_run` function using
its helpers (`build`, `run`, `expect`, `expect_config`, `config`, `out`).

- `just smoke-case [name...]` (T1): the binary in rootless `podman run
  --privileged`, podman loading and running the images. All twelve take
  about a minute once the binary is built. The shared state directory
  under `target/smoke/cases/.state`, mounted at `/var/lib/ranikust`, keeps
  the store between runs. `RANIKUST_SMOKE_REGISTRY=mirror.gcr.io` pulls
  the base image through Google's Docker Hub mirror, as the VM suite
  does, when Docker Hub's anonymous pull limit is reached.
- `just test-vm quick` (T2): the same as real root on the VM, Docker
  loading and running the images; `RANIKUST_QUICK_MODE=kaniko` builds them
  in kaniko mode in a root of the state directory, the VM not being a
  container (`RANIKUST_CASE_MODE=kaniko` builds them in kaniko mode on the
  container's `/` for `just smoke-case`, in a default rootless podman
  container where root may not mount, so `RUN` runs `Direct`; the binary,
  context and outputs sit under `/ranikust`); `RANIKUST_QUICK_CASES="run copy"`
  picks cases.
