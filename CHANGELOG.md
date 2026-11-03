# Changelog

All notable changes to ranikust are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a
minor version may contain breaking changes.

Entries are written for users. Each pull request gives its entry in the
`## Changelog` section of its description; at release time
`just release-prepare` gathers them into the version's section, which is
then edited by hand (ADR-0088, ADR-0089). Version 0.0.1 sums up everything
before it; the detail is in the ADRs and in git.

## [Unreleased]

## [0.0.1] - 2026-11-03

The first pre-alpha release: one static binary for x86_64 Linux (5.8 or
later) that builds a Dockerfile, as root, and writes or pushes an OCI image.
It has been measured against kaniko and BuildKit on three test workloads in
a VM, not yet on real projects. Do not rely on it in production yet.

### Added

- **Two ways to build, picked at start-up.** `--mode=auto` (the default)
  probes what the environment allows and says why it chose:
  - **overlay mode** where ranikust may mount (root on a host or VM, a
    `--privileged` container): each instruction's changes captured by
    overlayfs, independent stages built in parallel up to `--jobs`, every
    `RUN` in a sandbox of its own (namespaces, `pivot_root`, capabilities
    cut, a ranikust-owned init as PID 1);
  - **kaniko mode** where root may not mount (a default Docker container,
    most Kubernetes pods): as kaniko, on the container's `/`, stages one
    after another, each layer found by a parallel walk and hash; off `/`,
    a directory per stage, built in parallel in the full sandbox.
  - Elsewhere it stops with the step that failed and what to do instead.
    `--mode=overlay|kaniko` forces a mode, still checked.
- **Dockerfile support:** `FROM` an image, `scratch` or a stage; `ARG`,
  `ENV`, `LABEL`, `USER`, `WORKDIR`, `EXPOSE`, `VOLUME`, `ENTRYPOINT`,
  `CMD`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`; local `COPY`/`ADD` with
  wildcards, heredocs, `--from`, `--chown`, `--chmod` and `.dockerignore`;
  `RUN` in shell, exec and heredoc form, with `--mount=type=cache`,
  `--mount=type=secret` and `--network=none`. Where kaniko and BuildKit
  differ, ranikust generally follows BuildKit.
- **Outputs:** push to each `--destination`, a Docker tarball
  (`--tar-path`), an OCI layout (`--oci-layout-path`), kaniko's digest
  files, gzip or zstd layers (`--compression`), and `--reproducible`
  digests that do not depend on `--jobs` or the host.
- **kaniko's executor flags** keep their names and meaning (`--context`,
  `--dockerfile`, `--build-arg`, `--target`, `--destination`, `--no-push`,
  `--snapshot-mode`, `--ignore-path`…); those ranikust does not support
  are refused with the reason, never silently ignored. New: `--mode`,
  `--state-dir`, `--jobs`, `--network`, `--platform`, `--secret`,
  `--allow`, `--limit`, `--metrics-file`.
- **Bounds on what a registry or an image can make ranikust hold or wait
  for** (`--limit`, with defaults), and an opt-in JSON report of where a
  build spent its time and resources (`--metrics-file`).
- **Releases on GitHub:** a tarball with the binary, `THIRD-PARTY-LICENSES`
  (the notices of every crate and toolchain component it contains),
  `SHA256SUMS` and a build provenance attestation; the
  [installation guide](https://github.com/tran4774/ranikust/blob/main/docs/book/src/install.md)
  has the command that
  checks a download against the release workflow and its tag.
  ([#42](https://github.com/tran4774/ranikust/pull/42))

### Security

- The threat model, the Phase 1–2 security audit and its fixes are public
  (`docs/security/`). `--secret` values are read once at start-up and kept
  in memory; in the full sandbox a `RUN` does not see ranikust's command
  line or environment (in kaniko mode on `/` it can: see below).

### Known limitations

- x86_64 Linux only; no aarch64 build and no container image yet.
- Root only: a regular user is refused (rootless overlay mode is Phase 3,
  [#41](https://github.com/tran4774/ranikust/issues/41)).
- No layer cache: `--cache` is refused, `--cache=false` is accepted
  (Phase 4).
- `ADD` of a URL, a git repository or an archive, `COPY --link`,
  `RUN --mount=type=bind|tmpfs`, `ONBUILD`, registry mirrors and
  credential helpers stop the build with a message naming their phase
  (Phase 5).
- Kaniko mode on a container's `/`: every `RUN` runs directly on `/`, as in
  kaniko, and can read everything mounted into the container, registry
  credentials included; mount only what the build needs and prefer
  short-lived, push-only credentials. There a cache mount is left out with
  a warning, and a secret file and `--network=none` are refused.
- Until ranikust's own binary is bound read-only in the full sandbox
  ([#32](https://github.com/tran4774/ranikust/issues/32)), a `RUN` there is
  kept from it by three barriers: ranikust is not dumpable, refuses its
  own path as a command, and the kernel refuses to open a running binary
  for writing.
- Measured on the three test workloads in a VM (W1–W3), not yet on real
  projects or across many kernels and CI systems.

[Unreleased]: https://github.com/tran4774/ranikust/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/tran4774/ranikust/releases/tag/v0.0.1
