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
  changes its user or executes anything, and a process of the namespace
  cannot signal, trace or read it. When ranikust dies, the init dies, and the
  kernel kills the namespace.
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
  stays as the init. Where it executed the command before, it starts a new
  session, sets every signal disposition to the default, and forks: the new
  process, PID 2, does what the child did last (`setsid`, the session keyring,
  `WORKDIR`, the `USER` ids, the exec-candidate checks, `execve`). It arms its
  own death signal again after `setresuid`, toward the init, only as a second
  line.
- The init then holds no capability, is not dumpable, is named
  `ranikust-init` (`/proc/1/comm`), checks its death signal is still armed and
  ranikust still its parent, and closes every descriptor but the write end of
  a new status pipe. It installs no signal handler, so the kernel drops every
  signal a process of the namespace sends it; ranikust's `SIGKILL` from
  outside still acts. It reaps every process that reparents to it.
- When the command exits, the init writes the command's wait status (four
  bytes) on the status pipe and exits; the kernel kills what is left of the
  namespace, as when the command was PID 1. ranikust reports that status, so
  exit codes and signals read as before. A set-up or `execve` failure of the
  command reaches ranikust on the error pipe, as before; a failure of the
  init's own steps is `Step::Init`, with the command killed.
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
- VM smoke, host variant: ranikust killed with `SIGKILL` during a `RUN` whose
  `python3` cleared its own death signal; no `RUN` process and no init is
  left within 10 s.
- `commit_creds` clears the death signal on a change of uid, gid or a gain of
  capabilities, not when capabilities are lowered, so the init's drop keeps
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
