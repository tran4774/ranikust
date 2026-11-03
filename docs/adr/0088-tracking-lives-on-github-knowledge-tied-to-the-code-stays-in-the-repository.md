---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0088: Tracking lives on GitHub; knowledge tied to the code stays in the repository

## Context and Problem Statement

With the work moving onto GitHub issues and pull requests
([ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)),
four kinds of repository file still track work: the phase plans
(`docs/agents/phase-N.md`, checklists ticked by hand), the register of open
questions (`docs/open-questions.md`, 29 open entries, each pointed at from
code by an `[open: Q-NNN]` marker), the `Unreleased` section of
`CHANGELOG.md` (about 700 lines), and the agents' progress notes in local
memory. The maintainer: "Tôi đang muốn chuyển sang toàn bộ trên github,
không dùng các file trên repo nữa" (I want to move everything to GitHub and
stop using the files in the repository). The question is how much of that
can move, given that ADRs, design documents and the rules are cited from
code and read by agents that cannot reach GitHub.

## Considered Options

- **Option A: tracking moves to GitHub; knowledge tied to the code stays in
  the repository.** Plans, open questions, changelog entries and progress
  notes move; ADRs, design documents, research, the threat model,
  `rules.md`, crate cards and the book stay and change only through pull
  requests.
- **Option B: everything moves to GitHub, ADRs and design documents
  included.**

Option B is rejected:

- Code and tests cite ADRs about 900 times (908 mentions in 188 files of
  `crates/` and `tests/`), and a citation must stay a stable, reviewable
  pointer.
- Accepted ADRs are immutable, while issue text is editable and has no git
  history.
- The `host-safety` and `upstream-oracle` subagents cannot run Bash, so
  they could not read GitHub.
- graphify indexes only repository files.
- Every read would cost a network call, under GitHub's limit of 5,000
  requests per hour, and would stop working when GitHub is down.
- A clone or mirror of the repository would lose the history.

## Decision Outcome

Tracking moves to GitHub (Option A), and knowledge tied to the code stays in
the repository and changes only through pull requests.

Maintainer, 2026-11-03, after options A and B were laid out: "D3 chọn A"
(D3: choose A).

- **Phase plans.** Each phase is a milestone plus a tracking issue (scope
  and acceptance criteria) with one sub-issue per task, followed on the
  Project. `docs/agents/phase-1.md` and `docs/agents/phase-2.md` stay frozen
  as history; later phases get no `phase-N.md`.
- **Open questions.** Each becomes an issue labelled `question`: the title
  keeps `Q-NNN: ...` and the body keeps Context, Options, Recommendation,
  Interim and Blocks. The maintainer answers in a comment, and the issue
  closes when the ADR that records the answer merges. The markers become
  `[open: #N]`, and `just docs-lint` checks through the GitHub API that
  issue #N is still open. The Answered section of `docs/open-questions.md`
  stays as a frozen archive, because ADRs link to it.
- **Changelog entries.** A pull request writes them in a `## Changelog`
  section of its body, and no pull request edits `CHANGELOG.md`. At release
  time the sections are gathered into the release notes and into the
  version section of `CHANGELOG.md`
  ([ADR-0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md)).
- **Progress notes.** What an agent needs to know about where the work
  stands comes from the Project and its issues, not from local memory.
- **Stays in the repository,** changed only through pull requests: ADRs
  (each new ADR is its own pull request and is discussed there; merging it
  is accepting it), design documents, research, the threat model,
  `docs/agents/rules.md`, crate cards and the book.

### Consequences

- Good, because plans, questions and progress sit beside the pull requests
  and the Project, whose Status field has a `Needs decision` value for the
  questions.
- Good, because the changelog entry travels with the change it describes and
  no longer conflicts with parallel branches: GitHub's server-side merge is
  expected not to apply the `merge=union` driver that `.gitattributes` sets
  for `CHANGELOG.md` [inferred; to be confirmed by the first parallel pull
  requests].
- Bad, because `docs/adr/README.md` has the same `merge=union` driver and
  every ADR pull request appends an index row to it: parallel ADR pull
  requests may conflict there, and the later one is rebased on `main`
  before it merges [inferred].
- Good, because citations of ADRs, design documents and rules keep
  working offline, and every read of them is a local file.
- Bad, because the tracking is no longer in a clone: an offline reader does
  not see the plan or the open questions, and `just docs-lint` needs network
  access to GitHub for the marker check [inferred].
- Bad, because the subagents that cannot run Bash (`host-safety`,
  `upstream-oracle`) cannot read an issue; the main session passes in the
  text they need, as it does for graphify output today [inferred].
- Bad, because the 29 open questions and their markers have to be migrated
  once, and until then the old format stays valid [inferred].

## Evidence

- [code] `git grep -o "ADR-[0-9]\{4\}" -- crates tests` finds 908 mentions
  in 188 files at the commit that adds this ADR.
- [code] `docs/open-questions.md` has 29 entries above its Answered section;
  `CHANGELOG.md` has 707 lines, almost all under `Unreleased`;
  `.gitattributes` sets `merge=union` for `CHANGELOG.md` and
  `docs/adr/README.md`.
- [external] GitHub's REST API allows an authenticated user 5,000 requests
  per hour.
- [inferred] GitHub's server-side merge ignores the `merge=union` attribute;
  not verified, see Consequences.
- [code] [ADR-0001](0001-record-architecture-decisions.md) already
  considered "Record decisions in GitHub issues or discussions" and kept
  ADRs in the repository because issues are hard to find from the code and
  live outside it.

## More Information

- Agrees with ADR-0001 and does not change it: ADRs stay in `docs/adr/`.
  Companions: [ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
  (the flow) and [ADR-0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md)
  (releases).
- Follow-ups that land with their implementation, not with this ADR: the
  migration of the open questions and their markers, `scripts/docs-lint.py`,
  the tracking issues and sub-issues of the next phases, the pull request
  template's changelog section, `CONTRIBUTING.md`, `CLAUDE.md`,
  `docs/agents/rules.md` (the keep-in-sync matrix row for `CHANGELOG.md`),
  `docs/agents/workflow.md` and `docs/open-questions.md`. Until they land,
  those files still describe the repository-based tracking.
