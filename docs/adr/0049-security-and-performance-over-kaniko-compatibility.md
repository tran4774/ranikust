---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0049: Security and performance come before kaniko compatibility; flags that change nothing are refused, kaniko's variables are not read

## Context and Problem Statement

ranikust was designed as a drop-in for kaniko: its flags with their names
and meanings (ADR-0011), kaniko flags that change nothing accepted and
ignored with a warning, kaniko's `KANIKO_*` variables read or named in a
warning (ADR-0036), and BuildKit's behaviour followed wherever an
observable result was undecided (ADR-0032). Compatibility arguments then
started to steer open questions toward weaker or slower choices. On
2026-10-31 the maintainer set the priority: security first, then
performance, with setup kept short; compatibility with kaniko pipelines is
not a goal. The open questions were re-ranked that way and the maintainer
answered several of them.

## Considered Options

- Keep drop-in compatibility as a goal, weighed against security and
  performance case by case.
- Drop it as a goal: rank by security, then performance, keep setup short;
  parity with kaniko or BuildKit only breaks ties.

## Decision Outcome

Chosen: compatibility is not a goal. Kaniko's flags stay where their
meaning is what ranikust needs, under the names they already have; nothing
is accepted only to be ignored.

- **Flags (supersedes ADR-0011's goal and ADR-0036's "accepted, ignored
  with a warning" class).** A kaniko flag that changes nothing in ranikust
  is refused with its reason, like the flags of later phases: `--cache-dir`,
  `--use-new-run`, `--cleanup`, `--pre-cleanup`, `--image-fs-extract-retry`,
  `--force-build-metadata`, `--skip-unused-stages`,
  `--compressed-caching=false`, `--no-push-cache`. `--cache=false` stays
  accepted: it asks for what ranikust does. Flags already accepted keep
  their names; nothing is renamed for its own sake.
- **Variables (supersedes ADR-0036's variable class).** `KANIKO_*` and
  `FF_KANIKO_*` variables are neither read nor named in a warning:
  `KANIKO_NO_PUSH` no longer sets `--no-push`, and `KANIKO_REGISTRY_MIRROR`
  or `KANIKO_REGISTRY_MAP` no longer stop the run (with nothing reading
  them, images still come from the registry they name). `DOCKER_AUTH_CONFIG`
  stays: it passes registry credentials in CI without a file.
- **BuildKit (supersedes ADR-0032's rule in part).** BuildKit's behaviour
  decides only between options equal on security and performance. The
  answers already given under ADR-0032 stand.
- **Q-008:** one `--target`. A repeated flag or a comma-separated list is a
  usage error (exit 2): kaniko's list builds the stages after the first
  and throws them away. The stage graph keeps its list type; the CLI
  passes at most one.
- **Q-010:** `rnk-ignore` matches bytes, as built: a file whose name is not
  UTF-8 still matches an exclude.
- **Q-047:** semantics before names: a registry mirror or mapped registry
  never receives another registry's credentials and never serves a push;
  the flags are named after that is built (Phase 5), kaniko's names only
  where the meaning is exactly that.
- **Q-048:** no `login` subcommand: it would write credentials to disk,
  which a tool reading existing ones does not need. What `push` takes stays
  open (Q-048).
- **Q-260:** overlay mode refuses `--ignore-path`, `--single-snapshot` and
  `--ignore-var-run=false` for good: its layers hold only what `RUN` wrote,
  never container mounts or `/var/run`, and squashing costs a full
  re-encode. Kaniko mode keeps `--ignore-path` and `--ignore-var-run=false`
  while a case needs them, and `--single-snapshot` only if measured faster.

Not decided here: Q-016 (Phase 6), Q-040 (Phase 3), Q-251 and ADR-0035
(Phase 5), Q-110, Q-162 and ADR-0022 (waiting for the maintainer).
ADR-0010 (`RUN` on the host network) and ADR-0023 (`--snapshot-mode`)
stand; see their notes in the threat model and the `rnk-snapshot` card.

### Consequences

- Good, because no flag or variable is silently dropped: a pipeline that
  passes `--cleanup` or `KANIKO_NO_PUSH` learns at once that ranikust does
  not do what it asked (the variable by its absence of effect, documented).
- Good, because questions are settled on what protects the host and the
  images, and on what is fast.
- Bad, because a kaniko pipeline needs edits to move: drop the refused
  flags, give one `--target`, replace `KANIKO_NO_PUSH` by `--no-push`.
- Neutral, because a pipeline that sets `KANIKO_REGISTRY_MIRROR` pulls from
  the original registry instead of stopping.

## Evidence

- The maintainer's answers of 2026-10-31 to the re-ranked open questions
  (`docs/open-questions.md`, commit `9304237`).

## More Information

Supersedes the drop-in goal of [ADR-0011](0011-accept-common-kaniko-flags.md),
the ignored-flag and variable classes of
[ADR-0036](0036-kaniko-flags-in-phase-1.md), and the general rule of
[ADR-0032](0032-observable-behaviour-follows-buildkit.md) (its answers
stand). Answers Q-008, Q-010, Q-047 and Q-260, and the `login` part of
Q-048.
