---
status: accepted
date: 2026-11-08
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0136: Root's `--mode=auto` falls back to kaniko mode from a set-up that fails in the build's user namespace

## Context and Problem Statement

Before rootless overlay mode ([#79](https://github.com/tran4774/ranikust/issues/79),
[ADR-0107](0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)),
`--mode=auto` as root that may not mount built in kaniko mode. It now tries
rootless overlay mode first where its probe passes. The probe runs every step
in a child process, but the set-up in the build's user namespace can still
fail on what the child could not see: the store, the spawner's start, the
sandbox or overlay probe run again there. A user namespace cannot be left
once entered, and kaniko mode must never run in it, where it would act on the
container's `/` as the map's root; so such a failure ended the build, where
the same build had worked before rootless overlay mode. The host-safety review
of #79 raised it (finding 6(b)).

## Considered Options

- Fork before the namespace; the child enters it, sets up and builds; the
  parent waits outside it and, where the child asks before its build, goes on
  in kaniko mode in its own process (chosen).
- The same fork, but the parent re-executes the binary (`/proc/self/exe`)
  with `--mode=kaniko` and a note, as the maintainer's decision suggested.
  Not taken: a second start (the sealed copy, the kernel check, the metrics
  report's set-up) and the mount namespace ranikust started in, which kaniko
  mode on `/` scans (ADR-0129), read again after the first attempt; the
  messages would name `--mode=kaniko`, which the operator did not pass. In
  its own process the parent takes the very path `--mode=auto` took before
  rootless overlay mode.
- Document that the build ends (the state of #79 before the decision).
  Rejected by the maintainer.

## Decision Outcome

Where `--mode=auto` as root that may not mount tries rootless overlay mode,
ranikust forks before entering the build's user namespace, and a set-up that
fails in the child before its build falls back to kaniko mode in the parent,
which never entered the namespace.

The maintainer's answer (2026-11-08: "Re-exec để lùi về kaniko", "re-exec to
fall back to kaniko"), with the freedom to choose another design that keeps
its properties, stood for this.

- **Where.** Root's `--mode=auto` once the user namespace probe passed, the
  peer scan of `--allow proc.rbind` and overlay mode's flags checked, all
  outside the namespace. Never a user other than root (whom kaniko mode
  refuses), never `--mode=overlay`, never once the set-up succeeded: those end
  with the error, as before.
- **The child** enters the namespace and sets up. On any failure of the
  set-up (entering the namespace, the store, the sandbox probe, the overlay
  probe, the spawner) it closes its store and, where that store's open made
  the state directory a store and no other build holds it, empties the
  directory (`Store::abandon`; an open that fails once it wrote the marker
  empties it too, `Store::open_or_abandon`), then sends the reason through
  a pipe and exits with a status of the fallback's own (75), none of
  ranikust's. Once set up, it closes the pipe before the build: from then
  on the run ends as the child does.
- **The parent** drops its copy of the state directory's hold right after
  the fork, waits, and on a fallback (the reason, the fallback's status)
  with no stop signal received logs the reason and builds in kaniko mode,
  as `--mode=auto` did before rootless overlay mode, without a second
  probe line. Otherwise it ends as the child did: its exit code (1 for the
  fallback's status without its reason), or its signal.
- **Signals.** `SIGINT` and `SIGTERM` are blocked in the parent and read
  with `sigwaitinfo`, so that one sent to the process group and one sent to
  ranikust alone each count once; the parent sends the child `SIGUSR1` for
  each, and the child, which ignores the two itself, cancels its build on
  the first and stops at once on the second, as a build does on `SIGINT`.
  A stop before the child handles `SIGUSR1` ends the child by its default
  action, and the parent then ends by the stop signal (`128 + n` as a
  container's PID 1, which the kernel keeps from its own default action).
  While a failed set-up abandons its store, the child ignores `SIGUSR1`
  until it exits, and the parent then ends by the stop. A stop wins over a
  fallback and over its status. The child gets `SIGKILL` when the parent dies
  (`PR_SET_PDEATHSIG`, armed again before the build). The parent reaps any
  other child it gets, as PID 1.
- **The state directory** is where the child writes before the build,
  apart from its own user namespace's maps and the probe of `RUN` cgroups,
  which removes the cgroup it makes
  ([ADR-0110](0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md)).
  Probe directories are removed by the probe, by the store's open,
  and by `Store::abandon`. A directory that already held a persistent store
  (an earlier rootless overlay build) is kept, and kaniko mode on `/`
  refuses it, as it did before this decision.

### Consequences

- Good, because a container where rootless overlay mode's probe passes but
  its set-up fails still builds, in kaniko mode, as before #79.
- Good, because exit codes and the meaning of `SIGINT` and `SIGTERM` stay
  as they were for the operator.
- Bad, because root's `--mode=auto` build that may not mount runs as two
  processes; the parent, a copy-on-write image of ranikust at the fork, waits
  for the build's whole life, and `SIGUSR1` sent to the child stops its build
  too.
- Bad, because a fallback needs the state directory empty again: where it
  held a persistent store, or another build holds it, kaniko mode on `/`
  refuses it after the fallback, with its usual error.
- Bad, because the parent stays in ranikust's cgroup, so ranikust is not
  alone there: in a delegated tree a `RUN`'s cgroups take the single
  layout, never nested with `+cpu`
  ([ADR-0110](0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md)).

## Evidence

- A process cannot join an ancestor user namespace: `setns(2)` needs
  `CAP_SYS_ADMIN` in the target, which a process in a child namespace lacks
  there **[external]** (user_namespaces(7)).
- Blocked signals are queued for a namespace's PID 1 too; a signal with its
  default action that PID 1 sends itself is dropped **[external]**
  (pid_namespaces(7), signal(7)).
- `flock(2)` converts a shared lock to an exclusive one by removing it
  first; with `LOCK_NB` the conversion fails rather than waits **[external]**.
- Tests **[code]**: `rnk-sandbox` `supervise::tests` (what the parent does
  for each end of the child), `ranikust`
  `only_roots_auto_mode_falls_back_to_kaniko_mode`, `rnk-store`
  `abandon_empties_only_a_store_its_open_made_and_no_build_holds` and
  `an_open_that_fails_once_it_made_the_store_empties_it_where_asked`; T1 smoke
  case `rootless` (kaniko leg: `RANIKUST_DEBUG_FAULT=rootless-broken`, the
  set-up's last step once its sandbox started, builds in kaniko mode with
  the note and kaniko mode's line alone and leaves the state directory empty;
  `--mode=overlay` gets the error; `SIGKILL`, `SIGINT`, `SIGTERM` and
  `SIGINT` to the group through the waiting parent leave no process, and
  ranikust exits 137 for `SIGKILL`, else 1, as a build that never forks);
  VM smoke's user leg (a user other than root gets the error).

## More Information

Refines [ADR-0107](0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)
(root's `--mode=auto` that may not mount); the stop signals of kaniko mode on
`/` ([ADR-0077](0077-kaniko-mode-on-root-never-uses-a-persistent-store.md))
are unchanged. Implemented in `crates/rnk-sandbox/src/supervise.rs`,
`crates/ranikust/src/main.rs` (`rootless`, `Via`),
`crates/ranikust/src/setup/rootless.rs` and `crates/rnk-store/src/store.rs`
(`Store::abandon`, `Store::open_or_abandon`); described in
[`design/modes-and-detection.md`](../design/modes-and-detection.md#probe-order---modeauto).
