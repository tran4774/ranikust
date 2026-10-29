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
| T2 | Privileged integration: overlay as real root, `pivot_root`, `mknod`, Docker rootful variants, kaniko mode on `/`, the host-safety attack suite | root | A disposable VM, never the host | TODO: `just test-vm` | Planned |
| T3 | CI on every pull request: `fmt`, `clippy`, T0, static musl build, aarch64 check, cargo-deny, rustdoc, mdBook | None | GitHub Actions (`.github/workflows/ci.yml`) | `just ci` | In use |

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

## TODO: the VM harness (tier T2)

There is no VM harness yet, and therefore no `just test-vm` recipe and no
`vm-test` skill. Until it exists, T2 tests cannot run, and pull requests that
need them say so in their test plan.

The harness should follow the pattern of
[experiment 13](research/experiments/13-docker-rootful-vm.md) §6, which ran
real rootful Docker under cgroup v2 and v1 without root on the host:

1. **Fetch** an Ubuntu cloud image and check its SHA-256 against the
   published `SHA256SUMS` before use. Cache the verified image.
2. **Prepare** a copy-on-write qcow2 overlay on top of the cached image, so
   every run starts clean, and a cloud-init NoCloud seed (`user-data`
   installing Docker and the test dependencies, an SSH key generated for this
   run).
3. **Boot** with QEMU/KVM as the invoking user (access to `/dev/kvm` through
   its group or an ACL, never `sudo`), user-mode networking, and SSH
   forwarded to `127.0.0.1` only. Host services such as a local registry are
   reachable from the guest at `10.0.2.2`.
4. **Copy in** the static musl binary (`just build-static`), the test
   fixtures and the test scripts; **run** the suite over SSH; **copy out**
   the results.
5. **Vary** the environment: Docker `run` variants (default,
   `--cap-add SYS_ADMIN`, seccomp and AppArmor unconfined, `--privileged`),
   cgroup v2 and v1 (`systemd.unified_cgroup_hierarchy=0` and a reboot), and
   several kernels (5.15 and a current one).
6. **Tear down**: stop the VM and delete the overlay, the seed and the key,
   whatever the outcome of the run.

When it exists, add `just test-vm [suite]`, a CI job on a runner with KVM if
one is available, and a `vm-test` skill under `.claude/skills/`, and update
this page, `CONTRIBUTING.md` and `docs/agents/workflow.md`.

Also TODO: `just test-rootless` for tier T1, with the same rule that tests
write only below a temporary directory.

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

The recipe then checks that the stub `/etc/resolv.conf` did not reach the
layer, loads the layout into podman as an OCI archive (`podman load`; not
`skopeo`, which calls `unshare` itself and is refused where AppArmor
restricts unprivileged user namespaces, as on Ubuntu 24.04) and runs it.
It is not a
builder: one squashed layer, no diff per instruction, no xattrs; it goes
away when `rnk-builder` lands. It does not replace the VM: the sandbox runs
in a user namespace there, so real-root behaviour (T17, T18 of the threat
model, `MS_MOVE` under Docker's seccomp) stays untested.
