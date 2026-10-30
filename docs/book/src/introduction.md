# Introduction

ranikust builds container images from Dockerfiles, like
[kaniko](https://github.com/osscontainertools/kaniko). It ships as a single
static Linux binary, with no daemon and no container runtime, and it is meant
to run both inside a container and directly on a host or a VM.

> **Status: pre-alpha. Nothing described in this guide works yet.** There is
> no release, and the binary does not build images. The pages describe the
> design the code is being written to. See [Status and roadmap](status.md).

## Why ranikust

Where a builder runs decides what it is allowed to do. kaniko is made to run
inside a container and finds what a `RUN` command changed by walking the
whole filesystem.
Builders that rely on mounts or user namespaces stop working where those are
forbidden, which is the case in most locked-down CI containers and
Kubernetes pods.

ranikust covers both sides with one binary:

- where it may mount filesystems, it uses overlayfs snapshots and builds
  independent stages in parallel;
- where it may not, it falls back to kaniko's approach;
- where neither works, it stops and tells you what to change.

[How ranikust builds](how-it-builds.md) explains the two modes, and
[Choosing where to run it](where-to-run.md) lists which mode each environment
gets.

## Relationship to kaniko

- ranikust accepts kaniko's common command-line flags, so that existing
  pipelines can switch with few changes
  ([ADR-0011](https://github.com/tran4774/ranikust/blob/main/docs/adr/0011-accept-common-kaniko-flags.md)).
- Where kaniko and BuildKit build a Dockerfile differently, the image
  follows BuildKit (`docker build`), as the Dockerfile reference describes.
  Images are written with OCI media types by default. A new `ENTRYPOINT`
  clears a `CMD` inherited from the base image. History has one entry per
  instruction. Every config gets a default `PATH`, and empty fields of the
  base config are dropped. So the config bytes, and the image digest,
  differ from kaniko's even when the files are the same
  ([ADR-0032](https://github.com/tran4774/ranikust/blob/main/docs/adr/0032-observable-behaviour-follows-buildkit.md)).
- Its layer cache is its own and is not shared with kaniko's
  ([ADR-0005](https://github.com/tran4774/ranikust/blob/main/docs/adr/0005-own-versioned-cache-key.md)).
- It does not include kaniko's `bake`, the cache warmer, or S3, GCS and
  Azure build contexts
  ([ADR-0008](https://github.com/tran4774/ranikust/blob/main/docs/adr/0008-out-of-scope-bake-warmer-cloud-contexts.md)).
- Releases target x86_64 Linux first; aarch64 is kept buildable
  ([ADR-0009](https://github.com/tran4774/ranikust/blob/main/docs/adr/0009-x86-64-first-aarch64-ready.md)).

## More documentation

This guide is for people who will run ranikust. Contributors start with the
[README](https://github.com/tran4774/ranikust#readme), which links the
architecture, the decision records and the research behind them.
