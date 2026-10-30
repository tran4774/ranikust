---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer), by the rule that ranikust follows kaniko or BuildKit where they decide
---

# ADR-0040: Kaniko mode is picked when root may not mount, builds on `/` only in a container unless `--force`, and takes kaniko's snapshot flags

## Context and Problem Statement

Phase 2 task 2 ([phase-2](../agents/phase-2.md#task-order)) makes kaniko
mode reachable: `--mode=auto` must pick it where root may not mount, and
the kaniko flags that `ADR-0036` refused until Phase 2 must be accepted.
Without `--workdir`, kaniko mode builds on `/` and empties it between
stages ([modes-and-detection](../design/modes-and-detection.md#workdir-rules)),
which is safe only inside a container; a container check was never built
(Q-110 named it). Three questions follow: how ranikust tells it runs in a
container, what `--force` means, and what overlay mode does with the
kaniko-mode flags.

## Considered Options

- Container check: (a) kaniko's `GetContainerRuntime`, refusing outside a
  container unless `--force`; (b) always require `--workdir` outside
  `/ranikust` images; (c) no check.
- `--force`: (a) kaniko's meaning, building on `/` outside a container;
  (b) keep it ignored (`ADR-0036`).
- Kaniko-mode flags in overlay mode: (a) refuse those that change the
  image, warn about the others; (b) ignore them all with a warning;
  (c) implement them in overlay mode now.

(b) of the first question breaks every kaniko pipeline that runs the
executor image without flags; (c) deletes a host's `/` on a typo. (b) of
the third drops a flag that changes the image, which `ADR-0036` forbids;
(c) is outside task 2.

## Decision Outcome

ranikust does what kaniko does: `--mode=auto` picks kaniko mode when root
may not mount, kaniko mode on `/` requires a container by kaniko's own
detection unless `--force`, and kaniko's snapshot flags are accepted with
kaniko's meaning; in overlay mode the ones that would change the image are
refused for now.

- **Picking the mode.** `--mode=auto` enters the build's mount namespace
  first, before anything is written. Root refused there for lack of
  privilege (Docker's default profile, most pods) gets kaniko mode, and the
  log says why; otherwise overlay mode continues as before. Where root may
  mount but overlay is unusable, the probe's refusal stays until the
  per-stage variant (task 9).
- **Container check (a).** Without `--workdir`, kaniko mode builds on `/`
  only where kaniko's `GetContainerRuntime` finds a runtime
  (`kaniko@44aa7ddc0 pkg/util/proc/proc.go:78-149`): `/run/.containerenv`,
  `/.dockerenv`, a Kubernetes service account, a runtime named in
  `/proc/self/cgroup`, OpenVZ, gVisor, firejail, WSL, the `container`
  variable, `/run/systemd/container`, or an overlay mount on `/`. Ported in
  `crates/ranikust/src/container.rs`. With `--workdir`, the workdir checks
  of `ADR-0038` apply instead and no container is needed.
- **`--force` (a).** kaniko's meaning
  (`cmd/executor/cmd/root.go:240-245`): kaniko mode builds on `/` outside a
  container, with kaniko's warning. It changes nothing with `--workdir` or
  in overlay mode.
- **Flags.** `--ignore-path` (repeatable, absolute, cleaned as kaniko's
  `filepath.Clean`; a relative path, which kaniko keeps and never matches,
  is refused), `--ignore-var-run` (default true) with
  `--whitelist-var-run` as its deprecated name and kaniko's warning,
  `--single-snapshot`, `--materialize` and `--snapshot-mode` are kaniko-mode
  settings in `BuildConfig`, wired to the walk by tasks 3–6. `--materialize`
  changes nothing until the layer cache (Phase 4). The ranikust flag
  `--snapshot-workers N` sizes the walk (default 8, `ADR-0039`).
- **Overlay mode (a).** `--ignore-path`, `--single-snapshot` and
  `--ignore-var-run=false` stop the run with exit code 2: overlay mode
  would build another image than kaniko
  ([Q-260](../open-questions.md#q-260-should-overlay-mode-honour-kanikos-snapshot-flags)).
  `--snapshot-mode`, `--snapshot-workers`, `--materialize` and
  `--ignore-var-run=true` get a warning each.
- **State directory.** `/ranikust/state` on `/`, `<cwd>/.ranikust` with a
  `--workdir`, as `ADR-0038` decided; nothing is created until kaniko-mode
  builds exist.

### Consequences

- Good, because the kaniko executor's default use, no flags in a Docker or
  Kubernetes container, lands in kaniko mode without a flag.
- Good, because a host build on `/` takes the same explicit `--force` it
  takes with kaniko, and a `--workdir` build needs none.
- Bad, because `--mode=auto` now enters the mount namespace before
  checking overlay mode's state directory: an unprivileged user without
  `--workdir` gets the privilege refusal (exit 1) instead of the usage
  error (exit 2).
- Bad, because the detection is a heuristic: a container none of the
  signals names needs `--force`, and a host with `/.dockerenv` passes.
- Neutral, because the flags reach no walk yet; their meaning is tested at
  the flag level until tasks 3–6 use them.

## Evidence

- kaniko source at `44aa7ddc0`: `checkContained` and `--force`
  (`cmd/executor/cmd/root.go:63`, `:240-245`, `:448-450`),
  `GetContainerRuntime` (`pkg/util/proc/proc.go:78-187`), the flags
  (`cmd/executor/cmd/root.go:67-68`, `:155-170`, `:341`, `:347`, `:365`,
  `:371`, `:376`), `AddToDefaultIgnoreList`'s `filepath.Clean`
  (`pkg/util/fs_util.go:144-149`).
- [Experiment 13](../research/experiments/13-docker-rootful-vm.md) and
  [experiment 03](../research/experiments/03-privilege-matrix.md): root in
  Docker's default profile is refused `unshare(CLONE_NEWNS)`.
- T0 tests: `crates/ranikust/src/container/tests.rs` (every signal, its
  order), `src/config/tests.rs`, `src/cli/tests.rs`, `tests/cli.rs`.

## More Information

Refines [ADR-0036](0036-kaniko-flags-in-phase-1.md) (`--force` moves from
ignored to accepted; the Phase 2 refusals end),
[ADR-0023](0023-mode-flag.md) (`--mode=auto`) and
[ADR-0038](0038-kaniko-mode-settings-for-phase-2.md) (state directory).
Revisit the overlay-mode refusals with Q-260.
