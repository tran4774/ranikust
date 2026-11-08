# Testing

**TL;DR:** Tests are split into tiers by the privileges they need. Anything
above a regular user runs only in a disposable environment (a VM, a throwaway
container on the test machine, a GitHub-hosted runner), never on your
machine. To pick what to run, start at
[Which test for what](#which-test-for-what).

ranikust runs `RUN` as root, mounts filesystems and deletes root filesystems,
and it must behave correctly at every privilege level
([ADR-0004](adr/0004-support-every-privilege-level.md)); the tiers follow
from that.

## Tiers

| Tier | Contents | Privileges | Runs on | Command | Status |
|---|---|---|---|---|---|
| T0 | Unit and in-process tests: parsing, matching, encoding, path resolution on temp dirs | None | Any Linux machine, CI | `just test` | In use |
| T1 | Rootless integration: user namespaces, rootless overlay (`userxattr`), the `RUN` sandbox in a user namespace, rootless Podman | Regular user with entries in `/etc/subuid` and `/etc/subgid` | Developer machine; on Ubuntu 24.04 an AppArmor restriction leaves an unconfined binary's user namespace without capabilities **[measured]** ([ADR-0099](adr/0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md#evidence)), and later releases probably the same **[inferred]**; which legs run in which tier is in [ADR-0100](adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) (planned) | `just test-rootless` ([below](#the-rootless-lane-tier-t1)); `just smoke`, `just smoke-case` ([below](#smoke-tests-tier-t1)) | The rootless lane (profiles `default`, `privileged`); smoke tests |
| T2 | Privileged integration: overlay as real root, `pivot_root`, `mknod`, rootful Docker, kaniko mode on `/`, host-safety attacks | root | A disposable VM, never the host: the local VM, and on a full CI run ([CI lanes](#ci-lanes)) a GitHub-hosted runner ([below](#t2-on-github-hosted-runners)) | `just test-vm` ([below](#the-vm-harness-tier-t2)); CI's `.github/workflows/t2.yml` | Suites `unit`, `smoke`, `quick`, `secrets`, `e2e`, `push`, `ultra` |
| T2c | The root tests that mean the same in a container ([ADR-0063](adr/0063-root-tests-that-mean-the-same-in-a-container-run-in-rootful-docker-on-the-test-machine.md)): `mknod`, `chown`, `setgroups`, Direct `RUN` on a container's `/`, mounts in a private mount namespace, overlay mounts | root in a throwaway rootful Docker container (profile `default`, `sysadmin` or `netadmin`), never `--privileged`, no host volume | The disposable test machine (the VPS), and in CI a GitHub-hosted runner; never a developer's machine; also the VM's `unit` suite | `just test-container` ([below](#the-container-lane-tier-t2c)); CI's `t2.yml` | Suite `unit-c` |
| T3 | CI, in lanes chosen by what a pull request changes ([below](#ci-lanes)): rustfmt, clippy, T0 on x86_64, cargo-deny, rustdoc, mdBook and docs-lint; a full run adds the static musl build and the native aarch64 T0 (the T2 and T2c jobs are in their rows). The `Release` workflow dry-runs on a pull request that changes how a release is made ([CONTRIBUTING](../CONTRIBUTING.md#releasing)) | None | GitHub Actions (`.github/workflows/ci.yml`, `t2.yml`, `release.yml`) | `just ci` (everything `ci.yml` runs on a full run); `just test-release` (the release scripts) | In use |

### Rules

- T0 tests need no privileges and no network, and write only below a fresh
  temporary directory, which they remove on failure too. Some run tools of
  the machine: the `prune` tests, for the shell commands `prune --all`
  suggests, `bash`, `sh`, `flock` (util-linux) and GNU `find`, and, where
  the machine has them, `zsh`, `dash`, `ksh`, `mksh` and BusyBox.
- A test that needs more is marked `#[ignore = "T1: <profile>: <reason>"]`,
  `#[ignore = "T2c: <profile>: <reason>"]` or `#[ignore = "T2: <reason>"]`.
  Its harness runs it with `cargo test -- --ignored` in the right
  environment. A test marked `#[ignore = "inner: ..."]` is a step a T2 test
  starts again in a place of its own (a systemd scope, a container); it does
  nothing unless that test set its variables. Never remove the `ignore` to
  run it by hand, and never run a T2 or T2c test on the host. A
  GitHub-hosted runner counts as a disposable VM
  ([ADR-0090](adr/0090-t2-also-runs-as-root-on-github-hosted-runners.md)):
  only CI's `tests/vm/runner.sh` runs there.
- A root test is T2c (the smallest profile it needs) unless one of these
  keeps it T2, in the VM only
  ([ADR-0063](adr/0063-root-tests-that-mean-the-same-in-a-container-run-in-rootful-docker-on-the-test-machine.md)):
  - (1) a failure could harm the machine running it;
  - (2) it asserts something is blocked that a container's own guards
    (device cgroup, Docker's seccomp profile and capability set, AppArmor)
    would block anyway, so it would pass without proving anything (`nodev`,
    the bounding set, a refused device);
  - (3) it needs the host's own root (the `pivot_root` path, overlay as root
    on a host, `CAP_SYS_RESOURCE`, cgroup v1);
  - (4) it is an acceptance run or a benchmark.
- Running the `ranikust` binary in kaniko mode in a container builds on and
  empties that container's `/`; outside a container it builds in its state
  directory (ADR-0050). Outside the VM, run kaniko mode as root only in a
  throwaway container, with `--state-dir` under a fresh temporary
  directory.
- A bug fix comes with a test that fails without it.
- The sealed copy's T0 tests (`rnk-sandbox`'s `seal` module and the binary's
  `a_build_runs_from_a_sealed_copy_made_once`) expect a memfd copy:
  `vm.memfd_noexec` below 2 and a `TMPDIR` that allows `O_TMPFILE`, not on FUSE
  ([ADR-0101](adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md)).

## Which test for what

| Purpose | Run | Where | Time |
|---|---|---|---|
| A branch's correctness | `just check`; T1 smoke cases in both modes (`rnk-vps-lane run <branch> 'RANIKUST_SMOKE_REGISTRY=mirror.gcr.io just smoke-case'`, and with `RANIKUST_CASE_MODE=kaniko`); VM `unit` and `quick` (`sg kvm -c 'just test-vm unit quick'`, `RANIKUST_QUICK_MODE=kaniko` for kaniko mode) | host or the VPS lane; the local VM | minutes |
| The root tests of a branch that touches no VM-only path, or while the VM is busy | `just test-container unit-c` (T2c, three Docker profiles); the VM's `unit` still runs them too and is the reference before a merge that touches `rnk-sandbox`, `rnk-snapshot` mounts or `rnk-rootfs` deletion | the VPS lane (rootful Docker) | minutes |
| A change to the e2e harness or to how builders are compared | the light e2e: `sg kvm -c 'just test-vm e2e'` (W0, every builder) | the local VM | a few minutes |
| Performance numbers | the ultra benchmark: `sg kvm -c 'just test-vm ultra'` (nextjs-kaniko's W1–W3), only when the maintainer asks | the local VM, nothing else running on the host | tens of minutes per round |
| Acceptance of a phase | the ultra benchmark with rounds: `RANIKUST_E2E_ROUNDS=5 sg kvm -c 'just test-vm ultra'`, only when the maintainer asks | the same | an hour or more |

Nothing heavy runs by default: `just test-vm` alone runs `unit` and `smoke`,
and `e2e` is the light workload. `just test <filter>` passes a name filter to
`cargo test`; for one crate, use `cargo test -p <crate> <filter>`.

## Planned test suites

The plan comes from the verification plan of
[survey 04](research/survey/04-risks-and-roadmap.md) §4 and the open items of
[survey 00](research/survey/00-summary.md). Most of it exists:

| Suite | What it checks | Where it stands |
|---|---|---|
| Parser golden tests | Each Dockerfile in kaniko's `integration/dockerfiles/` (200) parses and expands to the same instructions as BuildKit's parser | T0, done (`rnk-dockerfile/tests/golden.rs`) |
| `.dockerignore` | Same results as moby's `patternmatcher` | T0, done (`rnk-ignore/tests/golden.rs`) |
| Property and fuzz tests | Parser, ignore matcher, tar write/read round trips, cache-key determinism | proptest in several crates (each [crate card](agents/README.md)'s `## Tests`); `cargo fuzz` on `extract` is planned (Phase 6) |
| Image content vs kaniko | Each image flattened to `(path, type, mode, uid, gid, size, sha256, link target, xattrs)` plus its config, compared with kaniko's | T2, done: `e2e` and `ultra` |
| Mode equivalence | Overlay and kaniko mode give the same flattened content, apart from the accepted differences of [ADR-0012](adr/0012-accept-divergence-between-modes.md); `--jobs 1` and the default give identical digests | T1/T2, done: `e2e`, `ultra`, the `reproducible` and `par-stages` cases |
| Privilege matrix | In each environment of [Where environments land](design/modes-and-detection.md#where-environments-land), ranikust picks the expected mode or refuses with guidance | Partly. Rootful environments are T2: the `smoke` (`--privileged`, `SYS_ADMIN`), `e2e` (`rk-kaniko` in a default container) and `secrets` suites. Rootless ones: rootless Podman is T1 (the `rootless`, `kaniko-flags` and `userns-probe` cases) and the VM's regular user T2 (the `smoke` suite's user leg); [ADR-0100](adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) puts the Docker-variant and k3s legs in T2 (planned). Gaps: [below](#not-covered-yet) |
| Host safety | Layers with `../etc/passwd`, symlinks and hardlinks pointing out; `RUN mount`, `setsid sleep 1000 &`, a fork bomb under a cgroup limit, writes to `/etc/resolv.conf`; a `--state-dir` at `/`, below `/home/<user>` and in a directory others may write. After each case: the host is unchanged, and no mount or process is left | T2, partly: the red-team cases of `quick`, the `secrets` suite and the `rnk-sandbox` and `rnk-rootfs` attack tests |

## Not covered yet

- Environments of the privilege matrix that need another host or setup:
  cgroup v1 (`systemd.unified_cgroup_hierarchy=0` and a reboot), Docker with
  `--cap-add SYS_ADMIN` alone (no suite runs it: the `smoke` suite and the
  container lane's `sysadmin` profile always add `apparmor=unconfined`), a
  regular user on Ubuntu 24.04 or later (needs that distribution), a
  Kubernetes pod under the restricted Pod Security Standard.
  [ADR-0100](adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md)
  decides which of these Phase 3 covers (planned) and which wait for Phase 6.
- Kernels before 5.11 (an older `..` walk; Debian 11 has 5.10) and an aarch64
  guest.
- A `--network=none` `RUN` under the rbind of `/sys` below a cgroup v1
  hierarchy (`net_cls,net_prio`) or a configfs mounted in the outer `/sys`:
  only Podman's cgroup2 mount is checked covered (`rootless-reach`, T1),
  the selection of every mount on the rbind in T0
  (`child::submounts::tests`).

## The VM harness (tier T2)

`just test-vm [suite]...` (`tests/vm/vm.sh`) runs the T2 suites as real root
in a disposable QEMU/KVM VM, following
[experiment 13](research/experiments/13-docker-rootful-vm.md) §6. Nothing
runs as root on the host, and nothing there uses `sudo` (only the guest's
user `rnk` has it): QEMU runs as you, with `/dev/kvm` through its group or an
ACL. The header of `tests/vm/vm.sh` lists every `RANIKUST_VM_*` variable. A
run goes through these steps:

1. **Build** on the host, as the user, with `cargo zigbuild`: the static
   musl test binaries of the crates that hold `#[ignore = "T2: ..."]` and
   `"T2c: ..."` tests (`unit`), and the `ranikust` binary with the
   `smoke-build` example, for every run: the network check builds with
   `ranikust` before any suite (step 5).
2. **Fetch** the Ubuntu cloud image of `RANIKUST_VM_RELEASE` (`noble`,
   kernel 6.8, by default; `jammy`, kernel 5.15) and check its SHA-256
   against the published `SHA256SUMS`. The verified image is cached in
   `tests/e2e/_vm-image/` of the main checkout (gitignored,
   `RANIKUST_VM_CACHE`), which every git worktree of it shares. Only one VM
   runs at a time on the machine: a run holds `flock` on
   `_vm-image/.vm.lock` from start to teardown, and a second run waits for
   it (`RANIKUST_VM_LOCK_WAIT`, default an hour).
3. **Provision** the base image once, from the verified cloud image booted
   with `tests/vm/cloud-init/provision`: it installs the packages (Docker
   from the Ubuntu archive, `zstd`, `pigz`, `busybox-static`, and `uidmap`
   for the regular user's `newuidmap`), disables the apt timers,
   removes what is per machine (SSH host keys, machine ID; no user, key or
   password is created) and powers off. The disk becomes one read-only
   qcow2, `base-<release>-<key>.qcow2`, with its SHA-256 beside it, checked
   before every use. `<key>` hashes the release, the cloud image's SHA-256,
   the package list and the provisioning seed, so a change builds a new
   base image (a few minutes, once) and deletes the release's old one; a base
   image that no longer matches its SHA-256 is built again.
   `RANIKUST_VM_GOLDEN=0` skips it: the run boots the cloud image and
   cloud-init installs the packages.
4. **Prepare** a run directory under `target/vm/`: a copy-on-write qcow2
   overlay on the base image (the base is only ever a read-only backing file;
   every write goes to the overlay, deleted with the run), a fresh SSH key
   and a NoCloud seed with a new instance ID (`tests/vm/cloud-init/user-data`:
   user `rnk` with passwordless `sudo` and the run's key; new SSH host keys
   and machine ID at first boot). Docker Hub images come through
   `RANIKUST_VM_IMAGE_REGISTRY`, by default `mirror.gcr.io` (Google's public
   mirror, no pull rate limit): the guest's Docker uses it as a
   `registry-mirrors` entry, and the suites pull `python:3.13-alpine` and
   `alpine:3.20` from it by name. `docker.io` pulls from Docker Hub itself,
   which allows anonymous clients only a few pulls per hour.
5. **Boot** QEMU as the invoking user, SSH forwarded to a free port on
   `127.0.0.1` only, one SSH connection shared by every command. The network
   reaches nothing on the host's loopback but what a suite needs
   ([ADR-0086](adr/0086-the-t2-vm-reaches-no-host-loopback-port-but-the-test-registries.md)
   has the design and the probes):
   - Egress (image pulls, apt, e2e fetches) goes through passt, IPv4 only,
     which maps no guest address to the host's loopback; DNS goes to the
     host's first IPv4 nameserver. A second NIC (QEMU's user network with
     `restrict=on`) carries only SSH and, while `push` runs, the host's test
     registries as `10.0.2.2:5000` and `10.0.2.2:5443`.
   - `vm.sh` runs `push` last, in an SSH step of its own, and opens the
     registries' gate only around that step; the gate never reopens, and it
     shuts when the guest reports the step's end or after about 900 s, which
     fails the run.
   - Before any suite, `tests/vm/guest/net.sh` probes a token `vm.sh` serves
     on the host's loopback, over TCP and UDP, from four vantages: root in
     the VM, a default `docker run`, a `--privileged` `docker run` and a
     ranikust `RUN`. Every address that could name the host must go
     unanswered, the registry ports must be shut, and each vantage must get
     out with the same tool. `tests/vm/guest/rawprobe.py` also writes raw
     frames, as root, straight to each NIC's backend (loopback, unspecified
     and own addresses, IPv4 and IPv6), which must go unanswered while
     controls through the same socket answer; the egress must have no IPv6.
     Any answer, a failed control or a failed image pull fails the run
     (`network: FAILED` in `summary.txt`, `network.log` in the results).
   - Needs QEMU 10.1 or later with its passt backend (Ubuntu 24.04's 8.2 and
     Debian 13's 10.0 are refused), passt 2024_10_30 or later, and `socat`
     for `push`; `vm.sh` checks every option it uses before boot.
   - Resources: 12 vCPUs (`RANIKUST_VM_CPUS`, at most the host's CPUs less
     2), 16 GiB (`RANIKUST_VM_MEM`), a 20 GB disk (`RANIKUST_VM_DISK`, 40 GB
     for `ultra`, at least 10 GB over the base image). With less than 16 GiB
     plus 3 GiB available on the host the memory default shrinks to fit,
     never below 4 GiB (8 GiB for `e2e` and `ultra`), and the run says so; it
     stops before booting when even that would leave the host under 2 GiB. A
     value set in a variable is used as is, with a warning if it leaves the
     host under 2 GiB. `vm.sh` also refuses to start when the host has less
     free space where the disk lives than its size less 2 GiB. What really
     bounds the disk is the guest (`e2e` stops below its free-space floor,
     below). The effective size goes into `env.txt` and `timings.txt`.
   - The cache directory must be a directory owned by you, not a symbolic
     link, and not writable by group or others: the default one is made
     `0700` when everything in it is yours and no link, while one named by
     `RANIKUST_VM_CACHE` is refused, never chmodded. `vm.sh` also refuses a
     base image that is not a self-contained qcow2 and cache or target paths
     that are not absolute or hold a comma (QEMU's option separator). All
     devices are virtio; the disk is `cache=unsafe` (it is thrown away).
6. **Copy in** the binaries and `tests/vm/guest/`, **run** the suites
   (`guest/run.sh`) detached in the guest while the host follows their log
   over SSH every 5 s, reconnecting for up to 15 minutes (a VM that stalls
   while the host swaps loses neither the run nor its results), and **copy
   out** `target/vm/results/<release>/` (retried): `env.txt` (kernel, Docker,
   cgroup, CPUs, memory, the VM's size, and what must be the run's own: its
   one user with one key, the SSH host key made at this boot, its machine
   ID), one log per suite, `summary.txt`, `suite-times.txt`, `timings.txt`
   (boot started, SSH up, cloud-init done, suites done, total, in seconds)
   and the VM's serial log (`base-image-serial.log` too when building the
   base image failed). The guest refuses to run any suite when it holds
   a user other than `rnk`, no SSH host key, or one older than its boot. The
   results reach the host as regular files and directories only (anything
   else is deleted on both sides), extracted into a new directory and put in
   place only then; teardown clears them the same way before it writes the
   serial log and `timings.txt`, through new files renamed into place.
7. **Tear down** on every exit path, Ctrl-C included: QEMU is stopped
   through its monitor (then `SIGKILL`), the SSH connection is closed, and
   the overlay, seed and key are deleted (the base image stays in the
   cache). The script fails if QEMU is still running.

| Suite | What it runs |
|---|---|
| `unit` | Every test marked `#[ignore = "T2: ..."]` or `"T2c: ..."` (the VM runs the container lane's tests too: it is the reference), found by name in `crates/`, as root with `--ignored`; each must run exactly once and pass. The log keeps each test's result line and the `measured:` lines a test writes to stderr past libtest's capture, as the cgroup tests of `rnk-sandbox` do with their numbers and `ranikust`'s layer tests with a case they skip. Experiment 09's deletion cases: D2 is `rnk-rootfs` `mounts_inside_the_root_survive` (a bind mount and a tmpfs inside a root survive its emptying); D1 (emptying a container's `/` keeps its volumes and runtime files) is the `hygiene-mounts` case in kaniko mode; D3 is `ranikust`'s state-directory tests and `rnk-store`'s lock of the state directory |
| `smoke` | `smoke-build` on `python:3.13-alpine` as real root on the VM, in `docker run --privileged` and in `docker run --cap-add SYS_ADMIN --security-opt apparmor=unconfined` (Docker's seccomp profile, so no `pivot_root`). Each variant checks the probe's root entry and runs `just smoke`'s checks (`tests/smoke/lib.bash`) plus real-root ones: a write to `/proc/sys` fails (T17), a device node the `RUN` makes does not open (T18), and none of the [Q-077](open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape) chroot escapes reaches a marker outside the stage root (a plain `chroot` is their control: there each must succeed). Docker then runs each image. The `ranikust` binary then builds `tests/smoke/e2e` in the same environment (the workdir a volume in Docker, since overlay refuses an upper directory on the container's own overlay root): it must reach overlay mode as root with a real overlay probe, no stub or secret may reach a blob, a `RUN --network=none` sees only `lo`, and Docker loads and runs the image as user `app`; the build's user-namespace probe line is printed, exactly one per build, and on the VM itself it must pass. In the `SYS_ADMIN` variant (no `NET_ADMIN`), `RUN --network=none` must be refused without `--allow network.none-userns` ([ADR-0033](adr/0033-network-none-loopback-through-a-user-namespace.md)); with it, over the default seccomp profile and under `--seccomp=unconfined` (one filter fewer), `tests/smoke/netns-check.py` checks from the `RUN`'s root what stays blocked (joining the namespace's owner, `unshare(CLONE_NEWUSER)`, x_tables, `TCP_REPAIR`, `AF_KEY`, nf_tables and xfrm netlink) and what works (loopback, rtnetlink, audit netlink: a later `RUN useradd` succeeds). On the VM itself: `--reproducible` gives one digest with `--jobs 1`, the default, 2 s later and with `--metrics-file`; the secret is in no build log and no metrics report; a cache mount written by one build is read by the next with the same state directory and not by a fresh one; builds in overlay mode and kaniko mode per stage run from a sealed memfd copy of the binary, and with the memfd skipped (`RANIKUST_DEBUG_FAULT=seal-tmpfile`) from an `O_TMPFILE` copy, which `tests/vm/guest/sealed-exe.py` checks while a `RUN` waits: ranikust and its init run one copy that root cannot open for writing; the `RUN` keeps the core-file limit ranikust started with, and reports why it cannot open its init's `/proc/1/exe` for writing (the init is not dumpable, ADR-0051, the barrier before the copy) ([ADR-0101](adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md)). In a default `docker run`, a `ranikust` build without `--mode` must pick kaniko mode and its user-namespace probe stop at `unshare`. As the VM's regular user, `smoke-build --userns` with the same `RUN`s (#78) must be refused where AppArmor restricts unprivileged user namespaces (Ubuntu 24.04), and elsewhere (22.04, `RANIKUST_VM_RELEASE=jammy`) choose, with no `--allow proc.rbind`, a new `proc` and an rbind of `/sys`, pass every check, and give an image Docker runs. The `ranikust` binary as that user builds the `rootless` case's `content` stage too (#79), as root does first: on 22.04 in rootless overlay mode with a new `proc` and root's `--reproducible` digest, then `tests/smoke/e2e`, whose `RUN --network=none` sees `lo` alone in `/sys/class/net` (a new `sysfs` for the network namespace the build owns), and Docker runs that image; on 24.04 refused, naming the step and the AppArmor switch. Where it builds, a set-up made to fail in the namespace (`RANIKUST_DEBUG_FAULT=rootless-broken`) is refused (exit 1) and never falls back to kaniko mode (ADR-0136). There too the `rootless-reach` case's `writes` stage, with a new `proc` and the host's writable `/sys/fs/cgroup` under the rbind of `/sys`, writes and reaches nothing outside its `RUN`, sees no ranikust process and, from `--network=none`, no network device outside its own, and a tree past `PATH_MAX` a `RUN` leaves in a cache mount, partly a mapped user's, goes with `ranikust prune --all` as that user, after which a build in the emptied directory runs. Inside a plain `chroot`, `smoke-build` and `ranikust` must refuse before any `RUN`. Afterwards the VM's `domainname` is unchanged and no mount or `smoke-build` process is left |
| `quick` | The [quick smoke cases](#quick-smoke-cases) built by the `ranikust` binary as real root on the VM, Docker loading and running each image; `RANIKUST_QUICK_CASES` picks cases. A minute or two: the suite for a smoke test or a proof of concept that needs real root. Not part of the default `just test-vm` |
| `e2e` | The light workload W0 ([below](#end-to-end-workloads-e2e)) built by kaniko and BuildKit as references and by `ranikust` in overlay mode, in kaniko mode on the `/` of a default `docker run` (`rk-kaniko`) and in kaniko mode as root on the VM (`rk-kaniko-host`, a directory per stage); times, image comparison, leaks, metrics. A few minutes with the VM's boot: the check of a change to the harness. Not part of the default `just test-vm` |
| `ultra` | The ultra benchmark: the same on `nextjs-kaniko`'s W1–W3, with its npm proxy; tens of minutes per round. Only when the maintainer asks for performance numbers or an acceptance run. Not part of the default `just test-vm` |
| `push` | One `ranikust build` as real root pushing to three registries and writing `--tar-path` and `--oci-layout-path`: a `registry:2` on the VM's loopback (plain HTTP, anonymous, unasked), the host's HTTP [test registry](#test-registries) as `10.0.2.2:5000` with `--insecure-registry`, and the host's HTTPS one as `10.0.2.2:5443` with `--registry-certificate` and credentials from a `config.json`. Each manifest read back must hash to the printed digest, the HTTPS one must refuse an anonymous read, Docker pulls and runs the loopback image and loads and runs the tarball and the layout, and every layout blob must hash to its name. Needs `just registry-up`; not part of the default `just test-vm` |
| `secrets` | ADR-0051: kaniko mode on the `/` of default Docker containers (`ubuntu:24.04`, no `--privileged`) with `RUN` steps that attack ranikust (`tests/vm/guest/secrets/attack.sh`; vectors V1–V13 in the header of `secrets.sh`): the state directory, `/proc/*/fd`, ranikust's `environ`, `mem` and `cmdline`, the `RUN`'s own environment, processes left alive, the same from `unshare -Ur`, another process's `environ`, the sealed copy of the binary through every thread's `exe`, `ptrace` and `process_vm_writev` (V13, with the VM's Yama `ptrace_scope` at 0 for the suite). Three scenarios (ranikust as PID 1, the same with `--cap-add SYS_PTRACE`, and started from `sh -c`); every vector must be blocked but the two ranikust only warns about (a `--build-arg` value in `cmdline`; the shell's `environ`), whose warnings must appear. The state directory may hold only the store's marker of a build in progress, `.ranikust-building` (the PID namespace and boot id). It also records whether the container's overlayfs takes `O_TMPFILE` and how ranikust made its state files, then runs `stopsig` (ADR-0077) and `memlimit` (ADR-0056: a 512 MiB layer built under `--memory 256m --memory-swap 256m` must succeed, its state on disk, never in `memfd`). Not part of the default `just test-vm` |

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
keeps the data. The T2 VM reaches them as `10.0.2.2` only while the `push`
suite runs (ADR-0086), so they need no wider binding. On a host whose
AppArmor restricts unprivileged user namespaces, only podman (which has a
profile) can read local storage, so the check pushes with `podman push` and
reads with `skopeo inspect`.

### End-to-end workloads (`e2e`)

`tests/vm/guest/e2e.sh` builds container images with every builder in the
VM, the way `nextjs-kaniko/bench/run-fair.sh` runs them, times each build,
and compares and checks the images. Its header lists the options. Two
profiles:

- **Light** (`just test-vm e2e`, the default): W0, the Dockerfile and files
  in `tests/vm/guest/e2e/w0`, on a base image the suite makes and pushes to
  its registry (`guest/e2e/base-w0.py`: the VM's static busybox, a file with
  an extended attribute, two names of one inode, a setuid file, a symlink, a
  sticky `/tmp`, files to delete; the same bytes every run). Its stages
  exercise what the comparison and kaniko mode's stage directories need: a
  `RUN` making a hardlink, a symlink and a setuid file and deleting a file
  and a directory of the base (whiteouts); three children of one stage and a
  grandchild (`FROM <stage>`, ADR-0059), two of them independent (`--jobs`);
  `COPY --from` two stages; `COPY` from the context and a `RUN` deleting a
  copied file. `ranikust` and `rk-kaniko-host` build it with
  `--reproducible` at `--jobs 1` and the default, whose contents must match.
  Every builder, one round, no warm-up, nothing pulled from a network: a few
  minutes with the VM's boot. Only the reference images go in (kaniko,
  BuildKit, `registry:2`); no `nextjs-kaniko` checkout is needed.
- **Ultra** (`just test-vm ultra`, `guest/ultra.sh`): the acceptance
  workloads of [Phase 1](agents/phase-1.md#acceptance-criteria), W1–W3 of
  `nextjs-kaniko`, without touching the host's `nextjs-kaniko` containers;
  tens of minutes per round, a 40 GB disk (the guest peaked at 6.3 GB on W1
  and W3 with the registry pruned). Only when the maintainer asks
  ([which test for what](#which-test-for-what)). The rest of this section
  describes it; the light profile shares everything but the seed, the npm
  proxy, the warm-up and the blob check (nextjs-kaniko's script).

The parts:

- **Seed.** `tests/vm/e2e-seed.py` streams in, read-only from the
  `nextjs-kaniko` checkout next to the main checkout
  (`RANIKUST_NEXTJS_KANIKO`): W1 `app`, W2 `app-cache`, W3 `app-par`; the
  base and tools images that the workloads name by digest, copied from the
  host registry's storage with their blobs so the digests hold; the
  verdaccio storage, so pnpm installs without npmjs; and
  `bench/digest-check/check-blobs.py`. The reference images (kaniko
  `localhost/kaniko-bench:stream`, BuildKit `v0.33.1-rootless`,
  `registry:2`, verdaccio 6.1.6) are saved once from the host's podman into
  `tests/e2e/_vm-image/e2e/`. About 2.8 GB goes in per ultra run.
- **Services.** A registry (`registry:5000`) and verdaccio
  (`verdaccio:4873`) on an internal Docker network, as `docker-compose.yml`,
  also published on the VM's loopback for `ranikust`, whose `RUN` shares the
  VM's network.
- **Builders.** kaniko with `cfg-final-opt.env` and
  `FF_KANIKO_STREAM_TAR_COMPRESS` (experiment 07's `k-stream`) and BuildKit
  as `run-fair.sh`'s `bk` (zstd), each in `docker run --cpus=4 --memory=8g`
  pinned to the VM's last 4 vCPUs (`--cpuset-cpus`; the registry and
  verdaccio on the others). `ranikust` three ways:
  - `ranikust`: overlay mode as root in a systemd scope with the same budget
    (`CPUQuota=400%`, `MemoryMax` and `MemorySwapMax` 8 GiB, as Docker gives
    a container with `--memory` alone, `AllowedCPUs` the same vCPUs);
  - `rk-kaniko`: kaniko mode on the `/` of a default `docker run` (root,
    Docker's capabilities, no option) from an empty image, the static binary
    mounted at `/ranikust/bin` (ADR-0048), run without `--mode` so that
    `--mode=auto` must pick kaniko mode there (the build fails unless its log
    says so);
  - `rk-kaniko-host`: `--mode=kaniko` as root on the VM in the same kind of
    scope, a directory per stage in its state directory (ADR-0059).

  Each build gets a fresh state directory. The builders' results directories
  are `/bench-out` and `/rnk-out`, not `/out`, which W3's Dockerfile writes:
  a mount point never reaches a layer in kaniko mode. W2 is built cold,
  without a cache. On W3 `ranikust` and `rk-kaniko-host` run once per
  `--jobs` value (`1 2 default`) with `--reproducible`. A builder container
  writes only its own directory (`<build>/mnt`, mounted as `/bench-out` or
  `/rnk-out`), whose `metrics.json` and `digest.txt` are copied out if they
  are regular files once the container is gone; everything the harness
  writes (`build.log`, `limits.txt`, `run.json`, the listing) goes to the
  build's directory, which no container reaches.
- **Timing and limits.** Time is taken as `run-fair.sh` takes it: start of
  the build container (or process) to its exit; one discarded kaniko build
  per workload warms the page cache and verdaccio first (ultra). Each
  build's own limits, read from its cgroup while it runs (`cpu.max`,
  `cpuset.cpus.effective`, `memory.max`, `memory.swap.max`), go to its
  `limits.txt` and `run.json` and to the log after its time, with its peak
  memory (`memory.peak`, page cache included; read by `limits.sh self` when
  a scope's build ends, sampled every 0.2 s while a container runs), which
  the report gives per builder as `Peak MiB`. They do not grow with the VM.
- **Registry and disk.** Every build starts from an empty `e2e/` namespace
  in a restarted registry, whose blobs no repository links any more are
  deleted first (`guest/e2e/prune-registry.py`: the registry never frees
  them). The pruner runs as root on a directory the registry's container can
  write, so it follows no symbolic link (every directory opened with
  `O_NOFOLLOW`, blobs reached through their parent's descriptor) and stops
  before deleting anything when a repository cannot be read or no blob is
  linked at all. `fstrim /` then hands the freed blocks back to the host
  (`discard=unmap`). A state directory is deleted only when nothing is
  mounted below it, and never across a filesystem. No build starts with less
  than `RANIKUST_E2E_MIN_FREE_GB` (10) free on the guest's disk once
  leftovers are pruned: the suite fails instead.
- **Checks** on the first round only (`RANIKUST_E2E_CHECK_ALL=1` checks
  every round), each after its build has been timed and before the next
  starts, never beside a timed build:
  - Each image is flattened from the registry (`guest/e2e/flatten.py`: path,
    type, mode, owner, size, sha256, link target, hardlink, xattrs;
    whiteouts applied; gzip and zstd). Each layer is read once per run into
    a cache under its blob digest, so a base layer every image shares is
    read once, the others side by side on the vCPUs the builds are not
    pinned to, through `pigz`. `flatten_v1.py` is the reference its listing
    is checked against, byte for byte, by `python3 -I -m unittest discover
    -s tests/vm/guest/e2e -p 'test_*.py'`.
  - The images are compared pairwise (`guest/e2e/compare.py`), with
    experiment 07's non-deterministic W1 files counted as noise and
    hardlinked entries apart (ADR-0012); configs are compared field by
    field. The kaniko-mode images are compared with kaniko's and with
    overlay mode's. `check-blobs.py` checks that every gzip layer describes
    itself.
  - On W3 the raw digests at each `--jobs` value are reported, and the
    images are compared by flattened content against the first value
    (`compare-jobs<a>-jobs<b>.txt`), with the workload's `du`-derived files
    (`/out/a.txt`, `/out/b.txt`, `/out/c.txt`, `/out/all.txt`) passed to
    `compare.py --noise`; any other difference fails the suite
    ([Q-187](open-questions.md#q-187-how-does-the-w3-digest-check-treat-the-workloads-du-output)),
    for each builder that takes `--jobs` (`digests-<builder>.txt`,
    `compare-<builder>-jobs<a>-jobs<b>.txt`).
  - Every `ranikust` image must hold nothing of `/ranikust`,
    `/var/lib/ranikust`, a `.ranikust-*` name, `/.dockerenv`,
    `/run/.containerenv`, the build context's or results' mount points, or
    the VM's state directories, and no `/etc/hosts`, `/etc/resolv.conf`,
    `/etc/hostname` or file under `/etc/ssl/certs` that kaniko's image of the
    workload lacks or holds with other content (the runtime mounts the first
    three, the harness binds the certificates; `guest/e2e/leaks.py`). A leak
    fails the suite.
  - kaniko's `metrics.json` and `ranikust`'s report are put side by side
    through the phase map of
    [metrics](design/metrics.md#comparing-with-kaniko)
    (`guest/e2e/metrics.py`).

  The references already differ from each other, which a `ranikust`
  comparison has to read against: BuildKit (rootless, in rootful Docker)
  gives the directories that a `RUN` or `WORKDIR` creates the setgid bit
  (`2755` where kaniko has `755`), and on W0 its image also differs in
  `/dev`, `/proc` and `/sys`. On W3, kaniko's image has no `/out/all.txt`
  (the last `RUN` writes it), its `/out` is `1000:1000 775` where BuildKit's
  is `0:0`, and the `/out/c.txt` contents differ too (not investigated
  yet).
- **Results** in `target/vm/results/<release>/e2e/` (`ultra/` for the ultra
  benchmark): `report.md`, with the time of each step summed over the builds
  (wipe, build, flatten, leaks, blobs, compare) and the suite's wall time;
  `phases.tsv` with each build's steps; and per workload and build
  `run.json`, `build.log`, `limits.txt`, metrics, the listing and config,
  `compare-*.txt`.
- **Options** are `RANIKUST_E2E_*` variables, checked at start-up (a wrong
  one stops the suite) and listed with their defaults in the header of
  `e2e.sh`: `WORKLOADS` (a subset such as `w1 w3` shortens an ultra run),
  `BUILDERS`, `ROUNDS` (3 or more for acceptance timings), `WARMUP`, `CPUS`,
  `MEMORY`, `CPUSET` (the vCPUs every builder is pinned to; `none` for the
  quota alone), `JOBS`, `CHECK_ALL` and `MIN_FREE_GB`.

A `ranikust` build that stops because a part is not built yet (`not
implemented`, a flag refused until a later phase) is recorded as `pending`
and does not fail the suite; any other failure does.

## The container lane (tier T2c)

`just test-container [unit-c]` ([ADR-0063](adr/0063-root-tests-that-mean-the-same-in-a-container-run-in-rootful-docker-on-the-test-machine.md))
runs the `#[ignore = "T2c: <profile>: ..."]` tests as root in throwaway
rootful Docker containers on the disposable test machine (the VPS), beside
the VM. `tests/container/remote.sh` sends the current branch **as
committed** through the lane helper (`RANIKUST_LANE`, default
`rnk-vps-lane`: `sync <ref>`, then `run <ref> '<command>'`, whose exit
status is the command's) and runs `tests/container/lane.sh` there, which:

1. refuses, before anything else, unless the machine carries the marker
   `/etc/ranikust/throwaway-test-host`: a regular file (not a link) owned by
   root in a root-owned directory, neither writable by group or others,
   holding this machine's `uname -n`. It then refuses unless
   `RANIKUST_CONTAINER_LANE=1` (set by `remote.sh`), when run as root, and
   with rootless Docker or Podman. It locks the marker itself, read-only (one
   lane run per machine, whoever starts it), removes the labelled
   (`org.ranikust.lane=t2c`) containers and images an interrupted run left,
   and refuses when a test name is in two of the lists (VM-only, `default`,
   `sysadmin`, `netadmin`), since tests are selected by name;
2. lists the tests of each profile from the markers and builds the test
   binaries of their crates as static musl binaries (`cargo-zigbuild test
   --no-run`);
3. builds an image (`tests/container/Dockerfile`: Ubuntu 24.04 by digest,
   `iproute2`, `python3` for the Direct `RUN` attack tests) with the
   binaries, the lists and `tests/container/run-profile.sh` copied in; no
   volume ever;
4. runs one container per profile, `--rm --network none`, tmpfs mounts with
   device nodes allowed at `/work` (`TMPDIR`) and `/var/tmp` (the overlay
   tests keep their state there; an overlay upper directory cannot sit on
   the container's own overlay root), `--memory 6g --pids-limit 8192
   --ulimit nofile=65536`, never `--privileged`:

   | Profile | `docker run` options | Tests |
   |---|---|---|
   | `default` | none (Docker's default capabilities, seccomp, AppArmor) | Direct `RUN` on the container's `/`, `mknod` of real device nodes, `chown` to other users |
   | `sysadmin` | `--cap-add SYS_ADMIN --security-opt apparmor=unconfined` | overlay mounts, mounts in a private mount namespace |
   | `netadmin` | `sysadmin` plus `--cap-add NET_ADMIN` | user and network namespaces with loopback up |

5. in each container, runs the profile's **control checks** first, which
   prove the container is what the profile says: root; a 1:5 device node in
   `TMPDIR` opens (Docker's `--tmpfs` is `nodev` by default, which would let
   a device test pass without opening anything); block nodes can be made;
   `chown`; `setgroups`; for `default`, exactly Docker's default bounding set
   (`00000000a80425fb`) and no mount namespace; for `sysadmin` and
   `netadmin`, a private mount namespace with a tmpfs, an overlay mount in
   `/work` and in `/var/tmp`, and a user namespace; for `netadmin`, loopback
   up in a new network namespace; for `sysadmin`, no `NET_ADMIN`. A failed
   check fails the run (exit 3): nothing is skipped;
6. then runs every test of the profile with `--ignored --exact
   --test-threads=1`, by full path (each ignored test of a binary whose last
   path component is listed, so a VM-only test whose name contains a listed
   one never runs here; the VM's `unit` suite selects the same way), every
   binary even after one fails; each test must run exactly once and pass
   (`T2c <profile> tests: N passed of N listed`).

The results are each container's output, written by the shell on the test
machine to `target/container/results/<profile>.log` there; locally the whole
run is in `target/container/last.log`. The image and any container left are
removed when the run ends, interrupted or not.

**One-time setup of a test machine** (by its owner, as root, only on a
disposable machine, never on a developer's machine):

```sh
sudo install -d -m 0755 /etc/ranikust
uname -n | sudo tee /etc/ranikust/throwaway-test-host >/dev/null
sudo chmod 0644 /etc/ranikust/throwaway-test-host
```

plus rootful Docker with the user in the `docker` group, `jq`, `flock` and
the toolchain of the VPS lane (Rust, `cargo-zigbuild`, zig).

**Accepted residual risk:** the `sysadmin` and `netadmin` profiles
(`SYS_ADMIN`, AppArmor unconfined, `NET_ADMIN`; Docker's default seccomp
profile kept) are as good as root on the test machine's kernel (for example,
a test or a bug could remount `/proc/sys` read-write and set
`kernel.core_pattern`). The machine's disposability is what makes that
acceptable, so the marker check is load-bearing: never create the marker
anywhere else, and treat a machine that carries it as one to rebuild, not to
trust. The one exception: on a GitHub-hosted runner, CI's
`tests/vm/runner.sh` makes a marker naming that runner for its one job,
behind its own guard
([ADR-0090](adr/0090-t2-also-runs-as-root-on-github-hosted-runners.md),
which amends ADR-0063 for it; [below](#t2-on-github-hosted-runners)).

**Tests that stay T2** (the VM, and CI's GitHub-hosted runners, only) are
found with `grep -rn 'ignore = "T2:' crates`; today all of them are
`rnk-sandbox` tests, and each `ignore` message says what it needs. Some
examples, with the reason from the [rules](#rules):

- (2): `a_direct_run_on_slash_keeps_only_dockers_capabilities` (a default
  container already has only Docker's set, so it would pass without proving
  the drop).
- (2) or (3): `a_mount_below_the_root_is_nodev_in_the_run` and
  `red_team_no_mount_of_the_root_tree_lets_a_device_node_open` (the full
  sandbox takes Docker's no-`pivot_root` fallback there, and the device
  cgroup also refuses most nodes, so `nodev` is not what is proven).
- (3): tests that require `RootEntry::PivotRoot`, which Docker's seccomp
  profile refuses (`probe_reaches_exec_as_root`,
  `a_run_joins_a_held_network_namespace`,
  `red_team_a_run_with_every_mount_holds_only_its_stdio`,
  `red_team_a_run_cannot_change_the_hosts_dev_nodes`,
  `red_team_no_run_outlives_the_thread_that_cloned_it`); and
  `direct_runs_in_a_chroot_with_placed_files_as_root`, which raises the
  `RLIMIT_CORE` hard limit and so needs `CAP_SYS_RESOURCE`.

## CI lanes

CI does not run every job on every push
([ADR-0095](adr/0095-ci-runs-by-lanes-a-fast-lane-on-drafts-and-the-full-run-when-ready-behind-two-gate-checks.md)).
A `changes` job in `.github/workflows/ci.yml` and in
`.github/workflows/t2.yml` runs `scripts/ci.py changes`, which reads what a
pull request changed (`git diff --name-only --no-renames <base>...<head>`)
and picks the lanes; every other job runs only when its lane is needed, and
skips itself otherwise.

| Lane | Paths that need it |
|---|---|
| `rust` | `crates/**`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.cargo/**`, rustfmt and clippy configuration, `justfile`, `scripts/check-static.sh` |
| `book` | `docs/book/**` |
| `t2` | the `rust` paths, `tests/vm/**`, `tests/container/**`, `tests/smoke/**` |
| `deny` | `Cargo.toml`, `Cargo.lock`, `crates/*/Cargo.toml`, `deny.toml`, `rust-toolchain.toml`, `.cargo/**` |
| every lane | `.github/workflows/**`, `scripts/ci.py`, `.gitattributes`, and any path no rule names (a new file under `.github/`, such as a local action, included) |
| no lane | the rest of `docs/`, top-level Markdown, `.claude/`, `.github/ISSUE_TEMPLATE/**`, `.github/PULL_REQUEST_TEMPLATE.md`, `.github/dependabot.yml`, `tests/ci/`, `tests/docs/`, the Release workflow's files (`.github/release.yml` among them), the local-only harnesses (`tests/bench`, `tests/e2e`, `tests/registry`) and tools (`scripts/review-pack.py`, `scripts/prune-worktrees.sh` with `tests/agents/`) |

`scripts/ci.py`'s `RULES` is the authority; `just test-ci` fails on a
tracked file no rule names, so a new path gets its lane when it is added.

What runs, by event:

| Job (check name) | Draft pull request (fast lane) | Full run |
|---|---|---|
| `rustfmt`, `clippy`, `test (x86_64 gnu)` | when `rust` | when `rust` |
| `cargo-deny` | when `deny` | when `deny` |
| `docs (rustdoc, mdBook)` | rustdoc when `rust`, mdBook when `book` | the same |
| `docs-lint` | always | always |
| `static release build (x86_64 musl)`, `test (aarch64 musl, native)` | no | when `rust` |
| `T2 unit`, `T2c unit-c`, `T2 smoke, quick, secrets` | no | when `t2` |

- **A full run** is a pull request that is ready for review (marking a draft
  ready starts one), a draft labelled `ci:full`, a manual run ("Run
  workflow" in the Actions tab, `workflow_dispatch`, on any branch) or a
  push to `main`. A push to `main` and a manual run count as changing every
  path, so they run every job. A manual run tests the branch's head, not its
  merge with `main`.
- **Gates.** `CI result` and `T2 result` are the only checks the `main`
  ruleset requires. Each needs every job of its workflow (a job skipped by
  its lane counts as passed) and fails when the `changes` job did not
  succeed, when an output it maps is not exactly `true` or `false` (checked
  by name: the runner drops an empty one), or when any job failed or was
  cancelled. It runs even then (`always()`; `!cancelled()` may leave the
  gate skipped, which counts as passed). The name carries the state: plain
  `CI result` and `T2 result` only on a full run of a pull request or of
  `main`; `CI result (draft, fast lane)` and `T2 result (draft, not run)` on
  a draft; `... (manual run)` on a manual run on another branch; `... (label
  change, nothing run)` after a label change. GitHub counts the check run of
  a name created last, so a draft or a manual run never stands in for a pull
  request's full run: after a draft is marked ready the pull request waits
  for its gates.
- **Labels.** Only `ci:full` on a draft (in any case: `CI:Full` too) changes
  what runs. Any other label change, or `ci:full` on a pull request that is
  already ready, starts a run that tests nothing; it never cancels another
  run, though a newer push still cancels the older run of the same pull
  request. After one, the job rows (`rustfmt`, `clippy`, ...) show "skipped"
  from that run: read the gates, which keep the last real run's verdict.
- **Who starts a full run.** The maintainer marks a pull request ready, adds
  `ci:full` or clicks "Run workflow" (whose gates, `... (manual run)`, report
  the run but do not count for the pull request). Agents use the
  maintainer's account, so GitHub cannot tell them apart: an agent asks
  before marking a pull request ready, and adds `ci:full` only when the
  maintainer asks or to verify a pull request that changes CI itself
  ([workflow.md](agents/workflow.md#github-flow)).
- **A re-run** ("Re-run jobs") repeats the lanes of the run it repeats: the
  draft state and labels of that event, and the workflow files of that
  commit.
- **Caches.** rust-cache entries are saved only from `main`. On a push to
  `main`, a `caches` job in each workflow deletes the entries of its jobs
  that a newer entry with the same restore key supersedes
  (`scripts/ci.py stale-caches`), keeping one entry per restore key (the
  job's cache name, OS, architecture and toolchain hash; entries of an older
  toolchain stay until GitHub evicts them); only that job holds
  `actions: write`.
- **Timeouts.** Every job has one: 5 minutes for the gates, 10 for the small
  jobs, 20 for `clippy` and the docs, 30 for the tests, the static build and
  T2.
- `just ci` runs everything `ci.yml` runs on a full run (`just test-ci`
  first, as the `changes` job does); `t2.yml` has no recipe
  ([below](#t2-on-github-hosted-runners)).

## T2 on GitHub-hosted runners

On a full CI run ([above](#ci-lanes)) that touches the Rust code or the T2
harness, `.github/workflows/t2.yml` runs the T2 suites `unit`, `smoke`,
`quick` and `secrets` and the T2c lane `unit-c` as root directly on
GitHub-hosted `ubuntu-24.04` runners
([ADR-0090](adr/0090-t2-also-runs-as-root-on-github-hosted-runners.md)): a
runner is a fresh VM GitHub makes for one job and deletes after it, with
passwordless `sudo`, so it takes the VM's place there. Three jobs, each its
own runner, about five minutes of wall time together:

| Check | Suites |
|---|---|
| `T2 unit (root on the runner)` | `unit` |
| `T2 smoke, quick, secrets (root on the runner)` | `smoke`, `quick`, `secrets` |
| `T2c unit-c (rootful Docker on the runner)` | `unit-c` |

- `tests/vm/runner.sh <suite>...` is the entry point (its header has the
  details). Before anything else it refuses (exit 2) unless GitHub Actions
  reports a GitHub-hosted runner (not `act`), the machine looks like one
  (user `runner`, `HOME=/home/runner`, system vendor `Microsoft
  Corporation`) and the workflow sets `RANIKUST_T2_ON_RUNNER=1`, so it never
  runs on a developer's machine; `e2e`, `ultra` and `push` it refuses by
  name. Exit 1 is a failed suite (a refusal inside `lane.sh` counts as
  `unit-c` failing), 3 a failed set-up (staging, a build) with no results.
- It stages the same tree as the VM at `~/rnk` (`tests/vm/stage.sh`, which
  `vm.sh` uses too) and runs the same guest scripts as the runner's user,
  each privileged command under `sudo`. No network check: `guest/net.sh`
  keeps the VM's root off the developer host's loopback (ADR-0086), and the
  runner has nothing to keep it from; so a `unit`-only tree lacks the
  `ranikust` binary the VM stages for that check.
- For `unit-c` it makes a marker naming this runner, for the job
  (root-owned, the form `lane.sh` checks): the one exception to "never
  create the marker anywhere else" ([above](#the-container-lane-tier-t2c)).
  `lane.sh` then runs on the runner's rootful Docker: ADR-0063's profiles and
  control checks as on the VPS.
- The runner gets what the VM's base image adds (`busybox-static`, `zstd`,
  `pigz`, `uidmap`) and, for the smoke job, Docker with containerd's image
  store, as in the VM, whose `docker load` reads the OCI layout
  `smoke-build` writes.
  Images the guest scripts name come from `mirror.gcr.io`
  (`RANIKUST_VM_IMAGE_REGISTRY`), as in the VM; Docker's own pulls of
  `docker.io` names (the T2c image's `ubuntu@sha256:...` base) go to Docker
  Hub directly, since the runner's Docker has no registry mirror.
- `env.txt`, `summary.txt` and `suite-times.txt` go to `~/rnk/out` and the
  job's summary page; on failure, `~/rnk/out` (and `target/container/results`
  for T2c) is kept as an artifact for 7 days. Each job times out after 30
  minutes.
- The Rust caches of these jobs follow the rules of
  [CI lanes](#ci-lanes) and share the repository's 10 GB cache limit with
  `ci.yml`; `unit` and `unit-c` build the same test binaries and share one
  entry (`t2-tests`).
- The runner is not the VM (another kernel and Docker, AppArmor's
  `apparmor_restrict_unprivileged_userns=1`, an image GitHub updates
  weekly). The local VM stays the reference: `e2e`, `ultra`, `push`, kernel
  5.15 (`RANIKUST_VM_RELEASE=jammy`) and acceptance runs are local only.
- No secrets, a read-only token, never `pull_request_target`: a fork's pull
  request runs its code as root on GitHub's machine, which is what the
  runner is for. Never a self-hosted runner while the repository is public.
  Whether a fork's pull request runs without approval is the maintainer's
  repository setting (recommended: require approval for all outside
  collaborators).
- No `just` recipe runs this workflow: it is CI's alone, a deliberate
  exception to "`justfile` and `ci.yml` run the same commands".

## aarch64 under qemu-user

`just test-aarch64` (CI job `test-aarch64`, on a full run, `ADR-0009`)
builds with cargo-zigbuild. On an aarch64 host, which is where CI runs it
(GitHub's free `ubuntu-24.04-arm` runner for public repositories),
everything runs natively and no test is skipped; it is also CI's only
aarch64 build (`just check-aarch64`, a quicker type check, is local only,
ADR-0095). On an x86_64 host it runs everything under `qemu-aarch64`:

1. The static release binary: no loader and no `NEEDED` entries, then
   `ranikust version`; static-pie with a symbol table, like x86_64, when
   linked by `aarch64-linux-gnu-gcc` (natively by `cc`); with zig's linker
   (no cross gcc installed) it is not, and the recipe warns (Q-140).
2. The T0 tests of the workspace, through cargo's target runner
   (`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER`). `openat2` with
   `RESOLVE_IN_ROOT`, `statx` and the sandbox's unprivileged tests work under
   emulation.

QEMU 9.1 or later is needed, and the recipe refuses an older one: QEMU 8.2
(Ubuntu 24.04's `qemu-user`) has no `openat2` and fails most `rnk-rootfs`
tests with `ENOSYS`. `QEMU_AARCH64` names the emulator, else
`qemu-aarch64-static` or `qemu-aarch64` on `PATH`
([setup](development-setup.md#2-aarch64-when-you-touch-anything-platform-specific)
shows how to get one). The recipe prints the binfmt entry's interpreter.
Exceptions, all from emulation (the native CI job runs them all):

- `root::tests::open_gives_cloexec_path_fd_and_mount_id`,
  `seal::tests::the_binarys_mount_is_measured_on_the_file`,
  `seal::tests::a_file_in_the_root_is_weighed_by_the_roots_file_system` and
  `seal::tests::a_mount_root_is_reached_at_its_mount_point` are skipped:
  qemu-user's `statx` leaves `stx_mnt_id` at 0 (QEMU 10.0 and 10.2), so
  other mount-id comparisons pass only because both sides are 0 (ranikust's
  measurement of its binary's mount reads a 0 as `NoMountId`, which kaniko
  mode on `/` refuses; a fallback through `/proc/self/fdinfo` is
  [#95](https://github.com/tran4774/ranikust/issues/95)). The mount-ID checks
  that guard deletion (T4 of the threat model) are therefore not exercised
  under emulation; the native CI job covers them.
- `without_root_the_set_up_refuses_before_touching_the_state_dir`,
  `without_root_the_set_up_never_makes_the_default_state_dir`,
  `kaniko_variables_are_not_read`, `logging_flags_change_what_is_written`,
  `the_store_opens_inside_the_user_namespace_or_not_at_all`,
  `red_team_the_build_user_namespace_is_whole_or_not_entered`,
  `prune_all_empties_in_the_user_namespace_or_changes_nothing` and
  `prune_all_as_a_user_empties_a_state_directory_or_changes_nothing` are
  skipped: qemu-user runs a thread of its own, so the set-up, or
  `enter_build_user_namespace`, stops at its single-thread check before the
  privilege refusal or the user namespace the tests expect.
- `seccomp::tests::the_filter_refuses_what_it_should_and_nothing_else` and
  the tests of every `tests::kernel` module (rnk-sandbox's seccomp profiles,
  capability drops and keyring join under a filter) are skipped: their
  filters are written for aarch64 syscall numbers, and qemu-user cannot
  install them in the x86_64 host kernel. The same profiles still run on the
  tests' interpreter there.
- The tests that spawn an aarch64 binary themselves (ranikust's `cli` and
  `metrics`, rnk-store's `processes`, rnk-sandbox's
  `seal::tests::a_copy_an_interpreter_must_reopen_is_refused_before_the_exec`
  and
  `userns::tests::a_closures_child_writes_through_no_hook_or_subscriber_of_its_parent`,
  which runs in a copy of its test binary):
  without a binfmt_misc entry for aarch64 the kernel cannot execute it, so
  the recipe skips them with a warning.

Real aarch64 hardware is Phase 6.

<a name="smoke-test-until-the-builder-exists-tier-t1"></a>

## The rootless lane (tier T1)

`just test-rootless` (`tests/rootless/lane.sh`) runs the `#[ignore = "T1:
<profile>: ..."]` tests as root of rootless Podman containers on the
developer's machine: root there is the user's own uid, with the user's
subordinate ids, so nothing runs as real root. It refuses to run as root,
with Podman that is not rootless, or on a machine other than x86_64 (the
binaries it builds), then:

1. lists the tests of each profile from the markers (every T1 test must name
   one, and no name may be in both lists: tests are selected by name) and
   builds the test binaries of their crates as static musl binaries
   (`cargo-zigbuild test --no-run`) into `stage/` of a directory the run
   makes for itself, `target/rootless.XXXXXX/`; the stage is removed when
   the lane ends;
2. runs one container per profile from Ubuntu 24.04 by digest, `--rm
   --network none`, the stage mounted read-only at `/t1` and a tmpfs at
   `/work` (`TMPDIR`), the only place the tests write:
   - `default`: as Podman starts a container; the control checks require no
     `CAP_SYS_ADMIN`, a seccomp filter, and no mount namespace without a user
     namespace;
   - `privileged`: `--privileged`, as `just smoke`; `tests/rootless/run-profile.sh`
     prepares `RNK_T1_ROOT` (a bind mount and a tmpfs inside it, and the
     `.rnk-t1-root` marker) for the mount tests of `rnk-rootfs`;
3. checks in each container, before the tests, that root there is another
   uid outside, that a range of ids is mapped (the user needs entries in
   `/etc/subuid` and `/etc/subgid`) and that `chown` to another id works;
   then runs each listed test once and fails unless every one ran and
   passed. The lane prints the `control:` lines of those checks, each
   test's result line, the `measured:` lines a test writes to stderr past
   libtest's capture, error lines and the count `T1 <profile> tests: N
   passed of M listed`; for a profile that failed, the last 40 lines of its
   log too. Logs go to `<profile>.log` in the run's directory, which is
   removed when every profile passed or the lane stopped before any log,
   and kept, its path printed, when a profile failed.

Among the lane's tests, some check that a `RUN` opens nothing of a process
outside the build, each file refused with the error expected: under an rbind
of `/proc` (`default`, `Permission denied`), a process in the user namespace
ranikust started in
(`a_run_under_an_rbind_of_proc_opens_nothing_of_a_process_outside_the_build`)
and one in a user namespace of its own
(`a_run_under_an_rbind_of_proc_opens_nothing_of_a_process_in_another_user_namespace`),
and with a new `proc` (`privileged`, `No such file`,
`a_run_with_a_new_proc_opens_nothing_of_a_process_outside_the_build`).

CI does not run this lane, and the VM's `unit` suite runs only T2 and T2c
tests: T1 tests run only here.

## Smoke tests (tier T1)

`just smoke` builds the `smoke-build` example of `crates/ranikust`
statically and runs it inside rootless `podman run --privileged`, where root
is the user's own uid through the user namespace, so nothing it does reaches
the host as root. It strings the finished crates together:

1. pull `alpine:3.20` (`rnk-registry`), extract each layer and check its diff
   ID (`rnk-layer`, `rnk-rootfs`);
2. `enter_build_namespace`, the probe and `RUN`s through `rnk-sandbox`: a
   file write, `apk add jq` (network and DNS through the sandbox's `/etc`
   copies), and the `smoke_checks` of `tests/smoke/lib.bash`, one `RUN` each,
   run inside the sandbox (PID 2 under `ranikust-init`, which holds no
   capability and runs under the command's seccomp filters, host name,
   Docker's bounding set `a80425fb`, a `nodev` root, read-only `/dev/null`
   and `/proc/sys`, masked `/proc/kcore`, stdin not a terminal, no stray
   descriptors);
3. pack the root into one gzip layer and write an OCI layout (`rnk-layer`,
   `rnk-oci`).

The recipe then checks that the sandbox's stubs did not reach the layer
(`/etc/resolv.conf` not at all, `/etc/hosts` and `/etc/hostname` not with the
host name `ranikust`), loads the layout into podman as an OCI archive
(`podman load`; not `skopeo`, which calls `unshare` itself and is refused
where AppArmor restricts unprivileged user namespaces, as on Ubuntu 24.04)
and runs it. `smoke-build` is not a builder: one squashed layer, no diff per
instruction, no xattrs. It runs again with `--userns` in a default `podman
run` (no `--privileged`): the user-namespace probe, the build's user
namespace, then the sandbox's probe, which must refuse at the new `proc`
without `--allow proc.rbind`
([ADR-0126](adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md))
and with it choose an rbind of `/proc` and of `/sys` there (#78); the
`smoke_checks` run but for the first, which needs the `RUN`'s own `/proc`
(`smoke_checks_own_proc`), and the host name, which Podman's seccomp
profile refuses to set without `CAP_SYS_ADMIN`; the other checks of the
init find it as the shell's parent. podman then runs that image too. The
recipe then builds `tests/smoke/e2e` with the `ranikust` binary in the
same kind of container (two stages, `COPY --from`, wildcards, `.dockerignore`, cache and secret mounts,
`RUN --network=none`, heredocs, `WORKDIR`, `USER`, `ENV`, `LABEL`,
`EXPOSE`, `CMD`), loads its Docker tar into podman and runs it, checks
that no stub or secret reached a blob, and that `--reproducible` gives one
digest with `--jobs 1` and the default.

It does not replace the VM: the sandbox runs in a user namespace there, so
real-root behaviour (T17, T18 of the threat model, `MS_MOVE` under Docker's
seccomp) is checked by the `smoke` suite of `just test-vm` instead.

## Quick smoke cases

Small Dockerfiles in `tests/smoke/cases/<name>/`, one concern each, built by
the `ranikust` binary in a few seconds from `alpine:3.20` or `scratch`. They
are the default for a smoke test or a proof of concept; the nextjs-kaniko
workloads (`e2e`) are for large benchmarks only. A case's directory
(`case.sh` and its Dockerfiles) has the details; the checks that need the VM
are in `tests/vm/guest/quick.sh`.

| Case | What it checks |
|---|---|
| `minimal` | One `RUN` and a `CMD`; the image runs |
| `scratch` | `FROM scratch` with a root built in another stage, an exec-form `ENTRYPOINT`, the default `PATH` added to a config without one (ADR-0032) |
| `metadata` | The image config as BuildKit writes it: `ARG` before `FROM`, `ARG`/`ENV` expansion, `WORKDIR`, `USER name:group`, `LABEL`, `EXPOSE`, `STOPSIGNAL`, `HEALTHCHECK`, `SHELL`, `ENTRYPOINT` with `CMD`; with `--target shellform`, a shell-form `ENTRYPOINT` that clears the `CMD` inherited from its base stage |
| `copy` | `COPY` wildcards, one matching nothing (not an error), `.dockerignore` with an exception, `--chown`, `--chmod` octal and symbolic, a symlinked source (followed), a directory into a new destination, a heredoc; checked inside the image |
| `multistage` | Two independent stages at `--jobs 2`, `FROM <stage>`, `COPY --from` all of them; in kaniko mode on a container's `/` the log says the stages run one at a time and `--jobs` still bounds the uploads |
| `kaniko-flags` | The kaniko-mode flags' meaning (Phase 2 criterion 8): `--snapshot-mode=time` misses a rewrite of the same size and mtime that `full` catches; `--ignore-path` keeps a `RUN`'s files below it out of every layer; in the T1 kaniko leg, a build without `--mode` picks kaniko mode in a default container and says why, rootless overlay mode not having run: its user-namespace probe stops at `proc`, whose rbind works but needs `--allow proc.rbind`, and with that flag `--ignore-path` keeps kaniko mode, which honours it, with a note (#74, #79; other legs print the probe's line, in either form of it; each build logs exactly one). In overlay mode `--ignore-path` is refused (exit 2) and `--snapshot-mode` changes nothing. With the VM's kaniko quick suite (`RANIKUST_QUICK_MODE=kaniko`), `tests/vm/guest/quick.sh` also runs ADR-0054's refusals, ADR-0101's (`binary_mounts_on_slash`: the binary bound read-write, file or directory, is refused with the `:ro` guidance, and so are the binary bound `:ro` from a tree bound read-write too and a file with another hard link; bound `:ro`, its tree `:ro` too, or copied into the container's root, it builds; a fuse-overlayfs root is T0 only, `a_fuse_overlayfs_root_copies_up_as_overlayfs_does`, and a root that is not an overlay T0, `a_binary_in_a_root_that_is_not_an_overlay_is_weighed`, and T1, `binary-mounts`, since the VM's Docker gives overlayfs roots only), the red team of `--mode=auto`'s routing (`rt_*`), `--mode=auto` with the overlay probe refused after the user-namespace probe ran (`auto_overlay_refused_after_probe`, `RANIKUST_DEBUG_FAULT=overlay-refused`: kaniko mode with a directory per stage, one probe line; one line too on the overlayfs route, at most one on ramfs) and Phase 2 criterion 5's lock (`slash_lock_refuses_a_second_build`: a second kaniko-mode build on `/` in one container is refused before its state directory exists; the first runs from a sealed memfd copy of the binary, ADR-0101) |
| `copy-from` | Red team of what kaniko mode keeps (ADR-0051): a hostile stage (links out of its root, to the state directory, `/ranikust`, `/` and `/proc`; a FIFO, a setuid file, a hardlink, a `0700` directory of uid 1000, a device node) read with `COPY --from`, with its metadata as it was in the image; builds whose sources reach the state directory, `/ranikust` or `/proc` fail without an image |
| `copy-from-root` | `COPY --from=<stage> / /all/`: the whole stage, without the state directory, `/ranikust` or `/proc`; in kaniko mode on the container's `/` the whole root is kept |
| `copy-from-daemon` | A `setsid` process left by a `RUN` (kaniko#1697) changes the root while kaniko mode with `RUN` `Direct` walks, keeps, empties and copies it back: the build fails closed, or its image holds nothing of the state directory, `/ranikust` or the secret |
| `run` | `RUN` in shell and exec form, a heredoc and a `#!` heredoc, a cache mount a second build sees, a secret as a file (`0400`, by its sha256) and as `env=`, `--network=none` seeing only `lo`, a `RUN` that changes nothing adding no layer, only history (ADR-0058). With `RANIKUST_CASE_MODE=kaniko` (`RUN` `Direct` on the container's `/`, ADR-0057) `direct.Dockerfile` runs instead, and the red team of `direct-attack.Dockerfile`: a `#!` heredoc cannot rewrite its memfd through `/proc`, descriptors stay limited to the `RUN`'s pipes, `/dev/null` and its script, background writers are gone before the next `RUN`, and these spellings are refused, never run without (`tests/smoke/cases/run/case.sh`): a secret file through `dst=` or `Destination=`, an empty `env=`, `type=bind` after `type=secret`, `--network` as `$N` or `NONE`, `--network=none` on a heredoc |
| `hygiene` | Issue-mining fixes (ADR-0052, ADR-0058): a `RUN` sees the base's `/var/run -> /run` link and the image keeps it (D4); `COPY --chmod=0640` applies to the copied directories too (D3); `RUN true` twice and an existing `WORKDIR` twice add no layer, only `empty_layer` history. Kaniko mode: two builds in one container (`rnk_run_twice`), the second finds the marker and starts from an emptied `/` (D1) |
| `hygiene-mounts` | Red team (ADR-0052 D1, D4; ADR-0058). Every mode: steps that only delete a file, replace a directory by an empty one, or change a mode or an mtime keep their layer, `RUN true` gets `empty_layer` only. Kaniko mode (`RANIKUST_CASE_MODE=kaniko`): two builds in one container holding volumes at `/run/vol` and `/srv/vol`; the second finds the marker and empties `/`, both volumes keep exactly their files, and no layer of either image holds them, `/run/.containerenv`, `/.dockerenv` or `/ranikust` |
| `reproducible` | `--reproducible`: one digest across builds a second apart and at `--jobs 1` and `2`, `created` at the epoch, another digest without the flag |
| `stage-dirs` | Red team of per-stage directories (ADR-0059). Every mode: sibling stages at `--jobs 4` reach neither each other nor a mount, a sysctl or a raw disk; a `setsid` writer a `RUN` leaves is gone before a stage built `FROM` its stage starts; a child of a hostile stage (links out, a FIFO, device nodes, a setuid file with a hardlink, a 15 TiB sparse file) gets the links as links, the hardlink, the file sparse; `--reproducible` gives one digest at `--jobs 1` and `4` with stages built `FROM` stages, copied or handed over to the only stage that reads them (ADR-0061, ADR-0062). The VM's kaniko mode off `/` (`RANIKUST_QUICK_MODE=kaniko`): no FIFO or device node in the child, no build directory outlives a build, builds that fill a small tmpfs state directory fail and leave nothing behind, a late `COPY --from` reader of a parent still finds it, and SIGINT during a hand-over stops the build with nothing changed outside its `builds/` and `tmp/` entries |
| `par-stages` | Four independent stages read by a final stage: with `--reproducible` one digest at `--jobs 1` and `4` and the same as `Dockerfile.chain` (the same stages forced one after the other). With `RANIKUST_QUICK_BENCH=1` (VM only), the bench for Phase 2 acceptance criterion 7: chained against independent stages at `--jobs 1` and the default, in kaniko mode off `/` and in overlay mode, each build in a systemd scope capped like the e2e builders (4 CPUs, 8 GiB) or uncapped, with wall time, the scope's `memory.peak` and the metrics of round 1 (`bench.md`, `bench.tsv`); `RANIKUST_QUICK_BENCH_ROUNDS` rounds after a warm-up (default 3) |
| `run-cgroup` | What a `RUN` reads of its cgroups goes into its layer ([ADR-0110](adr/0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md)): two stages that record `/proc/self/cgroup` and `/proc/1/cgroup` give one digest with `--reproducible` at `--jobs 1` and 4, built twice. Where the `RUN`s got cgroups of their own (the T1 leg: nested), they read `0::/` and their init's beside it, never ranikust's pid or a `RUN`'s number; where no `RUN` gets cgroups (the VM's ssh session), only the builds and their digests are checked, and the case says so. A root `RUN` holds no `CAP_SYS_ADMIN` in any set, cgroups or not |
| `run-cgroup-escape` | Red team of T24 ([ADR-0110](adr/0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md)): where a `RUN` gets cgroups (the T1 leg: nested), ranikust's and the init's cgroup namespaces are out of its reach. It cannot `setns` into the init's cgroup namespace (the open of `/proc/1/ns/cgroup` refused with `EACCES`, or the `setns` with `EPERM`) and no `/proc/<pid>/cgroup` it can read names ranikust's pid, built with the default profile and with `--seccomp=unconfined`. Under the default profile it cannot make a user namespace (`EPERM`). Unconfined, which measures what a `RUN` that may make one can do, the `setns` is refused from a user namespace it makes too, and a cgroupfs it mounts there is rooted at the cgroup it is in, with no cgroup below it; these checks need a host where a `RUN` may make one, and elsewhere the case says they did not run. Where no `RUN` gets cgroups (the VM's ssh session), only the build is checked, and the case says so. Its build installs `python3` from Alpine's package repository |
| `seccomp` | `--seccomp` ([ADR-0109](adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)): a `RUN` in the full sandbox runs under one seccomp filter more without the flag and with `--seccomp=default` than with `--seccomp=unconfined`, and is refused the `personality(ADDR_NO_RANDOMIZE)` of busybox's `linux64 -R`, which `unconfined` allows; a profile file that refuses `setpriority` (busybox's `nice`) takes the default's place; one whose default action is `SCMP_ACT_KILL_PROCESS` or `SCMP_ACT_TRAP` fails the build (exit 1) as a `RUN` whose init was killed by `SIGSYS`, with the profile as the likely cause, never as an exit code of the command. In the T1 kaniko leg (`RUN` `Direct` on the container's `/`) `--seccomp=default` and `unconfined` are refused (exit 2) and a `RUN` runs under as many filters as a plain container. With the VM's kaniko quick suite, `tests/vm/guest/quick.sh` also runs `seccomp_on_slash`: the same refusals, a file included, and the same count in a default Docker container |
| `seccomp-scope` | Red team of `--seccomp`'s reach (ADR-0103, ADR-0109): every process of a `RUN`'s PID namespace, a background and a `setsid` process included, runs under the same filters without `no_new_privs`; a setuid-root program run by another user gets Docker's capabilities (`a80425fb`) and the same filters; one `--reproducible` digest without the flag, with `unconfined` and with an allow-all profile file. Nothing in the T1 kaniko leg (`RUN` `Direct` on the container's `/`) |
| `self-exe` | Pins a barrier of the `#!/proc/self/exe` residual (threat model section 6, [ADR-0101](adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md)): `RUN ["/x"]`, a script whose `#!` line names `/proc/self/exe`, makes the kernel run ranikust's own binary, its sealed copy, in the sandbox; its command line refuses the script path (`unrecognized subcommand '/x'`) and the build fails |
| `sealed-exe` | Red team of ADR-0101 (T21): a `RUN` (`reach.sh`) reaches no thread of ranikust's or its init's: their `exe` is not read, opened for writing or run, their descriptors, `map_files`, `root`, `environ` and `mem` do not open (a process of the `RUN`'s own is the control), and the `RUN` holds no descriptor of ranikust's and no `RANIKUST_*` variable |
| `binary-mounts` | Red team of ADR-0101's refusal of a writable mount reaching the binary on `/` (T21), in containers the case starts itself: hostile names (tab, newline, a literal `\040`, a mount point with a newline) reach or do not as their unescaped paths do; a grandparent or the same file bound read-write elsewhere is refused, a sibling or a directory below the binary's builds; the binary bound read-write is refused with the memfd or every copy skipped and with a forged marker; with names changed on the host once the binds exist, a writable bind of a link of the binary whose name, or the binary's own bound name, was removed is refused, and another file bound read-write and replaced builds; under podman, a directory root (`--rootfs`) builds with one link and is refused with a hard link outside it or its parent bound read-write. Nothing is made in a refused build's state directory |
| `binary-mounts-made` | Red team of the same refusal (ADR-0101, T21) on mount tables made inside a default container by a privileged `podman exec` in its mount namespace, ranikust keeping the container's capabilities (podman only: a privileged `docker exec` keeps the profiles that deny `mount`): the binary's tree bound read-write then covered by a later mount, and the same file bound read-write under the binary's read-only bind or over its name (the binary run from a descriptor opened before), are refused as reached; another writable mount of its file system covered later, another file bound over its name, and its directory covered are refused as unplaced; a mount on a writable mount and a chain of 1000 writable binds build, and the binary's tree bound read-write while that chain's walk runs is refused by the second read of the table. In a directory root where only a `/.dockerenv` made and removed in a loop tells a container, runs go on until one finds no container and its copy finds one, which is refused as not weighed. Nothing is made in a refused build's state directory |
| `pid-peers` | ADR-0101's refusal of a PID namespace shared with a process of another mount namespace that ranikust reaches as it starts (T21), in containers the case starts itself, under podman or Docker: a default container with `--pid container:<peer>` and the binary bound `:ro`, from a directory no peer binds, is refused with the PID-namespace guidance before its state directory exists and before any `RUN` when the peer is root with a writable bind of its own (the red team's configuration, with a `RUN` that only looks); with a peer run as uid 1000, or with no peer, it builds; and in a container given `CAP_SYS_ADMIN` whose seccomp profile refuses `mount`, where the set-up fails after its `unshare` (`half_entered`), it builds with no peer and is refused with a root peer, ranikust started from `sh` or exec'd. In every build with `RUN`s that goes ahead (with a root peer that has a capability ranikust lacks and a writable bind, or `half_entered` with no peer), one `RUN` goes through every process and, with the root peer, sees its `sleep` as PID 1 and does not reach it; another holds only its standard streams. No `RUN` runs in a refused build. With no peer, a `sleep` of the container itself unshared into a mount namespace of its own just before ranikust starts is refused |
| `userns-probe` | Red team of the user-namespace probe of `--mode=auto` (#74; T4, T7, T8, T9), the build without `--mode` in every leg: one probe line per build, in either form of the line, and a `RUN` (`seen.sh`) sees nothing of the probe: no process in another user namespace, no mount named for it in its own or PID 1's table, no descriptor of its directory, no `RANIKUST_*` variable and no probe directory in the state directory. T1 only: with `RANIKUST_DEBUG_FAULT=overlay-refused`, a `--privileged` container is refused for overlay mode (exit 1, ADR-0054, one probe line, no image) and a default one builds in kaniko mode as without it; a state directory on tmpfs lands in the mode the volume does; ranikust started from `sh` and sent `SIGKILL`, `SIGINT` or `SIGINT` to its group as soon as a probe child shows (the case fails if none is caught in eight tries) leaves no process in another user namespace and no probe mount; once a later build has started, no probe directory is left in the state directory |
| `rootless` | Rootless overlay mode end to end (#79, [ADR-0107](adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md), [ADR-0126](adr/0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)), T1, Podman only, both legs, each mode with a state directory of its own. Without `--allow proc.rbind`, `--mode=overlay` is refused (exit 1) and the refusal names the flag; with it, `--pid=host` is refused (exit 1, no image) as the host's PID namespace, a kernel thread in sight ([ADR-0134](adr/0134-with-allow-proc-rbind-the-pid-namespace-check-reads-files-no-ptrace-check-guards.md)), and otherwise the build runs in rootless overlay mode with a read-only rbind of `/proc`. Its `--reproducible` digest equals overlay mode as root's (`--privileged`), also at `--jobs 1`. In the image: ownership by a mapped user, a setgid directory and a symlink; a `setcap` file capability is exported. ranikust sent `SIGKILL`, `SIGINT`, `SIGTERM` or `SIGINT` to its group during a `RUN` leaves no process of another user namespace and exits 137 for `SIGKILL`, else 1 (one stop cancels the build), and the next build finds no directory of the killed builds in the state directory. In the kaniko leg (root that may not mount), a set-up made to fail in the build's user namespace (`RANIKUST_DEBUG_FAULT=rootless-broken`) at its last step, once the sandbox started, without `--mode` falls back to kaniko mode on `/` with a note and kaniko mode's line alone ([ADR-0136](adr/0136-root-auto-mode-falls-back-to-kaniko-mode-from-a-set-up-that-fails-in-the-builds-user-namespace.md)), builds the image and leaves its fresh state directory empty, while with `--mode=overlay` it is refused (exit 1, no image, no kaniko mode) |
| `rootless-reach` | Red team of rootless overlay mode (#79), T1, Podman only, both legs, with `--allow proc.rbind`. A `RUN` writes no kernel, cgroup or sysfs file (`core_pattern`, `modprobe`, `sysrq-trigger`, `drop_caches`, `uevent_helper`, `cgroup.procs` and others, `lo`'s `mtu` also from `--network=none`), makes no cgroup, mounts nothing over `/proc`, sees the cgroup root alone, and reads neither the root nor the environment of any process outside its PID namespace, while it reads its own (the control; T17, T21, T24). A `RUN --network=none` lists no network device in `/sys/class/net` but its own `lo` (none under the rbind of `/sys`) and reads no address but `lo`'s; `/sys/fs/cgroup`, Podman's cgroup2 mount, lists nothing, nor does any directory a mount below `/sys` names. A tree past `PATH_MAX`, partly a mapped user's and unsearchable, is exported or refused by the exporter, and the build's directories go (T15). Processes a `RUN` leaves (`setsid`, a double fork, `nohup`) stop writing to a cache mount before the next `RUN` and are gone when ranikust exits (T8); a `RUN`'s exit 75 fails the build (exit 1) with no fallback ([ADR-0136](adr/0136-root-auto-mode-falls-back-to-kaniko-mode-from-a-set-up-that-fails-in-the-builds-user-namespace.md)). `--pid container:` a root peer, or a regular user's (PID 1 there), and ranikust started by `podman exec` where a root peer joined are refused (exit 1, no image; ADR-0129, [ADR-0134](adr/0134-with-allow-proc-rbind-the-pid-namespace-check-reads-files-no-ptrace-check-guards.md)); under `--init` it builds. ranikust sent `SIGKILL`, `SIGTERM` or `SIGINT` at points of its set-up leaves nothing that stops the next build, which succeeds and leaves no build, `tmp` or probe directory. In the kaniko leg, the fallback's `RUN` runs in the container's user namespace and the rootless build's in another; ranikust sent `SIGKILL`, `SIGTERM` or `SIGINT` at points of a set-up made to fail (`RANIKUST_DEBUG_FAULT=rootless-broken`), its store opened or abandoned or not yet, leaves no store that stops kaniko mode on `/`, even chosen with no fallback (`--ignore-path`), which builds a stage copied from with `COPY --from` (only a build not taken for one cut short does) and leaves no marker (a kill after kaniko mode's own store opened is ADR-0066's case and is reported apart); a probe directory a killed kaniko-mode `RUN` planted in the state directory, with links out, is removed by the next rootless build, nothing they name touched (T4, T21) |
| `userns` | Root's `--userns` (#83, [ADR-0104](adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)), T2 only (the VM's `sudo docker` engine), with a `ranikust` range it adds to `/etc/subuid` and `/etc/subgid` and restores after. Refused before anything is written without the range, below a directory the range's first uid cannot search, and with a state directory of root's, left as it was; a `RUN`'s `uid_map`, `gid_map` and ids read from the host are the range's, and a file it makes is owned there by its first ids; the `--reproducible` digest equals the same build's without `--userns`, and the image keeps its owners and its setgid and setuid bits. Prints ADR-0074's reach (`measured (ADR-0074)`) of a host account with a non-root `RUN`'s ids, which must be nothing, and of one with the ids they map to on the host. In the kaniko leg, `--userns --mode=kaniko` is a usage error |
| `deep-tree` | A stage leaves a tree 3000 levels deep, its path past `PATH_MAX`: the build succeeds, the next stage's root holds nothing of it, and nothing new is left in the state directory's `builds/` and `tmp/`. At T1 ranikust runs under 1024 descriptors |
| `special-files` | Red team of `COPY` and special files (ADR-0060). The context, made by `case.sh` (git keeps none), holds FIFOs and Unix sockets at the top, nested and behind a symlinked directory; a stage leaves FIFOs and, where root may make one, a device node. Kaniko mode on a container's `/` (`RANIKUST_CASE_MODE=kaniko`, `RUN` `Direct`): none reaches the image, from the context or `COPY --from`, and the log warns once per copy; with the full sandbox the FIFOs are kept, as BuildKit. A node a `RUN` makes never opens in a later `RUN` |

Every build is also checked for a stub or the secret in any blob and the
secret in the build log. `tests/smoke/cases.bash` runs them; a case is a
`Dockerfile`, its context and a `case.sh` with a `case_run` function using
its helpers (`build`, `run`, `expect`, `expect_config`, `config`, `out`).

- `just smoke-case [name...]` (T1): the binary in rootless `podman run
  --privileged`, podman loading and running the images; no name runs every
  case. The shared state directory under `target/smoke/cases/.state`, mounted
  at `/var/lib/ranikust`, keeps the store between runs. The binary mounted is
  a copy in `target/smoke/cases/.bin`: cargo's is a hard link of its `deps/`
  copy, which kaniko mode on `/` refuses (ADR-0101).
  `RANIKUST_SMOKE_REGISTRY=mirror.gcr.io` pulls the base image through
  Google's Docker Hub mirror, as the VM suite does, when Docker Hub's
  anonymous pull limit is reached.
- `just test-vm quick` (T2): the same as real root on the VM, Docker loading
  and running the images; `RANIKUST_QUICK_CASES="run copy"` picks cases.
  `RANIKUST_QUICK_MODE=kaniko` builds them in kaniko mode in a root of the
  state directory, the VM not being a container.
  `RANIKUST_QUICK_BENCH=1` adds the `par-stages` bench (light: a synthetic
  workload, about 10 minutes), the way to look at stage parallelism without
  the ultra benchmark.
- `RANIKUST_CASE_MODE=kaniko` builds them in kaniko mode on the container's
  `/` for `just smoke-case`, in a default rootless podman container where
  root may not mount, so `RUN` runs `Direct`; the binary, context and
  outputs sit under `/ranikust`.
