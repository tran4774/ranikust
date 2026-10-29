---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0012: Accept content and digest divergence between modes

## Context and Problem Statement

The same Dockerfile can be built in overlay mode or in kaniko mode depending on
privileges ([ADR-0002](0002-two-snapshot-modes.md)). The survey first assumed
that only layer digests could differ. The experiments showed that overlay's
copy-up splits hardlinks and copies up files whose only change is metadata, so
flattened content can differ too. Must the two modes produce identical images?

## Considered Options

- Make overlay mode emulate kaniko (re-hash lowers, rewrite hardlink peers).
  Expensive, and diverges from BuildKit, which behaves like overlay.
- Accept the divergence, document it, and test against an explicit list of
  accepted differences.

## Decision Outcome

Layer digests, and in known cases layer content, may differ between overlay
mode and kaniko mode; ranikust accepts this, documents it, and tests the modes
against each other on flattened content with an explicit accepted-differences
list, while requiring identical digests within one mode.

Accepted differences:

- **Hardlink splitting.** Writing in place to a file that is hardlinked to a
  file in a lower layer changes only that name in overlay mode, and every
  link name in kaniko mode. Content and hardlink structure differ.
- **Metadata changes on hardlinked files.** `touch` or `chmod` on one link
  name changes the shared inode, so kaniko mode adds every link name to the
  layer; overlay copies up only the touched name.
- **Link-count-only changes.** `ln /etc/passwd /x` makes overlay copy up
  `/etc/passwd`; kaniko mode leaves it out.
- **Copied-up but unchanged files.** Overlay includes every file that was
  opened for writing, even if content and metadata end up identical (for
  example rewritten with its mtime restored); kaniko mode drops it because
  its hash is unchanged. An optional export-time hash filter may drop them in
  overlay mode too. None occurred on W1, where every rewrite changed mtime.

Within one mode, a parallel build (`--jobs` default) and a sequential build
(`--jobs 1`) must give the same digests.

### Consequences

- Good, because overlay mode needs no emulation layer and behaves like
  BuildKit.
- Bad, because switching modes (for example moving a pipeline from a
  privileged runner to a locked-down pod) can change image content, and the
  shared layer cache ([ADR-0005](0005-own-versioned-cache-key.md)) can mix the
  two behaviours.

## Evidence

- [Experiment 01](../research/experiments/01-overlay-semantics.md): after
  writing `hl1`, its lower-layer peer `hl2` kept the old content in overlay.
- [Experiment 06](../research/experiments/06-overlay-vs-walk.md) §2.5:
  `touch` on every `.json` file gave kaniko mode 1,813 extra files in the pnpm
  store (same inode, new mtime); `sed -i` creates a new inode and caused no
  difference.
- [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md) §3.1:
  three differences after two `RUN`s (`/opt/hl2` content, `/etc/passwd` after
  `ln`, `/` mode 555 vs 755); the last one is an overlay bug that the exporter
  fixes ([overlay-snapshot](../design/overlay-snapshot.md)).
- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md) §3: on W1,
  overlay, Go walk and two kaniko runs differed only in the same 1,654
  non-deterministic files.

## More Information

Test plan: [survey 04](../research/survey/04-risks-and-roadmap.md) §4 item 3
and risk R4. The accepted-differences list lives with the cross-mode test
suite and in the user book.
