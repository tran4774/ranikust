---
name: sync-docs
description: Before opening a pull request, check that ARCHITECTURE.md, docs/design, the ADRs, the pull request's Changelog section, the book and the security docs match the diff, following the "Keep in sync" rules in docs/agents/rules.md. Run as /sync-docs [base-ref].
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

The rules are the rows of the "Keep in sync" table in
`docs/agents/rules.md`; read it, it is the one list. For every row, decide:
triggered or not, with the evidence (`path:line` in the diff), and whether
the matching doc is already updated in this change. Easy to miss: an error
message or the supported environments (user-visible); mode detection,
overlay snapshots, kaniko mode, scheduling, the `RUN` sandbox, cache keys or
the store (a design doc); capabilities, namespaces, paths written,
credential handling or the store's sharing (`SECURITY.md`, the threat
model); a change that contradicts an accepted ADR (a new ADR, never an edit
of the old one).

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
