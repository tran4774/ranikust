# Experiment 04: Dockerfile parser

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Can existing Rust crates parse Dockerfiles and match `.dockerignore` patterns the way BuildKit does, or must ranikust write its own? |
| **Answer** | Neither is usable as is. `parse-dockerfile` 0.1.8 matches BuildKit on 198/200 valid kaniko test Dockerfiles only after about 125 lines of glue, and fails on heredocs in the middle of a `RUN`; the `ignore` crate still disagrees on 5/38 `.dockerignore` cases after pattern translation. Recommendation: port BuildKit's parser (≈ 3,000–3,300 lines of Rust) and `moby/patternmatcher` (350–400 lines). |
| **Related** | [ADR-0015](../../adr/0015-hand-written-dockerfile-parser.md); [survey chapter 03](../survey/03-rust-architecture.md) |
| **Original** | `docs-rust/poc/04-dockerfile-parser/` (Vietnamese, not in this repository) |

## Summary

- `parse-dockerfile` 0.1.8 **cannot be used directly**. It parses most files
  correctly, but **fails on heredocs in the middle of a `RUN` command**
  (`RUN cat <<EOF > /out`, `RUN python3 <<EOF`), the most common heredoc
  form.
- With about 125 lines of glue, it matches BuildKit on **198/200 valid files
  (99%)** of kaniko's integration Dockerfiles. Both remaining files fail
  because of mid-command heredocs.
- The crate **does not expand variables**. That code has to be written
  whether or not the crate is used.
- **Recommendation: write the parser** by porting BuildKit's `parser`,
  `instructions` and `shell/lex.go`, about **3,000–3,300 lines of Rust**,
  and keep this experiment's harness as a compatibility test. This is
  slightly above the 2,000–3,000-line estimate in survey chapter 03.
- `.dockerignore`: the assumption holds. Used directly, the `ignore` crate
  disagrees on **19/38 cases**; with a pattern-translation layer, still on
  **5/38**, and the rest cannot be fixed by translating patterns. **No Rust
  crate ports patternmatcher.** A port is about 350–400 lines of Rust.

## Questions

The experiment tested two assumptions from
[survey chapter 03](../survey/03-rust-architecture.md) §3 (section numbers
refer to the original chapter):

1. The `parse-dockerfile` crate (taiki-e) is compatible enough with BuildKit
   to serve as the parser.
2. `.dockerignore` has to be ported from `moby/patternmatcher`, because the
   `ignore`/`globset` crates have different semantics.

It also asked:

3. How much code would a port take, including variable expansion?
4. Which other Rust crates exist, and how do they fare on the same files?

## Environment

- Run on 2026-10-29 on the development machine (see the
  [research README](../README.md#machine)), with podman; neither Go nor Rust
  was installed on the host.
- No timings were taken, so CPU limits, cache state and load do not apply.

Table 1. Versions.

| Component | Version | Notes |
|---|---|---|
| BuildKit (the reference) | `github.com/moby/buildkit v0.34.0` | The version in kaniko's `go.mod`. `diff` against `kaniko/vendor`: `parser/`, `instructions/`, `shell/` are identical |
| `moby/patternmatcher` | v0.6.1 | Identical to kaniko's vendored copy |
| Go | 1.26.9 (`golang` image) | The host's Go 1.21 is too old for BuildKit v0.34 |
| Rust | 1.99.1 (`rust:1-bookworm` image) | |
| `parse-dockerfile` | **0.1.8** (latest, 2026-07-10) | |
| `ignore` | 0.4.34 (2026-10-27) | |

## Method

**Dockerfiles.** A Go program parses each file with BuildKit
(`parser.Parse` + `instructions.ParseInstruction`) and writes a normalised
JSON "golden" file, without expanding variables. A Rust program parses the
same file with `parse-dockerfile` and writes the same JSON shape. A Python
script compares the two and classifies the differences. The normalised JSON
holds:

- the escape character;
- the `ARG`s before the first `FROM`;
- the stages: base, name, flags;
- per instruction: the command, flags as `name=value`, `shell`/`exec` form,
  args, heredocs (content, `expand`), and the child command of `ONBUILD`.

Heredoc file names in `COPY` are **not** compared, because the crate does
not keep them (see "What the crate loses").

Two Rust-side variants:

- **minimal glue**: split key/value pairs for `ENV`/`LABEL`/`ARG`. The crate
  returns a raw string, so this is mandatory. It ports BuildKit's
  `parseWords` + `parseNameVal`, about 100 lines. Stage names are also
  lower-cased (1 line).
- **full glue** (`RK_GLUE=1`): three more steps. Strip `\`+newline
  sequences and comment lines inside shell-form commands (together about
  23 lines), and sort `EXPOSE` ports (1 line).

Four file sets:

| Set | Files | Content |
|---|---|---|
| `kaniko/integration/dockerfiles` | 201 | kaniko's integration test Dockerfiles |
| `testdata/extra` | 14 | Written for this experiment: heredocs, escapes, JSON form, BOM/CRLF… |
| `testdata/invalid` | 13 | Syntax errors, to see whether the crate rejects what BuildKit rejects |
| BuildKit `parser/testfiles*` | 37 | BuildKit's own parser test files |

A third Rust program only counts how many files three other parser crates
accept, without comparing content.

**`.dockerignore`.** 38 pattern cases × 42 paths = 1,596 cells. The paths
include files and directories, `.git`, nested `node_modules`, and names with
spaces and `[`. The reference is `PatternMatcher.MatchesOrParentMatches`,
the function BuildKit and the docker CLI use. The `ignore` crate was run as
a tree walk: a path is excluded if it, or a parent directory, is ignored.
Two comparisons: the `.dockerignore` lines used directly, and with a
translation layer (normalise like `ignorefile.ReadAll`, then prefix `/` to
anchor at the root).

## Results

All results are **[measured]** unless labelled otherwise.

### Parse results

Table 2. Files accepted and files matching the BuildKit golden output.

| Set | Files | Go OK | Rust OK | Match (minimal glue) | Match (full glue) |
|---|---|---|---|---|---|
| `kaniko/integration/dockerfiles` | 201 | 200 | 198 | 172 | **198** |
| `testdata/extra` (written here) | 14 | 14 | 11 | 4 | 8 |
| `testdata/invalid` | 13 | 3 | 5 | 3 (+8 rejected by both) | 3 (+8 rejected by both) |
| BuildKit `parser/testfiles*` | 37 | 25 | 26 | 16 | 23 |

- The file Go rejects in the kaniko set is
  `Dockerfile_dockerignore_relative.dockerignore`. It is not a Dockerfile,
  so Rust rejects it too.
- Over every file BuildKit accepts (200 + 14 + 3 + 25 = 242), full glue
  gives **232/242 matches (95.9%)**.
- The BuildKit set has 9 files without `FROM`, which are parser-only tests.
  Both sides reject them, so they carry no information.

Table 3. Classes of difference.

| Class | Files | Detail | Fixable by glue? |
|---|---|---|---|
| Line continuation in shell-form commands | 26 (kaniko), 7 (BuildKit) | The crate returns the text verbatim, with `\`+newline and interior comment lines. BuildKit joins the lines and drops the comments | Yes, about 23 lines |
| `EXPOSE` not sorted | 1 (extra), 2 (BuildKit) | BuildKit sorts the port list | Yes, 1 line |
| **Heredoc in the middle of `RUN`** | 2 (kaniko: `issue_1713`, `mz824`), 2 (extra) | The crate accepts a heredoc only when `<<` starts the arguments (its code has `// TODO: here-doc is not always start from first`). `RUN cat <<EOF > f`, `RUN python3 <<EOF` and `RUN <<A … && cat <<B` all fail: the body is read as instructions | **No.** The crate must be fixed; finding `<<` needs a shell lexer to split words |
| Heredoc: the `<<EOF` token is lost from the command | 3 (extra) | `RUN <<EOF bash` gives args `"bash"`; BuildKit keeps `"<<EOF bash"`. The crate also loses the delimiter name, so the shell command cannot be rebuilt | Partly. A fake delimiter can be inserted, but its original position is unknown |
| `\\` at end of line treated as a continuation | 1 (BuildKit `trailing-backslash`) | `ENV C trailing\\backslash\\` swallows the following `ENV D …` line, giving 6 instructions instead of 7. **A genuine crate bug** | No |
| `\` at end of file | 1 (BuildKit `continue-at-eof`) | The crate keeps the trailing `\` | Yes (the current glue does not do it) |
| `ENTRYPOINT []` | 1 (extra) | BuildKit accepts it (to reset the entrypoint); the crate reports an error | No |
| Crate more lenient | 3 | Does not check flag names (`COPY --foo`, `--user`) or stage names (`AS 1bad`) | Yes; must be done in the instructions layer anyway |

These groups **match completely**, at least once glue is applied:

- `# escape=` + backtick;
- a misplaced directive (correctly not treated as a directive);
- BOM + CRLF;
- comments between continuation lines;
- blank lines between continuation lines;
- JSON form, including `[ ]`, a trailing comma, single quotes (which fall
  back to shell form), and `A`;
- `--mount` + exec form;
- nested `ONBUILD` (the crate parses it into a tree; BuildKit keeps a string
  that the golden program re-parses);
- `HEALTHCHECK NONE`/`CMD`;
- legacy `ENV k v w`;
- quoted values;
- `<<-EOF` (tabs stripped);
- a quoted delimiter (`expand=false`);
- `COPY <<F1 <<F2 /dest/`;
- `ADD --checksum --keep-git-dir --unpack`.

### Variable expansion (**[code]**; no Rust written)

- `parse-dockerfile` has **no** expansion API; all 3,038 lines of its
  `lib.rs` are parser (`parse-dockerfile@0.1.8 src/lib.rs`).
  `FROM ubuntu:${V}` and `COPY --chown=$U` come back verbatim. The BuildKit
  golden does not expand at parse time either. Expansion must be written in
  either case.
- The source to port is `buildkit@v0.34.0 frontend/dockerfile/shell/lex.go`:
  722 lines, of which **553 lines of code** (excluding comments and blank
  lines). It supports:
  - `$a`, `${a}`;
  - `${a:-b}`, `${a-b}`, `${a:+b}`, `${a+b}`, `${a:?b}`;
  - `${a#p}`, `${a##p}`, `${a%p}`, `${a%%p}` (globs converted to regex);
  - `${a/p/r}`, `${a//p/r}`;
  - single and double quotes, an optional escape character, special
    parameters (`$$`, `$@`…), and `ProcessWords` (word splitting).
- Reusable test vectors exist: `shell/envVarTest` (238 cases) and
  `shell/wordsTest` (30 lines).
- **[inferred]** Estimate: **about 600–700 lines of Rust** for the lexer,
  plus about 150 lines deciding which instruction expands which field
  (`Expand()` in `instructions/commands.go`: ENV, ARG, LABEL, COPY/ADD flags,
  WORKDIR, USER, EXPOSE, VOLUME, STOPSIGNAL, FROM).
- A side note, from reading code only: kaniko calls
  `shell.NewLex(parser.DefaultEscapeToken)` at
  `kaniko@44aa7ddc0 pkg/util/command_util.go:66,84,105`, so kaniko always
  expands with `\` as the escape character, even when the file sets
  ``# escape=` ``. The effect was not confirmed by running it.

### What the crate loses (**[code]** and output)

- **Heredoc names.** `HereDoc` has only `span`, `expand` and `value`.
  BuildKit uses the heredoc name as the file name in `COPY <<EOF /dir/`.
- **The position of `<<EOF` in a `RUN` command.** BuildKit needs it: if the
  command is only a heredoc, the file runs as a script; otherwise the whole
  line goes to the shell.
- **Shell-form commands are not joined.** The crate keeps the escape
  characters.
- **`ENV`/`ARG`/`LABEL` are a single string.** The crate does not split
  key/value pairs.
- **Flags are not checked.** The crate does not parse `--mount`,
  `--network` or `--security`. BuildKit has 310 lines just for `--mount`
  (`commands_runmount.go`).

### `.dockerignore`

Table 4. The `ignore` crate against `PatternMatcher.MatchesOrParentMatches`.

| Comparison | Cases differing | Cells differing |
|---|---|---|
| `ignore` with the `.dockerignore` lines used directly | **19/38** | 89/1,596 |
| `ignore` + translation layer (normalise like `ignorefile.ReadAll`, then prefix `/` to anchor at the root) | **5/38** | 8/1,596 |

Differences with direct use:

1. **A pattern without `/` matches at any depth in gitignore**, but only at
   the root in dockerignore. Affected cases: `foo`, `foo/`, `*.txt`,
   `temp?`, `node_modules`, `a.tx?`, `[a-b].txt`, `a.txt` (for example,
   `*.txt` also excludes `sub/a.txt`).
2. **`./foo`, `foo/../a.txt`, `  a.txt  `:** dockerignore runs
   `filepath.Clean` and trims; gitignore does neither, so nothing matches.
3. **`*/`:** gitignore matches directories only. Dockerignore cleans it to
   `*`, which also matches files (11 cells).
4. **`!*/` after `*`:** gitignore re-includes directories only, so files
   stay excluded. Dockerignore turns it into `!*`, which excludes nothing
   (28 cells).

Differences left after translation (not fixable by translating patterns):

5. **A parent directory is excluded, but a child is re-included with `!`.**
   Dockerignore allows this (BuildKit still descends into a directory when
   exclusion patterns exist); gitignore does not. Four cases:
   - `docs` + `!docs/keep.md`
   - `docs/` + `!docs/**/keep.md`
   - `*` + `!foo/bar`
   - `sub/**/` + `!sub/foo/x`
6. **`[!b]`:** patternmatcher treats `!` as a literal (only `[^b]` negates);
   gitignore treats `[!b]` as negation.

Differences inside the Go side:

- kaniko uses `PatternMatcher.Matches`, a deprecated API
  (`kaniko@44aa7ddc0 pkg/util/fs_util.go:1240,1248`, **[code]**). Its result
  differs from `MatchesOrParentMatches` in **7 cells**.
- With `**/foo`, kaniko's function does not exclude `foo/a.txt` and
  `foo/bar/…`. With `**/node_modules`, it does not exclude
  `node_modules/p/…`.
- Whether kaniko skips a whole subtree once a directory is excluded was not
  checked end to end. ranikust has to choose between BuildKit's and kaniko's
  behaviour (recommendation: BuildKit).

**Is there a Rust port of patternmatcher?** None found, searching the
crates.io API for `dockerignore`, `patternmatcher` and `docker ignore`
(**[external]**, 2026-10-29):

- `use-dockerignore` 0.0.1 (2026-05-26, 171 downloads) has only data types
  (`DockerIgnoreNegation`…), no matching function.
- `cuenv-ignore` only generates ignore files.

Source to port: `patternmatcher.go` has 312 lines of code, `ignorefile.go`
51. **[inferred]** Estimate: about **350–400 lines of Rust**, using `regex`,
plus tests taken from the experiment's case file and from upstream.

### Related crates

Table 5. Rust crates related to Dockerfiles and BuildKit. Registry and
GitHub figures are **[external]**, from crates.io and GitHub on 2026-10-29;
the last two columns are **[measured]** and count only files parsed
**without error**, not content.

| Crate | Latest | Released | Downloads (total / 90 days) | Status | kaniko, 201 files | extra, 14 files |
|---|---|---|---|---|---|---|
| `parse-dockerfile` (taiki-e) | 0.1.8 | 2026-07-10 | 36,063 / 29,115 | Active (push 2026-10-02, 10 stars, 2 issues) | 198 | 11 |
| `containerfile` (lib `dockerfile`) | 0.1.0 | 2026-08-23 | 68 | New, a single release, no repository link | 198 | 11 |
| `dockerfile-parser` (HewlettPackard) | 0.9.0 | 2024-10-31 | 565,271 / 58,312 | **Repository archived.** Uses pest, no heredocs | 193 | 3 |
| `dockerfile-parser-rs` (slimreaper35) | 3.4.0 | 2026-10-09 | 32,462 / 9,078 | Active. **No ONBUILD, HEALTHCHECK or MAINTAINER** | 183 | 4 |
| `buildkit-rs` | 0.1.0 | 2023-04-11 | 1,622 | gRPC client for BuildKit, not a parser. Abandoned | — | — |
| `buildkit-client` / `bkit` | 0.1.4 / 0.1.3 | 2025-11-10 | 438 / 101 | gRPC clients | — | — |
| `buildkit-frontend`, `buildkit-llb`, `buildkit-proto` | 0.3.0 / 0.2.0 / 0.2.0 | 2020-03-04 | about 10–14 thousand | Abandoned | — | — |

- `containerfile` fails on `test_env`: it rejects the legacy quoted form
  `ENV x="a b"`. It also fails on BOM, blank lines between continuation
  lines, and multiple heredocs in one `RUN`.
- `dockerfile-parser` fails on every heredoc.

## Findings

1. `parse-dockerfile` 0.1.8 fails on heredocs placed after the start of a
   `RUN` argument list, and fixing that needs a shell lexer inside the crate
   (Table 3).
2. With about 125 lines of glue it matches BuildKit on 198/200 valid kaniko
   files and 232/242 files overall (95.9%) (Table 2).
3. The crate also has genuine bugs (`\\` at end of line) and rejects
   `ENTRYPOINT []`, and its data model drops heredoc names, the `<<EOF`
   position and key/value structure (Table 3, "What the crate loses").
4. No Rust Dockerfile crate expands variables; BuildKit's `lex.go` (553
   lines of code) has to be ported regardless.
5. The `ignore` crate disagrees with patternmatcher on 19/38 cases directly
   and 5/38 after translation, because gitignore cannot re-include a child
   of an excluded directory and reads `[!b]` differently (Table 4).
6. No crate on crates.io ports `moby/patternmatcher`.
7. kaniko uses the deprecated `PatternMatcher.Matches`, which differs from
   BuildKit's `MatchesOrParentMatches` in 7 cells, and always expands with
   `\` as the escape character.

## Implications for ranikust

[ADR-0015](../../adr/0015-hand-written-dockerfile-parser.md) records the
decision to write the parser. The reasons from this experiment:

- The mid-command heredoc bug sits in the crate's core. Fixing it needs a
  shell lexer to split words, exactly as BuildKit does, and that lexer has
  to be written anyway for variable expansion.
- The crate's data model (spans, raw strings, no heredoc names) differs from
  BuildKit's. Every difference needs a glue layer or a fork.
- What the crate does well (line joining, directives, JSON form) is only
  about 1,000 lines, the easiest part of BuildKit's parser.

Table 6. Size estimate (**[inferred]**), from Go lines of code excluding
comments and blank lines.

| Part | Go source (lines of code) | Rust estimate |
|---|---|---|
| Parser: line joining, directives, heredocs, JSON, errors | `parser/*.go` ≈ 1,030 | 900–1,000 |
| Instructions: typed commands, flags, `--mount`/`--network`/`--security`, validation | `instructions/*.go` ≈ 1,800 | 1,300–1,400 |
| Variable expansion | `shell/lex.go` 553 | 600–700 |
| Per-instruction `Expand()` | part of `commands.go` | about 150 |
| **Total** | | **≈ 3,000–3,300** |
| `.dockerignore` (`rk-ignore`) | 363 | 350–400 |

Using the crate anyway would mean forking `parse-dockerfile` to fix
mid-command heredocs, the `\\` line ending and `ENTRYPOINT []`, and still
writing the lexer, the key/value splitting, the instructions layer and the
flags. That saves about 800–1,000 lines but leaves a fork whose model does
not fit to maintain. Not worth it.

Compatibility testing: keep the Go golden program and the comparison script
as a CI harness, running the golden side in a Go container. Three more
reusable test sources: BuildKit `parser/testfiles*`, `shell/envVarTest`, and
the `.dockerignore` cases.

For `.dockerignore`, follow BuildKit's `MatchesOrParentMatches`, not
kaniko's deprecated `Matches`.

## Limitations and open questions

- The Rust expansion lexer was not written; 600–700 lines is an estimate.
- Content was not compared (only errors counted) for `containerfile` and the
  two other crates.
- Heredoc file names in `COPY`, `DocComment`, line positions and lint
  warnings were not compared.
- kaniko's end-to-end `.dockerignore` behaviour was not tested (for example,
  the `**/foo` difference during a real walk), nor was the escape character
  during expansion.
- Parse performance was not measured (unimportant: files are small).
- No search for a patternmatcher port outside crates.io (for example in the
  sources of Rust tools on GitHub).

## Reproduction

The PoC lived in `docs-rust/poc/04-dockerfile-parser/` (archived outside
this repository). `./run.sh` needs podman (or `ENGINE=docker`) and python3
on the host. It runs two throwaway containers (`--rm`) that share the cache
volumes `rk-gomod` and `rk-cargo`, mounting kaniko's
`integration/dockerfiles` read-only; results go to `out/`, and
`out/report.txt` holds the full comparison.

| File | Role |
|---|---|
| `go/golden/main.go` | Parses with BuildKit (`parser.Parse` + `instructions.ParseInstruction`) and writes normalised JSON. No variable expansion |
| `go/ignore/main.go` | Runs patternmatcher on `testdata/ignore-cases.json` |
| `rust/src/bin/parse.rs` | Parses with `parse-dockerfile` and writes the same JSON shape. `RK_GLUE=1` enables full glue |
| `rust/src/bin/ignore.rs` | Runs the `ignore` crate on the same cases |
| `rust/src/bin/others.rs` | Counts the files the other parser crates accept |
| `compare.py` | Compares the JSON and classifies differences |
| `testdata/extra/` | The 14 hand-written files |
| `testdata/invalid/` | The 13 invalid files |

The BuildKit test files are copied out of the Go module cache
(`frontend/dockerfile/parser/testfiles*` and `testfiles-negative`) during
the run. The Go image is pinned by digest; the Rust crates are pinned with
`=` versions in `Cargo.toml`.

## References

1. [Survey chapter 03: Rust architecture](../survey/03-rust-architecture.md), §3 and the code-size estimate.
2. BuildKit v0.34.0: `frontend/dockerfile/parser/`,
   `frontend/dockerfile/instructions/` (`commands.go`,
   `commands_runmount.go`), `frontend/dockerfile/shell/lex.go`,
   `shell/envVarTest`, `shell/wordsTest`. **[code]**
3. `moby/patternmatcher` v0.6.1: `patternmatcher.go`,
   `ignorefile/ignorefile.go`. **[code]**
4. `parse-dockerfile` 0.1.8 (taiki-e), `src/lib.rs`. **[code]**
5. kaniko: `kaniko@44aa7ddc0 pkg/util/command_util.go:66,84,105`,
   `kaniko@44aa7ddc0 pkg/util/fs_util.go:1240,1248`. **[code]**
6. crates.io API and GitHub repository metadata for the crates in Table 5
   (accessed 2026-10-29). **[external]**
