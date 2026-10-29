---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0008: Out of scope: bake, warmer, cloud build contexts

## Context and Problem Statement

kaniko ships several features beyond building one Dockerfile: `bake` (build
several targets from an HCL Bakefile), a cache warmer that pre-pulls base
images, and build contexts fetched from S3, GCS and Azure Blob Storage. Each
adds code, dependencies (cloud SDKs) and test surface. Which of them belong in
ranikust?

## Considered Options

- Port all of them for feature parity with kaniko.
- Leave them out, while keeping the context-source interface open.

## Decision Outcome

ranikust does not implement `bake`, the cache warmer, or S3, GCS and Azure build
contexts; `rnk-context` keeps an open trait so new context sources can be added
later.

- Supported context sources: local directory, tar archive, git repository and
  HTTPS URL.
- The persistent store ([ADR-0006](0006-persistent-store.md)) covers the main
  use of the warmer: base layers stay on disk between builds.
- Multi-platform builds (running another architecture's `RUN` through
  `binfmt_misc`) are not on the roadmap either
  ([ADR-0009](0009-x86-64-first-aarch64-ready.md)).

### Consequences

- Good, because no cloud SDKs end up in the static binary and the 1.0 estimate
  shrinks.
- Bad, because users of these kaniko features must fetch the context
  themselves (for example to a tar file or an HTTPS URL) or drive several
  builds from a script.

## Evidence

- [Survey 05](../research/survey/05-decisions.md) Q6.
- [Survey 01](../research/survey/01-kaniko-today.md) §2: `pkg/bake`,
  `pkg/warmer` and the cloud sources of `pkg/buildcontext` in kaniko.
- [Survey 04](../research/survey/04-risks-and-roadmap.md) §2–§3: listed under
  "not done"; dropping them offset part of the estimate increase from rootless
  support [inferred].

## More Information

Revisit if users ask for a specific cloud source; it would be a new
implementation of the `rnk-context` trait, not a change to the build core.
