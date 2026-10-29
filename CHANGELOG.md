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

[Unreleased]: https://github.com/tran4774/ranikust/commits/main
