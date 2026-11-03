---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0082: The full sandbox binds ranikust's binary read-only over itself; `Direct` on `/` keeps today's barriers

## Context and Problem Statement

Until `execve`, the child of a `RUN` is a copy of ranikust, so
`/proc/self/exe` is ranikust's own binary on the host. The child refuses an
exec candidate reached through a magic link, but the kernel opens a script's
interpreter itself: a command whose `#!` line names `/proc/self/exe` runs
ranikust's binary in the sandbox, with the script as its first argument
(runc's CVE-2019-5736 vector), and so does a symlink swapped between the
child's check and `execve` (a cache a parallel stage writes). This is threat
T7 and an accepted risk in section 6 of the
[threat model](../security/threat-model.md).

Three barriers hold today and are pinned by tests: ranikust is non-dumpable
from its first line, so another process of the `RUN` cannot open
`/proc/<pid>/exe` of that copy; its command line refuses the script path as a
subcommand and exits 2 (quick case `self-exe`); and the kernel refuses to open
a running binary for writing, `ETXTBSY`
(`a_running_binary_cannot_be_opened_for_writing`), which holds while ranikust
runs. An `ETXTBSY` that a future kernel drops would leave only the first two.
How is ranikust's own binary kept out of a `RUN`'s reach before `execve`
(Q-422)?

## Considered Options

- (a) At start-up, copy the binary into a sealed memfd and re-exec from it, as
  runc does: `/proc/self/exe` then names a file no one can write. It costs one
  copy of the binary (11.6 MiB, static musl, x86_64) in memory for the
  process's life and one `execve` at start-up, a few milliseconds
  **[inferred]**.
- (b) Bind the binary read-only over itself in the build's mount namespace, as
  runc 1.2 does with an overlay **[external]**: one mount at start-up, well
  under a millisecond and no copy **[inferred]**, only where ranikust may
  mount, which is where the full sandbox runs.
- (c) A small static trampoline the child execs first, which then runs the
  command: one more `execve` per `RUN`, about a millisecond **[inferred]**, and
  a second binary to ship or embed, with no change at start-up.
- (d) Keep the barriers above and document the residual.

## Decision Outcome

In the full sandbox ranikust binds its own binary read-only over itself in the
build's mount namespace, as runc 1.2 does, and `Direct` on a container's `/`
keeps the barriers it has today, with the residual documented: option (b) for
the full sandbox and option (d) for `Direct` on `/` (the maintainer,
2026-11-03: "Q-422: (b) cho Full sandbox, (d) cho Direct trên /").

- **Full sandbox** (overlay mode, kaniko mode with a directory per stage): one
  bind mount of the binary over itself, read-only, made at start-up in the
  mount namespace ranikust builds in. No copy of the binary and no re-exec.
- **`Direct` on `/`** (kaniko mode on a container's `/`, where nothing can be
  mounted): no change. The three barriers stay, with their tests, and the
  residual is documented in the threat model, `ETXTBSY` being a kernel
  behaviour that a future kernel may drop. A `RUN` there already shares the
  container with ranikust (ADR-0051), which is why (d) is enough on `/` (the
  question's recommendation).
- **Not taken:** (a) and (c). Q-422's option (c) and Q-451's option (b), an
  executed small init, were one mechanism; neither is built, and Q-451 was
  answered with (a) on the same day (see More Information).
- **Implementation: a follow-up.** Nothing is built by this ADR. Until the
  bind exists, the full sandbox has the barriers above, as `Direct` does.

### Consequences

- Good, because the full sandbox no longer depends on `ETXTBSY` alone: the
  bound file is read-only, so no process of the `RUN` can open ranikust's
  binary for writing through `/proc/<pid>/exe` whatever the kernel does
  **[inferred]**.
- Good, because it costs no memory and no extra `execve` **[inferred]**, unlike
  (a) and (c).
- Bad, because `Direct` on `/` keeps the residual: a `#!/proc/self/exe` line
  still runs ranikust's binary in the container, refused only by its command
  line and by non-dumpability, and a write to it is stopped only by `ETXTBSY`.
- Bad, because the two variants differ: the full sandbox and `Direct` do not
  give the same guarantee, and the documents must say which is which.
- Neutral, because nothing changes for a build until the follow-up lands.

## Evidence

No new measurement; the maintainer decided on the entry of Q-422 as written.

- The barriers [measured]: rnk-sandbox T2 test
  `a_running_binary_cannot_be_opened_for_writing`, and the quick case
  `self-exe` of the smoke suite
  ([testing](../testing.md)): the script path is refused as a subcommand and
  the build fails.
- Costs of the options: the binary's size, 11.6 MiB, as recorded in Q-422;
  the milliseconds are estimates **[inferred]**.
- runc 1.2 binds an overlay over its binary **[external]**, as the entry of
  Q-422 states; not re-checked against runc's source here.

## More Information

Answers Q-422. Refines the T7 row and section 6 of the
[threat model](../security/threat-model.md), and the per-`RUN` sequence of
[sandbox](../design/sandbox.md). Related:
[ADR-0003](0003-dockerfile-trust-and-default-isolation.md) (the full sandbox),
[ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md) (`Direct`
on `/`), [ADR-0080](0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md)
(the init, a copy of ranikust).

Q-451 ("should a `RUN`'s init be a small executed binary instead of a copy of
ranikust?") was answered on 2026-11-03 with option (a): the init stays a
copy-on-write copy of ranikust, at the cost measured for ADR-0080. The answer
is recorded in `docs/open-questions.md`, not in an ADR of its own.

Follow-up (to be built, then the docs below updated): the read-only bind in
`rnk-sandbox`, with a T2 test that the bound binary cannot be opened for
writing; then the threat model (T7, section 6), the sandbox design and the
`rnk-sandbox` card say that the full sandbox binds it and `Direct` does not.
