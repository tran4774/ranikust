# Changelog

All notable changes to ranikust are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a
minor version may contain breaking changes.

Entries are written for users. `just changelog` drafts them from the
Conventional Commit messages with git-cliff; the draft is then edited by hand.

## [Unreleased]

### Added

- Repository setup: a Cargo workspace with one crate per component (no
  functionality yet), a static musl release build, CI (format, clippy, tests,
  aarch64 check, cargo-deny, docs), architecture decision records, the
  research that preceded the code, a user guide skeleton, and contributor
  and security policies.
- Opt-in build metrics: with `RANIKUST_METRICS_FILE=<path>` set, ranikust
  writes one JSON report when it exits, also after an error or a panic:
  time, counts and bytes per build phase, per stage, per instruction and
  per snapshot, bytes per registry host, CPU and peak memory of ranikust and
  of its `RUN` processes, and cgroup v2 and network counters. The sections
  follow the `KANIKO_METRICS_FILE` report of kaniko's metrics fork where the
  meaning matches. `RANIKUST_METRICS_SUMMARY=1` prints a short summary to
  stderr. Unset, nothing is collected and nothing changes; the image is the
  same either way.

### Changed

- Building the static musl binary from source needs zig 0.16.0 and
  cargo-zigbuild 0.24.0, which compile its C dependencies; see
  `CONTRIBUTING.md`.

[Unreleased]: https://github.com/tran4774/ranikust/commits/main
