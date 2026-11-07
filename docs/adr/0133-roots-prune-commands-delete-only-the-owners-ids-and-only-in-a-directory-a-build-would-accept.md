---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0133: Root's `prune` commands delete only the owner's ids in a user's state directory and are given only for a directory a build would accept

## Context and Problem Statement

[ADR-0105](0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md)
makes `ranikust prune --all` refuse when root runs it: root removes a state
directory with ordinary tools. Root is printed the shell commands that empty
and remove the directory (the pull request of
[#76](https://github.com/tran4774/ranikust/issues/76)). Review of that pull
request found a Low weakness in them, in general words: root's emptying of
another user's state directory deleted entries of other accounts that the owner
had moved into it. The code is unreleased.

The question put to the maintainer on 2026-11-07: is the finding fixed in the
pull request of #76, or does it go to a draft advisory, which is
[ADR-0111](0111-a-low-or-medium-security-finding-is-a-draft-advisory-that-does-not-block-a-release.md)'s
default for a Low finding?

## Considered Options

- Fix it in the pull request of #76 ("Sửa luôn trong PR (khuyến nghị)").
- A draft advisory with a later fix, ADR-0111's default for a Low finding. Not
  taken: the code is unreleased, so there is nothing to advise on.

For how to fix it, three alternatives were listed with the decision and not
chosen:

- `-nouser` for the emptying: it also matches the subordinate ids of other
  users **[inferred]**, which is what the fix must leave alone.
- Run root's emptying in a user namespace that maps only the owner's ids. No
  reason beyond the choice made below was recorded.
- Give root commands only for directories root owns. A user's state directory
  then gets none **[inferred]**.

## Decision Outcome

The Low finding is fixed in the pull request of #76, not sent to a draft
advisory, and root's commands for a user's state directory delete only the
entries of the owner and of the owner's subordinate ids, run only after checks
on the directory, and are given only for a directory a build would accept (the
maintainer, 2026-11-07, answering whether a Low review finding is fixed in the
pull request of #76 or goes to a draft advisory: "Sửa luôn trong PR (khuyến
nghị)", fix it in the pull request (recommended)).

The maintainer answered where the fix goes. The five choices below were made
by the agent in the same pull request, under that answer, and refine
ADR-0105 for the commands root is given.

1. **Which entries.** For a directory owned by uid U other than 0, the
   emptying deletes only entries owned by U or by U's first `/etc/subuid`
   range: the ids the build's user namespace maps, read with the same
   `subids::lookup`. With no range (`subids::lookup` gives none) or a failed
   read, only U. Anything else is left in place, and the commands stop before
   `rmdir`, saying so. `-nouser` is not used. Root's own directory keeps the
   unfiltered emptying, because root's store holds image uids.
2. **Which directories.** Root gets commands only for a directory a build
   would accept: the build's state-directory rule (`statedir::check_chain`
   with the directory's owner as the user, so the directory is not writable by
   group or others and its parents are safe) and not a system directory.
   Before this rule, commands were given for any marked directory whose
   top-level names the store makes.
3. **The directory reached.** After one `cd -P`, the commands check the
   directory's device, inode, owner and mode (`stat -c %d:%i:%u:%a`). The mode
   is the one the rule judged in 2.
4. **`PATH`.** The commands refuse to run when root's `PATH` holds an empty or
   relative entry, because they run tools from inside a directory another
   user owns.
5. **Tools and mounts.** They require GNU findutils with `FTS(FTS_CWDFD)` (a
   walk by descriptor), as already decided in the pull request. `-xdev` stops
   at another filesystem but not at a bind mount of the same one, so the
   guidance requires that nothing is mounted below the directory. ranikust's
   own deletion compares mount IDs.

Not covered: the refusal when root runs `prune --all`, which stands.

### Consequences

- Good, because root's emptying no longer deletes entries of other accounts in
  a user's state directory, and the commands are corrected before any release.
- Good, because root gets commands only for a directory a build would use, so
  a directory with an unsafe mode or parents gets none.
- Good, because the commands check the directory they reached, and do not run
  with a `PATH` that can resolve a tool inside it.
- Bad, because another account's entries in a user's state directory stop
  root's emptying, and root removes them by hand.
- Bad, because a directory a build would refuse gets no commands.
- Bad, because a shell with a relative `PATH` entry gets a refusal.
- Bad, because the commands cannot see a bind mount of the same filesystem
  below the directory: the guidance asks that there is none, where ranikust's
  own deletion compares mount IDs.
- Neutral, because root's own state directory keeps the unfiltered emptying.

## Evidence

Nothing is measured here. The commands and their tests are in the pull request
of #76, not on `main` yet.

- **[code]** `statedir::check_chain` (`crates/ranikust/src/statedir.rs`) is
  ADR-0050's rule: the first entry of the chain is the state directory, owned
  by the user and not writable by group or others, and each parent is owned by
  root or the user and not writable by group or others, but root's sticky
  directory.
- **[code]** `subids::lookup` (`crates/ranikust/src/subids.rs`) gives the
  first range of the user in `/etc/subuid` and `/etc/subgid`, none for root,
  and none where either file has no usable range. It errors on a file that
  cannot be read, a missing file giving no range.
- The [threat model](../security/threat-model.md) T4 (stage cleanup deletes
  user data) states why ranikust's own deletion compares the mount ID:
  `st_dev` misses bind mounts of the same filesystem.
- **[inferred]** `-nouser` also matches other users' subordinate ids: nothing
  measures it.

## More Information

Refines ADR-0105 for the commands root is given; ADR-0105 stays accepted and
its body is not rewritten. Related: ADR-0111 (a Low finding is a draft
advisory by default; not taken here, as the code is unreleased, and the finding
is worded in general words, with no attack steps),
[ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)
(why a state directory holds files of subordinate ids) and
[ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md) (the
state-directory rule).

No `question` issue is answered, so the pull request that carries this ADR
closes none.

Follow-ups, in the pull request of #76: the T4 row of the
[threat model](../security/threat-model.md), the `prune` section of
[cache-and-store](../design/cache-and-store.md#prune), the `ranikust` card,
`ARCHITECTURE.md` and the book, and the `## Changelog` entry.
