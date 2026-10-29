---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0015: Hand-written Dockerfile parser and `.dockerignore` matcher

## Context and Problem Statement

kaniko does not parse Dockerfiles itself: it uses BuildKit's Go parser,
instruction set and shell lexer, and moby's `patternmatcher` for
`.dockerignore`. ranikust must reproduce those semantics exactly
(heredocs, escapes, variable expansion, `ONBUILD`, ignore exceptions). Is
there a Rust crate good enough to build on?

## Considered Options

- `parse-dockerfile` 0.1.8 plus glue code.
- A fork of `parse-dockerfile` with the heredoc and escape bugs fixed.
- Other crates: `dockerfile-parser` (archived, no heredoc),
  `dockerfile-parser-rs` (no `ONBUILD`, `HEALTHCHECK`, `MAINTAINER`),
  `containerfile` (new, single release).
- A hand-written port of BuildKit's `parser`, `instructions` and
  `shell/lex.go`; the `ignore` crate or a port of `patternmatcher` for
  `.dockerignore`.

## Decision Outcome

`rnk-dockerfile` is a hand-written port of BuildKit's parser, instruction set
and shell lexer (about 3,000–3,300 lines of Rust), and `rnk-ignore` is a
hand-written port of moby's `patternmatcher` (about 350–400 lines).

- Reference versions: BuildKit v0.34.0 and `patternmatcher` v0.6.1, the
  versions vendored by kaniko.
- `.dockerignore` follows `MatchesOrParentMatches`, as BuildKit and the
  docker CLI do, not kaniko's deprecated `Matches`.
- The experiment's Go golden harness becomes the compatibility test: parse
  with BuildKit, normalize to JSON, compare. Reused vectors: BuildKit's parser
  test files, `shell/envVarTest` (238 cases), `shell/wordsTest`, and the
  38 `.dockerignore` cases.

### Consequences

- Good, because one data model follows BuildKit exactly, including heredoc
  names and positions that crates discard.
- Good, because the shell lexer is needed for variable expansion anyway; no
  crate offers it.
- Bad, because about 3,000 lines must be written and kept in step with
  BuildKit.

## Evidence

- [Experiment 04](../research/experiments/04-dockerfile-parser.md):
  `parse-dockerfile` fails on heredocs in the middle of a command
  (`RUN cat <<EOF > /out`, the most common form) and does not expand
  variables; with about 125 lines of glue it matched 198/200 valid kaniko
  integration Dockerfiles and 232/242 files overall (95.9 %). BuildKit's
  `shell/lex.go` has 553 lines of code. The `ignore` crate differed from
  `patternmatcher` in 19/38 cases used directly and 5/38 after pattern
  translation; the rest cannot be fixed by translation. No Rust port of
  `patternmatcher` exists on crates.io.
- [Survey 03](../research/survey/03-rust-architecture.md) §3.

## More Information

A fork of `parse-dockerfile` would save 800–1,000 lines but leave a
diverging model to maintain; rejected. kaniko expands variables with the `\`
escape character even when the `escape` parser directive selects a backtick
(`kaniko@44aa7ddc0 pkg/util/command_util.go:66`, also lines 84 and 105)
[code, effect not tested]; ranikust
follows BuildKit unless a compatibility test shows users depend on kaniko's
behaviour [inferred].
