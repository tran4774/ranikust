# Introduction

ranikust builds container images from Dockerfiles, like
[kaniko](https://github.com/osscontainertools/kaniko). It is one static Linux
binary, with no daemon and no container runtime, and runs inside a container
or directly on a host or VM.

> **Status: pre-alpha.** ranikust builds as root, in overlay mode or in kaniko
> mode, or as a regular user, in rootless overlay mode, and its releases are
> pre-releases ([Installing](install.md)). The layer cache and some
> instructions come later; each page says what is not built yet. See
> [Status and roadmap](status.md).

## Why ranikust

Where a builder runs decides what it may do. kaniko runs in any container, but
finds what a `RUN` changed by walking the whole filesystem. Builders that need
mounts or user namespaces stop working where those are forbidden: most
locked-down CI containers and Kubernetes pods.

ranikust uses what the environment allows:

- where it may mount filesystems, overlayfs snapshots, with independent stages
  built in parallel;
- where it may not, kaniko's approach;
- where neither works, it stops and tells you what to change.

[How ranikust builds](how-it-builds.md) explains the two modes;
[Choosing where to run it](where-to-run.md) lists the mode each environment
gets.

## Relationship to kaniko

- It accepts kaniko's common flags, so existing pipelines switch with few
  changes ([ADR-0011]); [Command line](usage.md) lists what each does.
- Where kaniko and BuildKit build a Dockerfile differently, the image follows
  BuildKit (`docker build`) ([ADR-0032]): OCI media types by default, a new
  `ENTRYPOINT` clears an inherited `CMD`, one history entry per instruction, a
  default `PATH` in every config, empty fields of the base config dropped. The
  config, and so the image digest, differ from kaniko's even for the same
  files.
- Its layer cache is its own and is not shared with kaniko's ([ADR-0005]).
- It leaves out kaniko's `bake`, the cache warmer, and S3, GCS and Azure build
  contexts ([ADR-0008]).
- Releases target x86_64 Linux; aarch64 is kept buildable ([ADR-0009]).

This guide is for people who run ranikust. Contributors start with the
[README](https://github.com/tran4774/ranikust#readme).

[ADR-0005]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0005-own-versioned-cache-key.md
[ADR-0008]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0008-out-of-scope-bake-warmer-cloud-contexts.md
[ADR-0009]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0009-x86-64-first-aarch64-ready.md
[ADR-0011]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0011-accept-common-kaniko-flags.md
[ADR-0032]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0032-observable-behaviour-follows-buildkit.md
