---
status: proposed
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0057: A `Direct` `RUN` on `/` runs a `#!` heredoc from a sealed memfd and refuses what needs a mount or a namespace

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
writing `/dev/stdout` (kaniko#2928). The maintainer ranks security, then
performance, then few flags (ADR-0049).

## Considered Options

- `#!` heredoc: (a) a sealed memfd executed with `execveat`, the
  interpreter reading it as `/dev/fd/<n>`; (b) a file placed under the
  container's `/dev/pipes` and removed after the `RUN`; (c) refuse it.
- `RUN --mount`, secret files, `--network=none`: (a) refuse with the
  reason; (b) emulate (copy files in and out, write secrets as files on
  `/`, run on the container's network).
- `/dev/stdout` for a non-root `USER`: (a) a pipe per stream owned by the
  `RUN`'s user and a relay thread; (b) change the owner of ranikust's own
  streams, as runc does for a container's; (c) leave it.

(b) for the heredoc names a file every process of the container can read
and that a crash leaves behind; (c) loses a common Dockerfile form. (b)
for mounts writes secrets to the image's filesystem or runs a build that
asked for no network on the network: silently weaker than asked. (b) for
the streams changes the owner of whatever ranikust's stdout is, the
operator's terminal or log file included.

## Decision Outcome

A `Direct` `RUN` on `/` runs a `#!` heredoc from a sealed memfd, refuses
what needs a mount or a namespace with its reason, and gives a non-root
`USER` pipes of its own for the streams it inherits.

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
- **Refusals** (`Error::Direct` in the sandbox; earlier, with guidance, in
  the builder and the CLI), before any cache is leased or secret read:
  `RUN --mount=type=cache` [open: Q-381]; a secret as a file (no `env=`,
  or a `target=`) [open: Q-380]; `RUN --network=none` and the
  `--network=none` flag (the flag at start-up, before the pull)
  [open: Q-382]. A secret as a variable (`env=` without `target=`) works:
  it needs no file. `bind`, `tmpfs` and `ssh` stay refused in every mode
  until Phase 5 [open: Q-383].
- **Streams.** For a `RUN` whose `USER` is not root, each of stdout and
  stderr it would inherit becomes a pipe whose inode belongs to that user,
  and a thread copies it to ranikust's stream; joined once the `RUN`'s
  processes are gone. Every isolation, the full sandbox included.
- **`CAP_SETPCAP`.** `Sandbox::start_direct` checks what ADR-0051 checks
  per `RUN` (capabilities beyond Docker's defaults and no `CAP_SETPCAP` to
  drop them), so such a container is refused before the pull.
- **Survivors.** A T2 test asserts that no zombie of a `Direct` `RUN`
  remains, besides no live process.

### Consequences

- Good, because no file of a heredoc is named in the container: nothing to
  remove before the walk, nothing left by a crash, nothing in a layer.
- Good, because a build never silently runs with less than it asked for:
  no secret written to the image's files, no network for a `none` build.
- Good, because `RUN echo > /dev/stdout` under `USER nobody` works in both
  modes, as under BuildKit.
- Bad, because cache mounts, secret files and `--network=none` do not work
  in kaniko mode on `/` until Q-380 to Q-382 are settled; such builds need
  overlay mode (a container that may mount).
- Bad, because a non-root `RUN`'s output goes through a pipe and a thread:
  one copy per byte, small next to the command's own work.
- Neutral, because the script descriptor stays open in the `RUN`'s
  processes (the interpreter must reopen it); it holds only the script,
  sealed.

## Evidence

- T1 (`RANIKUST_CASE_MODE=kaniko just smoke-case run`, rootless podman,
  no mount privilege): the `#!` heredoc, the secret as a variable and
  `USER nobody` writing both streams build; each refusal is reported
  [measured, 2026-11-01].
- T0: `heredoc_scripts_are_sealed_memfds`,
  `direct_refuses_what_needs_a_mount_or_a_namespace`,
  `direct_runs_take_secrets_as_variables_only`,
  `direct_runs_refuse_network_none`. T2:
  `a_direct_heredoc_runs_from_an_anonymous_file` (root and uid 65534),
  `a_direct_run_leaves_no_process_behind` (no zombie).
- binfmt_script names a script executed through a descriptor
  `/dev/fd/<n>` and refuses a close-on-exec descriptor [external, Linux
  `fs/binfmt_script.c`, `fs/exec.c` `BINPRM_FLAGS_PATH_INACCESSIBLE`].
- runc gives a container's stdio to the container's user
  (`fixStdioPermissions`) [external]; kaniko#2928 is the failure without.

## More Information

Refines ADR-0043 (what `Direct` runs) and ADR-0051 (the `CAP_SETPCAP`
check, the sweep); follows ADR-0056 (no named state while a `RUN` runs).
Open: Q-380 (secret files), Q-381 (cache mounts), Q-382
(`--network=none`), Q-383 (`bind` and `from=` mounts) under `Direct`.
