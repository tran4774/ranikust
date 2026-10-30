---
name: refactor
description: The end-of-phase clean-up of ranikust - behaviour-preserving refactoring and simplification, one crate per branch, with digests, host safety and crate boundaries held fixed. Use when the maintainer asks for the refactor/clean-up pass after a phase, or for a refactor of one crate; not for feature work or bug fixes (dev-loop).
---

# End-of-phase refactor

After each phase the maintainer runs one clean-up pass over the code that
phase added or changed. This skill is how. It changes how the code reads,
never what it does: the same layers, digests, refusals, errors, spans and
syscalls, byte for byte.

The loop and the rules on behaviour, tests and "a smell is a hypothesis"
are adapted from [cskwork/clean-code-skill](https://github.com/cskwork/clean-code-skill)
(MIT), which follows Fowler's *Refactoring*. No text was copied. The rest is
ranikust's own rules applied to refactoring.

## What clean means here

In order of weight:

1. **Safe by construction.** Every root path goes through
   `rnk_rootfs::RootPath`, every check acts on the fd it checked, no
   `unwrap`/indexing reachable from untrusted input, fallible calls never
   silenced (`docs/agents/rules.md`). A refactor that makes one of these
   harder to see is not clean, however short.
2. **Inside its boundaries.** Each crate's `# Boundaries` in `lib.rs` and
   `ARCHITECTURE.md` hold; a refactor never moves a responsibility across
   crates. That is an architecture change: ADR first.
3. **Readable by a stranger.** Names say what, doc comments say why, one
   level of abstraction per function, the surrounding code's idiom.
4. **No duplicate knowledge.** One place for one rule (a resolve flag set,
   a kaniko default, a whiteout form). Two pieces of code that look alike
   but answer different questions stay apart.
5. **Small.** Dead code, unused `pub`, unused dependencies, speculative
   generality removed.

## Scope and order

- Scope is the diff of the phase: `git diff <tag-or-commit-before>..main --stat`,
  crate by crate. Code the phase did not touch is out of scope unless a
  finding there blocks a change inside it.
- Order by risk, lowest first, so the tooling and habits settle before the
  sensitive crates: `rnk-ignore`, `rnk-metrics`, `rnk-oci`, `rnk-dockerfile`,
  `rnk-cache`, `rnk-sched`, `rnk-context`, `rnk-layer`, `rnk-registry`,
  `rnk-builder`, `ranikust`, then `rnk-store`, `rnk-snapshot`, `rnk-sandbox`,
  `rnk-rootfs`.
- One crate per branch: `refactor/<crate>-phase-<n>` from the latest `main`.
  Several crates may run as parallel forks in separate worktrees; never two
  on one crate.

## Per crate

### 1. Baseline (before any edit)

- `just check` green on the branch's base. If it is red, stop: fixing it is
  a separate change.
- Record what must not move:
  - `cargo test -p <crate>` summary line;
  - the crate's public API: `cargo doc -p <crate> --no-deps` and the `pub`
    items at its root;
  - for crates on the build path (`rnk-layer`, `rnk-snapshot`,
    `rnk-builder`, `ranikust`, `rnk-rootfs`): the digests of the quick smoke
    cases (`just smoke-case`; kaniko mode with `RANIKUST_CASE_MODE=kaniko`)
    built with `--reproducible`.
- Weak tests make a refactor blind. Where the code to change has none,
  write characterization tests first (what it does now, not what it
  should), commit them alone.

### 2. Find candidates

Run, read, then decide. None of these is applied blindly.

- `/simplify` on the crate's files from the phase: reuse, simplification,
  efficiency.
- `cargo clippy -p <crate> --all-targets -- -W clippy::pedantic` as a
  report only. The workspace keeps its picked lints (`Cargo.toml`
  `[workspace.lints]`); promote a pedantic lint there only with a reason
  that holds for every crate.
- `cargo machete` (or `cargo +nightly udeps`) for unused dependencies;
  `just deny` after removing one.
- Unused `pub`: `unreachable_pub` warns already; also check `pub` items no
  other crate uses (graphify: `graphify affected <symbol>`).
- Smells that matter most in this code base:
  - a long function mixing syscalls, policy and messages;
  - the same `openat2` flag set or the same check written in two places;
  - parameter lists that travel together (a struct is missing);
  - `bool` parameters that should be enums; stringly typed modes;
  - errors built with `format!` where a variant with fields belongs;
  - a `match` with `_` on our own enum (the rules forbid it);
  - comments that narrate what the next line does.
- For crates with `unsafe` or raw syscalls (`rnk-rootfs`, `rnk-sandbox`,
  `rnk-snapshot`, `rnk-store`, `rnk-layer`): `rust-review` (if installed).
  For public APIs about to be relied on: `sharp-edges`.

A smell is a hypothesis. Before changing it, say in one line what it costs
(a reader misled, a rule duplicated, a bug made likely). No cost, no
change.

### 3. The loop

For each candidate:

1. Pick the smallest transformation that removes it (rename, extract
   function, introduce a struct, replace bool with enum, move a helper to
   where its callers are, delete).
2. Make that one change.
3. `cargo check -p <crate>`, `cargo test -p <crate>`, `cargo clippy -p <crate>
   --all-targets -- -D warnings`.
4. Read the diff. If it is not clearly easier to read or safer, revert it.
5. Commit it alone: `refactor(<crate>): <what>`, the body says why.

Never mix a behaviour change into a refactor commit. A bug found on the way
gets its own `fix(...)` commit with a test that fails without it (dev-loop),
or a note in the report when it is out of scope.

### 4. Verify (after the last commit)

- `just check`; `just doc` when public items or docs changed; `just deny`
  when dependencies changed; `just check-aarch64` when `cfg`, syscalls or
  integer widths were touched.
- Public API unchanged, or changed only by removing what nothing used;
  every caller in the workspace updated in the same branch.
- The baseline digests rebuilt and identical. A digest that moves is a
  behaviour change: find it and revert it.
- Spans and their fields unchanged (`design/metrics.md`); the crate's
  `tests/spans.rs` passes.
- `rnk-rootfs`, `rnk-sandbox`, `rnk-snapshot`, `rnk-store`, or any diff
  touching paths, fds, mounts, deletion or `RUN`: the `host-safety`
  subagent on the diff, with the graphify context it cannot fetch itself
  (CLAUDE.md). Their T2 tests in the VM (`just test-vm unit`, the
  `vm-test` skill).
- `reviewer` subagent on the branch with the scope "behaviour-preserving
  refactor of <crate>" and nothing else.
- Docs: a refactor rarely changes them; when it renames or moves a public
  item, update the card, `ARCHITECTURE.md` and design references
  (`/sync-docs`). No CHANGELOG entry unless users can see it.

### 5. Report

Per crate: candidates found, applied (commit per change), rejected and why,
bugs found (fixed or noted), lines before and after, what was run and what
it printed, digests compared. Merge only when the maintainer asks.

## Do not

- Do not refactor during feature work, under a deadline, or in a crate whose
  phase is still open.
- Do not rewrite a module. If it seems beyond repair, write that up for the
  maintainer instead.
- Do not unify code that merely looks alike, change error messages users
  see, reorder syscalls, widen a resolve flag set, swap a fd-relative call
  for a path, or replace an explicit check with a "simpler" one.
- Do not touch `tests/` fixtures copied from kaniko or BuildKit, generated
  files, or `docs/research/` (frozen).
- Do not raise MSRV, add a dependency or change an edition as part of a
  clean-up.
- Do not loosen a test, `#[allow]` a lint or add `#[ignore]` to make a step
  pass.
