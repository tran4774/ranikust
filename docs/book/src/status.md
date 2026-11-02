# Status and roadmap

## Where the project is

ranikust is **pre-alpha**. There is no release. The binary builds a
Dockerfile in overlay mode as root, stages in parallel, or in kaniko mode
where root may not mount (on a container's `/`, as kaniko, or a directory
per stage elsewhere), and writes an OCI layout, a Docker tar or pushes
([Command line](usage.md)). Both modes have been compared with kaniko on
the test workloads in a VM, not yet on real projects. Its build metrics already work:
with `--metrics-file=<path>`, it writes a
JSON report of where the run spent time and resources when it exits.

What exists:

- a survey and experiments that tested the design before any code was
  written ([research](https://github.com/tran4774/ranikust/tree/main/docs/research));
- the decisions taken from them
  ([architecture decision records](https://github.com/tran4774/ranikust/tree/main/docs/adr));
- the crate layout of the workspace, a static musl build and CI.

## Roadmap

The work is planned in phases. There are no dates.

| Phase | Goal | Status |
|---|---|---|
| 0. Spike | Prove the risky parts: the `RUN` sandbox, overlay snapshots, mode detection | Mostly done by the pre-code experiments |
| 1. First internal milestone | Overlay mode as root, parallel stages, local build context, the common Dockerfile instructions, registry pull and push, Docker tar and OCI layout output | Builds end to end; acceptance runs left |
| 2. kaniko mode | Snapshot by filesystem walk; on a container's `/` stages one at a time, elsewhere a directory per stage built in parallel (`--parallel-stages` refused, ADR-0060) | Built; closing checks left |
| 3. Rootless and detection | Overlay mode in a user namespace, automatic mode selection, refusing with guidance, `--seccomp` and `--userns` | In part: automatic mode selection and refusing with guidance are built; overlay in a user namespace, `--seccomp` and `--userns` are not |
| 4. Cache | Layer cache in a registry, with ranikust's own versioned keys | Not started |
| 5. Feature coverage | `ADD` from URLs and git, more `RUN --mount` and `COPY` options, registry mirrors, credential helpers, `prune` | Not started |
| 6. Hardening and release | kaniko's 200 integration Dockerfiles in every mode, fuzzing, the host-safety test suite, packaging | Not started |

Version 1.0 needs all three ways of building: overlay as root, rootless
overlay, and kaniko mode.

## Not planned

Multi-platform builds, kaniko's `bake` and cache warmer, and S3, GCS or Azure
build contexts
([ADR-0008](https://github.com/tran4774/ranikust/blob/main/docs/adr/0008-out-of-scope-bake-warmer-cloud-contexts.md)).

## Following along

Changes are listed in the
[changelog](https://github.com/tran4774/ranikust/blob/main/CHANGELOG.md).
