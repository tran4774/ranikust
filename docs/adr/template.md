---
# MADR 4.0 short variant, plus an Evidence section. Copy this file to
# NNNN-title-with-dashes.md (or run the /adr skill) and replace every
# placeholder. Delete these comment lines.
status: proposed # proposed | rejected | accepted | deprecated | superseded by ADR-NNNN
date: YYYY-MM-DD # date of the last status change
decision-makers: Tran Nguyen (maintainer)
---

# ADR-NNNN: {Short title naming the problem and the chosen solution}

## Context and Problem Statement

{Two to six sentences: the situation, the forces at play, and the question
being decided. Link the survey chapter or issue where the question came up.}

## Considered Options

- {Option 1}
- {Option 2}
- {Option 3}

{One line per option on why it was or was not taken, if not obvious.}

## Decision Outcome

{One sentence that states the decision on its own, so that a reader who stops
here knows what was decided.}

{Specifics: values, flags, conditions, scope, what is explicitly not covered.
Bullets are fine.}

### Consequences

- Good, because {positive consequence}.
- Bad, because {negative consequence or cost}.
- Neutral, because {optional}.

## Evidence

{Links to the experiments and survey chapters that back the decision, each with
the key numbers. Label anything that is not measured: [code], [external],
[inferred].}

- [Experiment NN: title](../research/experiments/NN-name.md): {key numbers}.
- [Survey NN: title](../research/survey/NN-name.md) §N: {what it shows}.

## More Information

{Related ADRs (supersedes / superseded by / refines), design docs that
implement the decision, open questions, and when to revisit.}
