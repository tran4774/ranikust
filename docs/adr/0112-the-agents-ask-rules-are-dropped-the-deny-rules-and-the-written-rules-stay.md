---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0112: The agents' ask rules are dropped; the deny rules and the written rules stay

## Context and Problem Statement

[ADR-0093](0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md)
put 35 `ask` rules in `.claude/settings.json` (merge, ready, `gh api`
writes, settings commands, `git tag`, pushes that can reach `main`) and left
open whether they prompt under `defaultMode: bypassPermissions`
([#45](https://github.com/tran4774/ranikust/issues/45)). The maintainer's
sessions run in that mode, set in the gitignored
`.claude/settings.local.json`, and still needed manual approvals. Claude
Code prompts on an `ask` rule in every mode, and the rules of all settings
files are merged, so `settings.local.json` cannot lift a project rule: the
only way to stop the prompts is to change the committed file. The
maintainer works alone on the repository and wants no manual approval.

## Considered Options

- **Ask only for merge and settings changes** (the recommended option):
  keep ask for `gh pr merge` and `gh api .../merge`, pushes to `main`,
  repository settings (`gh repo edit`, `secret`, `variable`, `ruleset`,
  environments, release and tag refs through `gh api`, every `gh api -X
  DELETE`) and `gh auth`; drop the rest.
- **Drop every ask rule**: no prompt is left apart from the decision window
  (`AskUserQuestion`) and `rm` of dangerous system paths; the deny rules
  stay. Merge, push to `main` and settings changes are then held only by
  the written rules and by GitHub's `main` ruleset, which agents using the
  maintainer's admin account can bypass.
- **Change nothing.**

The first keeps a prompt on the actions that matter most; it was not taken
because the maintainer wants none. The third keeps the prompts.

## Decision Outcome

`.claude/settings.json` has no `ask` rules, so a merge, `gh pr ready`, a
push to `main`, a `gh api` write or a change of repository settings no
longer stops at a permission prompt; the deny rules and the written rules
stay as they are.

The maintainer answered on 2026-11-06, in the working session, to the
question "Session đã ở chế độ bypassPermissions. Các lần duyệt tay đến từ 35
rule `ask` trong `.claude/settings.json` (ADR-0093): Claude Code luôn hỏi
với rule `ask`, kể cả khi bypass, và `settings.local.json` không ghi đè
được. … Bạn chọn mức nào?" (The session is already in `bypassPermissions`
mode. The manual approvals come from 35 `ask` rules in
`.claude/settings.json` (ADR-0093): Claude Code always asks on an `ask`
rule, even under bypass, and `settings.local.json` cannot override it. …
Which level do you choose?), with the option "Bỏ toàn bộ ask (100%)" (drop
every ask rule). It was not the recommended option, "Chỉ hỏi khi merge/đổi
cấu hình" (ask only for merge and settings changes).

What this sets:

- **Ask:** none. The `permissions.ask` array is deleted, with its prompts
  for `gh pr merge` and `ready`, `gh api` writes and `gh api graphql`, `gh
  repo edit`, `gh secret`/`variable`/`ruleset`/`workflow`, `gh auth`
  changes, `git tag` and the pushes that can reach `main`. Under
  `bypassPermissions` the prompts left are those Claude Code never
  auto-approves: the decision window (`AskUserQuestion`), `rm` of critical
  system paths and cross-session messaging.
- **Deny and allow:** unchanged. The deny rules of ADR-0093 keep refusing
  force, tag and mirror pushes, `gh release` writes, `gh issue delete`,
  `gh repo delete`/`rename`/`archive`/`deploy-key`, the readers of the `gh`
  token, `tests/vm/runner.sh`, `sudo`, `mount` and the rest of the list.
- **Written rules:** unchanged. An agent still asks the maintainer, in the
  conversation, before it marks a pull request ready, merges, pushes
  `main`, creates or pushes a tag, releases, changes repository settings,
  rulesets or environments, or replies to or closes what other people
  opened ([ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
  D1). Only the prompt that enforced them goes.
- **Guard:** for those actions it is now the written rule, GitHub's `main`
  ruleset and the `release` environment's reviewer. Neither GitHub control
  stops the maintainer's admin account, which the agents use.
- **ADR-0093's open point** (whether ask rules prompt under
  `bypassPermissions`) is answered: they do (Evidence), and with the rules
  gone the question has no object. The rest of ADR-0093 stands: the deny
  rules, the issues agents open, and "guardrails, not a boundary".

### Consequences

- Good, because a session in `bypassPermissions` meets no permission prompt
  except the ones Claude Code never auto-approves.
- Good, because the costs ADR-0093 listed for the ask patterns go: broad
  matches (`git tag -l`, a bare `git push` of a feature branch) and prompts
  for the commit statuses and sub-issues that D1 lets agents do unasked.
- Bad, because a mistaken merge, push to `main`, `gh api` write or settings
  change now runs without a prompt and is seen only afterwards.
- Bad, because the deny rules cover `git push` of tags and `gh release`
  writes but no `gh api` path: a `gh api` write to a tag ref, a ruleset, an
  environment or a merge is held by the written rule alone [code].
- Neutral, because the rules were already a guardrail, not a boundary
  (ADR-0093): a command wrapped in another or spelled differently passed
  them.

## Evidence

No experiment; the maintainer's choice between options, and what the
documentation and the session logs show.

- [external] Claude Code documentation,
  <https://code.claude.com/docs/en/permission-modes>, "Actions no mode
  auto-approves": "Claude Code doesn't auto-approve the following in any
  mode, including `bypassPermissions` … Tools matched by an explicit ask
  rule"; and <https://code.claude.com/docs/en/permissions>: "a matching ask
  rule prompts even when a more specific allow rule also matches the same
  call". Read on 2026-11-06, Claude Code 2.1.318. This answers the open
  point of ADR-0093 and #45 ("Check whether ask rules prompt under
  `defaultMode: bypassPermissions`") by the documentation, not by an
  experiment.
- [measured] A scan of the session transcripts (542 transcripts, 43,865
  Bash calls) for commands that match the 35 rules, approximating the
  harness's `*` matching [inferred]: `gh api … -X` 68 (PATCH `pulls/N` 23,
  PUT `pulls/N/merge` 12, POST `security-advisories` 10, POST
  `issues/N/sub_issues` 5), `git push … main …` 32, `gh pr ready` 26,
  `git tag` 20 (mostly `git tag -l`), `gh pr merge` 18, `gh api graphql` 4,
  `gh repo edit` 2, bare `git push` 2. The scan includes calls made before
  the rules existed (2026-11-03), so it bounds the prompts from above.
- [code] `.claude/settings.json` at `3e8bc6e` holds 35 `ask`, 67 `allow` and
  98 `deny` entries; this change deletes the first array only.

## More Information

- Amends [ADR-0093](0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md)
  (its Ask bullet and its open point on `bypassPermissions`) and the guard
  of D1 in [ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md);
  the rest of both stands.
- [ADR-0092](0092-a-release-is-verified-by-workflow-and-tag-and-built-without-cache-or-unprotected-environment.md)
  and [ADR-0095](0095-ci-runs-by-lanes-a-fast-lane-on-drafts-and-the-full-run-when-ready-behind-two-gate-checks.md)
  name the ask rules as what holds the tag rules and `gh pr ready`; their
  text is not edited, and since this ADR the guard they name is the written
  rule.
- Implemented in `.claude/settings.json` and described in
  `docs/agents/workflow.md` (How the limits are held), `docs/testing.md`,
  `docs/security/threat-model.md` (T23) and `CLAUDE.md`.
- Answers [#45](https://github.com/tran4774/ranikust/issues/45); the pull
  request that carries this ADR closes it.
- Revisit when a bot identity or a command-parsing hook is proposed
  (ADR-0087, ADR-0093), or when anyone but the maintainer runs agents on the
  repository.
