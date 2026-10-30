---
name: vm-test
description: Run ranikust's tier-T2 tests (real root, pivot_root, mknod, rootful Docker) in the disposable QEMU/KVM VM with `just test-vm`, read the results, and add a T2 test or check. Use whenever a change touches rnk-sandbox, rnk-snapshot, rnk-rootfs or anything that needs root, or when asked to run T2 or VM tests.
---

# T2 tests in the VM

Real root never runs on the host. `just test-vm` boots a fresh VM as the
user, runs the suites as root inside it and deletes it again; the full
description is in `docs/testing.md#the-vm-harness-tier-t2`.

## Run

```sh
just test-vm              # unit and smoke, Ubuntu 24.04 (kernel 6.8)
just test-vm unit         # only the #[ignore = "T2: ..."] tests
RANIKUST_VM_RELEASE=jammy just test-vm   # kernel 5.15
just test-vm e2e          # nextjs-kaniko W1-W3: kaniko, BuildKit, ranikust
```

- `e2e` runs only when named: about 15–25 minutes and 2.8 GB copied in,
  read from the `nextjs-kaniko` checkout next to the main checkout and
  from the host's podman images (docs/testing.md#end-to-end-workloads-e2e).
  Its `report.md` holds the times, `compare-*.txt` the flattened-image
  comparison (`result: other=0` is what Phase 1 acceptance needs), and a
  `ranikust` build that hits an unbuilt part is `pending`, not a failure.

- Docker Hub images are pulled through `mirror.gcr.io` by default
  (`RANIKUST_VM_IMAGE_REGISTRY`; `docker.io` for Docker Hub itself, which
  rate-limits anonymous pulls: a `Toomanyrequests` failure means that).
- Run it in the background: a cold run (image download, release build,
  boot, cloud-init installing Docker) takes several minutes.
- If `just` or `cargo-zigbuild` is not found, put `~/.cargo/bin` on `PATH`.
- Never `sudo` anything on the host to make it pass. No `/dev/kvm` access
  means stop and tell the maintainer.

## Read the results

`target/vm/results/<release>/`: `summary.txt` (one line per suite),
`env.txt` (kernel, Docker, cgroup), `<suite>.log`, `serial.log` (boot and
cloud-init, for a VM that never answered SSH). Quote the summary and the
failing lines in your report; a run that did not happen is "not run", not
"passes".

## Add to it

- **A Rust test that needs root:** mark it `#[ignore = "T2: <why>"]` in a
  crate's tests. The `unit` suite finds it by the `fn` name under the
  attribute and fails unless every listed test runs exactly once, so keep
  T2 test names unique across the workspace.
- **A check inside the sandbox:** shared with `just smoke` in
  `tests/smoke/lib.bash`; only-as-real-root checks go in
  `tests/vm/guest/smoke.sh`.
- **A new suite:** `tests/vm/guest/<suite>.sh`, then add its name to the
  list in `tests/vm/vm.sh` and to the suite table in `docs/testing.md`.
- A check that something cannot happen needs a control that shows the
  check would see it (as the plain-`chroot` escape control in `smoke.sh`).
