---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0001: Record architecture decisions

## Context and Problem Statement

ranikust starts from a survey that ran sixteen experiments and settled twelve
open questions (Q1 to Q12) before any code was written. That survey is frozen
in [`docs/research/`](../research/README.md). The design documents will keep
changing with the code. Neither place is suitable for answering "why is it
like this?" a year from now: the research does not change, and the design
docs describe only the current state. Contributors and coding agents need a
stable, citable record of each decision, its alternatives and its evidence.

## Considered Options

- Architecture Decision Records in `docs/adr/`, MADR 4.0 short variant.
- Keep the decisions table in the survey (survey 05) as the only record.
- Explain decisions inline in the design docs.
- Record decisions in GitHub issues or discussions.

The survey is frozen at its pre-code state and cannot record later decisions;
design docs get rewritten; issues are hard to find from the code and live
outside the repository.

## Decision Outcome

Every architecturally significant decision is recorded as one ADR in
`docs/adr/`, using the MADR 4.0 short variant plus an Evidence section.

- File name `NNNN-title-with-dashes.md`; numbers are never reused.
- At most about one page. The decision is stated in one sentence at the top
  of Decision Outcome.
- An accepted ADR is not edited except for its status. A change of mind is a
  new ADR that supersedes the old one.
- Evidence links point to experiment reports and survey chapters under
  `docs/research/`; unmeasured claims carry `[inferred]`.
- Design docs describe the current design and link the ADRs for the "why".
- The process is described in [`README.md`](README.md); the `/adr` skill
  automates numbering and the index.

### Consequences

- Good, because the reasoning behind each choice survives code rewrites and
  people leaving.
- Good, because reviewers and agents can check a change against a short,
  stable list of decisions.
- Bad, because every significant change needs an extra document, and a
  decision changed twice must be read as a chain of ADRs.
- Bad, because links into `docs/research/` must stay valid while that tree is
  frozen.

## Evidence

- Repository setup study, `docs-rust/setup/01-nghien-cuu-tai-lieu-va-skill.md`
  §3.2 (Vietnamese, not in this repository): compares ADR practice in Rust
  and container-tooling projects and proposes this layout [external].
- [MADR 4.0.0](https://adr.github.io/madr/), released 2024-09-17 [external].
- [Survey 05: decisions](../research/survey/05-decisions.md): the Q1–Q12
  decisions that ADR-0003 to ADR-0014 record.

## More Information

ADR-0003 to ADR-0014 map one-to-one to Q1 to Q12. ADR-0015 to ADR-0020 record
decisions that came out of the experiments. Revisit this ADR only if the team
moves decision records out of the repository.
