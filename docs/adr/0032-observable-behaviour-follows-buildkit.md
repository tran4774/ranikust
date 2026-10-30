---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0032: Observable build behaviour follows BuildKit

## Context and Problem Statement

By the end of Phase 1's second wave, `docs/open-questions.md` held more than
seventy open questions. Many asked whether ranikust should behave like kaniko
or like BuildKit where the two differ: image config edits, history, `COPY`
rules, `RUN --mount` options, registries. Others were internal design or
belonged to later phases. Answering each by hand blocked the builder and the
acceptance runs. Several entries also described BuildKit from memory
(`[inferred]`), and some of those descriptions were wrong. The question is
how to settle these questions in one rule, and what each answer is.

## Considered Options

- Decide each question separately, with the maintainer, as before. Slow, and
  each answer needs its own research.
- Follow kaniko for everything observable. That matches the tool ranikust
  replaces, but kaniko is unmaintained and diverges from the Dockerfile
  reference in several places (ENTRYPOINT and CMD, EXPOSE, PATH).
- Follow BuildKit for everything observable, checked in its source, and
  escalate only what BuildKit's model allows and ranikust's does not, or
  what costs significant performance. Chosen: BuildKit implements the
  documented Dockerfile behaviour, and `Q-051` and `ADR-0027` already
  followed it.

## Decision Outcome

Where a build's observable result (image, layers, config, history, `RUN`
environment, registry traffic) is undecided, ranikust does what BuildKit
does, verified in BuildKit's source.

- Escalated to the maintainer instead: (B1) BuildKit can do it but
  ranikust's model cannot (a technical limit), and (B2) choices with a
  significant performance impact. Today: Q-045 (git contexts), Q-183
  (`--network=none` without `CAP_NET_ADMIN`) and Q-049 (filtering
  unchanged copied-up files).
- Not affected: the meaning of kaniko's flags (`ADR-0011`), kaniko mode's
  own questions, internal design (crate APIs, store layout), and accepted
  ADRs.
- The answers below supersede interim picks that followed kaniko. The code
  changes land in follow-up branches. Each removes its `[open: Q-NNN]`
  markers.

| Q | Decision | BuildKit evidence (`buildkit@cd8ee244`) |
|---|---|---|
| Q-006 | OCI media types by default, whatever the base image and `--reproducible`; gzip by default; `--image-format` overrides | `exporter/containerimage/export.go:576-581`, `solver/llbsolver/compat/compat.go:12-13`, `util/compression/compression.go:78` |
| Q-009 | The parser accepts every instruction; the builder refuses what it cannot run yet | `frontend/dockerfile/instructions/parse.go` |
| Q-013 | The writer takes missing parents' metadata from the merged view | containerd `pkg/archive/tar.go:718-745` (vendored), `util/overlay/overlay_linux.go:111-116` |
| Q-014 | Cross-repository mounts with upload fallback, in Phase 1 | containerd `core/remotes/docker/pusher.go:200-225` (vendored), `util/push/push.go:169-205` |
| Q-022 | `--secret` takes the full syntax: `id=`, `type=file\|env`, `src=`/`source=`, `env=`; with no source, the variable named by the id, else the file; at most 500 KiB | `cmd/buildctl/build/secret.go:29-64`, `session/secrets/secretsprovider/store.go:17-40`, `secretsprovider.go:15` |
| Q-025 | Exported layers carry only the `security.capability` xattr | containerd `pkg/archive/tar.go:658-665` (vendored), `util/overlay/overlay_linux.go:116` |
| Q-026 | No layer merging; too many lowers fail with a clear error | containerd `plugins/snapshots/overlay/overlay.go:608` (vendored) |
| Q-043 | `prune` ages by last use, with BuildKit's tiers as defaults | `cmd/buildkitd/config/gcpolicy.go:71-106`, `gcpolicy_unix.go:6-10` |
| Q-044 | Remote `ONBUILD` triggers extend the graph after base configs resolve, repeated until stable | `frontend/dockerfile/dockerfile2llb/convert.go:557-597` |
| Q-046 | Any `https://` context is fetched as plain HTTPS, Azure hosts included | `frontend/dockerui/context.go:339-346`, `:136-144` |
| Q-065 | Loopback registries (`localhost`, `127.0.0.0/8`, `::1`) use plain HTTP unasked; other hosts only with `--insecure*` | `util/resolver/resolver.go:33-60`, containerd `core/remotes/docker/registry.go:218-251` (vendored) |
| Q-080 | `rnk-builder` resolves sources with BuildKit's wildcard and error rules: an empty wildcard match copies nothing and is no error; no trailing-`/` rule | `frontend/dockerfile/dockerfile2llb/convert_copy.go:283-293`, `solver/llbsolver/file/backend.go:309-312`, fsutil `copy/copy.go:45-66` (vendored) |
| Q-081 | The context sub-path applies only to fetched contexts | `frontend/dockerui/context.go:169-173`, `:348-353` |
| Q-100 | Blobs are fsynced; extracted layers are not | containerd `plugins/content/local/writer.go:97`, `core/diff/apply/apply_linux.go:56-67` (vendored) |
| Q-120 | `COPY` follows a symlinked source, copies all xattrs, merges an existing top-level destination, recreates special files, turns sockets into empty regular files, applies `--chmod` to directories, and accepts symbolic `--chmod` | `convert_copy.go:67-87`, `:283-293`; fsutil `copy/copy.go:127-137`, `:456-469`, `copy_nowindows.go:15-45` (vendored) |
| Q-130 | A new `ENTRYPOINT` clears an inherited `CMD` unless the stage set one; `EXPOSE` takes `tcp\|udp\|sctp`, lower-cases it and expands ranges; `HEALTHCHECK --start-interval` is kept; an absolute `WORKDIR` stays as written | `convert.go:1614-1642`, `convert_expose.go:15-44`, `util/system/path.go:34-121` |
| Q-131 | One history entry per instruction: `empty_layer` for metadata, a layer for `RUN`/`COPY`/`ADD` and `WORKDIR` other than `/`, none for `FROM`/`ONBUILD`; no author | `convert.go:1544-1567`, `:1593-1596`, `:1797-1830` |
| Q-132 | Empty values and fields outside the OCI and Docker image specs are dropped from the config | `convert.go:714`, `frontend/dockerui/build.go:54`, image-spec `config.go:26-61` (vendored) |
| Q-150 | A layer whose extraction depends on the layers below it is keyed by its chain; other layers by diff ID | `cache/blobs.go:388-420` |
| Q-151 | `mount(2)` with many lowers uses relative names from a thread that did `unshare(CLONE_FS)` and `chdir` | containerd `core/mount/mount_linux.go:119-134`, `:513-547` (vendored) |
| Q-160 | Base layers are kept as pulled under `--reproducible`; only later layers are rewritten | `exporter/containerimage/writer.go:451-490` |
| Q-161 | The fixed time is `SOURCE_DATE_EPOCH`, else the Unix epoch; times above it are clamped to it, older times kept | `writer.go:809-841`, `util/converter/converter.go:76-86` |
| Q-180 | `VOLUME` only records the config; an empty name is refused | `convert.go:1654-1665` |
| Q-181 | Cache `sharing=` is enforced: `shared`, `locked` (wait), `private` (a new empty cache when the existing one is busy); `uid`/`gid`/`mode` apply at creation and are part of the cache key | `solver/llbsolver/mounts/mount.go:68-160`, `convert_runmount.go:48-62`, `:116-135` |
| Q-182 | A secret is a build-private file, mode `0400` root by default, with `mode`/`uid`/`gid` honoured; `env=` is supported | `solver/llbsolver/mounts/secretmount_unix.go:16-85`, `client/llb/exec.go:779-781`, `convert_secrets.go:22-86` |
| Q-184 | `RUN --network=host` needs the operator's `--allow network.host`, which lifts `--network=none` | `frontend/dockerfile/dockerfile2llb/convert_runnetwork.go:11-23`, `util/entitlements/entitlements.go:155-160` |
| Q-185 | Any Linux config without `PATH` gets the default `PATH` | `convert.go:792-802`, `util/system/path.go:13-25` |
| Q-186 | A cancel kills the running `RUN` with SIGKILL | `executor/runcexecutor/executor.go:605-626`, `:674-712` |

### Consequences

- Good, because a Dockerfile behaves as the Dockerfile reference and
  `docker build` say, and BuildKit's test suite becomes a source of goldens.
- Good, because most open questions are settled at once, with evidence.
- Bad, because images differ from kaniko's in config and history bytes:
  OCI media types by default, per-instruction history, dropped empty fields,
  `PATH` in every config, the inherited `CMD` cleared by `ENTRYPOINT`.
  Users comparing digests with kaniko see a difference. `kaniko-map.md`
  lists each one.
- Neutral, because kaniko's flag names and meanings still hold (`ADR-0011`).

## Evidence

- BuildKit source at commit `cd8ee2441a4cb4d29f816bab2b009eac131784d7`
  (2026-10-29), read for every question; citations in the table above
  [code]. Behaviour in buildx or runc that this checkout does not contain
  is marked `[external]` in the open-question entries.
- The check corrected five `[inferred]` claims in the register:
  - BuildKit keeps only `security.capability` (Q-025).
  - It does filter unchanged copied-up files (Q-049).
  - It keeps an absolute `WORKDIR` as written (Q-130).
  - It mounts many lowers from a thread, not a child process (Q-151).
  - `private` caches are new, not copies (Q-181).
- kaniko at `44aa7ddc0` for the contrast, cited in each entry.

## More Information

- Answers Q-006, Q-009, Q-013, Q-014, Q-022, Q-025, Q-026, Q-043, Q-044,
  Q-046, Q-065, Q-080, Q-081, Q-100, Q-120, Q-130, Q-131, Q-132, Q-150,
  Q-151, Q-160, Q-161, Q-180, Q-181, Q-182, Q-184, Q-185 and Q-186.
- Related: `ADR-0011` (kaniko flags), `ADR-0012` (accepted divergence
  between modes), `ADR-0027` (the context walk follows BuildKit), `Q-051`.
- The differences from kaniko are listed in
  [kaniko-map.md](../agents/kaniko-map.md#behaviour-to-match).
- Revisit an answer when BuildKit changes the behaviour, or when a golden
  against BuildKit contradicts the cited code.
