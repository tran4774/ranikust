---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0099: Rootless mode publishes extracted layers by rename, and runs every store operation on them inside the build's user namespace

## Context and Problem Statement

Publishing an extracted layer renames its directory from `tmp/` into `layers/`.
Renaming a directory to another parent needs write permission on the directory
itself, so a layer whose root is `0555` (some images) fails with `EACCES`
without `CAP_DAC_OVERRIDE`. As root (Phase 1) it works. Rootless overlay mode,
planned for Phase 3 ([#41](https://github.com/tran4774/ranikust/issues/41)),
extracts layers inside a user namespace, so the files hold subordinate ids, and
the user cannot rename, `chmod` or unlink entries of those ids from outside the
namespace. How are read-only layer roots published without root (#5, Q-152)?

## Considered Options

- (a) Give the layer root `u+w` until it is published, then set its mode
  (option (a) of #5).
- (b) Extract into the final directory under the lock and publish a marker
  instead of renaming (option (b) of #5).
- (c) Keep publishing by rename, and run every store operation on extracted
  layers inside the build's user namespace as its mapped root, as BuildKit's
  rootless mode does. Not in the issue's list; recommended and accepted.

(a) was not taken: its sequence (`chmod u+w`, rename, `chmod 0555`) fails outside
the namespace on a layer of subordinate ids, with `EPERM`, `EACCES` and `EPERM`
(R5 below), so it cannot work without the namespace; inside the namespace the
rename of a `0555` directory already works when its uid and gid are mapped (R2 to
R4), so the `u+w` step has nothing left to do. Where only the gid is unmapped (R6,
R7) (a) would work, but ranikust never creates such a directory, and an unmapped
owner from a tar fails at `chown` first.

(b) was not taken: a marker beside a `0555` directory is never blocked (R5's
note), so publication would work, but clean-up still cannot: a populated `0555`
tree of subordinate ids cannot be removed from outside the namespace (unlink
`EACCES`, `chmod` `EPERM`), and extraction needs the namespace to give files
their ids. (b) would therefore change the atomic rename into a marker protocol
with crash recovery of its own and still need (c)'s namespace **[inferred]**.

## Decision Outcome

In rootless mode ranikust keeps publishing an extracted layer by renaming its
directory from `tmp/` into `layers/`, and every store operation on extracted
layers (extraction, publication, clean-up after a crash, prune) runs inside the
build's user namespace as its mapped root: option (c), not in the issue's list
(the maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as recommended).

- **Planned, Phase 3.** Nothing is built by this ADR; it belongs to task 6 of
  #41 (extract and publish layers inside the namespace). Overlay as root and
  kaniko mode keep the store as it is, and so does a layer root of `0555` as
  root.
- **Every directory the store creates carries a mapped uid and a mapped gid.**
  An unmapped gid alone breaks the rename, even inside the namespace (R6, R7).
- **No layer-root mode is changed**; modes stay as the tar gives them.

### Consequences

- Good, because the publication protocol stays what
  [cache-and-store](../design/cache-and-store.md#atomic-publication) describes:
  a layer is written under `tmp/` and renamed, so a failed or killed extraction
  publishes nothing and the rename stays atomic.
- Good, because it is what BuildKit's rootless mode does: it extracts as the
  mapped root of its user namespace and refuses to run rootless otherwise
  **[external]**.
- Bad, because the store can be touched only from inside the namespace, so the
  store opens there and the whole build runs there, not only the `RUN`'s child;
  a user cannot empty their own state directory from outside it
  ([ADR-0105](0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md)).
- Bad, because on Ubuntu 24.04 by default an unconfined binary's namespace has
  no usable capabilities (see Evidence), so this path works there only with the
  sysctl at `0` or an AppArmor profile for the binary.

## Evidence

**[measured]** by `poc-runner` in the test VM, 2026-11-04. Environments: Ubuntu
24.04.5, kernel 6.8.0-147 (`kernel.apparmor_restrict_unprivileged_userns=1` at
boot), and Ubuntu 22.04.5, kernel 5.15.0-198 (no such sysctl). User `rnk`, uid
1000, with `rnk:100000:65536` in `/etc/subuid` and `/etc/subgid`; `uidmap` is not
in Ubuntu's base image, so it was installed with apt for the test. Same results
on ext4 and tmpfs; `rename(2)` and `renameat2(RENAME_NOREPLACE)` never differed.
Maps: single `0 1000 1`; multi `0 1000 1 | 1 100000 65535` through
`newuidmap` and `newgidmap`. The scripts and raw results were kept in the
session's scratchpad, not in the repository. The table is for Ubuntu 24.04 with
the restriction off (sysctl `0`) and for Ubuntu 22.04:

| Row | Scenario | `0555` directory renamed to another parent | Option (a): `chmod u+w`, rename, `chmod 0555` | Create or unlink inside the `0555` directory |
|---|---|---|---|---|
| R0 | real root, no namespace (control) | ok | ok | ok |
| R1 | plain user, no namespace, directory owned by the user | `EACCES` | ok | `EACCES` |
| R2 | single map, container root, directory owned by c0 | ok | ok | ok |
| R3 | multi map, container root, directory owned by c0 | ok | ok | ok |
| R4 | multi map, container root, directory owned by c1000 (host 100999) | ok | ok | ok |
| R5 | R4's directory, plain host user outside the namespace | `EACCES` | `EPERM`, `EACCES`, `EPERM` | `EACCES` |
| R6 | single map, uid mapped, gid unmapped (host `docker`, seen as 65534) | `EACCES` | ok | `EACCES` |
| R7 | multi map, same unmapped-gid fixture | `EACCES` | ok | `EACCES` |

- Under the single map `chown` to c1000 gives `EINVAL`; under the multi map it
  works.
- R5 with `0755` is still `EACCES`; only `0777` lets the host user move the
  directory: the write bit decides. A marker file beside a `0555` directory is
  never blocked (it needs write on the parent only). Removing a populated `0555`
  tree of subordinate ids from outside the namespace fails (unlink `EACCES`,
  `chmod` `EPERM`); a rename within the same parent from outside works.
- Ubuntu 24.04 with the default restriction, unconfined binary: the single map
  cannot be set up (`uid_map` write `EPERM`, `setgroups` write `EACCES`); the
  multi map exists with full `CapEff`, but the `unprivileged_userns` AppArmor
  profile denies every capability (`dmesg`: `apparmor="DENIED"
  operation="capable" … capname="dac_override"`), so the rename to another
  parent gives `EACCES`, `chown` to c1000 `EPERM` and `unshare(CLONE_NEWNS)`
  `EPERM`. The sysctl at `0`, or an AppArmor profile for the binary,
  `flags=(unconfined) { userns, }`, restores them (the profile was verified for
  mounting and `chown` only).
- Not covered: kernel 7.0 and kernels other than 6.8 and 5.15; overlayfs, FUSE,
  NFS or idmapped state directories; a reduced capability set; ranges not
  starting at 100000; rootless Podman or Docker user namespaces; namespaces
  created by root under the restriction; crash consistency.

**[external]** BuildKit `cd8ee2441a4c` with the containerd v2.4.2 it vendors,
read by `upstream-oracle`:

- Snapshotters extract in place and commit by metadata only
  (`cache/refs.go:1420-1456`); the overlay snapshotter renames an empty `new-*`
  directory to `snapshots/<id>` within one parent before extraction
  (`vendor/github.com/containerd/containerd/v2/plugins/snapshots/overlay/overlay.go:523-527`),
  and `Commit` is a metadata change (`300-318`).
- Rootless BuildKit extracts as the mapped root inside its user namespace:
  `cmd/buildkitd/main_oci_worker.go:209-211` refuses `--oci-worker-rootless`
  unless the euid is 0 in a user namespace; the snapshotter and applier are the
  same either way (`worker/runc/runc.go:90,155`).
- containerd's archive apply sets directory modes at creation, defers only
  mtimes (`pkg/archive/tar.go:322-338`), and never takes the layer root's (`./`)
  mode from the tar (`tar.go:264-277`).
- Crash recovery: the committed snapshot record is the marker; GC removes
  orphaned directories (`overlay.go:373-429`); temporary leases are dropped at
  worker start (`worker/base/worker.go:224-230`).

## More Information

Answers #5 (Q-152), which blocked Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Related:
[ADR-0021](0021-rnk-store-crate.md) (the store crate and its publication),
[ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md) (the rootless
state directory `~/.local/share/ranikust`),
[ADR-0098](0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)
(a one-id map and its refusal),
[ADR-0105](0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md)
(emptying the state directory),
[ADR-0106](0106-extraction-in-a-user-namespace-skips-device-nodes-and-counts-them.md)
(device nodes in rootless extraction).

Follow-ups, with task 6 of #41: `rnk-store`, `rnk-snapshot` and `rnk-rootfs`
open and extract inside the namespace, with a T1 test of a `0555` layer root
and of a `SIGKILL` during extraction. When it is built, change
[overlay-snapshot](../design/overlay-snapshot.md),
[cache-and-store](../design/cache-and-store.md), the three crate cards, add the
`## Changelog` entry if the change is user-visible, and check the threat model.
