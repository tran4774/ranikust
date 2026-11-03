# Status and roadmap

ranikust is **pre-alpha**: every release is a pre-release, and there is no
stable release yet ([Installing](install.md)).

## What works

As root, ranikust builds a Dockerfile in overlay mode (stages in parallel) or,
where root may not mount, in kaniko mode (on a container's `/`, as kaniko, or
in a directory per stage elsewhere). It pushes the image or writes an OCI
layout or a Docker tar ([Command line](usage.md)). Both modes have been
compared with kaniko on three test workloads in a VM, not yet on real
projects. With `--metrics-file=<path>` it writes a JSON report of where the run
spent time and resources.

## What is not built yet

The work is planned in phases, without dates. The
[README](https://github.com/tran4774/ranikust#roadmap) lists them; the
[Project](https://github.com/users/tran4774/projects/1) tracks them.

| Missing | Phase |
|---|---|
| Builds as a regular user (rootless overlay mode), `--seccomp`, `--userns` | 3, [#41](https://github.com/tran4774/ranikust/issues/41) |
| The layer cache (`--cache`) | 4 |
| `ADD` of a URL, a git repository or an archive, and `ADD --checksum`, `--keep-git-dir` and `--unpack=true`; `COPY` and `ADD` with `--link` or `--exclude`; `COPY --parents`; `RUN --mount=type=bind`, `tmpfs`, `ssh` and `type=cache,from=` (on a container's `/`, whether and how bind, tmpfs and `from=` mounts work is open: [#24](https://github.com/tran4774/ranikust/issues/24)); `ONBUILD`; git, tar and HTTPS build contexts; registry mirrors; credential helpers; the `prune` subcommand | 5 |
| kaniko's 200 integration Dockerfiles in every mode, fuzzing, the host-safety test suite, packaging | 6 |

An instruction, or a kaniko flag, of a later phase stops the build with a
message naming the phase. Version 1.0 needs all three ways of building:
overlay as root, rootless overlay, and kaniko mode.

Not scheduled: the `push` subcommand (it would take an OCI layout,
[ADR-0052](https://github.com/tran4774/ranikust/blob/main/docs/adr/0052-push-input-aliases-and-issue-mining-fixes.md))
and `--dryrun`
([ADR-0036](https://github.com/tran4774/ranikust/blob/main/docs/adr/0036-kaniko-flags-in-phase-1.md)).

Not planned: multi-platform builds, kaniko's `bake` and cache warmer, and S3,
GCS or Azure build contexts
([ADR-0008](https://github.com/tran4774/ranikust/blob/main/docs/adr/0008-out-of-scope-bake-warmer-cloud-contexts.md)).

Changes are listed in the
[changelog](https://github.com/tran4774/ranikust/blob/main/CHANGELOG.md).
