---
name: vm-test
description: Run ranikust's tier-T2 tests (real root, pivot_root, mknod, rootful Docker) in the disposable QEMU/KVM VM with `just test-vm`, read the results, and add a T2 test or check. Use whenever a change touches rnk-sandbox, rnk-snapshot, rnk-rootfs or anything that needs root, or when asked to run T2 or VM tests.
---

# T2 tests in the VM

Real root never runs on the host. `just test-vm` boots a fresh VM as the
user, runs the suites as root inside it and deletes it again; the full
description is in `docs/testing.md#the-vm-harness-tier-t2`.

On a pull request, CI also runs `unit`, `smoke`, `quick`, `secrets` and the
T2c `unit-c` as root on GitHub-hosted runners (`.github/workflows/t2.yml`,
ADR-0090): read them with `gh pr checks` and `gh run view --log-failed`.
They do not replace the VM for `e2e`, `ultra`, `push`, kernel 5.15 or an
acceptance run, and `tests/vm/runner.sh` never runs outside CI.

## Run

```sh
just test-vm              # unit and smoke, Ubuntu 24.04 (kernel 6.8)
just test-vm unit         # only the #[ignore = "T2: ..."] and "T2c: ..." tests
just test-vm quick        # the quick smoke cases (tests/smoke/cases) as real root
RANIKUST_QUICK_CASES="run copy" just test-vm quick   # some of them
RANIKUST_VM_RELEASE=jammy just test-vm   # kernel 5.15
RANIKUST_VM_RELEASE=alma9 just test-vm   # AlmaLinux 9, SELinux enforcing: the el9 suite only
just test-vm e2e          # light e2e: W0 by every builder, a few minutes
just test-vm ultra        # ultra benchmark (W1-W3): only when the maintainer asks
just registry-up && just test-vm push   # push to loopback, HTTP and HTTPS registries
just test-vm secrets      # attacker RUNs vs kaniko mode on / in default Docker (ADR-0051)
```

- **Smoke test or proof of concept as real root: `quick`**, the small
  Dockerfiles of `tests/smoke/cases` (a new concern is a new case there, see
  the `test` skill). Not `e2e`.
- **`e2e`** for a change to the e2e harness or to how builders are
  compared. **`ultra`** (nextjs-kaniko's W1–W3, tens of minutes per round)
  only when the maintainer asks for performance numbers or an acceptance run
  (`RANIKUST_E2E_ROUNDS=5 sg kvm -c 'just test-vm ultra'`), never on an
  agent's own initiative (docs/testing.md#which-test-for-what).
- `secrets`, `push`, `e2e` and `ultra` run only when named. A change to
  kaniko mode on `/`, the store's anonymous mode or `Isolation::Direct`
  reruns `secrets`. `push` needs the host's test registries
  (`just registry-up`, which the VM reaches as `10.0.2.2`). `e2e` copies in
  only the reference images from the host's podman; `ultra` also reads the
  `nextjs-kaniko` checkout next to the main checkout (2.8 GB copied in;
  docs/testing.md#end-to-end-workloads-e2e). Their
  `report.md` holds the times, `compare-*.txt` the flattened-image
  comparison (`result: other=0` is what Phase 1 acceptance needs), and a
  `ranikust` build that hits an unbuilt part is `pending`, not a failure.
- Docker Hub images are pulled through `mirror.gcr.io` by default
  (`RANIKUST_VM_IMAGE_REGISTRY`; `docker.io` for Docker Hub itself, which
  rate-limits anonymous pulls: a `Toomanyrequests` failure means that).
- One VM at a time, one image cache shared by every worktree: a second run
  waits on its lock (the `test` skill has the rules and the clean-up
  policy). The VM's size, base image and `timings.txt` are in
  `docs/testing.md`.
- Run it in the background: a cold run (image download, base image, release
  build) takes several minutes. If `just` or `cargo-zigbuild` is not found,
  put `~/.cargo/bin` on `PATH`.
- Never `sudo` anything on the host to make it pass. No `/dev/kvm` access
  means stop and tell the maintainer.

## Read the results

`target/vm/results/<release>/`: `summary.txt` (one line per suite), `env.txt`
(kernel, Docker, cgroup, CPUs, memory, the VM's size and the run's own
user, key, host key and machine ID), `<suite>.log`, `suite-times.txt`,
`timings.txt` (boot, SSH, cloud-init, suites), `serial.log` (for a VM that
never answered SSH; `base-image-serial.log` when building the base image
failed). Quote the summary and the failing lines in your report; a run that
did not happen is "not run", not "passes".

## Add to it

- **A Rust test that needs root:** `#[ignore = "T2c: <profile>: <why>"]`
  when it means the same in a throwaway rootful Docker container (profile
  `default`, `sysadmin` or `netadmin`), else `#[ignore = "T2: <why>"]`;
  docs/testing.md#rules says which. The `unit` suite runs
  both, finds each by the `fn` name under the attribute and fails unless
  every listed test runs exactly once, so keep root test names unique across
  the workspace.
- **A Dockerfile feature to check end to end:** a case in
  `tests/smoke/cases/<name>/` (`Dockerfile`, a tiny context, `case.sh`); it
  runs at T1 (`just smoke-case <name>`) and T2 (`quick`) alike.
- **A check inside the sandbox:** shared with `just smoke` in
  `tests/smoke/lib.bash`; only-as-real-root checks go in
  `tests/vm/guest/smoke.sh`.
- **A new suite:** `tests/vm/guest/<suite>.sh`, then add its name to the
  list in `tests/vm/vm.sh` and to the suite table in `docs/testing.md`.
- A check that something cannot happen needs a control that shows the
  check would see it (as the plain-`chroot` escape control in `smoke.sh`).
