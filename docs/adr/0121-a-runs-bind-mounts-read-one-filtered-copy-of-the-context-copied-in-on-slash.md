---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0121: A RUN's bind mounts read one filtered copy of the context, copied in on /

## Context and Problem Statement

`RUN --mount=type=bind|tmpfs|ssh` and `from=` are refused in every mode until Phase 5.
Four questions decide how they work (see [the roadmap](../../README.md#roadmap)):

- **How a bind presents the local context.**
  [ADR-0032](0032-observable-behaviour-follows-buildkit.md) (BuildKit) hides
  `.dockerignore`d files from a bind and discards `rw` writes. The local context is a
  live directory of a lower-trust owner
  ([ADR-0076](0076-the-build-context-owner-is-a-lower-trust-principal.md)), who can
  change it during the build: bound directly, it would show ignored files and, `rw`,
  take writes. The default `source` is the whole context.
- **Mounts on `/`** ([#24](https://github.com/tran4774/ranikust/issues/24), Q-383).
  In kaniko mode on `/` nothing can be mounted
  ([ADR-0057](0057-direct-run-heredoc-from-memfd-and-refusals.md)). kaniko copies a
  context bind in at the target, moves an existing target aside and restores it,
  removes the parents it created, does not enforce read-only, has no tmpfs, and skips
  `from=` with a warning.
- **A placed file's target** ([#31](https://github.com/tran4774/ranikust/issues/31),
  Q-454). [ADR-0085](0085-a-secret-file-on-slash-goes-at-its-target-is-ignored-by-its-own-path-and-removed-after-its-run.md)
  places a secret file at its target on `/` and removes it after the `RUN`. Writing it
  replaces what the image holds there, and creating parents leaves them in the next
  diff. Bind copies on `/` are placed by the same mechanism.
- **`RUN --mount=type=ssh`.** Neither the roadmap nor survey 04 lists it; kaniko only
  warns on it. BuildKit forwards the client's SSH agent into the `RUN`; here every
  process of that `RUN` could sign with the operator's keys.

## Considered Options

Mounts on `/` (#24):

- (a) Refuse them on `/`, as Q-380 and Q-381.
- (b) Copy the source in, read-only by convention only, removed and the target
  restored before the walk: a copy per `RUN`, and residue to clean after a crash.
- (c) `tmpfs` alone, as an empty directory removed before the walk.

A placed file's target (#31):

- (a) Refuse the `RUN` when the target exists or a parent is missing: refuses the
  common `/run/secrets/<id>`.
- (b) Refuse an existing target; create missing parents and remove those ranikust
  created after the `RUN`.
- (c) Replace an existing file and restore it from a copy kept out of every `RUN`'s
  reach: costs that copy.

How a bind presents the context:

- (a) One filtered copy of the context per build, made at the first bind and bound
  read-only by every mount; a `rw` mount gets a private copy of its source, discarded
  after: one context's size on disk, once.
- (b) A filtered copy of each mount's source per `RUN`: no reuse, the copy cost on
  every `RUN`.
- (c) Bind the context directory read-only, refusing a source that holds ignored files
  and refusing `rw`: no copy, but some Dockerfiles cannot build.

`type=ssh`:

- (a) Backlog: it stays refused with its reason, and a Backlog issue keeps it visible.
- (b) Phase 5: a `--ssh` flag, an agent-socket bind, a threat-model row, one more task.

## Decision Outcome

A `RUN --mount=type=bind` without `from=` reads one filtered copy of the local context,
made once per build; on `/`, where nothing can be mounted, a bind is copied in at its
target as kaniko does (an existing target is moved aside and put back), `tmpfs` becomes
an empty directory, and `from=` is refused; a secret file whose target exists is
refused, and missing parents are created and removed again; and `type=ssh` goes to the
Backlog.

The maintainer, 2026-11-06, in the question window: "Bản sao context, như kaniko trên
/ (khuyến nghị)" ("A copy of the context, as kaniko does on / (recommended)"). It stood
for options (b) and (c) of #24 with `from=` refused on `/`, option (b) of #31 with #31
and #35 moved into Phase 5, option (a) of how a bind presents the context, and option
(a) of `type=ssh`.

- **One filtered copy (a).** The copy is filtered as `.dockerignore` filters the
  context, made at the first bind and bound read-only by every mount. A `rw` mount gets
  a private copy of its source, discarded after the `RUN`. Every mount sees the same
  content, as BuildKit's one transferred snapshot, and the owner's later changes cannot
  reach a `RUN`.
- **Three rules for what is at a target on `/`.** One placing mechanism (create the
  missing parents, place, remove) serves all three; they differ in what an existing
  target gets:
  - **Secret files on `/`, #31 (b).** An existing target is refused; missing parent
    directories are created, and the ones ranikust created are removed after the `RUN`.
    This refines ADR-0085. #31 and #35 (the secret files of ADR-0085) move into Phase 5,
    since the bind copies build on the same mechanism.
  - **Bind copies on `/`, #24 (b).** The bind's source is copied in at the target,
    read-only by convention only. An existing target is moved aside and put back before
    the walk, and the parents ranikust created are removed, as kaniko does.
  - **`tmpfs` on `/`, #24 (c).** An empty directory at the target, removed before the
    walk; an existing target is handled as for a bind.
- **Why the rules differ.** A secret must never be mixed with what the image holds, so a
  secret file is refused where something already is. A bind copy is kaniko's established
  behaviour, so kaniko's Dockerfiles keep building in the mode meant for them.
- **`from=` on `/`.** It is refused with its reason: kaniko never had it, and a stage
  copy would cost a full tree.
- **`type=ssh` (a).** It stays refused with its reason, and a Backlog issue keeps it
  visible: no user asked, kaniko never had it, and it exposes a new credential.

Not settled here, for the task that builds the placing: how the per-build copy and the
per-`RUN` copy on `/` relate.

### Consequences

- Good, because kaniko's Dockerfiles keep building in kaniko mode on `/`.
- Good, because every mount sees the same content with `.dockerignore`d files hidden,
  and the context owner's later changes cannot reach a `RUN`.
- Good, because one placing mechanism serves secret files and bind copies on `/`, built
  once.
- Good, because a secret file never mixes with what the image holds at its target.
- Bad, because the copy costs one context's size on disk per build, and a `rw` mount a
  private copy besides.
- Bad, because on `/` read-only holds by convention only, a copy is made per `RUN`, and
  a crash can leave residue to clean.
- Bad, because a `from=` bind cannot build on `/`, and `type=ssh` cannot build anywhere,
  until a later decision.
- Bad, because a `RUN --mount=type=secret` whose target already exists in the image is
  refused on `/`.
- Neutral, because an existing target is refused for a secret file and moved aside for a
  bind copy or `tmpfs`: two rules, each stated where the mount is documented.

## Evidence

- **[external]** kaniko@44aa7ddc0 (`FF_KANIKO_RUN_MOUNT_BIND`, on by default): a bind is
  copied in from the context only, any other `from=` is logged and skipped, an existing
  target is moved to a swap directory and restored, missing parents are created and
  removed, and `.dockerignore`d files are not copied
  (`pkg/commands/run.go:219-279`); `ro` and `rw` are not read, so writes are possible and
  vanish **[inferred]**; tmpfs and ssh only warn (`run.go:280-281`). Its integration cases
  are `Dockerfile_test_issue_1568` and `dockerfiles-with-context/issue-1568`.
- **[external]** buildkit@cd8ee244: a bind with no `from=` is the main context filtered by
  `.dockerignore`, read-only unless `rw`; writes of a `rw` mount go to a throwaway child
  that is released (`frontend/dockerfile/dockerfile2llb/convert_runmount.go:72`, `:103-107`,
  `solver/llbsolver/ops/exec.go:428-430`); ssh forwards the client's agent socket
  (`frontend/dockerfile/dockerfile2llb/convert_ssh.go:11-50`).
- **[code]** `check_supported` refuses `bind`, `tmpfs`, `ssh` and `from=` in every mode
  (`crates/rnk-builder/src/mounts.rs:296-335`); ADR-0085's placing is decided and not
  built.

## More Information

Answers #24, #31 and two Phase 5 questions (how a bind presents the context, `type=ssh`).
Refines ADR-0085 (its existing target and missing parents), which stays accepted with its
body unchanged. Each of the three rules is the option the maintainer chose for its
question; setting them side by side, with the reason they differ and the `tmpfs` case
following a bind's, was added when Phase 5 task 13b (#152) needed it, and his words
name the options, not this comparison. Related: ADR-0057 (nothing is mounted on `/`),
[ADR-0084](0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md),
[ADR-0078](0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md).
The marker on #30 in `mounts.rs` stays: a cache mount on `/` is still #30's.

Follow-ups, with Phase 5's tasks and not before they are built:
[sandbox](../design/sandbox.md) and [kaniko-mode](../design/kaniko-mode.md) design docs,
the `rnk-builder` and `rnk-snapshot` cards, the kaniko map, the book, the threat model
(T21, and section 6 for read-only by convention), and the `## Changelog` entry.
