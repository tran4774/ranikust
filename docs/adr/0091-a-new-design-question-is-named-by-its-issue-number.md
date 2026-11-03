---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0091: A new design question is named by its issue number

## Context and Problem Statement

[ADR-0088](0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md)
moved the open design questions from `docs/open-questions.md` to GitHub
issues labelled `question`. Until then every question had a hand-allocated
number, `Q-NNN`, that the register, the `[open: Q-NNN]` markers in code and
docs, and the ADRs that answered it all used; parallel agents were each given
a range of numbers so that two branches would not take the same one.
ADR-0088 does not say how a question raised after the move is named: it can
keep a `Q-NNN` in its title as well as its issue number, or be named by its
issue number alone.

## Considered Options

- **Issue number only.** A new question is `#N`; its markers are
  `[open: #N]`; it has no `Q-NNN`.
- **Keep `Q-NNN`.** A new question's title starts with the next free
  `Q-NNN`, as before, besides its issue number.

Keeping `Q-NNN` is rejected: someone has to find the next free number by
hand across the issues and the archive, and two agents raising questions on
parallel branches can take the same one, which is why each needed its own
range; GitHub already gives every issue a unique number at creation.

## Decision Outcome

A new design question is named only by its GitHub issue number: its title
is the question alone, and every marker that waits on it is `[open: #N]`.

The maintainer answered on 2026-11-03, in the working session, the question
"câu hỏi MỚI đánh số thế nào?" (how are NEW questions numbered?) with the
option "Chỉ dùng số issue (Recommended)" (use the issue number only).

- The 29 questions migrated on 2026-11-03 (issues #1 to #31) keep their
  `Q-NNN:` title prefix, so the ADRs and documents that name them by
  `Q-NNN` still lead to them; `docs/open-questions.md` keeps a heading for
  each that points at its issue.
- No new `Q-NNN` is allocated. Parallel agents get ranges of ADR numbers
  only.
- In a `///` or `//!` comment the marker is written `\[open: #N]`, because
  rustdoc reads `[open: #N]` as a link.
- `just docs-lint` checks that every `[open: #N]` names an open issue
  labelled `question`.

### Consequences

- Good, because a question's identity comes from GitHub at creation, so no
  two branches can collide and no one has to look up the next free number.
- Good, because a marker is one lookup away from its issue.
- Bad, because questions now have two naming schemes: the 29 migrated ones
  are known by both `Q-NNN` and `#N`, later ones by `#N` only.
- Bad, because `#N` means nothing outside GitHub: in a clone without
  network access a marker cannot be resolved to its question [inferred].

## Evidence

No experiment; the maintainer's choice between two options.

- [code] `docs/agents/workflow.md` (before this change) gave each fork "its
  own range of question and ADR numbers in its brief" to avoid collisions.
- [code] The migration of 2026-11-03 created issues #1 to #31 for the 29
  questions, with pull requests taking the numbers in between: issue
  numbers are allocated by GitHub, shared with pull requests, and unique.

## More Information

- Refines [ADR-0088](0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md),
  which moved the questions to issues; companion of
  [ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md).
- Implemented in `docs/agents/rules.md`, `docs/agents/workflow.md`,
  `CLAUDE.md`, `CONTRIBUTING.md`, `docs/open-questions.md`, the Design
  question issue form, the `scribe` agent, the `/adr` skill and
  `scripts/docs-lint.py`.
