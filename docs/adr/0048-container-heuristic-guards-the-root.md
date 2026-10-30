---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0048: Kaniko mode builds on `/` when the container heuristic says so, or with `--force`

## Context and Problem Statement

Kaniko mode without `--workdir` builds on `/`: it extracts the base over
it and empties it between stages (ADR-0046). Outside a container only
`--force` lets it run (ADR-0040), and whether ranikust is in a container is
kaniko's heuristic (`container::detect`: marker files, cgroup names, the
`container` variable, an overlay `/`). A host can match it by accident (an
overlay root, a leftover `/.dockerenv`) and then lose its `/` without a
flag (Q-321).

## Considered Options

- (a) Keep kaniko's heuristic and `--force`.
- (b) Also require an explicit flag naming `/` (for example `--workdir=/`)
  when the heuristic is the only evidence.
- (c) A first-run confirmation; it does not fit an unattended CI job.

## Decision Outcome

Chosen: (a). In a container ranikust builds on `/` with no flag, as kaniko
does; outside one it needs `--force`.

- The maintainer weighed the extra flag against the risk: a build in a
  container is the main use of kaniko mode, and every container user would
  have to add the flag to guard against hosts that match the heuristic by
  accident. Setup kept short is a goal alongside security.
- Revisit if a host is ever seen to match the heuristic; (b) is then the
  cheap hardening.

### Consequences

- Good, because a container build needs no extra flag.
- Bad, because a host that matches the heuristic by accident (an overlay
  `/`, a leftover `/.dockerenv`, a `container=` variable) has its `/`
  extracted over and emptied between stages without `--force`.
- Neutral, because a host `--workdir` is unaffected: its checks
  (ADR-0047) do not depend on the heuristic.

## Evidence

- kaniko@44aa7ddc0 `cmd/executor/cmd/root.go:240-245` (`--force`),
  `pkg/util/proc/proc.go:78-149` (`GetContainerRuntime`); ported in
  `crates/ranikust/src/container.rs`.

## More Information

Answers [Q-321](../open-questions.md#q-321-is-the-container-check-enough-to-guard-a-kaniko-mode-build-on-),
confirms ADR-0040's container check for building on `/`.
