---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0096: Agents review a change on the local branch before every push, and remove merged worktrees

## Context and Problem Statement

[ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
put the review findings on the pull request: a branch was pushed, a draft
pull request opened, and the `reviewer` and `host-safety` rounds ran
against it, each fix pushed in turn. On 2026-11-04 four documentation pull
requests (#52, #53, #54 and the CI change #51) went through two or three
rounds that way. Each fix round was a push, so each started CI and T2 on
the pull request and notified watchers, and an early push of #54 carried
text that a later round removed. Separately, the worktrees the agents leave
behind were never removed: 32 of them, each with its own `target/`, held
126 GB.

## Considered Options

- Keep reviewing on the pull request (ADR-0087 as written).
- Review on the local branch until no blocker is left, then push.

## Decision Outcome

Agents run the reviews a change needs (`reviewer`; `host-safety` and
`red-team` where `docs/agents/workflow.md` says) and the whole fix loop on
the local branch, and push only when no blocker is left; a later fix, after
CI or the maintainer's comments, goes through the same local loop before it
is pushed.

- The first push opens the draft pull request with a summary of the review
  rounds; a later round's summary is a comment. The details of an open
  weakness stay out of both (ADR-0087, D6).
- After a merge, the main session runs `just prune-worktrees`, which
  removes an agent worktree only when nothing in it can be lost: the
  branch's commit is the head of a pull request merged into `main`, or the
  branch has no commit of its own and the worktree has been idle for a day;
  nothing is changed, untracked or ignored outside `target/`; no VM run is
  live in it; it is not locked, detached, on `main` or the caller's own.
- This amends ADR-0087's Flow, which posted the review findings on the pull
  request; the rest of ADR-0087 stands.

### Consequences

- Good, because a pull request opens as a reviewed change, and CI runs once
  per reviewed state rather than once per fix round.
- Good, because nothing reaches the public repository before review, which
  matters for text about weaknesses.
- Good, because finished worktrees no longer fill the disk.
- Bad, because the review rounds are no longer visible on GitHub as they
  happen; only their summary is.
- Neutral, because CI still runs on every push to a pull request; it is not
  a substitute for the local review, nor the other way round.

## Evidence

- Pull requests #52, #53 and #54 (2026-11-04): two to three review rounds
  each, every round pushed. [external]
- 32 agent worktrees removed on 2026-11-04 freed 125 GB. [external]
- Claude Code locks a subagent's worktree when it starts it, but a
  worktree whose agent was resumed had no lock while the agent ran, so
  `just prune-worktrees` cannot rely on the lock alone. [external, observed
  with `git worktree list --porcelain`]

## More Information

- Amends [ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
  (Flow).
- The process: [workflow.md, GitHub flow](../agents/workflow.md#github-flow)
  and its [fix loop](../agents/workflow.md#host-safety-fix-loop).
- The script: `scripts/prune-worktrees.sh`, tested by
  `tests/agents/prune-worktrees-test.sh` (`just test-prune-worktrees`).
