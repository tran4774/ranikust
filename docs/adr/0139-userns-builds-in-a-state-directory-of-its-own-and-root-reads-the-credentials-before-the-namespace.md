---
status: accepted
date: 2026-11-08
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0139: `--userns` builds in a state directory of its own, and root reads the credentials before the namespace

## Context and Problem Statement

[ADR-0104](0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)
decided how `--userns` runs
([#83](https://github.com/tran4774/ranikust/issues/83)): the whole build in a
user namespace whose root is the first uid and gid of a `ranikust` range of
`/etc/subuid` and `/etc/subgid`, and, as its option (d), extracted layers keyed
by their id map in the same state directory as root's builds. Building #83
showed two things ADR-0104 did not weigh. The namespace's root is an
unprivileged uid of the host, which cannot reach root's state directory, root's
own and `0700`
([ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md)); opening it
to that uid would expose root's extracted layers, whose setuid-root files are
real ones. And the build runs as that uid on the host, so it reads the registry
credentials and the `--secret` sources with that uid's permissions, which root
usually never granted it. Which state directory does `--userns` use, and who
reads the credentials? Building #83 also settled smaller points, recorded here
with them.

## Considered Options

The state directory:

- (d) ADR-0104's: layers keyed by their id map in root's own state directory,
  opened to the range's first uid.
- (e) A state directory of its own, owned by the range's first uid and gid
  (recommended, chosen).

(d) would give the range's uid a way into a directory that holds root's
setuid-root layers; (e) costs a second store, with its own extracted copies and
its own prune.

The credentials:

- Root reads the docker config and the secret sources before the build enters
  the namespace; the context and the Dockerfile stay read by the range's uid
  (recommended, chosen).
- The range's uid reads everything, the operator granting it access to each
  file.

## Decision Outcome

`--userns` builds in a state directory of its own, owned by the first uid and
gid of root's `ranikust` range, and root reads the registry credentials and the
secret sources before the build enters the user namespace.

The maintainer, 2026-11-08: "(e) Store riêng (khuyến nghị)" ("(e) A separate
store (recommended)") for the state directory, and "Root đọc credential
(khuyến nghị)" ("Root reads the credentials (recommended)") for the
credentials.

- **The state directory** is the range's: owned by its first uid and gid,
  `0700`, below directories that only root owns, none writable by group or
  others even where sticky (that uid could rename what it owns there), each
  searchable by those ids by its mode bits. A missing one is made for them,
  with the missing directories above it root's, mode `0711`. Root's own state
  directory is refused, never given away, as is any at, below or above root's
  default, `/var/lib/ranikust`, missing or not: made for the range, it would
  refuse root's own builds there. Each refusal comes before anything is
  written.
- **What this replaces.** ADR-0104's option (d) kept `--userns`'s layers in
  root's state directory, keyed by their id map. Under `--userns` they now live
  in the range's own state directory, never beside root's. They are still keyed
  by the namespace's id map there, as in every user namespace; rootless mode
  keeps (d) as ADR-0104 and
  [ADR-0128](0128-a-canary-guards-nested-namespaces-a-budget-bounds-device-names-and-a-device-left-out-keeps-the-lower-entry.md)
  say.
- **Pruning** that directory is a task of its own,
  [#207](https://github.com/tran4774/ranikust/issues/207).
- **The credentials.** Root reads the docker config and the `--secret` sources
  before the build enters the namespace
  ([#206](https://github.com/tran4774/ranikust/issues/206)). The context and the
  Dockerfile stay read by the range's uid, and one it cannot read is refused
  with a clear error. Until #206 lands, the range's uid reads the credentials
  and the secret sources too, and the docs say so.

Decided while building #83, as implementation choices under ADR-0104:

- **Root only.** `--userns` from a user other than root is a usage error
  (exit 2), before anything is written: only root may write the range's map,
  and that user's build already runs in a user namespace of its own.
- **Never another mode.** A `--userns` build whose set-up fails is refused; it
  never falls back to a mode without the user namespace, as root's
  `--mode=auto` does
  ([ADR-0136](0136-root-auto-mode-falls-back-to-kaniko-mode-from-a-set-up-that-fails-in-the-builds-user-namespace.md)).
  It is a usage error with `--mode=kaniko`.
- **No overlap with real ids** (decided in the host-safety review of #83): a
  `ranikust` uid range that holds an id of `/etc/passwd` or overlaps another
  line of `/etc/subuid` is refused before anything is written, naming the id or
  the line, and the same for the gid range against `/etc/group`, the primary
  gids of `/etc/passwd` and `/etc/subgid`. So is an account named `ranikust`
  in `/etc/passwd` or a systemd userdb record, to which `newuidmap` and
  `newgidmap` would give the range.
  Another line counts as every range a version of shadow's tools may read it
  as, and one whose numbers cannot be read so is refused
  ([sandbox](../design/sandbox.md#--userns)). Only those local files are read:
  a static binary has no NSS, so accounts and ids that only NSS knows
  (LDAP, SSSD, the ids of systemd's userdb records, `systemd-homed` and
  `DynamicUser` accounts) are not checked, though a userdb drop-in record
  named `ranikust` is refused, nor are the ranges other allocators hand
  out: an NSS `subid:` module, which `newuidmap` then reads instead of the
  files, the kubelet and systemd ([sandbox](../design/sandbox.md#--userns)
  lists them). The docs advise a
  `<start>` outside them, and a warning names an NSS `subid:` module.
- **[ADR-0098](0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)'s
  refusal of an owner outside the range** applies as in every user
  namespace, before extraction; ways out that name the `ranikust` range, not
  rootless mode's, are a task of their own,
  [#208](https://github.com/tran4774/ranikust/issues/208).
- **[ADR-0074](0074-setuid-through-proc-root-of-a-full-sandbox-run-is-nosuid.md)'s
  residual, measured again** under `--userns`, is recorded where that residual
  is: section 6 of the threat model and
  [sandbox](../design/sandbox.md#--userns) (Evidence).

### Consequences

- Good, because the range's uid never gets a way into root's state directory,
  so root's setuid-root layers stay out of its reach.
- Good, because a missing range, a range that overlaps real ids, and a state
  directory that is not the range's own are refused before anything is
  written.
- Good, because root reading the credentials (#206) keeps them out of the
  range's uid's reach and needs no change to their files' modes.
- Bad, because `--userns` builds keep their own extracted copies of the layers
  they use, beside root's, and need a prune of their own (#207).
- Bad, because until #206 lands the operator must let the range's uid read the
  docker config and the secret sources a `--userns` build uses.
- Neutral, because the overlap check reads local files only, which the docs
  say, with the allocators it cannot see.

## Evidence

- ADR-0074's residual under `--userns` **[measured]** (2026-11-08, kernel 6.8,
  Ubuntu 24.04, the `userns` quick case, which asserts it): for a dumpable
  `USER 1234:1234` `RUN`, neither a host account `1234:1234` nor one with the
  ids they map to on the host reads its `/proc/<pid>/environ` or its root, while
  the latter reads both of a process of its own (the control); the `RUN`'s
  `uid_map` read from the host is `0 <start> 65536`. Recorded in
  [threat model §6](../security/threat-model.md#6-accepted-risks) and
  [sandbox](../design/sandbox.md#--userns).
- The state directory's rule is [code] (`crates/ranikust/src/statedir.rs`) with
  T0 tests, and the `userns` quick case refuses root's state directory, root's
  default, a directory below it, and one below a parent the range's uid cannot
  search, each with nothing written **[measured]** (same runs).
- That the range's uid cannot reach root's `0700` state directory, and would
  reach root's setuid-root layers were it opened to it, is **[inferred]** from
  ADR-0050's rule and the kernel's permission checks.

## More Information

Partly supersedes
[ADR-0104](0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md):
its option (d) for `--userns`, the extracted layers "under `--userns` … in the
same state directory", with the Good consequence of one store, one lock
protocol and one prune for `--userns`. The rest of ADR-0104 stands: option (a),
the `ranikust` entry refused where missing (option (g)), the key by id map in
rootless mode, and the init's uid. ADR-0104 is not wholly superseded, so its
front matter stays `accepted`. Refines
[ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md) (whose state
directory is the range's under `--userns`) and
[ADR-0003](0003-dockerfile-trust-and-default-isolation.md). Cites ADR-0074,
ADR-0098 and ADR-0136.

Answers two questions put to the maintainer in the question window while
building #83; they have no `question` issue, so this ADR closes none. The
tasks it names are #206 (root reads the credentials), #207 (prune the range's
state directory) and #208 (ADR-0098's refusal under `--userns`). The docs it
changes: [sandbox](../design/sandbox.md#--userns),
[cache-and-store](../design/cache-and-store.md), the threat model, the crate
cards of `ranikust` and `rnk-sandbox`, the book, and the pull request's
`## Changelog` entry for `--userns`.
