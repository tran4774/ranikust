---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0053: A local context is a plain path: the `dir://` scheme is dropped, and `--context-sub-path` applies only to fetched contexts

## Context and Problem Statement

kaniko takes a `--context` without `://` as a local directory and also
accepts `dir://<path>`; its `--context-sub-path` reaches only the second
form and the fetched ones, because a plain path returns before the join
(`kaniko@44aa7ddc0 cmd/executor/cmd/root.go:599-601`, `:625-630`).
BuildKit has no `dir://`: a local context is a path, and its sub-path
applies only to fetched contexts (`frontend/dockerui/context.go:169-173`,
`:348-353`). On 2026-10-30 Q-081 was answered, under
[ADR-0032](0032-observable-behaviour-follows-buildkit.md), as: the sub-path
applies only to fetched (git, HTTPS, tar) contexts. The code kept kaniko's
`dir://` form, to which it still applies the sub-path
(`crates/ranikust/src/config.rs`, `context_with_sub_path`), and
[ADR-0036](0036-kaniko-flags-in-phase-1.md) accepted that. Two spellings
of one thing, with a flag that works for one and is ignored for the other,
is what ADR-0032's answer was meant to remove. The maintainer was asked
whether to keep the form.

## Considered Options

- Keep `dir://` and apply `--context-sub-path` to it, as the code and
  ADR-0036 do today.
- Drop `dir://`: a local context is a plain path, and the sub-path applies
  only to fetched contexts, as the answer under ADR-0032 already says
  (the recommendation put to the maintainer, its option (b)).

## Decision Outcome

Chosen: the kaniko-only `dir://` context syntax is dropped; a local
context is a plain path, and `--context-sub-path` applies only to fetched
contexts (git, HTTPS, tar).

- `--context=dir://<path>` is no longer a context form: it is refused, with
  a message that names the plain path to use.
- `--context-sub-path` joins a sub-directory onto a fetched context after
  it is fetched, staying inside it (no absolute path, no `..`). Local
  directories do not take it. Fetched contexts arrive in Phase 5, so until
  then the flag has no context it applies to.
- `rnk_context::parse_spec` no longer recognises `dir://`; a bare path is
  the directory form.
- Supersedes in part: ADR-0036's input bullet, where `--context-sub-path`
  is joined onto a `dir://` context and `dir://` is accepted, and the
  `--context` meaning ADR-0011 takes from kaniko, as far as it includes
  `dir://`. Everything else in both stands.
- The code change (`config.rs`, `cli.rs`, `main.rs` warning, `rnk-context`
  `parse_spec`, the `dir://` tests, `tests/vm/guest/e2e.sh`) is not part of
  this ADR's commit. It lands with the Phase 2 task 7 integration branch
  ([phase-2.md](../agents/phase-2.md)); until then the code still accepts
  `dir://`.

### Consequences

- Good, because there is one way to name a local context, and a flag that
  applies to a context applies to all contexts of its kind.
- Good, because the behaviour is BuildKit's, as ADR-0032 recorded.
- Bad, because a kaniko pipeline that passes `--context=dir://...` needs
  the prefix removed (ADR-0049: compatibility with kaniko is not a goal).
- Neutral, because what a local context does with a `--context-sub-path`
  (ignore with a warning, or refuse) stays as it is today and is left open
  (see More Information).

## Evidence

- BuildKit, `frontend/dockerui/context.go:169-173`, `:348-353` [external]:
  the sub-path applies to fetched contexts only; no `dir://` form.
- kaniko, `cmd/executor/cmd/root.go:599-631`, `pkg/buildcontext/dir.go:25`
  [external]: `dir://` reaches the join, a plain path does not.
- `crates/ranikust/src/config.rs` (`context_with_sub_path`,
  `context_sub_path_ignored`) and its test
  `context_sub_path_applies_to_dir_contexts` [code]: today's behaviour,
  which this ADR changes.
- No experiment: the question is one of interface, not of measured cost.

## More Information

Partly supersedes [ADR-0011](0011-accept-common-kaniko-flags.md) and
[ADR-0036](0036-kaniko-flags-in-phase-1.md) where they accept `dir://`;
their bodies are unchanged. Completes the answer to Q-081 recorded under
[ADR-0032](0032-observable-behaviour-follows-buildkit.md), which stands.
Priority rule: [ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md).

Not decided here: whether `--context-sub-path` given with a local context
is ignored with a warning (as the code does for a plain path today) or
refused (ADR-0049's rule for flags that change nothing). The code keeps
the warning until the maintainer says otherwise.
