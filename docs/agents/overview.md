# Overview

**TL;DR:** ranikust builds OCI images from Dockerfiles like kaniko, as one
static binary: overlayfs snapshots where it may mount, a kaniko-style walk and
hash where it may not. Pre-alpha: Phases 1 and 2 are done and the releases so
far are pre-releases. This page is the entry point; the pages it links own the
detail.

## What ranikust is

- A Rust rewrite of kaniko's executor: Dockerfile and context in; image
  pushed, or written as a Docker tar or an OCI layout.
- One static-pie musl binary (x86_64 released, aarch64 kept buildable) for
  hosts and containers. It replaces kaniko's executor, `tini` and credential
  helpers (`ADR-0009`, `ADR-0020`).
- Common kaniko flags are accepted (`ADR-0011`, [kaniko-map.md](kaniko-map.md));
  kaniko's cache is not (`ADR-0005`). Out of scope: bake, the warmer, cloud
  contexts, multi-platform builds (`ADR-0008`).
- It writes nothing outside the state directory or store, the requested
  outputs, its own delegated cgroup with the cgroups it makes below it
  (`ADR-0110`), and, in kaniko mode in a container, `/`; there is no
  work-directory flag (`ADR-0050`).
- Two snapshot modes, chosen by probing at start-up: overlay and kaniko
  (`ADR-0002`, `ADR-0014`). Where neither works it refuses with guidance.

## Where each part is described

| Topic | Read |
|---|---|
| Build pipeline, step by step, and which crate owns each step | [`ARCHITECTURE.md`](../../ARCHITECTURE.md#data-flow-of-a-build) |
| The two modes side by side | [`ARCHITECTURE.md`](../../ARCHITECTURE.md#the-two-modes), [modes-and-detection](../design/modes-and-detection.md) |
| The probe and its order (`--mode=auto`) | [modes-and-detection](../design/modes-and-detection.md#probe-order---modeauto) |
| Crates, boundaries and dependency direction | [`ARCHITECTURE.md`](../../ARCHITECTURE.md#code-map), [`#intended-dependency-direction`](../../ARCHITECTURE.md#intended-dependency-direction) |
| Rules that hold everywhere | [`ARCHITECTURE.md`](../../ARCHITECTURE.md#cross-cutting-invariants), [rules.md](rules.md) |
| What is done and what is next | the [Project](https://github.com/users/tran4774/projects/1); Phase 3 is [#41](https://github.com/tran4774/ranikust/issues/41); [phase-1.md](phase-1.md) and [phase-2.md](phase-2.md) are frozen |

## Where state lives

| Place | Holds | Detail |
|---|---|---|
| Build root | The root being built: `/` in kaniko mode in a container (`flock`ed), else a directory ranikust makes in `<state>/builds/<id>/` | [modes-and-detection](../design/modes-and-detection.md#build-root-and-state-directory) |
| State directory | `--state-dir`, default `/var/lib/ranikust` for every mode. The store, `builds/<id>/`, the stub journal, `deps/` for `COPY --from`, stage images, secrets. Owned by the user, `0700`, below directories only root may write. On the ignore list in kaniko mode on `/`: never walked, snapshotted or deleted | [overlay-snapshot](../design/overlay-snapshot.md#state-directory-layout), `ADR-0050` |
| Store | In the state directory, owned by `rnk-store`: `blobs/sha256/`, `layers/<id>/`, `caches/`, `tmp/`. Content-addressed, immutable, per-digest `flock`, atomic rename, shared by concurrent builds | [cache-and-store](../design/cache-and-store.md#store-layout), `ADR-0006`, `ADR-0007`, `ADR-0021` |
| Outputs | `--destination`, `--tar-path`, `--oci-layout-path` and the three digest files; the only other place written, besides the metrics report | `ADR-0076` |
| Metrics report | `--metrics-file` (opt-in): one JSON file written at exit, never during the build | [metrics](../design/metrics.md), `ADR-0025`, `ADR-0037` |
