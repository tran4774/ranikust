# rnk-dockerfile

> **Status:** in progress (Phase 1 done; squashing is Phase 2) · **Phase:** 1 ·
> **Depends on:** — (pure data) · **Used by:** `rnk-builder` (`rnk-sched`, `rnk-cache` [open: Q-028]) · **Path:** `crates/rnk-dockerfile/`

**TL;DR:** A hand port of BuildKit v0.34.0's parser, instruction set and
shell lexer, plus the stage graph. BuildKit is the oracle: any difference from
its output on the same file is a bug, heredoc names and expansion included.

## Owns

- Lexer, parser directives, line continuation, heredocs, the JSON form.
- Variable expansion (port of `shell/lex.go`) and the typed instruction set
  with its flags (`--mount`, `--chown`, `--from`, …).
- `ARG`/`ENV` scoping (`StageScope`), the stage dependency graph, and
  kaniko-style stage squashing, used only when stages share the `/` rootfs
  in sequential kaniko mode (Phase 2, not written).

## Must not

- Touch the filesystem, network or processes: data in, data out. Remote
  `ONBUILD` triggers and base configs come in from the caller.
- Schedule or build stages (`rnk-sched`, `rnk-builder`); squash in overlay
  mode or on per-stage roots ([design/scheduler.md#the-graph](../../design/scheduler.md#the-graph)).

## API (planned)

```rust
pub fn parse_ast(src: &str) -> Result<Ast, Error>;                       // [done] raw tree, BuildKit parser.Parse
pub struct Ast { pub escape: char, pub directives, pub nodes: Vec<Node>, pub warnings } // [done]; Node = parser.Node, dump()
pub fn parse(src: &str) -> Result<Dockerfile, Error>;                    // [done] parse_ast + instructions.Parse
pub struct Dockerfile { pub escape: char, pub directives, pub meta_args: Vec<Located<Vec<ArgDecl>>>,
                        pub stages: Vec<Stage>, pub warnings }             // [done]
pub struct Stage { pub name: Option<String>, pub base: String, pub platform: Option<String>, pub lines, pub code,
                   pub comments, pub instructions: Vec<Located<Instruction>> } // [done] base unexpanded, name lower-cased
pub struct Located<T> { pub value: T, pub lines: Lines, pub keyword: String, pub code: String, pub comments } // [done]
pub enum Instruction { Run(Run), Copy(Copy), Add(Add), Env(Vec<KeyValue>), Arg(Vec<ArgDecl>), Label(Vec<KeyValue>),
    Workdir(String), User(String), Expose(Vec<String>), Volume(Vec<String>), Entrypoint(CmdLine), Cmd(CmdLine),
    Shell(Vec<String>), StopSignal(String), Healthcheck(Healthcheck), Onbuild(String), Maintainer(String) } // [done]
pub struct Run { pub cmd: CmdLine, pub mount_specs, pub mounts: Vec<Mount>, pub network, pub security, pub devices, .. } // [done]
pub struct Heredoc { pub name, pub file_descriptor: u64, pub expand: bool, pub chomp: bool, pub content } // [done]
pub fn parse_heredoc_marker(word: &str) -> Option<Heredoc>;              // [done] MustParseHeredoc, for RUN dispatch
pub struct Lexer { .. } // [done] lex.go: process_word(), process_words(), process_word_with_matches()
pub trait Env { fn get(&self, name: &str) -> Option<&str>; }            // [done] EnvGetter
impl Located<Instruction> { pub fn expand(&mut self, lex: &Lexer, env: &dyn Env) -> Result<(), Error>; } // [done] Expand() + dispatch
pub fn stage_graph(df: &Dockerfile, opts: &GraphOptions) -> Result<StageGraph, Error>; // [done] dockerfile2llb resolution
pub struct GraphOptions { pub build_args, pub build_platform: Platform, pub target_platform: Platform,
                          pub targets: Vec<String> }                       // [done] list of targets; ranikust passes one (ADR-0049)
pub struct StageGraph { pub global_args: EnvList, pub stages: Vec<StageNode>, pub targets: Vec<usize>, pub needed: Vec<usize>, .. } // [done]
pub struct StageNode { pub base: Base, pub platform, pub triggers, pub references: Vec<Reference>, .. } // [done]
pub struct StageScope { .. } // [done] apply(&Located<Instruction>) -> expanded; env(), config_env(), args()
pub struct Error { kind: ErrorKind, lines: Option<Lines>, message } // [done] Syntax|Instruction|Expansion|Graph
pub fn squash_for_shared_root(g: &StageGraph) -> Vec<SquashedStage>;     // [done] kaniko MakeKanikoStages/Squash; units in file order
pub struct SquashedStage { pub stage, pub chain: Vec<usize>, pub base: Base, pub save_image: bool,
                           pub layers_needed: bool, pub readers: Vec<(usize, usize)> } // [done] chain base first; readers = (stage, reference index)
pub fn layers_needed(g: &StageGraph) -> Vec<usize>;                     // [done] ADR-0038 lazy rule for per-stage roots
// (Q-044, ADR-0032) how remote base ONBUILD triggers enter stage_graph() without I/O
```

## Invariants

- No panic on any input (release aborts); recursion is bounded where Go's
  growable stacks hide it (nested `${}`, `ONBUILD ONBUILD`, JSON depth).
- Deterministic output: stage, instruction and edge order follow the file,
  no `HashMap` order leaks (BuildKit's random `FlagsUsed` order is sorted).
- Expansion uses the file's escape character, as BuildKit does (ADR-0015).
- Every instruction, heredoc and text error keeps its lines (BuildKit has no
  columns for instructions).

## Refs

- **Design:** [design/scheduler.md#the-graph](../../design/scheduler.md#the-graph) (edges, `--target` pruning, no squash in overlay mode)
- **Decisions:** [ADR-0015](../../adr/0015-hand-written-dockerfile-parser.md) (hand port of BuildKit v0.34.0; BuildKit over kaniko), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (shared `/` only when sequential)
- **Evidence:** [exp-04](../../research/experiments/04-dockerfile-parser.md#parse-results) estimated 3,000–3,300 lines; the port is about 4,600 lines of Rust without comments, inline unit tests included.
- **kaniko:** `kaniko@44aa7ddc0 pkg/dockerfile/dockerfile.go:80-93` (base
  lookup), `:219-239` (targets), `:264-282` (`--from`), `:297-415`
  (`MakeKanikoStages`; squash `:393-406`), `:482-497` (`Squash`);
  `pkg/dockerfile/buildargs.go:210` (predefined args);
  `pkg/util/command_util.go:66,84,105` (expansion always escapes with `\`)
- **External:** BuildKit v0.34.0 [parser/](https://github.com/moby/buildkit/tree/v0.34.0/frontend/dockerfile/parser), [instructions/](https://github.com/moby/buildkit/tree/v0.34.0/frontend/dockerfile/instructions), [shell/lex.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerfile/shell/lex.go), [dockerfile2llb/convert.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerfile/dockerfile2llb/convert.go) (args, stages, targets)

## Tests

- Goldens from BuildKit itself (`tests/golden-gen/`, Go in podman; its
  README says how to regenerate): raw tree and typed instructions of
  kaniko's 200 Dockerfiles, BuildKit's 37 parser test files and 63 edge
  cases (all match); 1,688 generated lexer cases (all match);
  `dockerfile2llb` expansion of the 200 files (197 match, 3 are rejected by
  BuildKit's dispatcher for builder-level reasons).
- Unit tests per module, `envVarTest`/`wordsTest` vectors, BuildKit's
  `testfiles` dumps, proptest (no panic, errors carry lines), graph and
  scope walks over the 200 files with kaniko's build arguments.

## Metrics

- One `rnk_dockerfile.parse` span per Dockerfile (`bytes_in`,
  `instructions`, `stages`); none per line or instruction
  ([rows](../../design/metrics.md#rnk-dockerfile)).
- Depends on `tracing` only; stays free of I/O.

## Pitfalls

- Heredocs are found with the shell lexer over the whole line (`<<` inside
  quotes is not one); a `RUN` whose command is only `<<EOF` runs the body,
  otherwise the body goes to the shell; `COPY <<EOF /dir/` names the file.
- BuildKit reads flags byte by byte as Latin-1 (`--chown=é` becomes `Ã©`),
  and a nested `${a:-b}` resets the lexer's word buffer; both are kept.
- `FROM` matches only earlier stage names, exactly (lower-case), the last
  definition winning; `scratch` can be shadowed by a stage of that name.
  `COPY --from` takes an index or a name in any case; `--mount from=` names
  only. A stage may read only stages defined before it. `FROM <stage>`
  matches exactly, as BuildKit does (Q-051).
- Not expanded at all: `RUN`/`CMD`/`ENTRYPOINT` commands, `SHELL`,
  `HEALTHCHECK`, `COPY --from`, `--exclude`. `ARG` without a default takes
  the global value; build arguments for undeclared names are invisible.
- `STOPSIGNAL`, `ARG` and `HEALTHCHECK NONE` ignore their flags; mounts are
  checked only for CSV and `from` until `expand()`.
- kaniko expands with `\` even when the file sets a backtick escape
  (`command_util.go:66`), resolves the `FROM` base case-insensitively
  (`dockerfile.go:80-93`), and fetches remote `ONBUILD` while planning
  (`dockerfile.go:354`).
- Known divergences, on purpose: non-ASCII variable names (Unicode
  categories approximated), a `COPY --from` index past the last stage (an
  error, BuildKit counts earlier image references), `--mount` without
  `from` adds no edge, platforms are not validated.

## Phase 1

- Done: parser with goldens, lexer with `envVarTest`, instructions, flags,
  `expand`, `StageScope`, `stage_graph` (`ARG` before `FROM`, `--build-arg`,
  built-in args, `--target`, local `ONBUILD` triggers).
- Phase 2: squashing for the shared root and the lazy-snapshot rule
  (`src/squash.rs`, ported from `kaniko@44aa7ddc0
  pkg/dockerfile/dockerfile.go:297-415`, `:482-497`): a base folds into
  its only child when no `COPY --from` or `RUN --mount from=` reads it
  and it is not the result; chains fold whole; a folded base that is
  another `--target` is still built on its own; every stage of a chain
  but the last runs without its `ONBUILD` instructions. kaniko does not
  know `RUN --mount from=`; here it blocks folding as `COPY --from` does.
  A saved image (a later unit builds `FROM` it) is walked ADR-0044.
- Deferred: `ONBUILD` execution, `--mount=type=bind`,
  `COPY --link/--parents/--exclude`, `ADD <url>`/git (phase 5). They are
  parsed ([Q-009](../../open-questions.md#q-009-does-the-parser-accept-instructions-deferred-to-phase-5),
  interim), and `rnk-builder` rejects them. `ADD --checksum` rules and image
  reference checks are the builder's too.
