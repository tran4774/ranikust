## Summary

<!-- What changes and why, for a reader who has not seen your work so far.
     Link the issue. The PR title follows Conventional Commits, because it
     becomes the squash commit message. -->

## Test plan

<!-- The commands you ran and what they showed. Name the tiers (T0-T3, see
     docs/testing.md) and say which ones you could not run and why. -->

Modes affected:

- [ ] overlay (root)
- [ ] overlay (rootless)
- [ ] kaniko
- [ ] kaniko with `--parallel-stages`
- [ ] none (no mode-specific behaviour)

## Checklist

- [ ] `just ci` passes locally.
- [ ] A bug fix includes a test that fails without the fix.
- [ ] Docs are in sync, per "Keep in sync" in `docs/agents/rules.md`, or none were
      needed: `ARCHITECTURE.md`, `docs/design/`, a new ADR for a new or
      reversed decision, `CHANGELOG.md` under Unreleased for user-visible
      changes, the book, `SECURITY.md` and the threat model.
- [ ] Code ported from kaniko or another project has a file header and a
      `NOTICE` entry (see `CONTRIBUTING.md`).

## AI assistance

- [ ] No AI tools were used.
- [ ] AI tools were used. Which, and for what:
      <!-- e.g. "Claude Code: wrote the first draft of the tests" -->
      I have reviewed, understood and tested all of the change.

## Suggested agent rule additions

<!-- Optional. A rule for `docs/agents/rules.md` that this work showed is needed: a
     non-obvious trap that will recur, stated specifically. Leave empty if
     none; maintainers add accepted rules in a separate commit. -->
