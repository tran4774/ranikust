---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0057: A `Direct` `RUN` on `/` runs a `#!` heredoc from a sealed memfd, drops cache mounts, refuses secret files and `--network=none`; every `RUN` writes its streams to pipes of its own

## Context and Problem Statement

Kaniko mode on a container's `/` runs `RUN` as a plain child process
(`Isolation::Direct`, ADR-0043, ADR-0050): no mount, no namespace. BuildKit
runs a `#!` heredoc from a file in a directory bound at `/dev/pipes`; the
full sandbox does the same with a directory in the state directory. On `/`
neither works: nothing can be bound, and ADR-0056 keeps no named file of
ranikust while a `RUN` runs. Before this ADR a `#!` heredoc on `/` failed on
a build directory the anonymous store never names, and `RUN --mount`,
secret files and `--network=none` failed with a generic "Isolation::Direct
cannot mount" message. The issue mining (`issue-mining/reports/summary.md`
items 2 and 7) added two cases: processes that outlive a `Direct` `RUN`
(swept since ADR-0051, zombies untested) and a `USER` other than root
writing `/dev/stdout` (kaniko#2928). The host-safety review added a third:
a root `RUN` inherited ranikust's own stdout and stderr, so with
`ranikust … > build.log` it could `ftruncate` or `pwrite` the operator's
log or set `O_NONBLOCK` on it. The maintainer ranks security, then
performance, then few flags (ADR-0049), and settled Q-380 to Q-382 on
2026-11-01: "Q380-382 làm theo đề xuất".

## Considered Options

- `#!` heredoc: (a) a sealed memfd executed with `execveat`, the
  interpreter reading it as `/dev/fd/<n>`; (b) a file placed under the
  container's `/dev/pipes` and removed after the `RUN`; (c) refuse it.
- Secret as a file (Q-380): refuse; a file on `/` removed before the walk;
  a symlink to a memfd; an inherited memfd with its path in a variable.
- Cache mount (Q-381): refuse; copy the cache in and out; leave the mount
  out with a warning.
- `--network=none` (Q-382): refuse; a seccomp filter on `socket`; a user
  and network namespace where seccomp allows; the container's network.
- Streams: (a) a pipe per stream, owned by the `RUN`'s user, copied by a
  thread, for every `RUN`; (b) change the owner of ranikust's own
  streams, as runc does for a container's; (c) pipes for non-root `USER`s
  only.

(b) for the heredoc names a file every process of the container can read
and a crash leaves behind. Writing a secret on `/` puts it on the
container's disk; running a `none` build on the network gives it what it
refused. Copying a cache costs two copies per `RUN` and leaves it behind
on a crash, while BuildKit documents that a build must not depend on a
cache's contents, so running without one is still correct. (b) for the
streams changes the owner of whatever ranikust's stdout is, the operator's
terminal or log file included; (c) leaves a root `RUN` holding them.

## Decision Outcome

A `Direct` `RUN` on `/` runs a `#!` heredoc from a sealed memfd, runs
without its cache mounts, refuses a secret as a file and `--network=none`,
and no `RUN` in any mode holds ranikust's own stream descriptors.

- **Heredoc.** `rnk-builder` passes the body as `RunSpec::script`; the
  argv stays BuildKit's `/dev/pipes/<name>` for the history only.
  `rnk-sandbox` writes it to a memfd (`MFD_EXEC` where the kernel knows
  it), seals it (`F_SEAL_SEAL | SHRINK | GROW | WRITE`), and the child
  clears close-on-exec on that one descriptor and calls
  `execveat(fd, "", AT_EMPTY_PATH)`. The script's `$0` is `/dev/fd/<n>`.
  Refused, never replaced by a named file, where the container has no
  `/dev/fd` directory or the kernel forbids executable memfds
  (`vm.memfd_noexec=2`), and on a `Direct` root other than `/` (no
  `/proc` there). The full sandbox keeps BuildKit's bound directory.
- **Secret as a file** (no `env=`, or a `target=`): refused, pointing to
  `env=` (Q-380). A secret as a variable works: it needs no file.
- **Cache mount**: left out with a warning naming its target; the `RUN`
  runs without the cache, and the warning says a `sharing=locked` cache
  then serializes nothing (Q-381). `cache,from=` stays refused.
- **`--network=none`**: refused, the flag at start-up before the pull and
  a `RUN --network=none` at that `RUN` (Q-382).
- **Order.** In every mode the mount types this phase lacks (`bind`,
  `tmpfs`, `ssh`, `cache,from=`) are refused before any secret is read or
  cache leased; under `Direct` the secret-file refusal and the cache
  drop come before that too. `Error::Direct` in the sandbox backs the
  builder's checks. `bind`, `tmpfs`, `ssh` and `from=` on `/` wait for
  Phase 5 [open: Q-383].
- **Streams.** For every `RUN`, each of stdout and stderr it would
  inherit becomes a pipe (owned by the `RUN`'s ids when not root; an id
  ranikust cannot give leaves it root's), copied by one thread per `RUN`
  to ranikust's stream. Once the `RUN` is reaped and swept, an `eventfd`
  stops the thread: it reads what is left without blocking and gives a
  stream that does not take writes two seconds, so a write end that
  outlived the `RUN` (over a Unix socket, or reopened through
  `/proc/<pid>/fd` by a process of the same user) never holds the build.
  A full stream (`EAGAIN`) is waited for with `poll`, not dropped.
- **`CAP_SETPCAP`.** `Sandbox::start_direct` checks what ADR-0051 checks
  per `RUN` (capabilities beyond Docker's defaults and no `CAP_SETPCAP` to
  drop them), so such a container is refused before the pull.
- **Survivors.** A T2 test asserts that no process of a `Direct` `RUN`
  remains, alive or zombie.

### Consequences

- Good, because no file of a heredoc is named in the container: nothing to
  remove before the walk, nothing left by a crash, nothing in a layer.
- Good, because a build never silently gets less isolation than it asked
  for: no secret written to the image's files, no network for a `none`
  build; a dropped cache costs time only, and says so.
- Good, because no `RUN` can truncate, rewrite or make non-blocking the
  operator's terminal or log, and `RUN echo > /dev/stdout` under
  `USER nobody` works in both modes, as under BuildKit.
- Bad, because secret files and `--network=none` do not work in kaniko
  mode on `/`; such builds need overlay mode (a container that may
  mount). A `sharing=locked` cache used as a lock serializes nothing
  there.
- Bad, because every `RUN`'s output goes through a pipe and a thread: one
  copy per byte, small next to the command's own work; output a stream
  still refuses two seconds after the `RUN` ended is dropped.
- Neutral, because the script descriptor stays open in the `RUN`'s
  processes (the interpreter must reopen it); it holds only the script,
  sealed. A `RUN`'s streams are no longer a terminal.

## Evidence

- T1 (`RANIKUST_CASE_MODE=kaniko just smoke-case run`, rootless podman,
  no mount privilege): the `#!` heredoc, the secret as a variable, the
  dropped cache mount (warning checked) and `USER nobody` writing both
  streams build; the secret file and both `--network=none` forms are
  refused [measured, 2026-11-01].
- T0: `heredoc_scripts_are_sealed_memfds`,
  `direct_refuses_what_needs_a_mount_or_a_namespace`,
  `direct_runs_take_secrets_as_variables_and_drop_caches`,
  `unsupported_mounts_are_refused_before_any_secret_is_read`,
  `direct_runs_refuse_network_none`, the relay's
  `finish_does_not_wait_for_a_surviving_writer` and
  `a_blocked_stream_is_given_up_after_the_drain_time`. T2:
  `a_direct_heredoc_runs_from_an_anonymous_file` (root and uid 65534),
  `a_direct_run_leaves_no_process_behind` (alive or zombie).
- binfmt_script names a script executed through a descriptor
  `/dev/fd/<n>` and refuses a close-on-exec descriptor [external, Linux
  `fs/binfmt_script.c`, `fs/exec.c` `BINPRM_FLAGS_PATH_INACCESSIBLE`].
- runc gives a container's stdio to the container's user
  (`fixStdioPermissions`) [external]; kaniko#2928 is the failure without.

## More Information

Refines ADR-0043 (what `Direct` runs) and ADR-0051 (the `CAP_SETPCAP`
check, the sweep); follows ADR-0056 (no named state while a `RUN` runs).
Answers Q-380, Q-381, Q-382; Q-383 (`bind`, `tmpfs` and `from=` mounts on
`/`) stays open for Phase 5. Revisit Q-382's seccomp option if builds on
`/` need `--network=none`.
