# Status and roadmap

## Where the project is

ranikust is **pre-alpha**. There is no release, and the binary only prints
its version. Its build metrics already work: with
`RANIKUST_METRICS_FILE=<path>` set, it writes a JSON report of where the
run spent time and resources when it exits.

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
| 1. First internal milestone | Overlay mode as root, parallel stages, local build context, the common Dockerfile instructions, registry pull and push, Docker tar and OCI layout output | Not started |
| 2. kaniko mode | Snapshot by filesystem walk, one root filesystem, optional parallel stages | Not started |
| 3. Rootless and detection | Overlay mode in a user namespace, automatic mode selection, refusing with guidance, `--seccomp` and `--userns` | Not started |
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
