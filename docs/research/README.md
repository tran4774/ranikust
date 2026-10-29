# Research

Before the first line of ranikust was written, a survey asked whether kaniko
could be rebuilt in Rust as a single static binary that also runs outside
containers, and backed every answer with experiments. This directory is that
survey, translated from Vietnamese and rewritten as English reports.

| | |
|---|---|
| **Status** | Frozen, 2026-10-29. Historical record; not updated as the code evolves. |
| **Authority** | Decisions now live in [`docs/adr/`](../adr/); the current design lives in [`ARCHITECTURE.md`](../../ARCHITECTURE.md) and [`docs/design/`](../design/). Where they disagree with this directory, they win. |
| **Language** | English. The Vietnamese originals and the PoC sources are archived outside this repository. |
| **Format** | See [`TEMPLATE.md`](TEMPLATE.md) for the report templates and evidence labels. |

Crate names in these reports use the survey-era prefix `rk-*`; the crates
were renamed to `rnk-*` (for example `rk-rootfs` → `rnk-rootfs`) on
2026-10-30, after the survey was frozen.

## How to read this

Start with the [summary](survey/00-summary.md). The survey chapters explain the
reasoning; the experiment reports hold the evidence, each with its environment,
method, results and limitations.

## Survey chapters

| # | Report | Topic |
|---|---|---|
| 00 | [Summary](survey/00-summary.md) | Chosen model, conclusions, what was and was not verified |
| 01 | [kaniko today](survey/01-kaniko-today.md) | kaniko's building blocks, size, and everything hard-wired to `/` |
| 02 | [kaniko-mode requirements](survey/02-kaniko-mode-requirements.md) | Workdir as rootfs, the `RUN` sandbox, refusing writes outside the workdir |
| 03 | [Rust architecture](survey/03-rust-architecture.md) | Crate map, library choices, what must be written from scratch |
| 04 | [Risks and roadmap](survey/04-risks-and-roadmap.md) | Risk register, MVP scope, phases, estimates, verification plan |
| 05 | [Decisions](survey/05-decisions.md) | Decisions Q1–Q12 and where each ADR now records them |
| 06 | [Overlay and parallelism](survey/06-overlay-and-parallelism.md) | Mode selection, overlay snapshots, lazy export, stage scheduling |

## Experiments

| # | Report | Question |
|---|---|---|
| 01 | [Overlay semantics](experiments/01-overlay-semantics.md) | How overlayfs behaves at each privilege level: options, whiteouts, copy-up, hardlinks, lowerdir limits |
| 02 | [RUN sandbox](experiments/02-run-sandbox.md) | Mount order, minimum privileges, chroot escape, capability dropping, stubs, overhead |
| 03 | [Privilege matrix](experiments/03-privilege-matrix.md) | Which environments (distros, Docker, Podman, Kubernetes, CI) land in which mode |
| 04 | [Dockerfile parser](experiments/04-dockerfile-parser.md) | Existing Rust parser and `.dockerignore` crates against BuildKit on 201 Dockerfiles |
| 05 | [Rust ecosystem](experiments/05-rust-ecosystem.md) | Static binary, aarch64, allocator, deterministic parallel compression, OCI and tar crates |
| 06 | [Overlay vs walk](experiments/06-overlay-vs-walk.md) | Diff cost of overlay upperdir walks against an optimized full walk |
| 06a | [Root re-run](experiments/06a-root-rerun.md) | Experiments 01, 02 and 06 repeated as real root on the host |
| 07 | [kaniko vs overlay](experiments/07-kaniko-vs-overlay.md) | Full W1 builds: upstream kaniko, optimized kaniko, Go walk, overlay |
| 08 | [Parallel stages and lazy export](experiments/08-parallel-stages-lazy-export.md) | W3 with `--jobs`, lazy vs eager export, against BuildKit and kaniko |
| 09 | [kaniko-mode prototype](experiments/09-kaniko-mode-prototype.md) | Rust walk and diff, mountinfo ignore list, safe filesystem deletion |
| 10 | [EXDEV and package managers](experiments/10-exdev-package-managers.md) | apt/dpkg, pip, yarn, cargo when renaming lower directories fails |
| 11 | [Root extras](experiments/11-root-extras.md) | AppArmor profile for unprivileged users, overlay on ext4/xfs/btrfs, `volatile`, nesting |
| 12 | [Parallel stages in kaniko mode](experiments/12-kaniko-mode-parallel.md) | One rootfs directory per stage without mounts: mechanisms, compatibility, performance |
| 12a | [Fake /proc](experiments/12a-fake-proc.md) | Static `/proc` snapshots for bare-chroot `RUN`: jest, terser, Go, bash |
| 13 | [Rootful Docker in a VM](experiments/13-docker-rootful-vm.md) | Real Docker 29 under cgroup v2 and v1: mode table, AppArmor, mknod, proot, kernel 5.15 |
| 14 | [Real builds without /proc](experiments/14-real-builds-without-proc.md) | `next build` of W1, Maven/Gradle on several JDKs, tsc/esbuild/vite |
| 15 | [Java, Next.js and concurrency](experiments/15-java-next-concurrency.md) | Java OOM under memory limits, JDK toolchains, Next 16/Turbopack, concurrent fake /proc |
| 16 | [cgroup v1 and cpuset](experiments/16-cgroup-v1-cpuset.md) | JVM container awareness per JDK, Node tools, W3 under v1, cpuset, final fake /proc sets |

## Workloads

The experiments reuse three benchmark builds from a separate kaniko
optimization project:

| Name | Dockerfile | Shape |
|---|---|---|
| W1 | `nextjs-kaniko/app/Dockerfile` | Next.js app; `deps` → `final` chain plus an independent `assets` stage; about 2.2–3 GB of files |
| W2 | `nextjs-kaniko/app-cache` | Cache paths: three stages from one base, a `COPY --from=<image>`, a large context `COPY`, and a last layer that changes between the warm and partial builds; not measured in this survey |
| W3 | `nextjs-kaniko/app-par` | Three independent `pnpm install` stages and a small `final` stage |

## Source snapshot

kaniko source references point at `44aa7ddc0`, a local benchmarking branch
made of 13 optimization commits on top of
[osscontainertools/kaniko](https://github.com/osscontainertools/kaniko)
`34ba9eadf` (v1.28.6 + 7). "Upstream kaniko" in the measurements means
`34ba9eadf` without those commits.

## Machine

Unless a report says otherwise: AMD Ryzen 9 9950X3D (16 cores / 32 threads),
29–30 GB RAM, NVMe with ext4 `/home`, Linux 7.0, podman 5.7 rootless, cgroup v2.
The machine was shared with other work, so each report records load where it
mattered.
