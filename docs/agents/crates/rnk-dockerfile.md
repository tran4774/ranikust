# rnk-dockerfile

> **Status:** in progress · **Phase:** 1 ·
> **Depends on:** — (pure data) · **Used by:** `rnk-builder` (`rnk-sched`, `rnk-cache` [open: Q-028]) · **Path:** `crates/rnk-dockerfile/`

**TL;DR:** A hand port of BuildKit v0.34.0's parser, instruction set and
shell lexer, plus the stage graph. BuildKit is the oracle: any difference from
its output on the same file is a bug, heredoc names and expansion included.

## Owns

- Lexer, parser directives, line continuation, heredocs, the JSON form.
- Variable expansion (port of `shell/lex.go`) and the typed instruction set
  with its flags (`--mount`, `--chown`, `--from`, …).
- The stage dependency graph, and kaniko-style stage squashing, used only
  when stages share the `/` rootfs in sequential kaniko mode.

## Must not

- Touch the filesystem, network or processes: data in, data out. Remote
  `ONBUILD` triggers and base configs come in from the caller.
- Schedule or build stages (`rnk-sched`, `rnk-builder`); squash in overlay
  mode or on per-stage roots ([design/scheduler.md#the-graph](../../design/scheduler.md#the-graph)).

## API (planned)

```rust
pub fn parse_ast(src: &str) -> Result<Ast, Error>;                       // [done] raw tree, BuildKit parser.Parse
pub struct Ast { pub escape: char, pub directives: Vec<Directive>, pub nodes: Vec<Node>, pub warnings: Vec<Warning> } // [done]
pub struct Node { pub keyword, flags, args, child, json, heredocs, original, lines: Lines, comments } // [done] parser.Node
pub struct Error { kind: ErrorKind, lines: Option<Lines>, message } // [done] lines always set for text errors
pub fn parse(src: &str) -> Result<Dockerfile, Error>;                   // [done] parse_ast + instructions.Parse
pub struct Dockerfile { pub escape: char, pub directives, pub meta_args: Vec<Located<Vec<ArgDecl>>>,
                        pub stages: Vec<Stage>, pub warnings }             // [done]
pub struct Stage { pub name: Option<String>, pub base: String, pub platform: Option<String>, pub lines, pub code,
                   pub comments, pub instructions: Vec<Located<Instruction>> } // [done] base unexpanded, name lower-cased
pub struct Located<T> { pub value: T, pub lines: Lines, pub keyword: String, pub code: String, pub comments } // [done]
pub enum Instruction { Run(Run), Copy(Copy), Add(Add), Env(Vec<KeyValue>), Arg(Vec<ArgDecl>), Label(Vec<KeyValue>),
    Workdir(String), User(String), Expose(Vec<String>), Volume(Vec<String>), Entrypoint(CmdLine), Cmd(CmdLine),
    Shell(Vec<String>), StopSignal(String), Healthcheck(Healthcheck), Onbuild(String), Maintainer(String) } // [done]
pub struct Run { pub cmd: CmdLine, pub mount_specs, pub mounts: Vec<Mount>, pub network, pub security, pub devices, pub flags_used } // [done]
pub struct Heredoc { pub name: String, pub file_descriptor: u64, pub expand: bool, pub chomp: bool, pub content: String } // [done]
pub struct Lexer { escape: char, .. } // [done] port of lex.go: process_word(), process_words(), process_word_with_matches()
pub trait Env { fn get(&self, name: &str) -> Option<&str>; } // [done] BuildKit EnvGetter
impl Located<Instruction> { pub fn expand(&mut self, lex: &Lexer, env: &dyn Env) -> Result<(), Error>; } // [done] Expand() + dispatch
pub fn stage_graph(df: &Dockerfile, args: &BuildArgs, target: Option<&str>) -> Result<StageGraph, GraphError>; // [design]
pub fn squash_for_shared_root(g: &StageGraph) -> Vec<SquashedStage>;     // [design] phase 2
// [open: Q-044] how remote base ONBUILD triggers enter stage_graph() without I/O
```

## Invariants

- No panic on any input (release aborts); deterministic output: stage,
  instruction and edge order follow the file, no `HashMap` order leaks.
- Expansion uses the file's escape character, as BuildKit does (ADR-0015).
- Every instruction and heredoc keeps its line and column.

## Refs

- **Design:** [design/scheduler.md#the-graph](../../design/scheduler.md#the-graph) (edges, `--target` pruning, no squash in overlay mode)
- **Decisions:** [ADR-0015](../../adr/0015-hand-written-dockerfile-parser.md) (hand port of BuildKit v0.34.0, ~3,000–3,300 lines; BuildKit over kaniko), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) (shared `/` only when sequential)
- **Evidence:** [exp-04](../../research/experiments/04-dockerfile-parser.md#parse-results): `parse-dockerfile` + 125 lines of glue
  matched 198/200 kaniko files, 232/242 overall; `lex.go` is 553 lines of code.
- **kaniko:** `kaniko@44aa7ddc0 pkg/dockerfile/dockerfile.go:104-127` (parse),
  `:219-239` (targets), `:264-282` (`--from`), `:297-415`
  (`MakeKanikoStages`; squash `:393-406`), `:482-497` (`Squash`);
  `pkg/dockerfile/buildargs.go:210` (predefined args);
  `pkg/util/command_util.go:66,84,105` (expansion always escapes with `\`)
- **External:** BuildKit v0.34.0 [parser/parser.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerfile/parser/parser.go), [instructions/parse.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerfile/instructions/parse.go), [instructions/commands.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerfile/instructions/commands.go) (`Expand`), [commands_runmount.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerfile/instructions/commands_runmount.go), [shell/lex.go](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerfile/shell/lex.go), [shell/envVarTest](https://github.com/moby/buildkit/blob/v0.34.0/frontend/dockerfile/shell/envVarTest), [parser/testfiles](https://github.com/moby/buildkit/tree/v0.34.0/frontend/dockerfile/parser/testfiles)

## Tests

- T0 goldens from exp-04's Go harness (BuildKit `parser.Parse` +
  `instructions.Parse` in a container, normalised JSON, committed): kaniko's
  `integration/dockerfiles/` (200 Dockerfiles; the 201st file is a
  `.dockerignore`), BuildKit `parser/testfiles*`, exp-04's extra and invalid.
- T0 vectors `shell/envVarTest` (238 cases), `shell/wordsTest`; fuzz `parse`
  and expansion; graph tests against `MakeKanikoStages` (`--target`, `--from`).

## Pitfalls

- `parse-dockerfile` failed on mid-command heredocs (`RUN cat <<EOF > /out`):
  finding `<<` needs the lexer's word splitting ([exp-04](../../research/experiments/04-dockerfile-parser.md#parse-results)); and no crate
  expands `${a:-b}`, `${a#p}`, `${a/p/r}` ([exp-04](../../research/experiments/04-dockerfile-parser.md#variable-expansion-code-no-rust-written)).
- Keep heredoc names (`COPY <<EOF /dir/` names the file) and the `<<EOF`
  position in `RUN`: alone it is a script, else the line goes to the shell
  ([exp-04](../../research/experiments/04-dockerfile-parser.md#what-the-crate-loses-code-and-output)).
- BuildKit joins continued shell-form lines, drops interior comments (26
  kaniko files), sorts `EXPOSE`, rejects `COPY --foo` and `AS 1bad`; `\\` at
  end of line is no continuation; `ENTRYPOINT []` is valid (exp-04 Table 3).
- kaniko expands with `\` even when the file sets a backtick escape
  (`command_util.go:66`), resolves `--from` case-insensitively or by index
  (`dockerfile.go:264-282`), and fetches remote `ONBUILD` while planning
  (`dockerfile.go:354`).

## Phase 1

- Order: parser with goldens → lexer with `envVarTest` → instructions, flags,
  `expand` → `stage_graph` (`ARG` before `FROM`, `--build-arg`, `--target`).
- MVP instructions: `FROM` (incl. `<stage>`), `ARG`, `ENV`, `RUN` (shell,
  exec, heredoc, `--mount=type=cache|secret`), local `COPY`/`ADD` (`--chown`,
  `--chmod`, `--from`), `WORKDIR`, `USER`, `LABEL`, `EXPOSE`, `VOLUME`,
  `ENTRYPOINT`, `CMD`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK` ([survey 04 §2](../../research/survey/04-risks-and-roadmap.md#2-mvp-and-10-scope)).
- Deferred: squashing (phase 2); `ONBUILD` execution, `--mount=type=bind`,
  `COPY --link/--parents/--exclude`, `ADD <url>`/git (phase 5).
  [open: Q-009] Parse them anyway so all 200 goldens pass, and let
  `rnk-builder` reject them.
