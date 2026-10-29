# golden-gen: BuildKit reference output

**TL;DR:** A small Go program that runs BuildKit v0.34.0's own Dockerfile
parser, instruction parser and shell lexer and writes their results as JSON.
rnk-dockerfile's golden tests (`tests/golden.rs`) compare against those
files. Regenerate with `./regen.sh`; it needs only podman.

## What it writes

| Output | Input | Content |
|---|---|---|
| `tests/golden/kaniko/*.json` | `tests/fixtures/kaniko/` (kaniko's 200 integration Dockerfiles) | One file per Dockerfile |
| `tests/golden/buildkit/*.json` | `tests/fixtures/buildkit/parser/` (BuildKit's parser test files) | One file per test case, named `<dir>_<case>` |
| `tests/golden/extra/*.json` | `tests/fixtures/extra/` (edge cases and errors written for ranikust) | One file per Dockerfile |
| `tests/golden/lex.json` | generated in `lex.go` with a fixed seed | Shell-lexer cases and BuildKit's results |

Each Dockerfile golden holds:

- `ok`, and on failure `error` and `error_line` (BuildKit's message and the
  first line of its location);
- `escape` and the number of `warnings` (empty continuation lines);
- `ast`: per instruction, BuildKit's `Node.Dump()`, its start and end lines,
  and its heredocs (name, file descriptor, expand, chomp, content), from
  `parser.Parse`;
- `meta_args` and `stages` with their typed `commands`, from
  `instructions.Parse`. `RUN` mounts are parsed completely by calling
  `RunCommand.Expand` with an expander that returns each word unchanged, as
  BuildKit's dispatcher does before it runs the command.

Variables are not expanded: expansion and `ARG`/`ENV` scoping happen in
BuildKit's `dockerfile2llb`, which these goldens do not cover.

## Regenerating

```sh
crates/rnk-dockerfile/tests/golden-gen/regen.sh
```

The script runs the program in `golang` 1.26.9 (pinned by digest; override
with `GO_IMG`) with `ENGINE=podman` by default. Go modules are cached in the
named volume `rk-gomod` (override with `GO_MOD_VOLUME`); the first run
downloads BuildKit v0.34.0 and its few dependencies from the Go module proxy.
Nothing is installed on the host and nothing outside `tests/` is written.

Then run `cargo test -p rnk-dockerfile --test golden` and commit the
changed goldens together with the fixture or generator change that caused
them.

## Changing the inputs

- **kaniko:** the fixtures are a copy of kaniko's
  `integration/dockerfiles/` at `kaniko@44aa7ddc0` without the one
  `.dockerignore` file. Copy the directory again to follow a newer kaniko.
- **BuildKit:** the fixtures are BuildKit v0.34.0's
  `frontend/dockerfile/parser/testfiles*` and `shell/{envVarTest,wordsTest}`.
  When BuildKit is upgraded, copy them again, change the version in
  `go.mod` (and `go.sum`), regenerate, and port the differences.
- **extra:** add a Dockerfile to `tests/fixtures/extra/`; names starting
  with `err-` are expected to fail.
- **lexer:** edit `fragments` or the case count in `lex.go`.
