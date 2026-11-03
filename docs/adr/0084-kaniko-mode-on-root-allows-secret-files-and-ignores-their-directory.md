---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0084: Kaniko mode on `/` allows secret files; the directory that holds them is on the snapshot ignore list, and copying one into the image is the user's choice

## Context and Problem Statement

Kaniko mode on a container's `/` runs a `RUN` directly, with nothing mounted
and no tmpfs of its own for the `RUN`, and the build root is the whole `/`,
which the build fills and empties. Three rules about secret files come from
that:

- **A `--secret src=` file in the root is refused at start-up** unless the
  ignore list keeps it. `paths_outside_the_root` in
  `crates/ranikust/src/main.rs` judges the context, the outputs, the metrics
  report and each file secret: one that lies in the root, where extracting a
  base or emptying it would replace or delete it, and that the ignore list does
  not keep (a volume, `/ranikust`, an `--ignore-path`), fails before the root
  changes with a usage error (exit 2): "put it on a volume or outside the
  root". On `/` nothing lies outside the root.
- **A secret as a file for a `RUN` is refused.**
  [ADR-0057](0057-direct-run-heredoc-from-memfd-and-refusals.md) (Q-380,
  option (a), 2026-11-01) refuses a `RUN --mount=type=secret` with no `env=`,
  or with a `target=`, before any secret is read, so that "a secret never
  touches the image's filesystem" (`direct_mounts` in
  `crates/rnk-builder/src/mounts.rs`). A secret as a variable (`env=` without
  `target=`) works.
- **A `--secret src=` file that is allowed gets a warning.**
  [ADR-0078](0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md)
  reads every `--secret` once at the start and warns for each file
  (`file_secret_warnings` in `crates/ranikust/src/exposure.rs`): the file is on
  the `/` every `RUN` shares, so any `RUN` can read it, not only the one that
  mounts it ([threat model](../security/threat-model.md#6-accepted-risks),
  section 6). Q-445 asked whether to refuse such files; its recommendation was
  to refuse them (option (b)).

Does kaniko mode on `/` keep refusing secrets as files, and what keeps a secret
file out of a layer if it does not?

## Considered Options

From Q-445:

- (a) Warn for each file secret and document it.
- (b) Refuse a file secret on `/` and accept secrets only from ranikust's
  environment.
- (c) Read it at the start, then require the operator's tooling to remove it
  before the first `RUN` (unenforceable).

And, from ADR-0057 (not taken then): write a secret file on `/` and remove it
before the walk; a symlink to a memfd; an inherited memfd with its path in a
variable.

## Decision Outcome

In kaniko mode on a container's `/`, `--secret` files work: the directory that
holds the secret files is put on the snapshot ignore list, so the build skips it
and no secret file reaches a layer, which lifts the two refusals that stop a
secret file there today, and what a `RUN` does with a file it can read, such as
copying it into the image, is the user's decision, which the documents warn
about (the maintainer, 2026-11-03: "Bật lại --secret. Bỏ qua thư mục này khi
build (ignore). Còn việc câu lệnh RUN của dockerfile có copy nó đi đâu hay
không thì đó là quyết định của người dùng. Nên ghi cảnh báo vào tài liệu").

- **The refusals that are lifted.** (1) The start-up refusal of a `--secret
  src=` file that lies in the root outside the ignore list
  (`paths_outside_the_root`): with the file's directory on the list the check
  passes, and the build neither replaces nor empties the file. (2) ADR-0057's
  refusal of a `RUN --mount=type=secret` as a file (the `Error::Invalid` of
  `direct_mounts`, "RUN --mount=type=secret as a file: kaniko mode on / runs RUN
  directly ..."). Q-445's option (b) is not taken.
- **What stays.** The same start-up check for the context, the outputs and the
  metrics report. The rest of ADR-0057: the heredoc from a sealed memfd, the
  dropped cache mount, the refusal of `--network=none`, the streams. A secret
  as a variable keeps working. The per-file warning of ADR-0078 is not changed
  by this decision.
- **The ignore list.** The directory that holds the secret files goes on the
  list for the build, as the mount points, `/ranikust` and the state directory
  already are ([ADR-0038](0038-kaniko-mode-settings-for-phase-2.md)): the walk
  never descends into it, so no secret file reaches a layer through the
  snapshot. Which directory that is for a `--secret src=` file, where ranikust
  places a secret file for a `RUN` on `/`, and whether it removes the file after
  the `RUN`, the maintainer's words do not say (Q-453, answered in ADR-0085).
- **What a `RUN` does with it is the user's.** Every `RUN` on `/` can read a
  secret file that exists, and can copy it into the image or anywhere else;
  ranikust does not stop it, and the Dockerfile's author decides.
- **The documents warn.** The book's usage page (`--secret`) and section 6 of
  the threat model say that every `RUN` on `/` can read a secret file and that
  copying it into the image is the user's choice.
- **Implementation: a follow-up.** Nothing in `crates/` is built by this ADR.
  Until it is, both refusals stand as they are, and the follow-up needs Q-453.

### Consequences

- Good, because a Dockerfile that uses `RUN --mount=type=secret` as a file,
  BuildKit's default form, builds on `/` instead of being refused and rewritten
  for `env=` or for overlay mode, and `--secret src=` works without the operator
  also naming the directory in `--ignore-path`.
- Bad, because ADR-0057's promise that a secret never touches the filesystem
  of the container ends for the file form: the secret is a file on `/` while
  the `RUN` that mounts it runs, and what keeps it out of a layer is the
  ignore list, not its absence.
- Bad, because every `RUN` on `/` can read a secret file that exists, not only
  the `RUN` that mounts it, as ADR-0078 already says for `--secret src=`; how
  long a placed file exists is part of Q-453.
- Bad, because everything below a directory on the ignore list is left out of
  layers, and is not replaced when a base is extracted nor deleted when the
  root is emptied **[code]** (the comment of `paths_outside_the_root`;
  `IgnoreList` in `rnk-rootfs`). A directory such as `/etc`, if it were chosen
  because a secret lies in it, would drop the image's own files. Which
  directory is ignored is Q-453.
- Neutral, because Q-445's recommendation (b) put security first, so that a
  secret handed to one `RUN` does not reach every `RUN`; the maintainer chose
  to keep such builds working and to leave the copying to the user.

## Evidence

No new measurement; the decision is about rules, and it was taken on the
entries of Q-445 and Q-380.

- The start-up refusal **[code]**: `paths_outside_the_root`
  (`crates/ranikust/src/main.rs`), with the tests around it in the same file,
  which change with the implementation.
- The refusal of a `RUN`'s secret file **[code]**: `direct_mounts` in
  `crates/rnk-builder/src/mounts.rs`; the tests that pin it:
  `direct_runs_take_secrets_as_variables_and_drop_caches` and
  `red_team_direct_secret_mount_spellings` (`rnk-builder`, `mounts/tests.rs`),
  and the `run` case with `RANIKUST_CASE_MODE=kaniko` of the smoke suite
  ([testing](../testing.md)). It was verified when it was made
  **[measured]**, 2026-11-01 (ADR-0057 Evidence): under rootless podman, with no
  mount privilege, the secret as a variable built and the secret file was
  refused.
- The warning today **[code]**: `file_secret_warnings` in
  `crates/ranikust/src/exposure.rs`, with the test
  `secret_variables_cover_env_secrets_and_valueless_build_args`.
- The ignore list keeps a path and everything below it **[code]**:
  `IgnoreList::insert` in `rnk-rootfs`; a file path is a valid entry as well as
  a directory's.
- BuildKit places a secret file at the target, else `/run/secrets/<id>`, mode
  `0400` **[code]** (`secret_mount` in `crates/rnk-builder/src/mounts.rs`,
  following BuildKit's `secretmount_unix.go`).

## More Information

Answers Q-445. Partly supersedes
[ADR-0057](0057-direct-run-heredoc-from-memfd-and-refusals.md): its refusal of
a secret as a file (Q-380) no longer stands once this is built; ADR-0057 stays
`accepted` and the rest of it stands. Refines
[ADR-0078](0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md)
(Q-445 closed) and the ignore list of ADR-0038 (an entry made per build).
Open: Q-453 (which directory, where the file goes, removal).

Follow-ups, in this order: answer Q-453; then in `ranikust` add the directory
to the ignore list and let `paths_outside_the_root` pass a secret file it keeps;
in `rnk-builder` accept a secret file in `direct_mounts`; in `rnk-sandbox` place
the file for a `Direct` `RUN` (and remove it, if Q-453 says so); change the tests
above; then update the book's kaniko-mode page and usage page,
[kaniko-mode](../design/kaniko-mode.md), the `rnk-builder` and `ranikust`
cards, the threat model's T21 row and `CHANGELOG.md`, which say the file form
is refused or must be on a volume.
