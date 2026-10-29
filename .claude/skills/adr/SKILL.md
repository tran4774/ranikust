---
name: adr
description: Create a new Architecture Decision Record in docs/adr from docs/adr/template.md, with the next free number, and add it to the index in docs/adr/README.md. Run as /adr <title> when a decision is made, reversed or superseded.
argument-hint: "<title> [supersedes NNNN]"
disable-model-invocation: true
allowed-tools: Read Glob Grep Edit(docs/adr/**)
---

# New ADR

Record the decision described by: $ARGUMENTS

The conventions live in `docs/adr/README.md`; read it and
`docs/adr/template.md` first and follow them over anything below.

## Steps

1. **Number.** Glob `docs/adr/[0-9][0-9][0-9][0-9]-*.md`, take the highest
   number and add one. Pad to four digits. Numbers are never reused, even for
   rejected ADRs.
2. **File name.** `docs/adr/NNNN-title-with-dashes.md`: the title in lower
   case, ASCII only, words joined by `-`, no trailing punctuation. Keep it
   short; it names the problem and the chosen solution.
3. **Content.** Copy `docs/adr/template.md`, delete its comment lines, and
   fill every section:
   - front matter: `status: proposed` (the maintainer accepts it in review,
     unless the user says it is already accepted), today's date,
     `decision-makers` as in the template;
   - heading `# ADR-NNNN: <Title>`;
   - Decision Outcome starts with one sentence that states the decision on
     its own;
   - Evidence links reports under `docs/research/` or a new experiment, with
     the key numbers. Label every claim that is not measured `[code]`,
     `[external]` or `[inferred]`. Do not invent evidence: if there is none,
     say so.
   Ask the user for anything you cannot fill from the conversation or the
   repository instead of guessing (options considered, reasons, costs).
4. **Supersedes.** If the new ADR replaces ADR-MMMM, say "Supersedes
   ADR-MMMM" under More Information, and in ADR-MMMM change only the front
   matter: `status: superseded by ADR-NNNN` and `date`. Never edit the body
   of an accepted ADR.
5. **Index.** Append a row to the table under "Index" in `docs/adr/README.md`
   with the same columns as the existing rows (link, title, status, date,
   origin). Update the row of a superseded ADR too.
6. **Follow-ups.** List which of these the decision changes, and offer to
   update them in the same change: `ARCHITECTURE.md` (crate boundaries),
   `docs/design/*.md` (how it works), `docs/book/` (user-visible behaviour),
   `CHANGELOG.md` (user-visible changes), `SECURITY.md` and
   `docs/security/threat-model.md` (isolation or privilege).
7. Show the user the new file path, the index row, and the follow-up list.
