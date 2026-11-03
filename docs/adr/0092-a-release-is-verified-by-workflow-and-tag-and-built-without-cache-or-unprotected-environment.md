---
status: proposed
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0092: A release is verified by its workflow and tag, and built without a cache or an unprotected environment

## Context and Problem Statement

[ADR-0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md)
decided how a release is made: a tag the maintainer pushes, a workflow that
builds from it without CI caches and attests the tarball, and a `release`
environment the maintainer approves. The review of its implementation
(pull request #42) found four gaps that the ADR does not settle:

- `gh attestation verify <file> --repo tran4774/ranikust` accepts an
  attestation made by **any** workflow of the repository on **any** ref. A
  branch or pull request of the same repository running a modified
  workflow with `id-token: write` and `attestations: write` could attest any
  file, and that file would pass the documented check: the tag ruleset and
  the environment would not matter.
- `mlugg/setup-zig`, which CI uses, restores the Zig archive from the
  Actions cache whatever its `use-cache` input says (`use-cache` covers only
  Zig's own cache directory), and does not check a restored archive again
  [code: `main.js` `retrieveTarball` at `d1434d0`]. A tag run can read the
  caches `main` wrote.
- A job that names an environment which does not exist creates it, with no
  protection [external]: a first tag pushed before the maintainer set up
  `release` would publish without approval.
- The built files were kept 7 days, while a deployment may wait for
  approval for 30.

## Considered Options

Verification:

- **Pin the signer workflow and the source ref** (chosen):
  `--signer-workflow tran4774/ranikust/.github/workflows/release.yml
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners`, with GitHub CLI
  2.102.0 or later.
- `--repo` alone: rejected, for the reason above.
- A signing key of our own (`cosign sign-blob` with a key): rejected as in
  ADR-0089, a key to manage.

Zig in the release build:

- **Download Zig from ziglang.org and check a pinned SHA-256** (chosen).
- Keep `mlugg/setup-zig` with `use-cache: false`: rejected, it still reads
  the Actions cache.

The environment:

- **The workflow checks the environment and stops a tag when it is missing
  or unprotected** (chosen), as well as documenting the set-up.
- Documentation only: rejected, a forgotten step would publish silently.

## Decision Outcome

Users verify a release by the workflow and the tag that built it, and the
release workflow uses no cache and refuses to go on without a protected
`release` environment.

- **Verification instructions** (README, the book's Installing page, the
  tarball's `INSTALL` note) give
  `gh attestation verify <tarball> --repo tran4774/ranikust
  --signer-workflow tran4774/ranikust/.github/workflows/release.yml
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners` and require
  GitHub CLI 2.102.0 or later: 2.51.0 added `--signer-workflow` and 2.68.0
  `--source-ref`, but before 2.102.0 the first was matched against the
  beginning of the certificate's identity only and the second without
  regard to case [external: gh release notes].
- **The attesting job** runs only for a pushed tag `v*`: the workflow's
  only push trigger is `tags: ["v*"]`, and the job's condition repeats it.
- **Zig** is downloaded from `ziglang.org` and checked against a SHA-256
  pinned in the workflow (0.16.0: `70e49664…3d00`, the value of
  ziglang.org's `index.json`, checked against the downloaded archive).
  `cargo-zigbuild` still comes from `taiki-e/install-action`, which uses no
  cache and checks the SHA-256 of what it downloads [code].
- **The environment.** Before the first tag, the maintainer creates
  `release` with the maintainer as required reviewer and deployments limited to
  the tag rule `v*`. `scripts/release.py check-environment` reads it
  through the API; the build job runs it for a tag before anything is
  built or attested, and the publishing job again before it publishes; a
  pull request's dry run reports what it finds.
- **Before any build or approval**, a tag run also checks that
  `CHANGELOG.md` has a finished section for the version, and the built
  binary must report that version.
- **The built files are kept 30 days**, as long as an approval may wait.

### Consequences

- Good, because a file attested by anything but the release workflow run
  for that tag fails the documented check.
- Good, because no cache entry, whoever wrote it, reaches a release build.
- Good, because a missing or unprotected environment stops a tag before
  anything is attested, instead of publishing without approval.
- Bad, because users need GitHub CLI 2.102.0 (September 2026) or later, and
  a longer command.
- Bad, because the Zig version and checksum live in two places
  (`ci.yml`'s `setup-zig` and `release.yml`'s download) and must change
  together.
- Neutral, because the environment check needs the `actions: read`
  permission in two jobs.

## Evidence

- [code] `mlugg/setup-zig@d1434d0` `main.js`: `retrieveTarball` calls
  `cache.restoreCache` for the archive unconditionally; `use-cache` guards
  only the global Zig cache directory.
- [external] gh 2.102.0 release notes: `--signer-workflow` was matched
  against the start of the certificate identity only, and `--source-ref`
  case-insensitively, before that release; 2.68.0 added `--source-ref`,
  2.51.0 `--signer-workflow`.
- [external] GitHub documentation: a workflow job that references a
  non-existent environment creates it, without protection rules.
- [measured] Zig 0.16.0 x86_64-linux: the archive downloaded on 2026-11-03
  has SHA-256 `70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00`,
  the value in ziglang.org's `index.json`.
- [code] `tests/release/test_release.py`: `check-environment` refuses a
  missing environment, one without a required reviewer and one deploying
  from anything but the tag rule `v*`.

## More Information

- Refines [ADR-0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md),
  whose follow-ups name the environment and the tag ruleset; it does not
  change its decision.
- Not decided here, for the maintainer: bundling the third-party licence
  notices of the statically linked dependencies with the binary, and
  GitHub's immutable releases setting.
- Revisit when GitHub CLI or the attestation format changes how the signer
  is matched, or when an aarch64 release is added (its own ADR, ADR-0009).
