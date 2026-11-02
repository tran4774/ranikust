---
status: accepted
date: 2026-11-02
decision-makers: Tran Nguyen (maintainer), by the rule to follow BuildKit where security does not say otherwise (ADR-0049)
---

# ADR-0075: A layer's `trusted.*` xattrs are never set

## Context and Problem Statement

Extraction (`rnk_rootfs::extract`, `extract_live`, `extract_overlay`)
set every PAX `SCHILY.xattr.*` record of a layer as stored, strictly, with
ranikust's own capabilities: real root in overlay mode and in kaniko mode
off `/`, and on a container's `/` whatever capabilities the container
gave ranikust. `trusted.*` needs `CAP_SYS_ADMIN`, and a filesystem may
give a name there a meaning whose checks it leaves to the privileged
caller. XFS keeps its raw on-disk ACLs as `trusted.SGI_ACL_FILE` and
`trusted.SGI_ACL_DEFAULT`. By kernel source, not measured here, XFS stores
such a value without validating it, and a malformed ACL makes later
permission checks fail for root too, before `CAP_DAC_OVERRIDE` applies. A
base image could then leave directories in the store's `tmp/`, a stage
directory or a container's `/` that no cleanup can delete. The question is
which xattr namespaces a layer may set.

## Considered Options

- (a) Set every namespace, as before.
- (b) Leave out `trusted.*`, set the rest, as BuildKit (containerd's
  `archive.Apply`) does.
- (c) Keep only `security.capability`, as kaniko's extraction does.

(a) lets an image write privileged filesystem metadata nobody checks. (c)
drops `user.*`, `system.*` (POSIX ACLs) and the rest of `security.*` that
images carry and that a `RUN` may rely on.

## Decision Outcome

Chosen option (b): extraction never sets a layer's `trusted.*` xattrs, in
every mode (`Whiteouts::Apply`, `Overlay`, `Literal` and live roots). The
check is on the name, before any syscall. It counts them as
`xattrs_skipped` on the `rnk_rootfs.extract` span and logs one warning per
layer that names the count and the first. `user.*`, `security.*` and
`system.*` are set as before, strictly: a refused set fails the
extraction, with no `ENOTSUP` or `EPERM` leniency.

- BuildKit: `trusted.` names get `ENOTSUP` before the syscall and are then
  skipped with a warning (`buildkit@cd8ee244
  vendor/github.com/containerd/containerd/v2/pkg/archive/tar_unix.go:146-152`,
  `pkg/archive/tar.go:431-434`); every other namespace goes to `lsetxattr`.
- kaniko restores only `security.capability`, on regular files
  (`kaniko@44aa7ddc0 pkg/util/tar_util.go:133-144`).
- Not changed: the escaping of `user.overlay.*` in the overlay form, the
  `security.capability` failure of an unprivileged extraction, and
  `COPY`, which copies a source's xattrs leniently (Q-120). Its sources are
  roots extracted this way, the build context, or a `RUN`'s output, and a
  `RUN` holds no `CAP_SYS_ADMIN`.
- Stricter than containerd, unchanged: containerd skips `ENOTSUP` in every
  namespace and `EPERM` on `user.*` for entries other than files and
  directories, where ranikust fails the extraction.

### Consequences

- Good, because no image can write an XFS ACL, or any other
  filesystem-interpreted `trusted.*` value, with ranikust's privileges.
- Good, because overlay mode's lower layers no longer hold an image's
  `trusted.overlay.*`, already inert under `userxattr` (ADR-0016).
- Neutral, because no built layer changes: layers ranikust exports keep
  only `security.capability` (ADR-0032), and base layers are reused as
  pulled.
- Bad, because a `RUN` that reads a `trusted.*` xattr of its base image
  does not find it, as under BuildKit.

## Evidence

- [code] `crates/rnk-rootfs/src/extract.rs` `SKIPPED_XATTR`; T0
  `trusted_xattrs_from_a_layer_are_never_set` (all four modes, unprivileged
  and as root) and `extract_counts_the_trusted_xattrs_it_leaves_out`.
- [external] upstream sources cited above, read by `upstream-oracle`.
- [inferred] the XFS behaviour, from kernel source. A T2 run on an XFS
  loop image would measure it; the fix does not depend on it.

## More Information

Follows [ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md):
BuildKit over kaniko, since security does not favour kaniko's narrower
rule here. Sibling of
[ADR-0041](0041-live-root-extraction-skips-device-nodes.md), which keeps
device nodes out of live roots. Design:
[rootfs](../design/rootfs.md#extraction).

Clarification (2026-11-03, host-safety review): "extraction" includes
overlay mode's copy-up of a hardlink target from a lower layer
(`extract_overlay`, `meta::Xattrs::Layer`), which leaves out `trusted.*`
too, so a lower layer extracted before this decision, or a filesystem that
lists its own `trusted.*` to root, cannot pass one on. Extracted layers
moved to a new store layout, `layers/v3/` (`rnk-store` `key.rs`), so those
extracted earlier under `layers/v2/` are never reused. `COPY` is unchanged,
as the decision says: it copies every xattr but overlay's own. Test:
`a_hardlink_copied_from_a_lower_layer_gets_no_trusted_xattrs` (T2c).
