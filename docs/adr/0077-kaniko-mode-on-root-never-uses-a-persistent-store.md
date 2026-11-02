---
status: accepted
date: 2026-11-02
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0077: Kaniko mode on `/` never uses a persistent store, shares its state directory with no other build, and leaves it empty

## Context and Problem Statement

In kaniko mode on a container's `/`, a `RUN` runs `Direct`: as root, on
the same `/`, with no namespace (ADR-0051). The state directory is a plain
path there, so a `RUN` can write it. The anonymous store of that mode
keeps nothing there with a name (ADR-0051, ADR-0056), but three gaps were
left:

- `ranikust` accepted a state directory holding a persistent store (its
  `.ranikust-state` marker) in every mode, and `Store::open_anonymous` used
  anything it found with a warning. A volume first used by overlay builds
  (a privileged container, or a host store bound into the container) was
  then in a `Direct` `RUN`'s reach. The builds of a persistent store reuse
  its published blobs and extracted layers without hashing them again, so
  a `RUN` could plant a layer under a chain id, and a later build would
  run its files with its own secrets and push the result.
- `Store::open` never took the state directory's `flock`, which only
  anonymous stores took. So a kaniko-mode build on `/` and persistent
  builds ran on one volume at once, and the `RUN` reached the other
  builds' private `builds/<id>` and `tmp/<id>`. ADR-0056's "a second build
  on it is refused" held only between two anonymous stores.
- Refusing a marked directory is not enough on its own. On an empty
  volume, a `RUN` can create the marker and a layer itself, and the next
  persistent build accepts them.

The book suggests the same volume, `/var/lib/ranikust`, for overlay mode in
a container and for kaniko mode on `/`, so sharing one is a plausible slip
under `--mode=auto`. The maintainer chose, on 2026-11-02, option (a) of
the hardening review: "2: chốt A. Tạm thời không cho mount cache, sẽ tìm
giải pháp mount cache trực tiếp sau. note vào backlog". That is: refuse a
persistent store in kaniko mode on `/`, keep no cache there for now, and
record a safe way to keep one later as a backlog question.

## Considered Options

- (a) Kaniko mode on `/` refuses a persistent store and keeps nothing
  between builds. One state-directory lock is honoured by both kinds of
  store.
- (b) Allow sharing, but hash every published entry again on every reuse.
- (c) Keep the behaviour, and document it as an accepted risk.

(b) would hash gigabytes on every build. It would still race a `RUN` that
writes while a persistent build runs, and it would not cover the other
builds' unpublished directories. (c) leaves a Dockerfile able to reach the
images and secrets of other builds.

## Decision Outcome

Chosen: (a). Kaniko mode on `/` builds with a temporary store only, never
beside another build on its state directory, and leaves the directory
empty at its end.

- **Refused on open.** `Store::open_anonymous` accepts a state directory
  holding nothing but its own marker (`.ranikust-building`) and names an
  earlier build left (`.ranikust-anon-*`).
  - Anything else is refused with `Error::StateNotEmpty`, under the lock
    and before anything is removed or written. That includes a persistent
    store's `.ranikust-state`, its layout, and files ranikust did not leave.
  - `ranikust::statedir` keeps its own check for every mode. This refusal
    comes after it, so the marked case, which that check accepts, is
    refused here.
- **One lock for both stores.** Each store holds a `flock` on the state
  directory for its life, taken before anything is written:
  - an anonymous store holds it exclusive, as before;
  - a persistent store holds it shared.

  Persistent builds still share a state directory. A kaniko-mode build on
  `/` never runs beside any other build there: `Error::Busy`, whichever
  starts first. ADR-0056's "a second build on it is refused" now holds
  across modes.
- **The marker is this build's.** After every `RUN`, and first thing at
  the end, the store makes `.ranikust-building` a regular file holding this
  build's mark again. Anything else under that name is removed through
  descriptors and written anew: nothing, a link, a FIFO, a directory a
  `RUN` filled, or another container's text, which would let the next
  build take the marker over. A marker a `RUN` changed poisons the fence,
  so the build makes no more names, and the marker stays at the end.
- **Emptied at the end.** `Store::close` of an anonymous store first closes
  the store's files, so that names a filesystem keeps for open removed
  files go with them. It then removes whatever a `RUN` left in the state
  directory, through descriptors (`rnk_rootfs::delete_contents`), never
  following a link or crossing a mount, and logs a warning that names
  what it removed. The lock on the directory goes last.
  - `.ranikust-building` goes only when the final listing shows nothing
    else and the build was clean. It stays after a build cut short, a
    `RUN` that may have left processes (ADR-0066), or a marker a `RUN`
    changed.
  - Anything that could not be removed, an entry on another mount say,
    is `Error::NotEmptied`. That fails the build, with a message saying
    not to give the directory or its volume to another build. A store
    still in use at the end, which cannot be closed, fails the build the
    same way, after its marker is written back.
  - A `RUN` can keep the marker from being made at the start of the close,
    by filling the filesystem (root has no reserve of inodes). The close
    then logs it and goes on. The emptying removes a forged
    `.ranikust-state` first, which frees an inode and leaves a directory
    no persistent store opens (`check_marker` and `ranikust`'s check refuse
    a non-empty one without it), and the marker is tried again right
    then, before the rest, which can take long. It is tried once more
    after the emptying, which frees what the `RUN` wrote, and stays. If it still cannot
    be made, the close fails with `Error::Unmarked`, which says that
    nothing marks the directory, or that what holds the marker's name is
    something a `RUN` made, and that the directory must be deleted; no
    message then claims the marker is there.
- **A `RUN` cannot stop the build.** A `Direct` `RUN` shares ranikust's PID
  namespace. As the container's PID 1, ranikust gets only the signals it
  handles, and it handles `SIGINT` and `SIGTERM` to stop a build, so a
  `RUN` could send them twice and skip the end.
  - On `/`, both are blocked before any thread starts and read through a
    `signalfd`, which gives the sender (`rnk_sandbox::StopSignals`).
  - A signal is obeyed only from the kernel (a terminal), from outside the
    PID namespace (`docker stop`, a kubelet: pid 0) or from ranikust's
    parent, and only as sent by `kill` or `tgkill`, whose sender the
    kernel sets.
  - Any other stop signal (a `RUN`'s) cancels the build, which then ends
    through `Store::close` as a failed build: the `RUN` killed, the marker
    made this build's again, the directory emptied. It never counts
    toward the immediate exit. A `RUN` can always end its own build, by
    failing, so this gives it nothing; it does stop a `RUN` from hiding
    the operator's stop: a flood of `SIGTERM` keeps one pending, so the
    operator's coalesces into it, and that flood itself now ends the
    build. The warning is logged at most every 10 seconds.
  - Before the second signal's exit, the marker is written back, since
    `Store::close` never runs then.
- **A volume is warned about.** A state directory that is a mount root (a
  volume) gets a warning that it must never serve overlay-mode or
  per-stage builds. One on another mount than `/` without being its root
  (a volume mounted at a parent, where a `RUN` can rename it and leave a
  look-alike) gets a warning to mount the volume at `--state-dir`
  itself.
- **Persistent stores keep off it.** `Store::open` refuses a state
  directory holding `.ranikust-building` in any form, with
  `Error::UsedOnSlash`. A kaniko-mode build on `/` used it and did not end
  cleanly, so its `RUN` may have written what a persistent store would
  reuse. A forged `.ranikust-state` beside the marker changes nothing.
- **No cache in that mode, by design for now.** No layer cache is kept
  between builds. `RUN --mount=type=cache` is left out with a warning, as
  ADR-0057 decided, and the warning now says that the mode keeps no cache.
  A registry cache (Phase 4) is not affected. How kaniko mode on `/` could
  keep a layer cache and cache mounts safely is Q-441.
- **The store's premise stays.** With the rules above, a persistent
  store's published entries are out of a `Direct` `RUN`'s reach. Builds
  keep reusing them without hashing again (`rnk-store` card). The invariant
  is tested: a directory a kaniko-mode build on `/` used is left empty, or
  is refused by persistent stores.

Not covered, and why the operator's rule carries the weight:
- A `RUN` can still end ranikust before its end.
  - Without ranikust as PID 1 (a CI shell, `docker run --init`), a `RUN`
    can `SIGKILL` it.
  - Even as PID 1, a `RUN` can make the kernel kill it, for example by
    raising ranikust's `oom_score_adj` and exhausting memory.
  - If it also removed `.ranikust-building` between two checks, what it
    wrote stays, a forged store marker included, and a later persistent
    store would open on it. A `RUN` is root on that filesystem, so no
    marker it can reach proves anything, and nothing in ranikust can stop
    a `RUN` from killing it there.
- The operator's rule is therefore load-bearing, not a footnote: a state
  volume used by kaniko mode on `/` never later serves overlay-mode or
  per-stage builds (threat model, section 6). Sharing one across builds
  in turn needs the store's entries checked against a key no `RUN` can
  reach, which is part of Q-441.
- A volume must be mounted at `--state-dir` itself. If it is mounted at a
  parent, a `RUN` can rename the state directory during the build and put
  a look-alike in its place. The store keeps marking the directory it
  holds, the renamed one, and the build fails (`leave_run`), but the
  look-alike left at the `--state-dir` path holds no marker. The message
  says to discard that directory or volume.
- Writes before a refusal are benign. `Store::open` creates a missing
  state directory, empty, before it takes the lock, since the lock needs
  the directory. `ranikust::statedir` may set a marked directory to mode
  `0700` before the store refuses it on `/`, and directories ranikust made
  have that mode already.

### Consequences

- Good, because a Dockerfile built in kaniko mode on `/` can no longer
  plant entries that other builds reuse, or read and change their private
  build directories, through a shared state volume.
- Good, because ADR-0051's "the directory stays empty" now holds against
  a `RUN`'s writes too, and the same-volume slip under `--mode=auto` is a
  clear refusal instead of a silent sharing.
- Bad, because kaniko mode on `/` keeps no local cache between builds. A
  volume that once held a persistent store must be emptied, or replaced,
  before kaniko mode on `/` uses it.
- Bad, because a persistent build takes one more `flock`. A filesystem
  without `flock` is refused (`Error::NoFlock`), as its per-digest locks
  already were.
- Bad, because in kaniko mode on `/` a `SIGTERM` from a process of the
  container other than ranikust's parent (a sibling script) only cancels
  the build: a second one does not stop ranikust at once. Stop it at once
  with `docker stop`, from outside, or from the parent.
- Neutral, because a clean kaniko-mode build on `/` whose `RUN` wrote
  nothing in the state directory sees no change.

## Evidence

- [code] `rnk-store` tests:
  - `an_anonymous_store_refuses_a_persistent_store`;
  - `an_anonymous_store_refuses_files_it_did_not_leave`;
  - `a_kaniko_build_on_slash_and_other_builds_never_share_a_state_directory`;
  - `a_kaniko_build_on_slash_leaves_nothing_its_run_wrote`, which plants a
    marker, a layer under a chain id, a symlink, a hardlink and a FIFO, and
    leaves the files they point to untouched;
  - `a_kaniko_build_on_slash_cut_short_keeps_persistent_stores_off_its_directory`.

  All five failed before the change. Written with the fixes of the
  host-safety review:
  - `a_marker_a_run_removed_is_written_again_and_stays`;
  - `a_marker_a_run_rewrote_for_another_container_is_replaced_and_stays`;
  - `a_marker_that_is_a_full_directory_is_replaced_by_a_fresh_one`;
  - `a_failed_close_after_a_run_removed_the_marker_keeps_one_and_fails`;
  - `the_marker_keeper_writes_a_removed_marker_where_the_store_is_never_closed`;
  - `a_tree_deeper_than_path_max_is_emptied_at_the_end`;
  - `a_refusal_names_what_the_state_directory_holds`;
  - `rnk-sandbox` `only_the_operators_stop_signals_are_obeyed`.
- [measured] VM `secrets` suite, `stopsig` scenario: a `RUN` removes the
  marker, forges a store on a volume and sends ranikust (PID 1) `SIGTERM`
  twice. The signals only cancel the build: it fails, never with the
  immediate exit 130, they are reported as not the operator's, and the
  volume keeps only the marker.
- [inferred] The poisoning and the cross-build reads were traced from
  source in a security review. They were not run: the tests above check
  the store's protocol, not a build.

## More Information

- Refines [ADR-0056](0056-kaniko-mode-state-on-disk-behind-a-run-fence.md):
  its exclusive lock is now honoured by persistent stores as well.
- Refines [ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md):
  the state directory stays empty even against a `RUN`'s writes, and is
  never a persistent store.
- Keeps [ADR-0057](0057-direct-run-heredoc-from-memfd-and-refusals.md)'s
  handling of cache mounts.
- Related: [ADR-0006](0006-persistent-store.md) (the persistent store),
  [ADR-0007](0007-run-anywhere-shared-store.md) (private build directories),
  [ADR-0066](0066-kaniko-mode-on-root-makes-no-names-after-a-build-cut-short.md)
  (when the marker stays).
- Backlog: Q-441 (a safe cache for kaniko mode on `/`).
- Threat model rows T13 and T21, and section 6.
