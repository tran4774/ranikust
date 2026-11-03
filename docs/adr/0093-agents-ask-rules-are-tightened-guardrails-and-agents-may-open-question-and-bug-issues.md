---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0093: The agents' ask and deny rules are tightened guardrails, and agents may open `question` and `bug` issues

## Context and Problem Statement

[ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
(D1) lists what agents may do on GitHub without asking and what needs the
maintainer, and says the guard is the agent rules and the ask rules of
`.claude/settings.json`. The first ask and deny rules written for it
(pull request #43) named a few `gh api` paths and push spellings; review
found them leaky: a `gh api` write to any other path, a push of `main`
spelled `HEAD`, `+main` or `--all`, a tag pushed by name, a force push
written after the branch, and the commands that print or hand out the
`gh` token all passed. Two questions followed: how to close that, and
whether an agent may open a `question` issue, or a `bug` issue for a flaky
test, without asking, since D1 allowed only issues "for work already in a
plan".

## Considered Options

For the rules:

- **Tighten the patterns** (ask on every `gh api` that can write, every
  spelling of a push to `main`; deny tag pushes, force pushes, mirror
  pushes, release writes and token readers), and document them as
  guardrails.
- **Keep the narrow patterns** and rely on the written rule.
- **A real boundary now** (a hook that parses the command, the sandbox, or
  a bot identity with its own permissions); not proposed for this change.

For the issues:

- **Agents open `question` and `bug` issues without asking.**
- **They ask first**, as for any issue outside a plan.

## Decision Outcome

The ask and deny rules are tightened to the reviewer's patterns and
documented as guardrails, not a boundary; and agents may open `question`
and `bug` issues without asking.

The maintainer answered on 2026-11-03, in the working session:

- (a) To the question "PR #43 thêm rule ask/deny … Làm thế nào?" (PR #43
  adds ask/deny rules … how to proceed?), the option "Siết pattern
  (Recommended)" (tighten the patterns): apply the reviewer's tightened
  patterns, document them as guardrails, not a boundary, and check whether
  ask rules prompt under `bypassPermissions`.
- (b) To the question "Agent có được tự mở issue `question` … và issue báo
  test flaky không cần hỏi không?" (may an agent open a `question` issue …
  and an issue reporting a flaky test without asking?), the option "Được,
  không cần hỏi (Recommended)" (yes, without asking).

What this sets:

- **Ask** (the maintainer confirms each time): every `gh api` call that can
  write (`-X`, `--method`, `-f`, `-F`, `--input`, and every `gh api
  graphql`); `gh pr merge` and `gh pr ready`; `gh repo edit`, `gh secret`,
  `gh variable`, `gh ruleset`, `gh workflow`, `gh issue transfer`, `gh label
  edit` and `delete`, `gh auth refresh`, `login`, `logout` and `switch`,
  `gh run delete`, `gh cache delete`; `git tag`; and a push that can reach
  `main`: a bare `git push`, a push ending in `origin`, `HEAD`,
  `HEAD:main`, `main`, `<ref>:main`, `refs/heads/main` anywhere, `--all`.
- **Deny** (never, even when the maintainer is asked): pushing a tag (by a
  `v…` name, `refs/tags/` anywhere, `--tags` or `--follow-tags`),
  `--mirror`, any force push (`--force`, `-f`, `--force-with-lease`,
  wherever it is written, and a `+<ref>` refspec); `gh release create`,
  `delete`, `edit` and `upload` (`gh release list` and `view` are allowed);
  `gh issue delete`; `gh repo delete`, `rename`, `archive` and
  `deploy-key`; reading the token (`gh auth token`, `gh auth status -t` or
  `--show-token`, `gh auth git-credential`, `git credential`, `gh config
  get` of `oauth_token`, reading `~/.config/gh/`, `rg` over `.config/gh` or
  for `oauth_token`); and executing `tests/vm/runner.sh`, which runs the
  suites as root on a CI runner (`tests/vm/runner.sh`, `./tests/vm/…`,
  `bash` or `sh` of it; `sudo` is denied anyway). That deny was part of
  the text of the option the maintainer chose ("Siết pattern"); it is
  written for the forms that execute the script, so reading or searching
  it stays possible.
- **Guardrails, not a boundary.** The rules match command text: a command
  wrapped in another (`bash -c`, a script, an alias) or spelled
  differently slips past them, and GitHub cannot tell an agent from the
  maintainer while both use the maintainer's account. The boundary is a bot
  identity with its own permissions (ADR-0087's possible later step) or a
  hook that parses the command. Whether ask rules still prompt under
  `defaultMode: bypassPermissions` is not verified; it is not tested by
  experiment on the maintainer's machine, but by `poc-runner` in the VM or
  a throwaway session ([#45](https://github.com/tran4774/ranikust/issues/45)).
- **Issues.** Amending D1, agents may open, without asking, a `question`
  issue (a design question, ADR-0091) and a `bug` issue (for example a
  flaky test), besides issues for work already in a plan. Each carries the
  `agent` label, and none holds the details of an unfixed vulnerability
  (D6). Replying to or closing issues and pull requests other people opened
  still needs the maintainer.

### Consequences

- Good, because the common ways to push to `main`, push or publish a tag,
  rewrite history or leak the token now stop at a prompt or a refusal.
- Good, because an agent that meets an undecided question or a flaky test
  records it on GitHub at once, where the maintainer sees it.
- Bad, because the patterns are broad: a harmless `gh api` read that
  happens to contain `-f` or `-F`, any `git tag -l`, and a bare `git push`
  of a feature branch now ask [inferred from the patterns].
- Bad, because two things D1 lets agents do without asking are `gh api`
  writes and now prompt: posting a commit status and adding a sub-issue
  (GraphQL `addSubIssue`).
- Bad, because the rules remain a guardrail: a wrapped command passes, and
  until a bot identity exists nothing on GitHub's side enforces the limits.
- Neutral, because whether ask rules prompt under `bypassPermissions` is
  open until someone checks it outside the maintainer's working session
  ([#45](https://github.com/tran4774/ranikust/issues/45)).

## Evidence

No experiment; the maintainer's choices between options.

- [code] The review of pull request #43 at commit `a594582` listed the
  spellings that passed the first rules (the ones the Context names).
- [code] A precedence check of the new `.claude/settings.json` over 102
  sample commands, approximating the harness's `*` matching (deny before
  ask before allow): the read-only `gh` commands stay allowed, feature
  branch pushes stay with the session's mode, and every listed write lands
  on ask or deny. The real matcher is the harness's, not this
  approximation [inferred].

## More Information

- Amends D1 of [ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
  (which issues agents open without asking) and its guard (the ask and deny
  rules); the rest of ADR-0087 stands. Question naming:
  [ADR-0091](0091-a-new-design-question-is-named-by-its-issue-number.md).
- Implemented in `.claude/settings.json` and described in
  `docs/agents/workflow.md` (GitHub flow), `docs/agents/rules.md` and
  `CLAUDE.md`.
- The check of ask and deny rules under `bypassPermissions` is task
  [#45](https://github.com/tran4774/ranikust/issues/45).
- Revisit when a bot identity or a command-parsing hook is proposed, or
  when #45 reports.
