---
status: proposed
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0097: Agents work in fresh, bounded contexts: briefed subagents instead of forks, compaction at 300k, a model per task

## Context and Problem Statement

Between 30 October and 4 November 2026 the agent workflow of ADR-0087,
ADR-0093 and ADR-0096 ran 16 main sessions and 392 subagent runs. The
maintainer hit the plan's rate limits and waited on slow turns. The
transcripts were measured (see Evidence): the cost is not the number of
agents or the size of tool output but the context each turn carries. The
main sessions ran at 350k-470k tokens of context per turn and compacted by
hand at 630k-964k; 89 implementation tasks ran as forks that inherited that
context (median 337k tokens, p90 782k) and then ran a median 94 turns each.
Re-reading the inherited prefix was 88% of what forks read and about 65%
of all subagent spend, while only 13 of 88 fork directives needed the
conversation: the directives already named the files, ADRs and decisions.
Every review-fix round went back through the main session as a message to
a finished fork, after the 5-minute prompt cache had expired, so the whole
context was rewritten. 142 of 173 general-purpose runs (audit hunters,
record and verify steps) ran on Opus by default. How should agents be
given context so that work stays fast, cheap and as accurate?

## Considered Options

- Keep forks and compact the main session earlier. Rejected: a fork still
  inherits 100k+ tokens it does not need, and the fork runs on the main
  model whatever the task.
- Fresh subagents briefed by reference, forks off by default. Chosen.
- Deny `Agent(fork)` by permission rule. Rejected for now: whether `ask`
  and `deny` rules hold under `bypassPermissions` is unverified (task #45);
  the documented `CLAUDE_CODE_FORK_SUBAGENT=0` is overridable per session.
- Review on Sonnet. Rejected for now: external comparisons show Opus
  catching the hard cases Sonnet misses, and review is about 13% of spend;
  revisit with the next measurement.

## Decision Outcome

Agents work in fresh, bounded contexts: implementation goes to a briefed
`implementer` subagent in its own worktree, forks are off by default, the
main session compacts at 300k tokens, and the model follows the task.

- **No forks by default.** `.claude/settings.json` sets
  `CLAUDE_CODE_FORK_SUBAGENT=0`. The maintainer starts one session with
  forks on (`CLAUDE_CODE_FORK_SUBAGENT=1 claude`) when the conversation
  itself is the input (a long design discussion to carry into the work).
- **The `implementer` subagent** (`.claude/agents/implementer.md`, Opus,
  `maxTurns: 400`, the `dev-loop` and `test` skills preloaded) does every
  implementation, fix and refactor, in a worktree it creates under
  `.claude/worktrees/` and reuses for follow-ups. Its brief is by
  reference: the issue text, the branch to start from, the files, ADRs and
  design sections that matter, the acceptance criteria and the decisions
  already made. It runs the `reviewer` rounds itself (at most three),
  commits without pushing and hands back one report of at most 40 lines.
  `host-safety` and `red-team` stay with the main session (ADR-0096).
- **Context budget.** Sessions and subagents compact at 300k
  (`CLAUDE_CODE_AUTO_COMPACT_WINDOW=300000`), the main session with the
  `Compact instructions` of `CLAUDE.md`; a new task starts a new session
  from a brief; `implementer` and `Explore` hand-backs are at most 40
  lines; no polling; anything that may run over five minutes runs in the
  background; independent read-only calls go in one message.
- **Follow-ups within the hour go to the same agent.** Subagent prompt
  caches live one hour (`CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL=1h`), so
  `SendMessage` to an agent that answered within the hour reads a warm
  cache and keeps its worktree and build output; later, a new agent
  briefed with the branch, its worktree, the commit range and the
  findings.
- **A model per task.** An unnamed subagent runs on Sonnet
  (`CLAUDE_CODE_SUBAGENT_MODEL=sonnet`; the vendored `security-audit`
  skill names none, so its agents are started with `model: opus`);
  `Explore` is overridden by a project agent on Haiku; Opus only for
  `implementer`, `reviewer`, `host-safety` and `red-team`. Every project
  agent carries a `maxTurns` cap sized from the measured runs, generous
  enough that only a runaway hits it and returns a partial result.
- **graphify once, then the lines it names.** A codebase question starts
  with `graphify query`, then reads only what it points to, in the same
  message, with no grep or cat to hedge; ambiguous names are retried as
  `path::Symbol` or by node id. Agent worktrees run `graphify` from the
  main checkout (or pass `--graph`) and never `graphify update`. A
  `.graphifyignore` keeps frozen research, audit reports, CI and the test
  harnesses out of the graph; `review-pack` drops same-file hits.
- Not covered: the maintainer's own hooks in `.claude/settings.local.json`
  (the graphify hook-guard notes were followed in 2-4% of cases and cost
  about 93k tokens per long session; slimming them is the maintainer's
  call), agent memory directories, and a PostToolUse filter for build
  output, which the measurements show is not needed (median tool result
  859 bytes, build output already piped through `tail`).

### Consequences

- Good, because a fresh agent starts at about 11k-35k tokens instead of
  337k-900k: the measured upper bound is about a third of all spend, and
  compaction at 300k roughly halves the main session's cost.
- Good, because a review round is a 5.5M-token reviewer run instead of a
  12.7M-token round trip through a fork, and the implementer's own loop
  keeps hand-backs out of the main context (37% of it today).
- Good, because Haiku and Sonnet take the locating, recording and audit
  fleet work that Opus did by default.
- Bad, because a brief must now carry what a fork used to inherit: a thin
  brief costs a round trip. The template in `workflow.md` lists what to
  pass.
- Bad, because one-hour cache writes cost more than five-minute ones; the
  saving holds while agents idle between rounds, which was 10.9% of spend.
  A brief that names an existing worktree keeps its build output; a new
  worktree rebuilds `target/` from scratch.
  Revisit if `/usage` shows cache writes growing.
- Neutral, because the `implementer` can still start `reviewer` runs
  (subagents nest up to three levels), so the fix loop of ADR-0096 is
  unchanged in substance.

## Evidence

Measured from the session transcripts on 2026-11-04 (API responses were
deduplicated by message id; cost units weight cache reads 0.1, cache writes
1.25-2, output 5) [measured]:

- 392 subagent runs, 89 forks. Fork start context median 337k tokens, p90
  782k, max 908k; median 94 turns, max 437. 88% of fork cache reads were
  the inherited prefix; forks were about 65% of subagent spend. 13 of 88
  directives needed the conversation. Nine forks compacted at 965k to
  58k-74k and finished the task.
- Main sessions: mean context 350k-470k per turn; five compactions, all
  manual, at 630k-964k; turns above 600k were 13%-63% of turns and
  12%-87% of cost per session. Hand-backs and notifications were 37% of
  the main context; 282 wake-up turns made no decision. The main session
  was about 15% of spend.
- Review rounds through a fork: 107 rounds, 25.6 turns and 12.7M cache
  tokens each; a fresh reviewer run 5.5M. 65 messages to finished agents
  and 102 fork wake-ups after more than five minutes idle rewrote the
  context: 10.9% of spend.
- Models: 142 of 173 general-purpose runs on Opus; reviewer median 47
  turns, host-safety 36, red-team 112, scribe 30-60, upstream-oracle 2
  runs.
- Tool output is not the problem: 27.6k tool results, median 859 bytes,
  largest 83 KB (whole-file reads), no full `cargo test` output.
- graphify: one graphify call per 20 searches and 19 reads; 63% of query
  answers truncated at the 2000-token budget; 36.5% of calls hedged with a
  grep in the same command; hook-guard suggestions followed in 3.8% of
  cases, about 93k tokens injected per long session; 43 "graph not found"
  failures and 107 `graphify update` runs in worktrees.

External [external]: Claude Code documentation on subagents (fork,
`maxTurns`, `autoCompactWindow`, `omitClaudeMd`, nesting depth), context
and compaction, prompt caching (five-minute TTL for subagents by default),
costs and `/usage` attribution; Anthropic's multi-agent research system
and context-engineering posts (briefs state objective, output format and
boundaries; Opus lead with Sonnet workers); Qodo's and CodeRabbit's
reviewer comparisons (Sonnet 5.5 close to Opus 5.5 on average, Opus ahead
on the hard cases). No controlled fork-versus-fresh benchmark exists
[inferred]: the saving above is computed from the measured prefix share.

## More Information

Amends ADR-0087 (what agents do), ADR-0093 (settings as guardrails) and
ADR-0096 (the local review loop; the loop's steps now name the
`implementer`). Implemented by `.claude/settings.json`, `.claude/agents/`,
`CLAUDE.md`, [`docs/agents/workflow.md`](../agents/workflow.md#claude-code),
the `dev-loop` skill, `.graphifyignore` and `scripts/review-pack.py`.
Revisit after two weeks of use with the attribution and prompt-cache lines
of `/usage`: whether cache writes grew,
whether thin briefs cost round trips, and whether a Sonnet `reviewer`
first pass with Opus only on the host-safety crates holds accuracy.
