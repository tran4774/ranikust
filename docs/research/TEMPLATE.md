# Research report templates

Every file under `docs/research/` follows one of the two templates below.
Copy the skeleton, keep the section order, and delete sections only where the
template says they are optional.

## Conventions

**Evidence labels.** Every non-obvious claim carries a label. Use one label;
combine two only when both apply, for example a documented behaviour that was
also reproduced locally (**[external]** **[measured]**):

| Label | Meaning |
|---|---|
| **[measured]** | Observed in an experiment; the report says where (experiment, run, table). |
| **[code]** | Read from source code; cite `repo@commit path:line`. |
| **[external]** | From a third-party source; cite the URL and the access date. |
| **[inferred]** | Our reasoning or estimate, not verified. |

**Numbers.** Timings are medians with `(min–max)` and the number of rounds,
for example `9.95 s (8.13–11.52), 5 rounds`. Always state the CPU limit and
whether the page cache was warm. Use `.` as the decimal separator and SI units
(`s`, `ms`, `MB`, `MiB`, `GB`).

**Source code references.** kaniko references use
`kaniko@44aa7ddc0 pkg/…:line` (see the snapshot note in `README.md`).

**Cross-references.** Link ADRs as `[ADR-0012](../../adr/0012-….md)`, other
reports by relative path, and design docs under `../../design/`.

**Tone.** Neutral, factual, past tense for what was done, present tense for
what holds. No marketing, no first person singular. Prefer tables to prose for
results.

---

## Template A: survey chapter (`survey/NN-*.md`)

```markdown
# <Title>

| | |
|---|---|
| **Status** | Frozen, 2026-10-29 |
| **Original** | `docs-rust/NN-….md` (Vietnamese, not in this repository) |
| **Superseded by** | <ADRs / design docs / ARCHITECTURE.md that now own this content, or "—"> |

## Summary

3–6 bullets: the questions this chapter answers and the answers.

## <Content sections>

Keep the original structure where it serves the reader; merge or reorder
where it does not. Every claim keeps its evidence label.

## Open questions

What the chapter left unresolved (optional if none).

## References

Numbered list of experiments, external sources and code locations used above.
```

## Template B: experiment report (`experiments/NN-*.md`)

```markdown
# Experiment NN: <Title>

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | One sentence. |
| **Answer** | One or two sentences, with the headline number. |
| **Related** | ADR-00xx, design docs, other experiments |
| **Original** | `docs-rust/poc/NN-…/` (Vietnamese, not in this repository) |

## Summary

3–6 bullets: what was tested, what was found, what it changes.

## Questions

Numbered list of the precise questions the experiment set out to answer.

## Environment

Host, kernel, CPU/RAM, container runtime and version, privilege level
(rootless podman / real root / rootful Docker in a VM), filesystems, relevant
security profiles (seccomp, AppArmor), cgroup version, and the load on the
machine if it was shared.

## Method

What was run and how: configurations, variants (give each a short name used
in the tables), number of rounds, timing method, how correctness was checked.

## Results

Tables first, then short commentary. Label every row or cell that is not
**[measured]**.

## Findings

Numbered, each one sentence plus its evidence (table/section reference).

## Implications for ranikust

What the design does because of this, linked to the ADR or design doc.

## Limitations and open questions

What was not covered, known measurement caveats, and follow-ups.

## Reproduction

How the experiment was run: scripts, images, commands, and the original PoC
location. PoC sources are archived outside this repository.

## References

Numbered list.
```
