---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0052: `push` takes an OCI layout, kaniko's old spellings are refused, and four fixes from the issue mining

## Context and Problem Statement

After ADR-0049 several questions were left, and mining the issues of
kaniko, BuildKit and buildah (outside the repository, 2026-10-31) found
four behaviours to settle. The maintainer answered them together.

## Considered Options

Per item below; each lists the option chosen.

## Decision Outcome

Chosen as listed; each item says when it is built.

- **Q-162 (C1).** A built layer is published in the persistent store under
  the digest computed while it is written, for later builds and the cache:
  Phase 4, overlay mode and kaniko mode on a host only (kaniko mode on `/`
  keeps no state between runs, ADR-0051).
- **Q-245 (C2).** Measure first: the time to mount and unmount per
  instruction on W1–W3; one mount for consecutive instructions is built
  only if that is a visible share of a build. The question stays open
  until measured.
- **Q-048 (C3).** `push` takes an OCI layout only, what `--oci-layout-path`
  writes; no docker-save tar. No phase yet.
- **kaniko's old spellings (C4).** `--tarPath`, `--snapshotMode`,
  `--customPlatform`, `--whitelist-var-run` and `--custom-platform` are
  refused, each naming the flag to use; one name per option.
- **A build on `/` inherits the previous one (D1).** A ranikust build in a
  container leaves a marker under `/ranikust` (on the ignore list); a
  later build that finds it empties `/` (the ignore list kept) before its
  first base. Without the marker `/` is extracted over, as kaniko, so the
  container's own tools stay.
- **`COPY` (D2).** A source that expands to an empty string is refused,
  instead of copying the whole context. `COPY --from=<name>` where `<name>`
  is no stage and a bare word (no tag, digest, registry or path, such as a
  mistyped stage name) is refused instead of pulling an image of that
  name; `--from=nginx:latest` still copies from an image.
- **`COPY --chmod` on directories (D3).** Kept as BuildKit: an octal mode
  applies to copied and created directories too; the book says so.
- **`/var/run` (D4).** Ignored only when it is a mount point; the image's
  `/var/run -> /run` symlink is kept (alpine 3.12 and later).

### Consequences

- Good, because the open items left by ADR-0049 are settled or scheduled.
- Bad, because pipelines using kaniko's camel-case flags must rename them.

## Evidence

- Issue mining, `issue-mining/reports/summary.md` beside the repository
  (cards `kaniko/3310`, `buildkit/7261`, `buildkit/7091`,
  `buildkit/5943`, `kaniko/1297`) [external].

## More Information

D1, D2 and D4 are built with Phase 2 tasks 7 and 8 (`docs/agents/phase-2.md`);
C4 with ADR-0050's branch. Answers Q-162 and Q-048.
