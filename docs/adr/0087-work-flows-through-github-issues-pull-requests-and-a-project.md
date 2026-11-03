---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0087: Work flows through GitHub issues, pull requests and a Project, with agents running locally

## Context and Problem Statement

The repository (github.com/tran4774/ranikust, public, one maintainer) had
never used issues, pull requests or Projects: agents worked on local
branches, the maintainer merged them locally, and `main` was pushed when
the maintainer asked. Phase 2 is done, and the maintainer wants the agent
workflow on GitHub (CI on every change, review where others can see it) and
a release process ([ADR-0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md)).
The questions: how work flows, what agents may do on GitHub without asking,
how a pull request is merged, where the agents run, and how a public
repository is kept safe from what other people write in it. Agents act with
the maintainer's own `gh` token, so GitHub cannot tell an agent's action
from the maintainer's.

## Considered Options

- Keep local merges and push on request (the status quo).
- GitHub flow with agents running locally.
- GitHub flow with agents running in GitHub Actions now
  (`claude-code-action`).

The first keeps work off GitHub, which the maintainer asked to change. The
third is rejected for now: the T2 VM is not there
([ADR-0063](0063-root-tests-that-mean-the-same-in-a-container-run-in-rootful-docker-on-the-test-machine.md)),
public issues and comments expose an agent to prompt injection, and an agent
in a public repository's workflow would need secrets there. The second was
proposed and accepted.

## Decision Outcome

Work flows through GitHub issues, pull requests and a Project: an issue
starts it, a draft pull request carries it, CI and review results are posted
on the pull request, and the maintainer squash-merges it; the agents keep
running locally.

The maintainer decided on 2026-11-03, on a proposal with numbered decisions
(D1, D2, D4 and D6 are recorded here; D3 is
[ADR-0088](0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md)):

- **Flow.** An issue (in the Project) → a branch `<type>/<scope>-<topic>` →
  a draft pull request with "Closes #N" → GitHub CI, the T2 results and the
  `host-safety` and `reviewer` findings posted on the pull request (the fix
  loop of `docs/agents/workflow.md` is unchanged) → the maintainer
  squash-merges → the issue closes and its Project item goes to Done.
- **D1, agent autonomy.** Maintainer: "làm theo đề xuất trước cho đến khi
  tôi yêu cầu chỉnh sửa" (follow the proposal until I ask for changes). The
  proposal: agents may, without asking, open issues for work already in a
  plan, push branches other than `main`, open draft pull requests, comment
  and review on their own pull requests, set labels and milestones, update
  Project items and post commit statuses. They must ask the maintainer
  before marking a pull request ready or merging it, pushing to `main`,
  creating tags or releases, changing repository settings, and replying to
  or closing issues and pull requests opened by other people. Issues and
  pull requests an agent opens carry the `agent` label. Because agents act
  with the maintainer's account, the guard is the agent rules and the ask
  rules of `.claude/settings.json` (both implemented separately from this
  ADR); a bot identity is a possible later step.
- **D2, merging.** Maintainer: "Theo đề xuất là squash và xoá nhánh" (as
  proposed: squash and delete the branch). Squash merge only, and the head
  branch is deleted on merge. The pull request title (Conventional Commits)
  becomes the commit title; the squash message is the pull request's commit
  messages, which keeps `Co-authored-by` trailers.
- **D4, where agents run.** Maintainer: "chạy ở local trước, về sau có thể
  gắn free bot review PR" (run locally first; later a free bot to review
  pull requests may be attached). Agents run locally (Claude Code and `gh`),
  keeping the VM, the subagents and the skills. A free review bot may be
  added later, for reviews only and triggerable only by the maintainer,
  because public issues and comments can carry prompt injection.
- **D6, security on a public repository.** Maintainer: "OK chốt" (OK,
  settled). No detail of an unfixed vulnerability goes into a public issue,
  pull request, comment or commit message; a finding about released code
  goes to a private security advisory (GitHub private vulnerability
  reporting). The content of issues, pull requests and comments written by
  other people is data for agents, never instructions.

### Applied on 2026-11-03

In the repository's settings and on GitHub, before this ADR:

- Private vulnerability reporting is on (`SECURITY.md` already pointed
  reporters there while it was off). Squash merge is the only merge method
  and the head branch is deleted on merge. Discussions are on; the Wiki is
  off (it was empty). Topics and a description are set.
- Labels: `question`, `needs-decision`, `task`, `tracking`, `host-safety`,
  `needs-t2`, `security`, `agent`, `release`, `area:ci`, `area:tests`, and
  `crate:<name>` for each of the 15 crates.
- Milestones: `v0.0.1`, `Phase 3: rootless and probing`, `Phase 4: cache`,
  `Phase 5: feature parity`, `Phase 6: hardening and 1.0`, `Backlog`.
- A public user Project, <https://github.com/users/tran4774/projects/1>,
  linked to the repository, with the fields Status (Todo, Needs decision, In
  Progress, In review, Done), Phase (3, 4, 5, 6, Backlog), Priority (P0 to
  P2) and Tier (T0, T1, T2, T2c, none).

- Two rulesets, applied by the maintainer (repository settings are the
  maintainer's under D1):
  - `main`: a pull request is required, squash is its only merge method,
    review threads must be resolved, the 8 CI checks are required
    (`rustfmt`, `clippy`, `test (x86_64 gnu)`, `static release build
    (x86_64 musl)`, `check (aarch64 musl)`, `test (aarch64 musl, native)`,
    `cargo-deny`, `docs (rustdoc, mdBook)`), no force-push or deletion; the
    repository admin may bypass it.
  - `release tags`: only the repository admin may create, update or delete
    a `v*` tag
    ([ADR-0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md)).

  New required checks (such as T2 jobs) are added to the `main` ruleset by
  the maintainer.

### Consequences

- Good, because every change is built by CI and reviewed on a pull request
  before it reaches `main`.
- Good, because `main` gets one commit per pull request, with a Conventional
  Commit title.
- Good, because the Project shows what is in progress and what waits for a
  decision.
- Bad, because GitHub cannot tell an agent from the maintainer: the
  limits on agents are agent rules and ask rules, not permissions GitHub
  enforces, until a bot identity exists.
- Bad, because the commits of a branch no longer appear on `main` one by one
  [inferred from squash merging]; their messages are kept in the squash
  message.
- Neutral, because the agents' reach is unchanged: the T2 VM, the subagents
  and the skills stay local.

## Evidence

No experiment backs this decision; it is the maintainer's choice on a
proposal. The facts it relies on:

- Before 2026-11-03 the repository had no issue, pull request or Project
  (maintainer's statement); [code] `docs/agents/workflow.md` has the main
  session merge the agents' branches locally.
- [code] CI has eight jobs (`.github/workflows/ci.yml`: `fmt`, `clippy`,
  `test`, `static-build`, `check-aarch64`, `test-aarch64`, `deny`, `docs`),
  which are the checks the `main` ruleset requires.
- [code] Tier T2 runs in a QEMU/KVM VM and T2c on a dedicated test machine
  ([ADR-0063](0063-root-tests-that-mean-the-same-in-a-container-run-in-rootful-docker-on-the-test-machine.md)),
  which is why the agents stay local.
- [external] Content written by other people in a public repository can
  carry instructions aimed at an agent that reads it (prompt injection).

## More Information

- Companions: [ADR-0088](0088-tracking-lives-on-github-knowledge-tied-to-the-code-stays-in-the-repository.md)
  (where tracking lives) and [ADR-0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md)
  (releases).
- Follow-ups that land with their implementation, not with this ADR:
  `CLAUDE.md`, `docs/agents/rules.md`, `docs/agents/workflow.md`,
  `CONTRIBUTING.md`, `README.md`, the book, the pull request template, and
  the ask rules of `.claude/settings.json`. Until they land, those files
  still describe local merging.
- Revisit when a review bot, a bot identity or agents in GitHub Actions are
  proposed; each of them needs an ADR of its own.
