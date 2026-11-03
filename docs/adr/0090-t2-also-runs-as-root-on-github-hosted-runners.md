---
status: proposed
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0090: T2 also runs as root on GitHub-hosted runners, on every pull request

## Context and Problem Statement

Tests that need real root (tier T2) ran only in the local QEMU/KVM VM
(`just test-vm`), one VM at a time on the maintainer's machine, and the
T2c tests also in throwaway rootful Docker containers on the VPS
(ADR-0063). With work moving onto GitHub pull requests (the maintainer,
2026-11-03), a pull request had no privileged result of its own unless
someone ran the VM locally and reported it. The maintainer asked whether
T2 can run on GitHub's free runners, and chose to run it directly on the
runner: "Chắc theo hướng chạy T2 trực tiếp trên runner" (2026-11-03). A
GitHub-hosted runner is a fresh VM GitHub makes for one job and deletes
after it, with passwordless `sudo` [external]: the property the VM gives T2.
The question: what runs there, on what terms, and what stays local.

## Considered Options

- Keep T2 local; post the VM's result to the pull request as a commit
  status from the developer's machine.
- Run the VM harness nested on the runner: KVM is available on standard
  Linux runners since 2024-04 [external], but the harness needs QEMU 10.1
  or later with passt (ADR-0086; Ubuntu 24.04 ships 8.2) and a 16 GiB VM
  on a 16 GiB runner.
- Run the suites as root directly on the GitHub-hosted runner.
- A self-hosted runner on the maintainer's machine.

The first keeps every result behind the maintainer's machine. The second
adds a QEMU build and a second layer of virtualisation for no isolation the
runner lacks. The fourth is ruled out: on a public repository a fork's pull
request would run its code as root on that machine. The third was chosen.

## Decision Outcome

The T2 suites `unit`, `smoke`, `quick` and `secrets`, and the T2c lane
`unit-c`, run as root directly on GitHub-hosted `ubuntu-24.04` runners in
`.github/workflows/t2.yml`, on every pull request and push to `main`; the
local VM stays the reference for everything else.

- **Jobs (check names):** `T2 unit (root on the runner)`, `T2 smoke, quick,
  secrets (root on the runner)`, `T2c unit-c (rootful Docker on the
  runner)`. Making them required checks is the maintainer's (the ruleset).
- **Entry point:** `tests/vm/runner.sh`, which refuses unless GitHub
  Actions reports a GitHub-hosted runner (`GITHUB_ACTIONS=true`,
  `RUNNER_ENVIRONMENT=github-hosted`) and the workflow sets
  `RANIKUST_T2_ON_RUNNER=1`; it runs as the runner's user and uses `sudo`
  per command, as the guest scripts do in the VM. It runs the same guest
  scripts on the same tree at `~/rnk`: `tests/vm/stage.sh`, moved out of
  `tests/vm/vm.sh` unchanged, stages it for both.
- **T2c:** the job makes the lane's marker (ADR-0063) itself, root-owned
  and named after the runner, which is the only machine that ever carries
  it, and runs `tests/container/lane.sh` there; the profiles, controls and
  terms of ADR-0063 are unchanged.
- **Set-up to match the VM:** the packages the VM's base image adds
  (`busybox-static`, `zstd`, `pigz`); for the smoke job, Docker with
  containerd's image store, the default of the VM's Docker 29, whose
  `docker load` reads an OCI layout (the runner's Docker 28 still had the
  classic store, and the smoke suite's first load failed on it).
- **No network check:** `tests/vm/guest/net.sh` keeps a VM's root off the
  developer host's loopback (ADR-0086). On the runner, root is the
  machine's own root and the machine serves nothing of anyone's.
- **Local only:** `e2e` and `ultra` (benchmarks: a shared runner's times
  are noise, and they read the host's podman images and the
  `nextjs-kaniko` checkout); `push` (the host's test registries behind the
  VM's registry gate); kernel 5.15 (`RANIKUST_VM_RELEASE=jammy`); the
  acceptance runs of a phase.
- **Rule change:** privileged tests run only in a disposable VM (the local
  QEMU VM, or a GitHub-hosted runner in CI) or, as T2c, in throwaway
  containers on the test machine; never on a developer's machine.
  `tests/vm/runner.sh` is for CI only, and refuses elsewhere.
- **Security terms:** `permissions: contents: read`, checkout without
  persisted credentials, no secrets, never `pull_request_target` (which
  would run a fork's code with a write token and the secrets); the Rust
  cache is saved only from `main`, so a pull request cannot fill the cache
  `main` reads; no self-hosted runner while the repository is public.

### Consequences

- Good, because every pull request, an agent's or a contributor's, gets
  the privileged results in about five minutes of wall time, in parallel,
  free on a public repository, without the maintainer's machine.
- Good, because the runner adds a second environment to the VM's: kernel
  7.0 (Azure), Docker 28, AppArmor with
  `apparmor_restrict_unprivileged_userns=1`, 3 GiB of swap.
- Bad, because the runner is not the VM: a different kernel, Docker and
  image, which GitHub updates weekly, so a run can break from an image
  change; a green runner is not a green VM, and acceptance still needs
  the VM.
- Bad, because the runner's disposability is GitHub's word [external], not
  something this repository can check, and a hostile pull request runs as
  root there (on GitHub's machine, with GitHub's outbound network);
  GitHub's default approval of first-time contributors' workflows is the
  guard against abuse of it.
- Neutral, because the image pulls of `smoke`, `quick` and `secrets` go
  through `mirror.gcr.io`, as in the VM, so they depend on it.

## Evidence

Proof of concept on pull request #22, `ubuntu-24.04` image 20261026.354.1:
kernel `7.0.0-1017-azure`, Ubuntu 24.04.5, Docker 28.0.4, cgroup v2,
AppArmor on, 4 CPUs, 15,983 MiB of memory and 3,071 MiB of swap, a root
filesystem of 147,719 MiB with 82,479 to 86,061 MiB free (GitHub
documents 14 GB of SSD [external]). The local VM for comparison: kernel
6.8.0-147, Docker 29.1.3, 12 CPUs, 16 GiB, 20 GiB disk. Builds were cold:
a pull request's run saves no cache.

| Job | Suite | Result | Suite time | Job time | Peak disk use over the start |
|---|---|---|---|---|---|
| T2 unit | `unit` | 46 of 46 listed | 89 s | 3 min 26 s | 3 MiB |
| T2c unit-c | `unit-c` | default 17, sysadmin 11, netadmin 1: 29 of 29, every control check passed | 175 s | 3 min 26 s | 5,412 MiB |
| T2 smoke, quick, secrets | `smoke` | ok: host, privileged and sysadmin variants, the three escape controls, the four plain-chroot refusals | 48 s | 4 min 32 s | 623 MiB |
| | `quick` | 17 of 17 cases | 41 s | | 226 MiB |
| | `secrets` | ok | 30 s | | 1,708 MiB |

- The first run (41622148209) failed `smoke` at `docker load` of the OCI
  layout `smoke-build` writes, under Docker 28's classic image store; the
  second (41623116016), with containerd's image store, passed every job.
  No test behaved differently on the runner's kernel.
- `tests/vm/vm.sh` with the staging moved to `tests/vm/stage.sh`: `just
  test-vm unit smoke` locally, see the pull request.
- KVM on standard runners, the runner's resources, and the read-only token
  and absent secrets of a fork's pull request: GitHub's documentation and
  changelog (2024-04-02) [external].

## More Information

Refines ADR-0063 (where root tests run) and the T2 tier of
`docs/testing.md`; ADR-0086's VM network isolation stays for the local VM.
Revisit when the repository becomes private (runner resources and
minutes change), if GitHub's runners stop being per-job VMs, or when a
nested VM on the runner is needed to test kernel 5.15.
