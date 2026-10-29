# rnk-ignore

> **Status:** done · **Phase:** 1 ·
> **Depends on:** `regex`, `thiserror` (pure data) · **Used by:** `rnk-builder` (`rnk-context`, `rnk-cache` [open: Q-028]) · **Path:** `crates/rnk-ignore/`

**TL;DR:** A port of moby `patternmatcher` v0.6.1 (as BuildKit v0.34.0 uses
it, built with Go 1.26): is this context path excluded by `.dockerignore`?
Not gitignore, and not kaniko's deprecated `Matches`. Go's own behaviour
(`Clean`, `filepath.Match`, `regexp` syntax, Unicode tables) is ported too.

## Owns

- Parsing `.dockerignore` contents (BOM, comments, trimming, `Clean`, leading
  `/`, the 64 KiB line limit), as `ignorefile.ReadAll` does.
- Deciding whether a context path is excluded, with exactly patternmatcher's
  semantics: pattern syntax, `!` exceptions and parent-directory matches,
  both `MatchesOrParentMatches` and the walk form `MatchesUsingParentResults`.
- BuildKit's rule for skipping an excluded directory in a walk (`fsutil`).

## Must not

- Read or walk the filesystem: callers walk and ask (`rnk-context`,
  `rnk-builder`).
- Approximate: any divergence from `patternmatcher` is a bug; no `ignore`,
  `globset` or `regex` syntax underneath (Go regexps are translated).
- Choose the ignore file (`<Dockerfile>.dockerignore` before `.dockerignore`):
  `rnk-context` does.

## API

Strings are bytes as in Go; `&str`, `String` and `&[u8]` are accepted
(`AsRef<[u8]>`) [open: Q-010].

```rust
pub fn read_patterns(contents: &[u8]) -> Result<Vec<Vec<u8>>, Error>;   // ignorefile.ReadAll (ADR-0015)
pub struct Matcher;                                                     // PatternMatcher, compiled in new()
impl Matcher {
    pub fn new<P: AsRef<[u8]>>(patterns: &[P]) -> Result<Matcher, Error>;
    pub fn matches_or_parent_matches(&self, path: impl AsRef<[u8]>) -> Result<bool, Error>;
    pub fn matches_using_parent(&self, path: impl AsRef<[u8]>, parent: &MatchInfo)
        -> Result<(bool, MatchInfo), Error>;                            // BuildKit's walk [open: Q-050]
    pub fn has_exclusions(&self) -> bool;
    pub fn may_reinclude_below(&self, dir: impl AsRef<[u8]>) -> bool;  // false: a walk may skip this excluded dir (fsutil)
}
pub struct MatchInfo;  // Default = "no parent information"
pub enum Error { LineTooLong { line }, IllegalExclusion, BadPattern { pattern }, MatchInfoMismatch }
```

Matching returns `Result` because Go validates patterns only partly in
`New`: a pattern can fail to compile when first evaluated (`a*[`), and one
never evaluated never fails.

## Invariants

- Same answer, including errors, as `PatternMatcher` v0.6.1 for every
  (patterns, path) pair, and the same reported paths as BuildKit's
  filtered walk, except the surrogate case in Pitfalls.
- No panic on any pattern or path; no I/O; no state between calls.
- Paths are relative and `/`-separated; methods do not clean them (Go's do
  not either). `.` (the context root) is never asked about (kaniko
  `fs_util.go:1236-1239`, Go's package-level helpers).

## Refs

- **Design:** — (no design doc; ARCHITECTURE [rnk-ignore](../../../ARCHITECTURE.md#rnk-ignore))
- **Decisions:** [ADR-0015](../../adr/0015-hand-written-dockerfile-parser.md) (port v0.6.1; `MatchesOrParentMatches`, not kaniko's `Matches`)
- **Evidence:** [exp-04](../../research/experiments/04-dockerfile-parser.md#dockerignore): the `ignore` crate differs in 19/38
  cases, 5/38 after translation; kaniko's `Matches` in 7/1,596 cells.
- **kaniko:** `kaniko@44aa7ddc0 pkg/util/fs_util.go:1195-1204` (ignore file
  lookup), `:1207-1219` (`ReadAll`), `:1223-1254` (`ExcludesFile`, deprecated
  `Matches` at `:1241` and `:1248`)
- **External:** patternmatcher v0.6.1 [patternmatcher.go](https://github.com/moby/patternmatcher/blob/v0.6.1/patternmatcher.go), [ignorefile.go](https://github.com/moby/patternmatcher/blob/v0.6.1/ignorefile/ignorefile.go#L24), [patternmatcher_test.go](https://github.com/moby/patternmatcher/blob/v0.6.1/patternmatcher_test.go); fsutil [filter.go](https://github.com/tonistiigi/fsutil/blob/83cac42c1c52/filter.go#L259) (walk); Go 1.26 `path/filepath/match.go`, `regexp/syntax/parse.go`; [Docker docs](https://docs.docker.com/build/concepts/context/#dockerignore-files)

## Tests

- T0 unit: upstream `patternmatcher_test.go` and `ignorefile_test.go`
  tables, Go's `Clean`/`Dir`/`Match` tables, the regexp translator.
- T0 golden (`tests/golden/`, written by `gen/gen.sh`: Go 1.26.9 in podman):
  exp-04's 38 cases × 42 paths, kaniko's three `.dockerignore` files, 1,200
  random pattern sets (12,470 cells, `MatchesOrParentMatches` and the walk
  chain, errors included); 645 `ReadAll` inputs; 641 BuildKit (`fsutil`)
  walks of a 51-entry tree. `GEN_SCALE=40` ran 434,660 cells once: only the
  surrogate case below differed.
- T0 property (`tests/props.rs`): no panic; `p` then `!p` re-includes;
  `./p`, `/p` equal `p`; walk equals parent check without exceptions;
  agreement with a direct glob interpreter for `* ? ** / .` patterns.

## Metrics

- None: matching is per path, so a span would be per file. Its cost is
  charged to `rnk_context.ignore` ([rows](../../design/metrics.md#rnk-ignore)).
- No `tracing` dependency needed.

## Pitfalls

- gitignore is not dockerignore: a pattern without `/` matches only at the
  root, so `*.txt` keeps `sub/a.txt` ([exp-04](../../research/experiments/04-dockerfile-parser.md#dockerignore)).
- `*/` cleans to `*` and also matches files; `!*/` after `*` becomes `!*` and
  re-includes everything (exp-04).
- A child can be re-included below an excluded parent (`docs` +
  `!docs/keep.md`): a walker asks `may_reinclude_below` before pruning.
- BuildKit's walk (`matches_using_parent`) and `matches_or_parent_matches`
  disagree for `dir/f` + `!dir` on `dir/f` (walk: excluded) [open: Q-050].
- `[!b]` is a class containing `!`; only `[^b]` negates (exp-04).
- `|`, `{` and `}` reach Go's regexp unescaped (its escape bitmap overflows):
  `a|b*` is an alternation, `a{2}*` a repetition, `x{*` a literal `{`.
- `filepath.Match` validates only up to the first failing chunk, so `a*[`
  passes `new` and fails when matched.
- A leading BOM is skipped in a pattern's regexp but not in exact, prefix
  or suffix matches, which compare the cleaned bytes (Go slices `**` off
  by bytes).
- `\pL`, `\p{Greek}` use Go's Unicode 15.0 tables (`src/unicode_tables.rs`,
  generated), not the `regex` crate's.
- Known divergence: `\x{D800}`–`\x{DFFF}` never matches; Go matches U+FFFD's
  bytes when it is in a one-pass regexp's literal prefix (needs Go's
  one-pass analysis to port).
- `ReadAll` tests for `#` before trimming (an indented `#` is a pattern),
  strips a BOM on the first line only, then cleans and drops one leading `/`.
- kaniko's `Matches` does not exclude `foo/a.txt` for `**/foo`
  (`fs_util.go:1241`); ranikust follows BuildKit (ADR-0015).

## Phase 1

- Done: `read_patterns`, `Matcher` with both match forms, the walk pruning
  rule, upstream tests, Go goldens, property tests.
- Next, in `rnk-context` (task 10): walk the context with
  `matches_using_parent` and `may_reinclude_below`, after Q-050.
