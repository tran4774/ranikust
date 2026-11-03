---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0085: A secret file on `/` goes at its mount's target, only its own path is ignored, and the file placed for a `RUN` is removed after that `RUN`

## Context and Problem Statement

[ADR-0084](0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md)
(2026-11-03) allows secret files in kaniko mode on a container's `/` and puts
"the directory that holds the secret files" on the snapshot ignore list. The
maintainer's words did not say which directory, where ranikust places a secret
file for a `RUN`, or whether it removes the file afterwards. That was Q-453.

Two kinds of file are involved. A `--secret src=` file lies wherever the
operator put it and is read once at the start (ADR-0078). The file a `RUN
--mount=type=secret` reads has to be written into the `/` all `RUN`s share,
since on `/` there is no tmpfs of the `RUN`'s own (ADR-0057); in the other
modes it is placed at the mount's `target=`, else `/run/secrets/<id>`, mode
`0400` (`secret_mount` in `crates/rnk-builder/src/mounts.rs`). An ignore list
entry covers the path and everything below it, which the walk skips, base
extraction does not replace and emptying the root does not delete, so ignoring
the directory of `--secret src=/etc/token` would ignore `/etc`.

## Considered Options

Which entry goes on the ignore list:

- (a) The secret file's own path only: the `src=` file, and the placed file at
  its target.
- (b) The file's directory, whatever it is.
- (c) A directory ranikust fixes for placed files, `/run/secrets`, with a
  `target=` outside it refused, and the `src=` file's own path as in (a).

For the placed file:

- (d) Removed after its `RUN` ends, so a later `RUN` cannot read it, and a
  crash leaves it only until the container ends.
- (e) Left until the build ends.

## Decision Outcome

In kaniko mode on `/`, only a secret file's own path goes on the snapshot ignore
list, never its directory, and the file placed for a `RUN` is removed after that
`RUN`: options (a) and (d) of Q-453, with both refusals that ADR-0084 lifts gone
(the maintainer, 2026-11-03: "Q-453 chọn (a)+(d), gỡ cả hai luật").

- **Ignore scope (a).** For a `--secret src=` file, its own path; for the file
  placed for a `RUN`, its own path, which is its target. No directory is added.
  `--secret src=/etc/token` ignores `/etc/token`, not `/etc`. Where ADR-0084 says
  "the directory that holds the secret files", read: the file's own path.
- **Placement.** The placed file goes at the target its `RUN --mount=type=secret`
  names, else `/run/secrets/<id>`, with the mode and owner the mount gives
  (default `0400`, root's), as `secret_mount` computes them for the other modes.
- **Removal (d).** The placed file is removed after its `RUN` ends, before the
  walk, so no later `RUN` can read it. A `--secret src=` file is the operator's
  and is not removed; it stays on `/` for the build, readable by every `RUN`
  (ADR-0078).
- **Both refusals go.** The start-up refusal of a `--secret src=` file that lies
  in the root outside the ignore list (`paths_outside_the_root` in
  `crates/ranikust/src/main.rs`), and ADR-0057's refusal of a `RUN
  --mount=type=secret` as a file (`direct_mounts` in
  `crates/rnk-builder/src/mounts.rs`). This confirms the reading of ADR-0084.
  The same start-up check for the context, the outputs and the metrics report
  stays.
- **Not decided here.** What the placement does when the target already exists
  in the image, or when its parent directories do not [open: Q-454].
- **Implementation: a follow-up.** Nothing in `crates/` is built by this ADR.
  Until it is, both refusals stand as they are.

### Consequences

- Good, because the ignore is as narrow as it can be: no sibling file of the
  image drops out of a layer, is skipped by base extraction or survives the
  emptying of the root because a secret lies beside it.
- Good, because no later `RUN` can read a secret file that was placed for an
  earlier `RUN` that mounted it.
- Bad, because a `--secret src=` file stays on `/` for the whole build, so every
  `RUN` can read it and can copy it into the image, as ADR-0078 and ADR-0084
  say; removal covers only the placed file.
- Bad, because a build killed during a `RUN` leaves the placed file in the
  container until the container ends, outside the walk (its path is ignored).
- Bad, because a path on the list is left out of layers whatever writes there
  **[code]**, so an image file at the same path as a target would be replaced by
  the placement and unseen by the walk. A target that already holds something
  is Q-454.
- Neutral, because (b), (c) and (e) are not taken.

## Evidence

No new measurement; the decision was taken on the entry of Q-453.

- An ignore entry may be a file path, and covers it and everything below it
  **[code]**: `IgnoreList::insert` in `crates/rnk-rootfs/src/ignore.rs`.
  `paths_outside_the_root` passes a path the list covers (`ignore.covers`).
- The placement and the mode and owner of a secret mount **[code]**:
  `secret_mount` in `crates/rnk-builder/src/mounts.rs`.
- The two refusals **[code]**: `paths_outside_the_root` and `direct_mounts`; the
  tests that pin them are listed in ADR-0084's Evidence.

## More Information

Answers Q-453; opens Q-454. Refines
[ADR-0084](0084-kaniko-mode-on-root-allows-secret-files-and-ignores-their-directory.md):
it replaces the directory by the file's own path and fills the placement and
removal that ADR left open; ADR-0084 stays `accepted`, and its title and body
still say "directory". Related:
[ADR-0057](0057-direct-run-heredoc-from-memfd-and-refusals.md),
[ADR-0078](0078-kaniko-mode-on-root-lends-run-no-capability-and-reads-secrets-once.md),
[ADR-0038](0038-kaniko-mode-settings-for-phase-2.md) (the ignore list).

Follow-ups, with ADR-0084's: answer Q-454; in `ranikust` put the file's own path
on the ignore list for each `--secret src=` file and let `paths_outside_the_root`
pass it; in `rnk-builder` accept a secret file in `direct_mounts`; in
`rnk-sandbox` place the file for a `Direct` `RUN` at its target and remove it
after the `RUN` (T2 test that it is gone before the walk and absent from the
layer); change the tests that pin the refusals; then update the book's
kaniko-mode and usage pages, `design/kaniko-mode.md`, the `rnk-builder` and
`ranikust` cards, the threat model's T21 row and `CHANGELOG.md`.
