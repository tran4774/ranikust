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
just test-vm quick        # the quick smoke cases (tests/smoke/cases) as real root
RANIKUST_QUICK_CASES="run copy" just test-vm quick   # some of them
RANIKUST_VM_RELEASE=jammy just test-vm   # kernel 5.15
just test-vm e2e          # large benchmark only: nextjs-kaniko W1-W3
just registry-up && just test-vm push   # push to loopback, HTTP and HTTPS registries
just test-vm secrets      # attacker RUNs vs kaniko mode on / in default Docker (ADR-0051)
```

- **Smoke test or proof of concept as real root: `quick`**, the small
  Dockerfiles of `tests/smoke/cases` (a few seconds each; a new concern is
  a new case there, see the `test` skill). Not `e2e`.
- **`e2e` only for a large benchmark or an acceptance measurement**: a
  12 GiB VM and 45–60 minutes for 3 rounds of all three builders. Prefer
  `RANIKUST_E2E_BUILDERS=ranikust RANIKUST_E2E_ROUNDS=1` unless official
  numbers are needed.

- `secrets` runs only when named: a few minutes; a change to kaniko mode
  on `/`, the store's anonymous mode or `Isolation::Direct` reruns it.
- `push` runs only when named and needs the host's test registries
  (`just registry-up`, docs/testing.md#test-registries), which the VM
  reaches as `10.0.2.2`.
- `e2e` runs only when named: 15–25 minutes per round and 2.8 GB copied in,
  read from the `nextjs-kaniko` checkout next to the main checkout and
  from the host's podman images (docs/testing.md#end-to-end-workloads-e2e).
  Its `report.md` holds the times, `compare-*.txt` the flattened-image
  comparison (`result: other=0` is what Phase 1 acceptance needs), and a
  `ranikust` build that hits an unbuilt part is `pending`, not a failure.

- Docker Hub images are pulled through `mirror.gcr.io` by default
  (`RANIKUST_VM_IMAGE_REGISTRY`; `docker.io` for Docker Hub itself, which
  rate-limits anonymous pulls: a `Toomanyrequests` failure means that).
- One VM at a time, one image cache: images are cached in
  `tests/e2e/_vm-image/` of the main checkout, shared by every worktree,
  and a second run waits on its lock (the `test` skill has the rules and
  the clean-up policy).
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
- **A Dockerfile feature to check end to end:** a case in
  `tests/smoke/cases/<name>/` (`Dockerfile`, a tiny context, `case.sh`);
  it runs at T1 (`just smoke-case <name>`) and T2 (`quick`) alike.
- **A check inside the sandbox:** shared with `just smoke` in
  `tests/smoke/lib.bash`; only-as-real-root checks go in
  `tests/vm/guest/smoke.sh`.
- **A new suite:** `tests/vm/guest/<suite>.sh`, then add its name to the
  list in `tests/vm/vm.sh` and to the suite table in `docs/testing.md`.
- A check that something cannot happen needs a control that shows the
  check would see it (as the plain-`chroot` escape control in `smoke.sh`).
