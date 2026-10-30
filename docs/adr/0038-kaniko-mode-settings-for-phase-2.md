---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0038: Kaniko-mode settings for Phase 2: workdir checks, state directory, ignore list, lazy snapshots, fake `/proc`, flag name

## Context and Problem Statement

Six open questions blocked the start of Phase 2, kaniko mode
([phase-2](../agents/phase-2.md)): who runs the workdir checks and what
counts as empty (Q-011), the final name of the parallel-stages flag
(Q-019), when a lazily skipped stage must still be snapshotted (Q-034),
what the ignore list holds (Q-035), where the state directory goes when
kaniko mode runs outside a container (Q-036), and whether the fake `/proc`
needs more files (Q-037). The maintainer answered them together on
2026-10-30. The seventh, the dependency picks (Q-038), waits for a
benchmark and is not decided here.

## Considered Options

- Q-011: the checks in (a) `rnk-rootfs` or (b) `ranikust`; a workdir of
  only mount points (c) counts as empty or (d) is refused.
- Q-019: (a) keep `--parallel-stages`; (b) another name.
- Q-034: decide per instruction while building, or once from the stage
  graph before the build.
- Q-035: (a) all of `/ranikust` or (b) only `/ranikust/state`; (c) add
  kaniko's `/etc/mtab`, `/tmp/apt-key-gpghome` and `/var/run` or (d) leave
  them out.
- Q-036: (a) keep `/ranikust/state`; (b) require `--state-dir`; (c) a
  hidden directory in the directory ranikust is started in (the
  maintainer's answer).
- Q-037: (a) add `memory.stat`; (b) keep fourteen files until a case fails.

## Decision Outcome

Kaniko mode runs its workdir checks in `ranikust`, keeps its state in
`/ranikust/state` on `/` and in `<cwd>/.ranikust` elsewhere, ignores what
kaniko ignores plus all of `/ranikust`, decides lazy snapshots from the
stage graph, keeps the fourteen fake `/proc` files, and names the opt-in
flag `--parallel-stages`.

- **Workdir checks (Q-011: b, d).** `ranikust` runs the checks of
  [kaniko-mode](../design/kaniko-mode.md#host-safety-checks) during
  process set-up, before anything is written, and holds the workdir
  `flock` for the whole build; `rnk-rootfs` provides only the primitives
  (`RootPath::open` with `RESOLVE_NO_SYMLINKS`, `statx` with `dev`, `ino`
  and `mnt_id`). A workdir holding only mount points has no marker and is
  refused, as today's interim.
- **Flag name (Q-019: a).** `--parallel-stages` is final.
- **Lazy snapshots (Q-034).** Before the build, from the stage graph and
  the outputs: a stage's layers are needed when it is the target, or when
  a stage whose layers are needed builds `FROM` it (squashed or not). Any
  other stage, used only through `COPY --from`, is not walked at all.
  Because the decision is fixed before the first instruction, no stage
  "later needs a layer", and no deletion has to be found after the fact.
  `--cache` (Phase 4) may add stages whose layers are needed; it changes
  the input of the rule, not the rule
  ([Q-042](../open-questions.md#q-042-how-do-cache-lookahead-and-cached-stage-skipping-work)).
- **Ignore list (Q-035: a, c).** Mount points under the root from
  `/proc/self/mountinfo`, `--ignore-path`, `/proc`, `/sys`, `/dev`, all of
  `/ranikust` (as kaniko ignores all of `/kaniko`, so an image may keep the
  binary there), the state directory wherever it lies inside the root,
  `/etc/mtab`, `/tmp/apt-key-gpghome`, and `/var/run` while
  `--ignore-var-run` is true (its default), with kaniko's meaning
  (`pkg/util/fs_util.go:72-89`, `cmd/executor/cmd/root.go:155-165` at
  `44aa7ddc0`). Paths are physical, as `rnk_rootfs::IgnoreList` requires;
  how kaniko treats `/var/run` when it is a symlink to `/run` is checked
  against its source when the list is built.
- **State directory (Q-036: c).** With the root `/` (in a container) the
  default stays `/ranikust/state`. With any other root (`--workdir` on a
  host, or per-stage directories) it is `<cwd>/.ranikust`, `<cwd>` being
  the directory ranikust was started in, as overlay mode's
  `<workdir>/.ranikust`. When it lies inside the root it is on the ignore
  list, as `/ranikust/state` is. `--state-dir` replaces either default.
- **Fake `/proc` (Q-037: b).** Fourteen files per cgroup version, as
  designed; `memory.stat` is added only when a build fails without it. The
  untried tools (conda, .NET, cargo, go, Python builds) get tests in
  Phase 2 task 10.

### Consequences

- Good, because every answer but the state directory is kaniko's own
  behaviour, so a kaniko pipeline keeps working unchanged.
- Good, because lazy snapshots need no bookkeeping during the build, and
  the rule is testable from the graph alone.
- Good, because a host build needs no flag for its store and keeps it next
  to where it was started.
- Bad, because the host default depends on the current directory: two
  builds started in different directories do not share a store unless
  `--state-dir` says so.
- Bad, because a workdir of only mount points cannot be used without first
  adding the marker by hand.
- Neutral, because the fake `/proc` may need a fifteenth file on cgroup v1
  with the limit on a parent cgroup.

## Evidence

- kaniko source at `44aa7ddc0`: the ignore list
  (`pkg/util/fs_util.go:72-89`, `:630`), `--ignore-var-run`
  (`cmd/executor/cmd/root.go:155-165`); see
  [kaniko-map](../agents/kaniko-map.md#behaviour-to-match).
- [Experiment 08](../research/experiments/08-parallel-stages-lazy-export.md):
  skipping intermediate-stage snapshots saved 4.7–5.3 s (30–33 %) on W3.
- [Experiment 09](../research/experiments/09-kaniko-mode-prototype.md):
  the mountinfo ignore list kept every mount out of the diffs.
- [Experiments 12a](../research/experiments/12a-fake-proc.md) and
  [16](../research/experiments/16-cgroup-v1-cpuset.md): the fourteen files
  per cgroup version.

## More Information

Refines [ADR-0022](0022-common-state-dir.md) (the default outside a
container, which it left open), [ADR-0014](0014-kaniko-mode-sequential-by-default.md)
(the flag's name) and [ADR-0036](0036-kaniko-flags-in-phase-1.md)
(`--ignore-var-run` and `--whitelist-var-run` take effect in Phase 2).
Design: [kaniko-mode](../design/kaniko-mode.md). Revisit lazy snapshots
with the cache (Phase 4) and the fake `/proc` set when a build fails
without `memory.stat`.
