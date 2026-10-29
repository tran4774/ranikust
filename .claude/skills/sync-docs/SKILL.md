---
name: sync-docs
description: Before opening a pull request, check that ARCHITECTURE.md, docs/design, the ADRs, CHANGELOG.md, the book and the security docs match the diff, following the "Keep in sync" rules in docs/agents/rules.md. Run as /sync-docs [base-ref].
argument-hint: "[base-ref, default main]"
disable-model-invocation: true
allowed-tools: Read Glob Grep Bash(git status *) Bash(git diff *) Bash(git log *)
---

# Check that the docs match the change

Base: `$ARGUMENTS` (use `main` when empty).

This is a check, not a rewrite. Report first; edit only what the user then
asks for.

## 1. Collect the change

- `git diff --name-status <base>...HEAD` for committed work, plus
  `git status --short` and `git diff` for uncommitted work.
- `git log --oneline <base>..HEAD` for the intent of each commit.

## 2. Apply each rule

For every rule, decide: triggered or not, with the evidence (`path:line` in
the diff), and whether the matching doc is already updated in this change.

| # | Triggered when the diff... | Doc that must change |
|---|---|---|
| 1 | adds, removes or renames a crate; moves a responsibility between crates; adds a dependency between internal crates; changes a crate's `# Boundaries` doc | `ARCHITECTURE.md` code map and boundaries |
| 2 | changes how something works: mode detection, overlay snapshots, kaniko mode, scheduling, the `RUN` sandbox, cache keys or the store | the matching `docs/design/*.md` (`modes-and-detection`, `overlay-snapshot`, `kaniko-mode`, `scheduler`, `sandbox`, `cache-and-store`) |
| 3 | makes or reverses a decision, or contradicts an accepted ADR | a new ADR (`/adr`); an accepted ADR's body is never edited |
| 4 | changes anything a user sees: a flag, a default, output, an error message, mode selection, supported environments | `CHANGELOG.md` under `Unreleased`, and the page in `docs/book/src/` |
| 5 | changes isolation, privileges, capabilities, namespaces, paths written, credential handling or the store's sharing | `SECURITY.md` scope and `docs/security/threat-model.md` |
| 6 | changes test tiers, recipes or CI jobs | `docs/testing.md`, `justfile` and `.github/workflows/ci.yml` stay identical, `CONTRIBUTING.md`, the commands in `CLAUDE.md` and `docs/agents/workflow.md` |
| 7 | ports code from kaniko or another project | file header, `NOTICE` entry, source commit (see `CONTRIBUTING.md`) |

Also check:

- every link added in the diff points at a file that exists;
- `docs/research/` is unchanged (it is frozen) unless the change is a typo
  fix the user asked for;
- a crate's `# Boundaries` doc and `ARCHITECTURE.md` say the same thing.

## 3. Report

A table with one row per rule: rule, triggered (yes/no), evidence, doc status
(updated / missing / not needed). Then a short list of the missing updates,
each with the exact file and what it should say. If nothing is missing, say
so in one line.
