---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0112: The agents' ask rules are dropped and `gh api` deletes and release writes are denied; the written rules stay

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

A review of dropping the rules found a gap. The deny rules of ADR-0093
match only the `gh <subcommand>` spellings (`gh release delete`, `gh repo
delete`, `gh issue delete`); the `gh api` forms of the same deletes, and of
a release write (the `PATCH` that once lost the tag of v0.0.1), were stopped
by `ask` alone.

## Considered Options

For the ask rules:

- **Ask only for merge and settings changes** (the recommended option):
  keep ask for `gh pr merge` and `gh api .../merge`, pushes to `main`,
  repository settings (`gh repo edit`, `secret`, `variable`, `ruleset`,
  environments, release and tag refs through `gh api`, every `gh api -X
  DELETE`) and `gh auth`; drop the rest.
- **Drop every ask rule**: no prompt is left but the ones Claude Code never
  auto-approves (the decision window, `AskUserQuestion`; `rm` of critical
  system paths; cross-session messaging; the offer named the first two); the
  deny rules stay. Merge, push to `main` and settings changes are then held
  only by the written rules and by GitHub's `main` ruleset, which agents
  using the maintainer's admin account can bypass.
- **Change nothing.**

For the `gh api` forms once the ask rules are gone:

- **Deny them** (the recommended option): every `gh api` with `-X DELETE` or
  `--method DELETE` in every spelling, GraphQL mutations whose name starts
  with `delete`, and every `gh api` write to `releases`. The agent is refused
  with no prompt, and the maintainer runs such a call himself with `! gh api
  …`. Still a text guardrail.
- **Add nothing.**

The first of the three ask options keeps a prompt on the actions that matter
most; it was not taken because the maintainer wants none. The third keeps
the prompts. A deny rule never shows a prompt, so denying the `gh api` forms
keeps the session automatic.

## Decision Outcome

`.claude/settings.json` has no `ask` rules, so a merge, `gh pr ready`, a
push to `main`, most `gh api` writes and a change of repository settings no
longer stop at a permission prompt, and it gains `deny` rules that refuse
every `gh api` delete, GraphQL `delete…` mutation, `gh api` write to
`releases` and `gh api` call naming `refs/tags`; the written rules stay as
they are.

The maintainer answered twice on 2026-11-06, in the working session:

- (a) To the question "Session đã ở chế độ bypassPermissions. Các lần duyệt
  tay đến từ 35 rule `ask` trong `.claude/settings.json` (ADR-0093): Claude
  Code luôn hỏi với rule `ask`, kể cả khi bypass, và `settings.local.json`
  không ghi đè được. … Bạn chọn mức nào?" (The session is already in
  `bypassPermissions` mode. The manual approvals come from 35 `ask` rules in
  `.claude/settings.json` (ADR-0093): Claude Code always asks on an `ask`
  rule, even under bypass, and `settings.local.json` cannot override it. …
  Which level do you choose?), the option "Bỏ toàn bộ ask (100%)" (drop
  every ask rule). It was not the recommended option, "Chỉ hỏi khi merge/đổi
  cấu hình" (ask only for merge and settings changes).
- (b) To the question "Reviewer phát hiện một lỗ khi bỏ hết `ask`. Rule
  `deny` hiện chỉ bắt cách gõ `gh release delete`, `gh repo delete`, `gh
  issue delete`… Trước đây, dạng `gh api` của các lệnh đó vẫn bị `ask` chặn
  lại. … Có thêm rule `deny` cho các dạng `gh api` này không? Rule `deny`
  không bao giờ hiện prompt, nên vẫn giữ được 100% auto." (A reviewer found a
  hole in dropping every `ask`. The `deny` rules only catch the spelling `gh
  release delete`, `gh repo delete`, `gh issue delete`… Before, the `gh api`
  form of those commands was still stopped by `ask`. … Should `deny` rules be
  added for these `gh api` forms? A `deny` rule never shows a prompt, so
  everything stays 100% automatic.), the option "Thêm deny cho gh api
  xoá/release (khuyến nghị)" (add deny rules for `gh api` deletes and release
  writes; the recommended option). The other option was "Không thêm" (add
  none). The question's examples of calls that would run unprompted:
  `gh api -X DELETE …/releases/N`, `gh api -X DELETE repos/<o>/<r>`, the
  GraphQL mutation `deleteIssue`, and a `gh api` `PATCH` of a release.

What this sets:

- **Ask:** none. The `permissions.ask` array is deleted, with its prompts
  for `gh pr merge` and `ready`, `gh api` writes and `gh api graphql`, `gh
  repo edit`, `gh secret`/`variable`/`ruleset`/`workflow`, `gh auth`
  changes, `git tag` and the pushes that can reach `main`. Under
  `bypassPermissions` the prompts left are those Claude Code never
  auto-approves: the decision window (`AskUserQuestion`), `rm` of critical
  system paths and cross-session messaging.
- **Deny:** the rules of ADR-0093 stay (force, tag and mirror pushes, `gh
  release` writes, `gh issue delete`, `gh repo
  delete`/`rename`/`archive`/`deploy-key` by their `gh` spelling, the readers
  of the `gh` token, `tests/vm/runner.sh`, `sudo`, `mount` and the rest), and
  25 patterns are added. They refuse:
  - any `gh api` with `-X` or `--method` `DELETE`, upper or lower case, with
    a space, with `=` or run together (`-XDELETE`, `--method=delete`);
  - a `gh api` call whose text contains `graphql` and then `delete` (the
    `delete…` mutations);
  - a `gh api` write to a path naming `releases`: `-X`, `--method`, `-f`,
    `-F`, `--field`, `--raw-field` or `--input`;
  - any `gh api` naming `refs/tags`, the `gh api` form of the tag push that
    is already denied (a `v*` tag starts the release workflow,
    [ADR-0092](0092-a-release-is-verified-by-workflow-and-tag-and-built-without-cache-or-unprotected-environment.md)).

  The agent is refused with no prompt; the maintainer runs such a call
  himself with `! gh api …`. The patterns also refuse a few reads: any `gh
  api` naming `refs/tags`, and a `releases` read that passes `-X GET` or a
  field [inferred from the patterns].
- **Still unprompted, held by the written rule alone** [inferred from the
  patterns]: the `gh api` forms of `gh repo rename` and `archive` (`PATCH`
  `repos/<o>/<r>`) and of `gh repo deploy-key` (`POST …/keys`), merges (`PUT
  …/merge`), and writes to rulesets and environments.
- **Allow:** unchanged.
- **Written rules:** unchanged. An agent still asks the maintainer, in the
  conversation, before it marks a pull request ready, merges, pushes
  `main`, creates or pushes a tag, releases, changes repository settings,
  rulesets or environments, or replies to or closes what other people
  opened ([ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
  D1). Only the prompt that enforced them goes.
- **Guard:** for those actions it is the written rule, plus the deny rules
  above for the forms they match, plus GitHub's `main` ruleset, the `release
  tags` ruleset and the `release` environment's reviewer. None of the three
  GitHub controls stops the maintainer's admin account, which the agents
  use.
- **ADR-0093's open point** (whether ask rules prompt under
  `bypassPermissions`) is answered: they do, and deny rules hold (Evidence);
  with the ask rules gone the question has no object. The rest of ADR-0093
  stands: the issues agents open and "guardrails, not a boundary".

### Consequences

- Good, because a session in `bypassPermissions` meets no permission prompt
  except the ones Claude Code never auto-approves.
- Good, because the costs ADR-0093 listed for the ask patterns go: broad
  matches (`git tag -l`, a bare `git push` of a feature branch) and prompts
  for the commit statuses and sub-issues that D1 lets agents do unasked.
- Good, because an irreversible `gh api` delete, a release write or a call
  on a tag ref is refused with no prompt, so the session stays automatic.
- Bad, because a mistaken merge, push to `main`, `gh api` write or settings
  change now runs without a prompt and is seen only afterwards.
- Bad, because the `gh api` forms of `gh repo rename`/`archive`/`deploy-key`,
  merges, rulesets and environments match no deny rule and are held by the
  written rule alone [inferred from the patterns].
- Bad, because the new patterns also refuse a few reads, which the
  maintainer then runs himself [inferred from the patterns].
- Neutral, because the rules were already a guardrail, not a boundary
  (ADR-0093): a command wrapped in another or spelled differently passed
  them.

## Evidence

No experiment; the maintainer's choices between options, what the
documentation says, and what the session logs show.

- [external] Claude Code documentation,
  <https://code.claude.com/docs/en/permission-modes>, "Actions no mode
  auto-approves": "Claude Code doesn't auto-approve the following in any
  mode, including `bypassPermissions` … Tools matched by an explicit ask
  rule"; and <https://code.claude.com/docs/en/permissions>: "a matching ask
  rule prompts even when a more specific allow rule also matches the same
  call". Read on 2026-11-06, Claude Code 2.1.318.
- [external] The same page, "Available modes": "Deny rules block in every
  mode, including `bypassPermissions`." Read on 2026-11-06, Claude Code
  2.1.318. With the line above this answers #45 ("Check whether ask rules
  prompt under `defaultMode: bypassPermissions`", which [ADR-0097](0097-agents-work-in-fresh-bounded-contexts-instead-of-forks.md)
  extends to deny rules) and the open point of ADR-0093: ask rules prompt,
  deny rules hold. It is the documentation, not an experiment.
- [measured] A scan of the session transcripts (542 transcripts, 43,865
  Bash calls) for commands that match the 35 ask rules: `gh api … -X` 68,
  `git push … main …` 32, `gh pr ready` 26, `git tag` 20 (mostly `git tag
  -l`), `gh pr merge` 18, `gh api graphql` 4, `gh repo edit` 2, bare `git
  push` 2. The matching approximates the harness's `*` matching, so the
  counts are indicative, and the scan includes calls made before the rules
  existed (2026-11-03), so it bounds the prompts from above.
- [measured] The same transcripts, the same approximate matching, against
  the new deny rules. Of the past `gh api` calls they would have refused `-X
  DELETE …/releases/N`, `-X DELETE …/git/refs/tags/<name>`, two `-X PATCH
  …/releases/N` (one of them the `PATCH` that lost the tag of v0.0.1) and `-X
  PUT …/immutable-releases`, a settings write that `*-X*releases*` refuses
  incidentally because its path contains `releases`. They would have left
  the routine writes: `PATCH` `pulls/N` (pull request bodies), `POST`
  `issues/N/sub_issues` and `dependencies/blocked_by`, GraphQL, `POST`
  `security-advisories`, and also `PUT` `pulls/N/merge` and writes to
  rulesets and environments, which the written rule alone holds. The
  maintainer's question counted 4 calls of this kind in the history, all
  made at his request.
- [code] `.claude/settings.json` at `3e8bc6e` holds 35 `ask`, 67 `allow` and
  98 `deny` entries; this change deletes the first array and adds 25 `deny`
  entries (123).

## More Information

- Amends [ADR-0093](0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md)
  (its Ask bullet, its Deny bullet, which gains the `gh api` forms above,
  and its open point on `bypassPermissions`) and the guard of D1 in
  [ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md);
  the rest of both stands.
- [ADR-0092](0092-a-release-is-verified-by-workflow-and-tag-and-built-without-cache-or-unprotected-environment.md)
  and [ADR-0095](0095-ci-runs-by-lanes-a-fast-lane-on-drafts-and-the-full-run-when-ready-behind-two-gate-checks.md)
  name the ask rules as what holds the tag rules and `gh pr ready`; their
  text is not edited, and since this ADR the guard of `gh pr ready` is the
  written rule alone, and the guard of tags is the written rule plus the deny
  rules on a tag push and on `gh api` calls naming `refs/tags`.
- [ADR-0097](0097-agents-work-in-fresh-bounded-contexts-instead-of-forks.md)
  ties #45 to whether ask and deny rules hold under `bypassPermissions`;
  this ADR answers it.
- Implemented in `.claude/settings.json` and described in
  `docs/agents/workflow.md` (How the limits are held), `docs/testing.md`,
  `docs/security/threat-model.md` (T23), `CONTRIBUTING.md` and `CLAUDE.md`.
- Answers [#45](https://github.com/tran4774/ranikust/issues/45); the pull
  request that carries this ADR closes it.
- Revisit when a bot identity or a command-parsing hook is proposed
  (ADR-0087, ADR-0093), or when anyone but the maintainer runs agents on the
  repository.
