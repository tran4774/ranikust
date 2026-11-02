---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0080: A full-sandbox `RUN` runs as PID 2 under an init that dies with ranikust

## Context and Problem Statement

A full-sandbox `RUN` gets its own PID namespace, and until now its command
was PID 1 of it. The child armed `PR_SET_PDEATHSIG(SIGKILL)` toward
ranikust's spawner thread, armed it again after `setresuid` (which clears
it), and executed the command; when that process died, the kernel killed the
rest of the namespace. That made the death signal the command's own, and so
T8 (no `RUN` process outlives ranikust) rested on the command keeping it.

It need not. The kernel clears the death signal when `execve` changes the
credentials (a set-user-ID or set-group-ID binary), and any process may clear
its own with `prctl(PR_SET_PDEATHSIG, 0)`, which needs no privilege. The red
team of the hardening pass measured both in the VM (Ubuntu 24.04, kernel 6.8):
a root `RUN` executing a copy of `sleep` setuid or setgid to 65534, and a
`python3` clearing its own signal, were still running 15 s after the thread
that cloned them had ended, while the control and `USER 1000:1000` died at
once. Nothing else kills the namespace when ranikust dies, so after a second
`SIGINT`, a `SIGKILL` (a CI timeout, `docker kill`, a pod's grace period),
the OOM killer or a panic abort, a hostile `RUN` keeps running as root with
Docker's 14 capabilities, the host network and write access to its upper
directory.

## Considered Options

- **A ranikust-owned init as PID 1, the command as PID 2.** The init keeps
  the death signal, which nothing in the namespace can clear: it never
  changes its user or executes anything. A process of the namespace cannot
  signal it (the kernel drops signals to a PID 1 without a handler), stop
  it, trace it or read its memory (it is not dumpable and a `RUN` lacks
  `CAP_SYS_PTRACE`). It can still read its `/proc/1/cmdline` (zeroed, below),
  lower its scheduling priority or pin it to a CPU (same uid, no capability
  needed), and make itself the init's tracee (`PTRACE_TRACEME`). When
  ranikust dies, the init dies, and the kernel kills the namespace.
- **A cgroup per `RUN`, killed with `cgroup.kill`.** Needs a writable,
  delegated cgroup v2 tree, which a default Docker container or most pods do
  not give; it would need a fallback where it matters most.
- **A watchdog process outside the namespace** that kills the `RUN` when
  ranikust dies. One more process to keep alive; it would itself need a death
  signal toward ranikust, and in kaniko mode on `/` a root `RUN` sees and
  kills it.
- **Accept and document** the residue in threat-model section 6.

## Decision Outcome

Chosen: a ranikust-owned init as PID 1 of every full-sandbox `RUN`'s PID
namespace, which runs the command as PID 2 (the maintainer, 2026-11-03).

- The child that sets the sandbox up (mounts, `pivot_root`, capability drop)
  stays as the init. Every child resets every signal disposition to the
  default first, with raw `rt_sigaction` (the C library's wrappers refuse
  some numbers and may lock), so that ranikust's handlers never run in it.
  Where it executed the command before, it sets itself not dumpable, zeroes
  its copy of the argument and environment strings ranikust started with
  (`/proc/self/stat` fields 48 to 51: `/proc/1/cmdline` has no ptrace check,
  and those strings hold `--build-arg` values, `--secret` paths and a
  `DOCKER_AUTH_CONFIG`), starts a new session, and forks: the new process,
  PID 2, waits on a close-on-exec go pipe, then does what the child did last
  (`setsid`, the session keyring, `WORKDIR`, the `USER` ids, the
  exec-candidate checks, `execve`). It arms its own death signal again
  after `setresuid`, toward the init, only as a second line.
- Meanwhile the init drops every capability, is named `ranikust-init`
  (`/proc/1/comm`), checks its death signal is still armed and ranikust
  still its parent, and closes every descriptor but the write end of a new
  status pipe; closing the go pipe's write end lets the command go on, so a
  failure of these steps (`Step::Init`, the command killed) always comes
  before the command ran. It installs no signal handler, so the kernel drops
  every signal a process of the namespace sends it; ranikust's `SIGKILL`
  from outside still acts. It reaps every process that reparents to it; a
  tracee stop (the command made it its tracer) is no end: it detaches,
  handing the stop signal on.
- When the command exits or is killed, the init writes the command's wait
  status (four bytes) on the status pipe and exits; the kernel kills what is
  left of the namespace, as when the command was PID 1. ranikust reports
  that status, so exit codes and signals read as before; a record that is
  neither an exit nor a signal, or an init that exited 0 without one, is an
  error. A set-up or `execve` failure of the command reaches ranikust on the
  error pipe, as before.
- A cancel still sends `SIGKILL` to the cloned child, now the init, which
  ends the namespace and the command with it.
- Not covered: `Isolation::Direct` (kaniko mode on a container's `/`) has no
  PID namespace and no init. There T8 rests on the container ending with
  ranikust when ranikust is its PID 1, and on the sweep of the `RUN`'s
  process tree after each `RUN` otherwise (ADR-0051).

### Consequences

- Good, because no command can outlive ranikust any more, whatever it does to
  its own death signal or credentials.
- Good, because orphans of the command are reaped by an init, as
  `docker run --init` does, instead of by whatever the command is.
- Bad, because every full-sandbox `RUN` costs one more `fork`, and one more
  process for the length of the `RUN`.
- Bad, because each init is a copy-on-write copy of ranikust for as long as
  its `RUN` lasts: memory ranikust frees or rewrites meanwhile stays charged
  once per live init. Measured in the VM (6 parallel stages with long
  `RUN`s, a cgroup of their own): the anonymous charge peaked 3.3 MiB above
  main's (49.1 against 45.7 MiB) and stayed 9 to 12 MiB above it while
  inits lived; `memory.current` peaked at 207.8 against 204.0 MiB
  **[measured]**. The worst case is one copy of ranikust's resident memory
  per live init **[inferred]**. An executed minimal init would remove it,
  the same mechanism as Q-422's trampoline [open: Q-451].
- Bad, because a root `RUN` can still slow its init's death, though not stop
  it: as a process of the same uid it may make the init `SCHED_IDLE` and pin
  it to one CPU, then keep that CPU busy. Measured in the VM (kernel 6.8):
  0.9 to 1.8 ms without that, 7.6 s with 8 spinners on the init's CPU,
  30.6 s with 32, about 1 s per spinner **[measured]**; bounded only by the
  processes the `RUN` may start (threat-model section 6). A per-`RUN`
  `cgroup.kill` would be a second line; not built.
- Neutral, because the command is no longer PID 1: `$$` is 2, and a command
  that expects to be PID 1 (to reap, or for PID 1's signal semantics) runs as
  under `docker run --init`. kaniko's `RUN` is a child of the executor, never
  PID 1 (`kaniko@44aa7ddc0 pkg/commands/run.go:288-411`, `exec.Command`);
  BuildKit's runc-run process is the container's PID 1 **[inferred]**.

## Evidence

- Red team, `rnk-sandbox` `red_team_no_run_outlives_the_thread_that_cloned_it`
  (T2, the VM, kernel 6.8): before this change, the setuid, setgid and
  `prctl` cases outlived the cloning thread by 15 s; after it, all five cases
  die with it **[measured]**.
- `rnk-sandbox` `the_run_is_pid_2_under_an_init_that_reports_its_status`
  (T2): the command is PID 2, PID 1 is `ranikust-init` with no capability,
  `kill -KILL 1` from the command leaves it running, and an exit code, a
  signal, an `execve` failure and a `WORKDIR` failure are reported as before.
- `rnk-sandbox` `red_team_a_run_can_slow_its_inits_death_but_not_stop_it`
  (T2, the VM, kernel 6.8): the namespace's death after the cloning thread
  ended, 0.9 ms and 1.8 ms without scheduling tricks; 7.6 s with the init
  made `SCHED_IDLE` and pinned to CPU 0 under 8 spinners, 30.6 s under 32
  (a one-off run); no process left in either **[measured]**.
- VM smoke, host variant: ranikust killed with `SIGKILL` during a `RUN` whose
  `python3` cleared its own death signal; no `RUN` process and no init is
  left within 10 s.
- `commit_creds` clears the death signal and the dump flag when the euid,
  egid, fsuid or fsgid changes, or when the new permitted set is not a subset
  of the old one; lowering capabilities is neither, so the init's drop keeps
  it; the init checks the signal is still armed after the drop
  (`prctl(PR_GET_PDEATHSIG)`) **[external]** (Linux `kernel/cred.c`).
- A PID namespace's init is killed with every process of the namespace when
  it exits, and signals from inside the namespace to an init without a
  handler are dropped **[external]** (`pid_namespaces(7)`).

## More Information

- Refines the T8 mitigation of `docs/security/threat-model.md` and the
  per-`RUN` sequence of `docs/design/sandbox.md`.
- Related: ADR-0003 (the full sandbox, otherwise unchanged) and ADR-0051
  (the `Direct` sweep, which keeps T8 where there is no PID namespace).
- Implemented in `crates/rnk-sandbox/src/child.rs` (`run_init`) and the
  status pipe of `crates/rnk-sandbox/src/spawner.rs`.
