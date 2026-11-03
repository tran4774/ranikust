---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0089: Releases are a release pull request and a tag, built and attested by a workflow

## Context and Problem Statement

ranikust has no release: `[workspace.package]` is at version `0.0.0`,
`SECURITY.md` says there is no release yet, and about 700 lines sit under
`Unreleased` in `CHANGELOG.md`. With Phase 2 done, the maintainer wants a
first release and a process for the next ones. The questions: how a release
is made and who may make it, what a release contains and how users can
verify it, which version numbers come when, and how much of it is
automated. The binary itself is decided: one static x86_64 musl binary
built through cargo-zigbuild ([ADR-0009](0009-x86-64-first-aarch64-ready.md),
[ADR-0020](0020-static-musl-via-cargo-zigbuild.md)); the changelog entries
come from the pull requests ([ADR-0088](0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md)).

## Considered Options

Automation:

- **A hand-run release pull request, then a tag that triggers a workflow**
  (chosen). It fits a changelog that is gathered from the pull requests'
  Changelog sections and edited by hand.
- **release-plz or release-please.** Rejected for now: they generate the
  changelog themselves.

Provenance of the files:

- **A GitHub build-provenance attestation plus `SHA256SUMS`** (chosen):
  Sigstore keyless, no key to manage.
- **`cosign sign-blob`**, keyless: the alternative that was weighed.

Also, for 0.0.1: x86_64 only, and no container image yet.

## Decision Outcome

A release is a release pull request followed by a tag the maintainer pushes,
and a workflow builds the static binary from that tag, attests it and
creates a draft pre-release.

Maintainer, 2026-11-03: "OK tạm theo quy trình này" (OK, follow this process
for now).

- **Release pull request.** `chore(release): vX.Y.Z`: the version bump in
  `[workspace.package]` and `Cargo.lock`, and the `CHANGELOG.md` version
  section, gathered from the `## Changelog` sections of the pull requests
  since the last release and edited by hand.
- **Tag.** The maintainer pushes the tag `vX.Y.Z`; a tag ruleset limits
  `v*` tags to the maintainer.
- **Workflow** `.github/workflows/release.yml`, run by the tag:
  - builds from the tag with `--locked` and without any CI cache, which
    avoids cache poisoning;
  - builds the static x86_64 musl binary through cargo-zigbuild
    ([ADR-0020](0020-static-musl-via-cargo-zigbuild.md)) and runs the same
    static-pie checks as `just build-static` (no program interpreter, no
    `NEEDED` entry, type `DYN`, a symbol table, `ranikust version` runs);
  - checks that the tag equals the version in `Cargo.toml`;
  - packs `ranikust-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` and writes
    `SHA256SUMS`;
  - makes a GitHub build-provenance attestation (Sigstore keyless;
    `gh attestation verify` checks it);
  - creates a draft pre-release. The publishing job runs in a GitHub
    environment `release`, which the maintainer approves.
- **Announcement.** A release announcement goes to Discussions.
- **Versions.** 0.0.1 now; 0.1.0 when Phase 3 lands; a patch release
  whenever `main` has user-visible changes and T2 is green. Nothing is
  published to crates.io: every crate has `publish = false`.

Choices for 0.0.1, proposed earlier and accepted with this process:

- **x86_64 only**
  ([ADR-0009](0009-x86-64-first-aarch64-ready.md)). An aarch64 release later
  needs its own ADR.
- **Attestation and `SHA256SUMS` rather than `cosign sign-blob`**: there is
  no key to manage.
- **No container image yet.** Users bind-mount the binary, as the tests do:
  `-v ./ranikust:/ranikust/bin/ranikust:ro`.
- **The 0.0.1 changelog is a condensed, user-facing summary with the known
  limitations,** not the 700 lines under `Unreleased`: they describe fixes
  to code that was never released, and their detail stays in the ADRs and
  in git.

### Consequences

- Good, because a release is built from a tag by a workflow that uses no
  cache and `--locked`, so the binary does not depend on a developer's
  machine or on a cached build directory.
- Good, because a user can check a downloaded file against `SHA256SUMS` and
  against its attestation with `gh attestation verify`, and the maintainer
  manages no signing key.
- Good, because nothing is published without the maintainer pushing the tag
  and approving the `release` environment.
- Bad, because the version bump and the changelog section are done by hand
  in every release pull request.
- Bad, because a user who wants a container image builds one: there is none
  to pull.
- Bad, because only x86_64 is released while aarch64 is kept buildable.
- Neutral, because an attestation says where and from which tag a file was
  built, not that the code is free of vulnerabilities [external].

## Evidence

- [code] `just build-static` and the CI job `static-build` already build
  the binary with `cargo zigbuild --release --locked` and run the
  static-pie checks listed above; the release workflow reuses them.
- [code] `[workspace.package]` is at `0.0.0`, and every crate has
  `publish = false`; `CHANGELOG.md` has 707 lines, almost all under
  `Unreleased`; `SECURITY.md` says there is no release yet.
- [external] GitHub build-provenance attestations are signed through
  Sigstore without a long-lived key, and `gh attestation verify` checks
  them.
- No measurement was needed: the decision is a process.

## More Information

- Builds on [ADR-0088](0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md)
  (the changelog sections in the pull requests) and
  [ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
  (agents ask before creating tags or releases, and a tag ruleset limits
  `v*` to the maintainer).
- Follow-ups that land with the implementation, not with this ADR:
  `.github/workflows/release.yml`, the `release` environment and the tag
  ruleset (repository settings, the maintainer's), the 0.0.1 changelog and
  release pull request, and the docs the first release changes
  (`SECURITY.md`'s "Supported versions", the README, the book).
- release-plz and release-please are rejected for now, not for good; an
  aarch64 release needs its own ADR.
