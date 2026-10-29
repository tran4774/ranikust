---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0027: Build-context filtering follows BuildKit's walk, not `MatchesOrParentMatches`

## Context and Problem Statement

[ADR-0015](0015-hand-written-dockerfile-parser.md) says `.dockerignore`
follows `MatchesOrParentMatches`. Porting moby `patternmatcher` showed that
BuildKit decides which context files it sends with a different routine: its
filtered walk (`fsutil`'s `filter.go`) uses `MatchesUsingParentResults` and
prunes directories that no exception can re-include. The two disagree: with
the patterns `dir/f` and `!dir`, the walk excludes `dir/f` and
`MatchesOrParentMatches` keeps it (checked against the Go code). Which one
does `rnk-context` follow? This was open question Q-050.

## Considered Options

- BuildKit's walk: `MatchesUsingParentResults` plus the pruning rule.
- `MatchesOrParentMatches`, as ADR-0015 states.

## Decision Outcome

`rnk-context` filters the build context exactly as BuildKit's walk does:
`rnk_ignore::Matcher::matches_using_parent` for every entry, carrying the
parent's `MatchInfo`, and `may_reinclude_below` to decide whether an
excluded directory is descended into.

- `matches_or_parent_matches` stays in `rnk-ignore` as an exact port, for
  callers that ask about a single path without walking.
- This supersedes the `MatchesOrParentMatches` bullet of ADR-0015; the rest
  of ADR-0015 stands.

### Consequences

- Good, because the files ranikust sends match what `docker build` and
  BuildKit send, which is what users test against.
- Bad, because the context walker must carry match state from parent to
  child instead of asking per path.
- Neutral, because kaniko uses a third behaviour (the deprecated `Matches`);
  differences from kaniko are listed in `docs/agents/kaniko-map.md`.

## Evidence

- [measured] `crates/rnk-ignore` golden tests: 641 runs of BuildKit's real
  filtered walk over a 51-entry tree, generated from the Go code, match the
  Rust port.
- [code] `tonistiigi/fsutil` `filter.go` at commit `83cac42c1c52` (the
  version BuildKit v0.34.0 uses): the walk calls
  `MatchesUsingParentResults` and prunes directories.

## More Information

- Supersedes the `.dockerignore` matching bullet of
  [ADR-0015](0015-hand-written-dockerfile-parser.md).
- Answers Q-050 in [`docs/open-questions.md`](../open-questions.md#answered).
- Implemented by `rnk-ignore` (merged 2026-10-30); `rnk-context`
  (Phase 1 task 10) applies it.
